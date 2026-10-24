//! TakeKit desktop control panel.
//!
//! Thin shell over `takekit-core`: it lists what can be recorded, starts a
//! take, polls it while it runs, and reports what landed on disk. Every
//! decision about capture and encoding lives in the core.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod control;
mod journal;
mod overlay;
mod place;
mod preview;
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
use takekit_core::take::DeleteError;
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
    /// The process behind the handle, so the picker can write it into the
    /// source. Without it the panel could only ever store `pid: 0` — the
    /// "unknown owner" that skips the recycled-handle check — and the check
    /// would exist everywhere except the one door real users come through.
    pid: u32,
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
                pid: w.pid,
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

#[derive(Serialize)]
struct CameraItem {
    name: String,
    /// `Some(false)` = the DirectShow listing tags it `(none)`: registered,
    /// but no output pin — a virtual camera whose host app is off. `None`
    /// when the listing cannot tell (older ffmpeg builds print no tag).
    live: Option<bool>,
    /// The ffmpeg backend (DirectShow) can open it.
    dshow: bool,
    /// The windows backend (Media Foundation) can open it.
    mf: bool,
}

#[tauri::command]
async fn list_devices() -> Vec<CameraItem> {
    tauri::async_runtime::spawn_blocking(|| {
        // The merged enumeration (dshow ∪ MF): one list for the picker and
        // the agent's control surface, each entry tagged with the backends
        // that can open it.
        takekit_core::devices::cameras(ffmpeg::locate().as_deref())
            .into_iter()
            .map(|c| CameraItem {
                name: c.name,
                live: c.live,
                dshow: c.dshow,
                mf: c.mf,
            })
            .collect()
    })
    .await
    .expect("list_devices task panicked")
}

#[derive(Serialize)]
struct MicItem {
    name: String,
    endpoint: String,
}

