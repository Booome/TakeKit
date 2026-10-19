//! TakeKit desktop control panel.
//!
//! Thin shell over `takekit-core`: it lists what can be recorded, starts a
//! take, polls it while it runs, and reports what landed on disk. Every
//! decision about capture and encoding lives in the core.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod journal;
mod overlay;
mod place;
mod proto;
mod reveal;
mod shortcut;
mod take;
mod tray;
mod tray_panel;

use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use takekit_core::encoder;
use takekit_core::ffmpeg;
use takekit_core::source::{Capture, ScreenRect, is_recordable, virtual_screen_rect};
use tauri::Emitter;
use tauri::Manager;
use tauri::window::Color;

use proto::{SettingsInfo, StartRequest, StatusInfo, StopResult, UiError};

// PROBE-ONLY: writes lifecycle checkpoints to a file set by TAKEKIT_PROBE_LOG.
// Lets us observe page load / JS execution / IPC round-trips without trusting
// screenshots. Remove once the WebView2 rendering question is settled.
pub(crate) fn probe(stage: &str) {
    let Ok(path) = std::env::var("TAKEKIT_PROBE_LOG") else {
        return;
    };
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!("[{ms}] {stage}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

/// Fold this process's extra Chromium flags into
/// WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS, before any window exists.
///
/// Why the environment variable and not wry's per-window
/// `additional_browser_args`: WebView2 checks the variable at every
/// environment creation and the value is process-wide, so every window ends
/// up with the same arguments. That uniformity is not optional — environments
/// sharing a user data folder must have identical options, or the second one
/// fails with ERROR_INVALID_STATE (0x8007139F) and takes the app down with
/// it. A per-window flag is exactly how that mismatch happens (it replaces
/// wry's defaults for one window only), which is how the B2 veil once died.
/// Full write-up: `docs/webview2-agent-sandbox.md`.
///
/// The base is wry 0.57's own default string: both the setter and the
/// variable replace it wholesale, and losing it drops the Edge feature
/// disables plus the autoplay policy the countdown beeps depend on.
fn webview_browser_args() {
    // A value is honoured as "off" so `TAKEKIT_WEBVIEW_NO_SANDBOX=0` in a
    // shell profile cannot silently disable the renderer sandbox.
    let no_sandbox = std::env::var("TAKEKIT_WEBVIEW_NO_SANDBOX")
        .map(|v| {
            // Empty / 0 / false / off / no all mean "off": a stray
            // `export TAKEKIT_WEBVIEW_NO_SANDBOX=` must not downgrade the
            // renderer sandbox silently.
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(false);
    let extra = std::env::var("TAKEKIT_WEBVIEW_ARGS")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let external = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if !no_sandbox && extra.is_none() && external.is_none() {
        return;
    }

    // wry 0.57, webview2/mod.rs: the default string is the Edge feature
    // disables plus the autoplay policy (autoplay defaults to true). Under
    // --no-sandbox the renderer's code-integrity check is dropped as well and
    // rides in the SAME switch: Chromium takes the last --disable-features
    // rather than the union, and that check is what aborts the renderer on the
    // sandbox's unsigned DLL (0x80000003, a FAIL_FAST breakpoint).
    let features = if no_sandbox {
        "msWebOOUI,msPdfOOUI,msSmartScreenProtection,RendererCodeIntegrity"
    } else {
        "msWebOOUI,msPdfOOUI,msSmartScreenProtection"
    };
    let defaults =
        format!("--disable-features={features} --autoplay-policy=no-user-gesture-required");
    // wry's string is always the base: dropping it would drop the autoplay
    // policy the countdown beeps depend on, and the Edge feature disables.
    // An externally set WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS is appended
    // after it — that is the documented way to pass flags such as
    // --remote-debugging-port — and wins wherever Chromium takes the last
    // value of a switch (a user override is the user's call).
    let mut args = defaults;
    if let Some(external) = external {
        args.push(' ');
        args.push_str(&external);
        probe("external webview args merged");
    }
    if no_sandbox {
        // Only needed under an injected agent sandbox, whose DLL makes the
        // Chromium renderer fail its integrity check and abort — the app then
        // dies mid-page-load with no Rust panic to show. A security downgrade:
        // never on in normal use.
        args.push_str(" --no-sandbox");
        probe("no-sandbox enabled");
    }
    if let Some(extra) = extra {
        args.push(' ');
        args.push_str(&extra);
        probe(&format!("extra args: {extra}"));
    }
    // Sound because this runs as the first statement of main(), before Tauri
    // spawns any thread and before any other code can read the environment:
    // the documented single-threaded window for set_var. WebView2 reads the
    // variable when each environment is created, which is later still.
    unsafe {
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", &args);
    }
}

#[tauri::command]
fn ui_ping(stage: String) {
    probe(&format!("ui:{stage}"));
}

#[derive(Serialize)]
struct WindowItem {
    hwnd: isize,
    title: String,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

#[tauri::command]
async fn list_windows(filter: String) -> Vec<WindowItem> {
    // EnumWindows walks every top-level window and reads its title; small, but
    // it is still machine probing and belongs on the blocking pool like the
    // rest (a synchronous command would run it on the UI thread).
    tauri::async_runtime::spawn_blocking(move || {
        // Our own windows are dropped: recording the panel would only ever
        // capture a take of the thing that is running the take.
        takekit_core::source::list_windows(&filter, Some(std::process::id()))
            .into_iter()
            .map(|w| WindowItem {
                hwnd: w.hwnd,
                title: w.title,
                x: w.rect.x,
                y: w.rect.y,
                w: w.rect.w,
                h: w.rect.h,
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

// Every command that probes the machine (ffmpeg subprocesses) or touches the
// recording lock runs off the main thread: synchronous commands execute on it,
// and one 2-second probe would freeze rendering, input and window dragging.

#[tauri::command]
async fn list_devices() -> Vec<String> {
    tauri::async_runtime::spawn_blocking(|| match ffmpeg::locate() {
        Some(ff) => ffmpeg::list_capture_devices(&ff),
        None => Vec::new(),
    })
    .await
    .expect("list_devices task panicked")
}

/// Which of these captures are still worth recording. A window source whose
/// window has closed, been hidden, or gone to another virtual desktop (or had
/// its app suspended) gets dropped by the panel instead of being offered: a
/// take of it would record whatever is behind it.
#[tauri::command]
async fn check_sources(captures: Vec<Capture>) -> Vec<bool> {
    tauri::async_runtime::spawn_blocking(move || captures.iter().map(is_recordable).collect())
        .await
        .expect("check_sources task panicked")
}

/// Everything the two list panes need, plus the unread counts behind the tab
/// markers. Read as a whole because the panes are redrawn together.
#[derive(Serialize)]
struct JournalView {
    takes: Vec<journal::TakeRecord>,
    log: Vec<journal::LogRecord>,
    unread_takes: usize,
    unread_log: usize,
    /// The worst level among the unread entries: it decides the tab's colour.
    unread_log_level: Option<journal::Level>,
}

fn journal_view() -> JournalView {
    let journal = journal::Journal::load();
    let unread_log_level = journal
        .log
        .iter()
        .filter(|e| e.at > journal.seen_log)
        .map(|e| e.level)
        .max();
    JournalView {
        unread_takes: journal.unread(journal::Pane::Takes),
        unread_log: journal.unread(journal::Pane::Log),
        unread_log_level,
        takes: journal.takes,
        log: journal.log,
    }
}

#[tauri::command]
async fn journal_get() -> JournalView {
    tauri::async_runtime::spawn_blocking(journal_view)
        .await
        .expect("journal_get task panicked")
}

/// The pane was looked at: its marker goes away, the entries do not.
#[tauri::command]
async fn journal_seen(pane: String) -> JournalView {
    tauri::async_runtime::spawn_blocking(move || {
        if let Some(pane) = journal::Pane::parse(&pane) {
            let _ = journal::Journal::update(|j| j.mark_seen(pane));
        }
        journal_view()
    })
    .await
    .expect("journal_seen task panicked")
}

#[tauri::command]
async fn journal_clear(pane: String) -> JournalView {
    tauri::async_runtime::spawn_blocking(move || {
        if let Some(pane) = journal::Pane::parse(&pane) {
            let _ = journal::Journal::update(|j| j.clear(pane));
        }
        journal_view()
    })
    .await
    .expect("journal_clear task panicked")
}

/// Write one warning or error to the log. The text is the same one the notice
/// showed: the pane is where a dismissed notice is still readable.
#[tauri::command]
async fn log_push(
    level: String,
    text: String,
    detail: Option<String>,
    dir: Option<String>,
) -> JournalView {
    tauri::async_runtime::spawn_blocking(move || {
        let entry = journal::LogRecord {
            at: journal::now_ms(),
            level: if level == "error" {
                journal::Level::Error
            } else {
                journal::Level::Warn
            },
            text,
            detail,
            dir,
        };
        let _ = journal::Journal::update(|j| j.push_log(entry));
        journal_view()
    })
    .await
    .expect("log_push task panicked")
}

/// Open a take folder from the history pane. Synchronous: it only spawns
/// Explorer, which returns at once.
#[tauri::command]
fn open_take(dir: String) -> Result<(), String> {
    reveal::reveal(&reveal::take_dir(&dir)?)
}

/// What the panel needs before it can offer to record at all.
#[derive(Serialize)]
struct EnvInfo {
    ffmpeg: Option<String>,
    encoder: Option<String>,
    /// Virtual-screen rect (all monitors, taskbar included) — the same
    /// rectangle a desktop source captures, so the frontend can preheat the
    /// engine probe for it while the user is still picking sources.
    screen: ScreenRect,
    /// Where a take lands unless the user picks somewhere else: the temp dir.
    default_out_root: String,
}

#[tauri::command]
async fn env_info() -> EnvInfo {
    tauri::async_runtime::spawn_blocking(|| {
        let ffmpeg = ffmpeg::locate();
        let encoder = ffmpeg
            .as_deref()
            .map(|f| encoder::pick(f).name().to_string());
        // trim_end_matches: the env var keeps a trailing separator, which
        // reads as noise in a text field.
        let default_out_root = std::env::temp_dir()
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_string();
        EnvInfo {
            ffmpeg: ffmpeg.map(|p| p.to_string_lossy().into_owned()),
            encoder,
            screen: virtual_screen_rect(),
            default_out_root,
        }
    })
    .await
    .expect("env_info task panicked")
}

/// Start the engine probe for `rect` in the background so `start_recording`
/// can reuse it (callers fire and forget — the first start pays 2s otherwise).
#[tauri::command]
async fn preheat_engine(rect: ScreenRect) {
    let _ = tauri::async_runtime::spawn_blocking(move || {
        if let Some(ff) = ffmpeg::locate() {
            let _ = ffmpeg::dda_available(&ff, rect);
        }
    })
    .await;
}

/// Native folder picker for the output root; `None` when the dialog is
/// cancelled. Synchronous on purpose: a modal Win32 dialog runs its own
/// message pump, and the UI behind it is blocked by the modality anyway.
#[tauri::command]
fn pick_folder() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("Choose output folder")
        .pick_folder()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Start a take. The prepare step probes the machine (up to two seconds), so it
/// runs off the main thread: a synchronous command executes on it, which would
/// freeze rendering, input and window dragging. The take itself only starts
/// when the veil's countdown is over (`overlay_finish`).
#[tauri::command]
async fn start_recording(req: StartRequest, app: tauri::AppHandle) -> Result<bool, UiError> {
    tauri::async_runtime::spawn_blocking(move || take::begin(&app, req))
        .await
        .map_err(|e| UiError::with_detail("err.internal", e))?
}

/// The countdown reached the end of GO: take the veil down, then start the
/// encoders. The order is the point — the veil is a screen-covering window,
/// and every screen capture (desktop *and* window sources, which record the
/// screen rectangle their window occupies) would otherwise record the last
/// part of GO. The settle gives the compositor a frame to drop it before
/// ffmpeg starts; what remains is ffmpeg's own start-up (a few hundred ms),
/// which no ordering can remove.
#[tauri::command]
async fn overlay_finish(app: tauri::AppHandle) -> Result<(), UiError> {
    overlay::hide(&app);
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        take::spawn(&handle).map(|_| ())
    })
    .await
    .map_err(|e| UiError::with_detail("err.internal", e))?
}

/// Esc during the countdown: drop the prepared take before anything starts.
#[tauri::command]
async fn overlay_cancel(app: tauri::AppHandle) {
    // A panicking cancel would leave the countdown stuck; there is nothing to
    // report back to the veil, so the failure only reaches the probe log.
    if tauri::async_runtime::spawn_blocking(move || take::cancel(&app))
        .await
        .is_err()
    {
        probe("overlay:cancel-panicked");
    }
}

/// Replace the global hotkey. `take::install_hotkey` re-registers the previous
/// one when this one is rejected, so a failed change never leaves the panel
/// without a way to start or stop. Returns the normalised form to display.
#[tauri::command]
async fn set_hotkey(app: tauri::AppHandle, hotkey: String) -> Result<String, UiError> {
    tauri::async_runtime::spawn_blocking(move || {
        let normalized = take::install_hotkey(&app, &hotkey)?;
        config::Config::update(|cfg| cfg.hotkey = normalized.clone())
            .map_err(|e| UiError::with_detail("err.save", e))?;
        Ok(normalized)
    })
    .await
    .map_err(|e| UiError::with_detail("err.internal", e))?
}

/// Turn the full-screen countdown on or off. The panel has a switch (the
/// seconds input arrives with B5), so it sends the value: any non-zero number
/// of seconds is "on", `0` is off. Written straight to the config rather than
/// through `save_session` — the countdown is not part of the capture session,
/// and a stale echo of it must never ride along with one.
#[tauri::command]
async fn set_countdown(secs: u8) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        config::Config::update(|cfg| cfg.countdown_secs = secs)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// What the panel starts from: the settings from last time.
#[tauri::command]
fn config_get() -> config::Config {
    config::Config::load()
}

/// The language and palette in force, as the file records them.
fn current_settings() -> SettingsInfo {
    let cfg = config::Config::load();
    SettingsInfo {
        language: cfg.effective_language(),
        theme: cfg.effective_theme(),
    }
}

/// The current pair, for a window that came up after a change was made.
#[tauri::command]
fn ui_settings() -> SettingsInfo {
    current_settings()
}

/// Switch the UI language.
///
/// Every window is told (`ui:settings`) rather than only the caller: the tray
/// popover and the panel are separate documents, and one of them changing
/// language while the other keeps the old one is a half-translated app.
#[tauri::command]
async fn set_language(app: tauri::AppHandle, language: String) -> Result<SettingsInfo, String> {
    // The panel only offers the two, but the command is reachable from a
    // hand-edited config too: an unknown value must land on a language this
    // build has strings for.
    let lang = if language == "en" { "en" } else { "zh" };
    tauri::async_runtime::spawn_blocking(move || {
        config::Config::update(|cfg| cfg.language = lang.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let info = current_settings();
    let _ = app.emit("ui:settings", &info);
    probe(&format!("ui:language={}", info.language));
    Ok(info)
}

/// Switch the palette (`auto` / `light` / `dark`).
#[tauri::command]
async fn set_theme(app: tauri::AppHandle, theme: String) -> Result<SettingsInfo, String> {
    let theme = match theme.as_str() {
        "light" => "light",
        "dark" => "dark",
        _ => "auto",
    };
    tauri::async_runtime::spawn_blocking(move || {
        config::Config::update(|cfg| cfg.theme = theme.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let info = current_settings();
    let _ = app.emit("ui:settings", &info);
    probe(&format!("ui:theme={}", info.theme));
    Ok(info)
}

/// Seeds the UI language and palette in the page before its first paint.
///
/// The stylesheets follow the system unless `<html data-theme>` says otherwise,
/// and i18n.js starts from the seeded language. Both have to be known before
/// the first frame — otherwise a user who picked dark or English sees the
/// opposite for a moment on every start. "auto" writes no theme: the media
/// query is already the right answer.
pub(crate) fn ui_script(language: &str, theme: &str) -> String {
    let mut script = format!("window.__TAKEKIT_LANG__ = \"{language}\";");
    if matches!(theme, "light" | "dark") {
        script.push_str(&format!(" window.__TAKEKIT_THEME__ = \"{theme}\";"));
    }
    script
}

/// The colour behind the page until it paints.
///
/// An explicit choice must not flash the opposite palette while the webview is
/// still loading, so it matches `--tk-canvas`. `auto` keeps the neutral dark
/// grey: reading the system preference means the registry, and a mid-grey
/// frame reads as "starting" in either palette.
fn window_background(theme: &str) -> Color {
    match theme {
        "light" => Color(0xf4, 0xf4, 0xf4, 0xff),
        "dark" => Color(0x19, 0x19, 0x19, 0xff),
        _ => Color(0x20, 0x20, 0x20, 0xff),
    }
}

/// Remember the capture setup, so a repeated take needs no re-picking.
#[tauri::command]
async fn save_session(session: proto::SessionConfig) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        config::Config::update(|cfg| {
            cfg.sources = session.sources;
            cfg.out_root = session.out_root;
            cfg.encoder = session.encoder;
            cfg.marks = session.marks;
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

// Async even though the snapshot itself is cheap: `start_recording` holds the
// lock for the length of a prepare, and a synchronous status would block the
// main thread on it.
#[tauri::command]
async fn status(app: tauri::AppHandle) -> StatusInfo {
    tauri::async_runtime::spawn_blocking(move || take::status(&app))
        .await
        .expect("status task panicked")
}

/// Finish the take and report what landed on disk. Runs off the main thread:
/// finalising blocks until every encoder has written its trailer.
///
/// `window` is the caller: the panel gets the result back from its own
/// command, while the tray (a different window) needs the outcome as an event
/// or nobody would see it.
#[tauri::command]
async fn stop_recording(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
) -> Result<StopResult, UiError> {
    let announce = window.label() != "main";
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || take::stop(&handle, announce))
        .await
        .map_err(|e| UiError::with_detail("err.internal", e))?
}

/// Cut one source out of a running take, leaving the others alone. The panel
/// calls this once a status tick shows a source that is no longer usable.
#[tauri::command]
async fn stop_source(app: tauri::AppHandle, id: String) -> bool {
    tauri::async_runtime::spawn_blocking(move || take::stop_source(&app, &id))
        .await
        .expect("stop_source task panicked")
}

/// Bring the panel back: the popover's "显示主窗口", and the way out of the
/// countdown, a cancelled take and a start that failed.
pub(crate) fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
    // The popover has done its job; leaving it on top of the window it just
    // asked for is noise.
    hide_tray_window(app);
    probe("ui:show-main");
}

#[tauri::command]
fn show_main(app: tauri::AppHandle) {
    show_main_window(&app);
}

/// Take the popover down. It closes itself when it loses focus, which a click
/// inside it does not do — `Esc` and "显示主窗口" both mean it is done.
pub(crate) fn hide_tray_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window(tray_panel::LABEL) {
        tray_panel::hide(&window);
    }
}

#[tauri::command]
fn hide_tray(app: tauri::AppHandle) {
    hide_tray_window(&app);
}

fn main() {
    // Before any window exists: WebView2 reads
    // WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS when each environment is
    // created, and one value then covers the whole process — the tray panel,
    // the veil and the panel itself all share one argument string. That
    // uniformity is not optional; see the function.
    webview_browser_args();
    probe("rust:setup");
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(take::AppState::default())
        // The window is built hidden (see `.visible(false)` below) and shown
        // once it has been placed, so it never flashes at the default spot
        // before the remembered position lands.
        .setup(|app| {
            // Read once at startup: the panel's global hotkey and the two
            // recording-feedback switches live here. A missing or corrupt file
            // just means defaults; see Config::load_from.
            let cfg = config::Config::load();
            probe(&format!(
                "config:hotkey={} countdown={} finalize={} sources={} out_root={} lang={} theme={}",
                cfg.effective_hotkey(),
                cfg.countdown_secs,
                cfg.finalize_panel,
                cfg.sources.len(),
                cfg.out_root.as_deref().unwrap_or("-"),
                cfg.effective_language(),
                cfg.effective_theme()
            ));
            // A hotkey that cannot be registered is not fatal — the panel still
            // works by button. The reason goes to the probe log for now; the
            // settings UI surfaces it later.
            match take::install_hotkey(app.handle(), &cfg.effective_hotkey()) {
                Ok(spec) => probe(&format!("hotkey:ready {spec}")),
                Err(reason) => probe(&format!("hotkey:unavailable: {reason}")),
            }

            // The tray is the only way back to a stopped take once the main
            // window hides itself during recording.
            tray::setup(app.handle())?;
            probe("tray:ready");

            // Built hidden so its page is loaded before the first click on the
            // icon; the click then only has to place and show it.
            tray_panel::create(
                app.handle(),
                &cfg.effective_language(),
                &cfg.effective_theme(),
            )?;
            probe("tray:panel-ready");

            // The prompt layer: built hidden so its page is ready the moment a
            // take starts (see overlay.rs). TAKEKIT_OVERLAY_DEMO shows it at
            // startup instead, which is how the cover gets measured.
            overlay::create(app.handle())?;
            probe("overlay:ready");
            if std::env::var("TAKEKIT_OVERLAY_DEMO").is_ok() {
                probe("overlay:demo");
                overlay::show(app.handle());
                // It comes back down on its own: long enough to look at the
                // cover (and photograph it), short enough that the "nothing is
                // eaten while it is hidden" half can be checked in one run.
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    overlay::hide(&handle);
                });
            }

            // The window is built here rather than in tauri.conf.json so that
            // the debug-only `--no-sandbox` flag stays out of normal builds:
            // that flag disables the Chromium renderer sandbox, which we only
            // need when an injected agent sandbox would otherwise crash the
            // renderer. It is passed process-wide through
            // WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS (see webview_browser_args)
            // — never per window, because environments that share a user data
            // folder must have identical options.
            let mut window = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("TakeKit")
            // One fixed size: the panel is a control strip, not a document
            // window, so a remembered size is one more thing to get wrong. The
            // source list scrolls instead of the window growing.
            .inner_size(900.0, 640.0)
            .resizable(false)
            // `resizable(false)` drops the resize frame but leaves the maximize
            // box live on Windows, which would let the panel leave its one size.
            .maximizable(false)
            // Built hidden and shown once it is placed. A visible window is
            // mapped at its default spot for a frame before the remembered
            // position lands, which reads as the panel flicking to the corner
            // on every start.
            .visible(false)
            .background_color(window_background(&cfg.effective_theme()))
            .initialization_script(ui_script(&cfg.effective_language(), &cfg.effective_theme()));

            // Placed at build time, not moved into place afterwards: the panel
            // has to appear where it was, never at the default spot first.
            let place = place::restore(app.handle(), cfg.window_pos);
            match place {
                Some((x, y)) => {
                    probe(&format!("place:restore {x},{y}"));
                    window = window.position(x, y);
                }
                None => probe("place:none"),
            }

            // Chromium flags for the whole process are folded into
            // WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS in main() — including
            // TAKEKIT_WEBVIEW_NO_SANDBOX=1 and the TAKEKIT_WEBVIEW_ARGS escape
            // hatch. Nothing is passed per window here: see the comment above
            // and docs/webview2-agent-sandbox.md.

            let main_window = window.build()?;
            match main_window.outer_position() {
                Ok(p) => probe(&format!("place:actual {},{}", p.x, p.y)),
                Err(e) => probe(&format!("place:unknown {e}")),
            }
            place::watch_moves(&main_window);
            // The veil and the popover are hidden windows and they keep the
            // process alive, so closing the panel must exit explicitly — the
            // alternative is an app that looks closed but keeps running with
            // no way to bring it back (there is no tray menu).
            main_window.on_window_event({
                let app = app.handle().clone();
                move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let state = app.state::<take::AppState>();
                        // Never wait for the recording lock here. `begin`
                        // holds it while calling tray APIs, and those block
                        // until this very thread runs them — waiting would
                        // deadlock the event loop. A contended lock means work
                        // is in flight, and work in flight means "do not exit".
                        let busy = state.starting.load(Ordering::SeqCst)
                            || state.finalizing.load(Ordering::SeqCst)
                            || match state.recording.try_lock() {
                                Ok(guard) => guard.is_some(),
                                Err(_) => true,
                            };
                        if busy {
                            // A take (or its finalise) has to survive an
                            // accidental close: hide the panel, stop from the
                            // tray when it is over.
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.hide();
                            }
                            probe("main:close-hidden");
                        } else {
                            app.exit(0);
                        }
                    }
                }
            });
            // Warm the DDA verdict while the user is still picking sources:
            // the probe is ~2.6s of D3D init and must never run on the click
            // path (`Session::prepare` only reads the cache).
            std::thread::spawn(|| {
                if let Some(ff) = takekit_core::ffmpeg::locate() {
                    let _ = takekit_core::ffmpeg::dda_available(
                        &ff,
                        takekit_core::source::virtual_screen_rect(),
                    );
                }
            });
            main_window.show()?;
            Ok(())
        })
        // Page loads are logged for every window (main, overlay, tray), which
        // is what shows the order they come up in. The window itself is shown
        // in setup — `webview.show()` here would only flip the webview's own
        // visibility, not the window's.
        .on_page_load(|_webview, payload| {
            let label = format!("{:?}", payload.event());
            probe(&format!("page_load:{label} url={}", payload.url()));
        })
        .invoke_handler(tauri::generate_handler![
            ui_ping,
            list_windows,
            list_devices,
            check_sources,
            env_info,
            preheat_engine,
            pick_folder,
            start_recording,
            overlay_finish,
            overlay_cancel,
            status,
            stop_recording,
            stop_source,
            show_main,
            hide_tray,
            set_hotkey,
            set_countdown,
            config_get,
            ui_settings,
            set_language,
            set_theme,
            save_session,
            journal_get,
            journal_seen,
            journal_clear,
            log_push,
            open_take,
        ])
        .run(tauri::generate_context!())
        .expect("error while running TakeKit");
    probe("rust:exit");
}