/// Microphones for the picker's sound groups. Named like the capture side
/// matches them (`Mic { device }` hits name or endpoint), so a picked row can
/// go straight into a source.
#[tauri::command]
async fn list_mics() -> Vec<MicItem> {
    tauri::async_runtime::spawn_blocking(|| {
        takekit_media::devices::microphones()
            .unwrap_or_default()
            .into_iter()
            .map(|m| MicItem {
                name: m.name,
                endpoint: m.endpoint,
            })
            .collect()
    })
    .await
    .expect("list_mics task panicked")
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

/// One still picture of a source, for the editor to draw its crop on. Off the
/// main thread: a 4K grab plus a PNG encode is real work, and a synchronous
/// command would run it on the thread that draws.
#[tauri::command]
async fn source_preview(capture: Capture) -> preview::Still {
    tauri::async_runtime::spawn_blocking(move || preview::still(&capture))
        .await
        .expect("source_preview task panicked")
}

/// The current caption of each window source, in the order asked.
///
/// A row shows a title, and titles move all the time — switch a file in an
/// editor, a tab in a browser, and the caption follows. The stored title is
/// only what it said when the source was added, so the row would keep naming a
/// file the user left an hour ago.
///
/// Read-only by construction: this must never go through `window_by_hwnd`,
/// which restores a minimized window as part of resolving. A label refresh
/// that pulled windows back onto the screen every couple of seconds would be
/// worse than the stale text. `None` means the handle is gone; the caller keeps
/// showing the last title it knew.
#[tauri::command]
async fn source_titles(captures: Vec<Capture>) -> Vec<Option<String>> {
    tauri::async_runtime::spawn_blocking(move || {
        captures
            .iter()
            .map(|capture| match capture {
                Capture::Window { hwnd, .. } | Capture::WindowAudio { hwnd, .. } => {
                    takekit_core::source::live_title(*hwnd)
                }
                _ => None,
            })
            .collect()
    })
    .await
    .expect("source_titles task panicked")
}

/// Bring a minimized window back, so its crop can be edited against a picture.
///
/// The panel never does this on its own: opening an editor must not reshuffle
/// the user's desktop, and restoring a window *does* activate it (Windows
/// semantics). So it is a button the user presses, and it answers whether the
/// window was minimized when asked.
#[tauri::command]
async fn restore_window(capture: Capture) -> Result<bool, UiError> {
    let hwnd = match capture {
        Capture::Window { hwnd, .. } | Capture::WindowAudio { hwnd, .. } => hwnd,
        _ => return Err(UiError::new("err.restore-not-a-window")),
    };
    tauri::async_runtime::spawn_blocking(move || takekit_core::source::unminimize(hwnd))
        .await
        .map_err(|e| UiError::with_detail("err.restore-failed", e))
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

/// Clear a pane — destructive on the history pane (the panel's clear is the
/// bulk form of the row's trash button): the folder behind every row goes to
/// disk-delete behind the same `take.json` guard a single delete uses, then
/// the rows and the dead log links follow. A folder that refuses to die keeps
/// its row — that row is the panel's only record that files are still there —
/// and a take that landed mid-clear is never touched at all.
///
/// The log clear stays row-only and loses no files by it: a log entry is a
/// note about a take, not the take itself (same rule as `log_delete`), and
/// its own storage is just the `journal.json` row this already rewrites.
#[tauri::command]
async fn journal_clear(pane: String) -> JournalView {
    tauri::async_runtime::spawn_blocking(move || {
        match journal::Pane::parse(&pane) {
            Some(journal::Pane::Takes) => {
                let dirs: Vec<String> = journal::Journal::load()
                    .takes
                    .iter()
                    .map(|t| t.dir.clone())
                    .collect();
                let mut deleted = std::collections::HashSet::new();
                for dir in dirs {
                    match takekit_core::take::delete_dir(&dir) {
                        Ok(()) => {
                            deleted.insert(dir);
                        }
                        Err(e) => crate::probe(&format!("clear:kept {e}")),
                    }
                }
                let _ = journal::Journal::update(|j| j.sweep_deleted_takes(&deleted));
            }
            Some(journal::Pane::Log) => {
                let _ = journal::Journal::update(|j| j.clear(journal::Pane::Log));
            }
            None => {}
        }
        journal_view()
    })
    .await
    .expect("journal_clear task panicked")
}

/// Delete one log line: the row goes, everything it might point at stays —
/// a log entry is a note about a take, not the take itself. `at` is the stamp
/// the entry carries (the row it was rendered from sends its own).
#[tauri::command]
async fn log_delete(at: u64) -> Result<JournalView, UiError> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut removed = false;
        let _ = journal::Journal::update(|j| removed = j.remove_log(at));
        if !removed {
            // Said out loud rather than a quiet no-op: the caller thinks a
            // row existed, and "it went away first" is the likely story.
            return Err(UiError::new("err.log-entry-gone"));
        }
        Ok(journal_view())
    })
    .await
    .expect("log_delete task panicked")
}

/// Delete one take: the folder on disk, the history row, and the log links
/// that point at it.
///
/// The folder goes first and its `take.json` guard (see
/// `takekit_core::take::delete_dir`) is what proves the path is one of ours —
/// a journal entry pointing anywhere else is refused and keeps its row. A
/// folder already gone (deleted outside the panel) is not an error: the row
/// is then a tombstone for nothing and goes with the delete. The log
/// *entries* stay (they are history too); only their now-dead "open the
/// folder" link is dropped.
#[tauri::command]
async fn take_delete(dir: String) -> Result<JournalView, UiError> {
    tauri::async_runtime::spawn_blocking(move || {
        takekit_core::take::delete_dir(&dir).map_err(|e| match e {
            // One user-facing sentence for all three refusals — the panel
            // shows "not a take folder" — while the precise reason (a file,
            // a missing manifest, a path with no final component) stays in
            // the `Display` the log and the control plane print.
            DeleteError::NotADirectory(_)
            | DeleteError::NotATake(_)
            | DeleteError::MissingManifest(_) => UiError::with_detail("err.not-a-take", &dir),
            DeleteError::Io(e) => UiError::with_detail("err.delete-failed", e),
        })?;
        let _ = journal::Journal::update(|j| {
            j.takes.retain(|t| t.dir != dir);
            for e in &mut j.log {
                if e.dir.as_deref() == Some(dir.as_str()) {
                    e.dir = None;
                }
            }
        });
        Ok(journal_view())
    })
    .await
    .expect("take_delete task panicked")
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

/// Grow the panel to 90% of the screen for the crop workspace. Synchronous on
/// purpose: window geometry calls want the main thread, and the caller is
/// waiting on the new size before it lays the workspace out.
#[tauri::command]
fn enter_crop_mode(window: tauri::WebviewWindow) -> Result<(), UiError> {
    place::enter_crop_mode(&window).map_err(|e| UiError::with_detail("err.internal", e))
}

/// Put the panel back where it was before the workspace grew it.
#[tauri::command]
fn exit_crop_mode(window: tauri::WebviewWindow) -> Result<(), UiError> {
    place::exit_crop_mode(&window).map_err(|e| UiError::with_detail("err.internal", e))
}

/// Open a take folder from the history pane. Synchronous: it only spawns
/// Explorer, which returns at once.
#[tauri::command]
fn open_take(dir: String) -> Result<(), UiError> {
    reveal::reveal(&reveal::take_dir(&dir)?)
}

/// One display, as the picker lists it (2026-10-21 多屏). `index` is the
/// one-based number `ScreenTarget::Monitor` takes.
#[derive(Serialize)]
struct MonitorInfo {
    index: usize,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
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
    /// Every attached display. The Windows backend captures one at a time
    /// (WGC has no virtual-desktop item), so the picker offers each one.
    monitors: Vec<MonitorInfo>,
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
        // One definition for the fallback take folder (`control`): this used
        // to carry its own copy of the expression, so `env_info` and the
        // control surface could disagree on the same empty setting.
        let default_out_root = control::default_out_root();
        let monitors = takekit_media::monitors()
            .unwrap_or_default()
            .into_iter()
            .map(|m| MonitorInfo {
                index: m.index,
                x: m.x,
                y: m.y,
                w: m.w,
                h: m.h,
            })
            .collect();
        EnvInfo {
            ffmpeg: ffmpeg.map(|p| p.to_string_lossy().into_owned()),
            encoder,
            screen: virtual_screen_rect(),
            monitors,
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
///
/// The title arrives **from the panel**, already in its language: a native
/// dialog is the one surface the webview's i18n cannot reach, so the string
/// travels into the command instead of Rust keeping its own copy of it. That
/// keeps the `i18n.js` dictionaries the single source of truth and this crate
/// free of user-facing sentences (DESIGN §4.12).
#[tauri::command]
fn pick_folder(title: &str) -> Option<String> {
    rfd::FileDialog::new()
        .set_title(title)
        .pick_folder()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Pick an executable file — the ffmpeg setting's browse button. The filter is
/// advisory (Windows lets the user type `*.*`), which is fine: `ffmpeg::locate`
/// only trusts a path that names a file.
#[tauri::command]
fn pick_file(title: &str) -> Option<String> {
    rfd::FileDialog::new()
        .set_title(title)
        .add_filter("executable", &["exe"])
        .pick_file()
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

/// Turn the full-screen stop screen on or off (B4/B6). Same split from
/// `save_session` as the countdown: the stop screen is not part of the
/// capture session either. Off means the stop progress lives on the panel's
/// status bar alone.
#[tauri::command]
async fn set_finalize_panel(on: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        config::Config::update(|cfg| cfg.finalize_panel = on)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Point the process at a specific ffmpeg, or back to the PATH lookup with an
/// empty string. Applied to the core immediately — the next prepare picks it
/// up — and remembered like every other setting.
#[tauri::command]
async fn set_ffmpeg_path(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let path = path.trim();
        let value = if path.is_empty() {
            None
        } else {
            Some(path.to_string())
        };
        ffmpeg::set_path(value.as_deref().map(std::path::PathBuf::from));
        config::Config::update(|cfg| cfg.ffmpeg_path = value)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Turn the agent's control surface on or off (`docs/agent-control.md`). Off
/// stops the server and removes the discovery file — a client that finds a
/// file must be able to trust what is in it. On brings it back on a fresh port
/// and token, so a copy of an old `control.json` stays useless.
#[tauri::command]
async fn set_control(app: tauri::AppHandle, on: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        config::Config::update(|cfg| cfg.control = on)?;
        if on {
            control::start(app).map(|_| ())
        } else {
            control::stop();
            Ok(())
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// What the panel starts from: the settings from last time.
#[tauri::command]
fn config_get() -> config::Config {
    let mut cfg = config::Config::load();
    // Sanitized here rather than in the panel: `valid_tab` is the only list of
    // real panes, and the JS trusts what it is handed instead of re-deriving.
    cfg.active_tab = cfg.effective_active_tab();
    cfg
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

/// Remember which tab was last open so a restart lands there — and so a
/// screenshot harness can seed the pane it wants without clicking anything.
#[tauri::command]
async fn set_active_tab(tab: String) -> Result<(), String> {
    // The file is hand-editable: only the four real panes store as themselves.
    let tab = config::valid_tab(&tab);
    tauri::async_runtime::spawn_blocking(move || {
        config::Config::update(|cfg| cfg.active_tab = tab.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    probe(&format!("ui:active-tab={tab}"));
    Ok(())
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

/// The window's own backdrop, which has to equal `--tk-canvas` in the theme
/// that will paint it: the native background is what shows for the frame
/// before the page's first paint, and a mismatch there reads as a flash.
///
/// `auto` cannot be answered before a window exists, so the builder gets the
/// light canvas (the WebView2 default and the majority system theme) and
/// `setup` corrects it the moment the real theme can be queried — while the
/// window is still hidden.
fn window_background(theme: &str) -> Color {
    match theme {
        "light" => Color(0xf5, 0xf6, 0xf8, 0xff),
        "dark" => Color(0x19, 0x19, 0x19, 0xff),
        _ => Color(0xf5, 0xf6, 0xf8, 0xff),
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
            cfg.backend = session.backend;
            cfg.marks = session.marks;
            cfg.raise_sources = session.raise_sources;
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

/// Stop the take's clock. Every source closes the file it is writing, so what
/// comes out is a clip per source instead of one file with dead time in it.
#[tauri::command]
async fn pause_recording(app: tauri::AppHandle) -> Result<(), UiError> {
    take::pause_recording(&app)
}

/// Start it again; each source opens its next clip on its next sample.
#[tauri::command]
async fn resume_recording(app: tauri::AppHandle) -> Result<(), UiError> {
    take::resume_recording(&app)
}

/// Add one source to the take that is recording right now.
///
/// The other half of `stop_source`: a take can grow while it runs, and what it
/// grew by is written into `take.json` with the instant its file begins. Off
/// the main thread because opening a camera takes the better part of a second
/// and the panel keeps ticking while it waits.
#[tauri::command]
async fn add_live_source(
    app: tauri::AppHandle,
    source: takekit_core::source::Source,
) -> Result<crate::proto::AddSourceResult, UiError> {
    tauri::async_runtime::spawn_blocking(move || take::add_live_source(&app, source))
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

/// The popover's way to add a source to a take that is already running: bring
/// the panel back with its picker open.
///
/// The picker itself is not duplicated into the popover — six kinds, a window
/// search and a device list do not fit in 220px, and a second copy of that
/// logic would be the one that drifts. The panel is where sources are chosen;
/// the popover only decides to go there.
#[tauri::command]
fn add_source_from_tray(app: tauri::AppHandle) {
    show_main_window(&app);
    // After the window is back, so the panel is not asked to open a picker in
    // a document that is still hidden (a hidden window paints nothing and the
    // popover would close over an empty panel).
    let _ = app.emit_to("main", "picker:open", ());
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
    let app = tauri::Builder::default()
        // First plugin: a second launch must reach the running instance's
        // callback and exit, never build a second panel over the first (two
        // panels fight over one recording lock and one hotkey). The callback
        // runs on this instance's main thread, so the same lock rules as the
        // close handler apply: try_lock only, never wait.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            let state = app.state::<take::AppState>();
            // `pending` counts too: during the countdown the recording slot is
            // still empty but the take is committed and the panel is away.
            let busy = state.starting.load(Ordering::SeqCst)
                || state.finalizing.load(Ordering::SeqCst)
                || match state.recording.try_lock() {
                    Ok(guard) => guard.is_some(),
                    Err(_) => true,
                }
                || match state.pending.try_lock() {
                    Ok(guard) => guard.is_some(),
                    Err(_) => true,
                };
            if busy {
                // Mid-take the panel hides itself (it would be in the frame);
                // the popover is the designed way back to a running take.
                crate::tray_panel::raise(app);
            } else {
                show_main_window(app);
            }
        }))
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
            // The configured ffmpeg location (empty = PATH lookup). Fed to the
            // core once here and again whenever `set_ffmpeg_path` changes it.
            ffmpeg::set_path(cfg.ffmpeg_path.as_deref().map(std::path::PathBuf::from));
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
            //
            // A create failure degrades to "no prompt layer": the take path
            // falls back to the no-veil start (take.rs), and taking the whole
            // app down over a cosmetic window would cost the recording too.
            match overlay::create(app.handle()) {
                Ok(_) => probe("overlay:ready"),
                Err(e) => probe(&format!("overlay:create-failed {e}")),
            }
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
            // `auto` is resolved here rather than at build time: the system
            // theme can only be asked of a window that exists, and the panel
            // is still hidden — the answer lands before the first visible
            // frame. Left unresolved, a dark-themed machine would flash the
            // light canvas the builder fell back to.
            if cfg.effective_theme() == "auto" {
                if let Ok(theme) = main_window.theme() {
                    let resolved = match theme {
                        tauri::Theme::Light => "light",
                        _ => "dark",
                    };
                    let _ = main_window.set_background_color(Some(window_background(resolved)));
                    probe(&format!("theme:auto->{resolved}"));
                }
            }
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
            // The agent's door (docs/agent-control.md): up before the panel
            // paints, so a client can talk to it the moment it is visible.
            // A failure here is logged, never fatal — the panel is the product
            // and a port it cannot bind is not a reason to refuse to record.
            if cfg.control {
                match control::start(app.handle().clone()) {
                    Ok(port) => probe(&format!("control:up port={port}")),
                    Err(e) => probe(&format!("control:failed {e}")),
                }
            } else {
                probe("control:off");
            }
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
            list_mics,
            check_sources,
            source_preview,
            source_titles,
            restore_window,
            enter_crop_mode,
            exit_crop_mode,
            env_info,
            preheat_engine,
            pick_folder,
            pick_file,
            start_recording,
            overlay_finish,
            overlay_cancel,
            status,
            stop_recording,
            pause_recording,
            resume_recording,
            add_live_source,
            stop_source,
            show_main,
            add_source_from_tray,
            hide_tray,
            set_hotkey,
            set_countdown,
            set_finalize_panel,
            set_ffmpeg_path,
            set_control,
            config_get,
            ui_settings,
            set_language,
            set_theme,
            set_active_tab,
            save_session,
            journal_get,
            journal_seen,
            journal_clear,
            log_delete,
            take_delete,
            log_push,
            open_take,
        ])
        .build(tauri::generate_context!())
        .expect("error while building TakeKit");
    app.run(|_app, event| {
        // The door closes with the process: a control.json left behind by a
        // clean exit is a stale file every client then has to second-guess
        // (and explain with a pid check). Removing it here is the promise
        // `control::stop` already keeps when the switch goes off.
        if matches!(event, tauri::RunEvent::Exit) {
            control::stop();
        }
    });
    probe("rust:exit");
}
