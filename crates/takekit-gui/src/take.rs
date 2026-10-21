//! The take lifecycle: prepare → countdown → start → poll → stop.
//!
//! The tauri commands, the tray panel and the global hotkey all end up here, so
//! the ordering rules (windows exist before capture starts, the recording lock
//! is held across a prepare) live in exactly one place.
//!
//! Since B3 a start is two steps: `begin` prepares the session and hands the
//! screen to the veil with a countdown, `spawn` starts the encoders when the
//! countdown reaches GO. Between them the take waits in `AppState.pending` and
//! can still be thrown away with `cancel` (Esc) — nothing has touched the disk
//! yet, which is what makes a cancel free.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use takekit_core::session::{Prepared, Recording, Session, Settings};
use takekit_core::source::{Backend, Capture};
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::proto::{
    BeepInfo, CountdownInfo, FinalizeInfo, LiveSourceInfo, OutcomeInfo, StartRequest, StartResult,
    StatusInfo, StopResult, UiError,
};
use crate::shortcut;

#[derive(Default)]
pub struct AppState {
    pub recording: Mutex<Option<Recording>>,
    /// A prepared take waiting for its countdown to reach GO. `Some` between
    /// `begin` and `spawn`/`cancel`; `None` lets a new take begin.
    pub pending: Mutex<Option<Prepared>>,
    /// Why the last begin/spawn failed. Cleared when a take is asked for; the
    /// panel's failed state reads it through `status` (B5).
    pub last_error: Mutex<Option<String>>,
    /// True from the moment `stop` takes the recording until the take is
    /// written out. The recording is already out of `recording` by then, so
    /// this flag is the only thing that says "the process must not exit yet".
    pub finalizing: AtomicBool,
    /// True while `begin` is preparing. The recording lock is not held across
    /// the probe, so this is what keeps two starts from racing.
    pub starting: AtomicBool,
    /// The stop flag of the thread watching for Esc while a take counts down.
    /// `None` when nothing is counting down.
    pub esc_watch: Mutex<Option<Arc<AtomicBool>>>,
    /// The shortcut currently registered, so replacing it unregisters exactly
    /// the old one instead of dropping every registration.
    pub hotkey: Mutex<Option<Shortcut>>,
}

/// Prepare a take and hand the screen to the veil: raise the window sources,
/// probe the toolchain, then count down. Nothing is recorded yet — `spawn`
/// starts the encoders when the countdown reaches GO.
///
/// The panel is hidden here, not after the countdown: window topology has to
/// settle before the first capture starts (DESIGN §4.6), and the veil is what
/// the user watches instead. A prepare failure returns early with the panel
/// still on screen, which is where its banner and the 220 Hz beep belong.
///
/// Returns whether the take started right away (countdown off). With a
/// countdown this is `false`, and `take:started` arrives later; the return
/// value exists so the panel can tell the two cases apart even when the event
/// beats the command response (it did in the countdown-off path).
pub fn begin(app: &tauri::AppHandle, req: StartRequest) -> Result<bool, UiError> {
    let state = app.state::<AppState>();
    // One begin at a time. The lock below is only taken for the checks: the
    // prepare can take seconds, and holding a lock across it would let the
    // window's close handler (which runs on the event loop) wait for it — the
    // same path that makes tray calls block on the main thread.
    if state.starting.swap(true, Ordering::SeqCst) {
        return Err(UiError::new("err.starting"));
    }
    let _starting = StartingGuard(&state.starting);

    if state.recording.lock().unwrap().is_some() {
        return Err(UiError::new("err.already-recording"));
    }
    if state.pending.lock().unwrap().is_some() {
        return Err(UiError::new("err.countdown"));
    }
    if req.sources.is_empty() {
        return Err(UiError::new("err.no-sources"));
    }
    *state.last_error.lock().unwrap() = None;

    // Wake the veil's audio before the probe below: creating an AudioContext
    // inside the first beep makes that beep land late (device init and resume
    // are async), and the countdown is judged by ear — a late first beep
    // shortens the first interval with nothing else looking wrong. The probe
    // can take seconds, which is exactly the free window this needs.
    let _ = app.emit_to(crate::overlay::LABEL, "overlay:wake", ());

    // A window source records whatever is painted where that window is, so a
    // window another app has covered would record the other app. Raise them
    // first: a minimized window has no usable rectangle until it is back, and
    // the plan built below — which capture engine to use, and the rectangle to
    // probe it at — is read off that rectangle. A window that has closed by now
    // is left to fail in `prepare`, which reports it properly; a plan that
    // fails for some other reason leaves the window where it was raised, which
    // is the price of measuring it only once it is back.
    //
    // The rank is logged with the result because `bring_to_front` can only
    // report so much: Windows 11 drops a plain raise from a background process
    // without saying so, and the ranks are what show whether the window moved.
    // Two separate decisions here, and only the second one is the user's.
    //
    // Un-minimizing is not optional: DWM stops composing a minimized window
    // entirely, so WGC delivers zero frames for one (measured — 45 frames
    // visible, 0 minimized, 46 back) and its client area reads 0x0 to
    // measure. A take of a still-minimized window would be empty, so it comes
    // back first, without the keyboard focus.
    //
    // Where it lands afterwards IS the user's call (`raise_sources`). On: the
    // front, which `bring_to_front` does along with the un-minimize. Off
    // (default): out of the way — it must be on screen to be captured, but it
    // must not cover the work the user is actually doing. Sinking it is free
    // for the Windows backend (WGC records the window itself however covered)
    // and wrong for the ffmpeg one (it records the screen area and would film
    // the cover), so that backend keeps whatever placement the restore gave it.
    for source in &req.sources {
        if let Capture::Window { hwnd, title, .. } = &source.capture {
            // The policy lives in core (it turns on capture facts, not UI
            // ones); the probe text is the panel's business. `sink` tells
            // core whether this backend ignores Z-order (WGC does, ffmpeg
            // does not).
            let sink = req.backend == Backend::Windows;
            match takekit_core::source::place_for_capture(*hwnd, req.raise_sources, sink) {
                takekit_core::source::PlacementOutcome::Raised { ok, before, after } => {
                    crate::probe(&format!(
                        "front:{title}={} rank {before:?}->{after:?}",
                        u8::from(ok)
                    ));
                }
                takekit_core::source::PlacementOutcome::Restored { sunk } => {
                    crate::probe(&format!("restored:{title} sink={}", u8::from(sunk)));
                }
                takekit_core::source::PlacementOutcome::Untouched => {}
            }
        }
    }

    let secs = crate::config::Config::load().countdown_secs;
    let prepared = Session::prepare(Settings {
        sources: req.sources,
        out_root: std::path::PathBuf::from(req.out_root),
        quality: req.quality,
        encoder: req.encoder,
        marks: req.marks,
        backend: req.backend,
    })
    .map_err(|e| {
        // The panel is still on screen and reports this through the rejected
        // command, but `status` is the other reader of the same fact (B5's
        // failed state) — a failure it cannot see would look like a take that
        // silently never happened.
        //
        // The code decides the sentence, the detail is whatever the core said:
        // the one failure users actually hit gets its own actionable wording.
        let err = match e {
            takekit_core::session::Error::NoFfmpeg => UiError::new("err.no-ffmpeg"),
            other => UiError::with_detail("err.prepare-failed", other),
        };
        *state.last_error.lock().unwrap() = Some(err.to_string());
        crate::probe(&format!("begin:failed {err}"));
        err
    })?;
    // Where the click-to-countdown time went: locate / encoder / engine.
    crate::probe(&format!("prepare: {:?}", prepared.timings()));
    crate::probe(&format!(
        "engine:{:?} probed={}",
        prepared.engine(),
        prepared.engine_probed()
    ));

    if secs == 0 {
        *state.pending.lock().unwrap() = Some(prepared);
        // From here the take is committed: the panel steps aside, and only the
        // spawn can follow.
        crate::tray::set_recording(app, true);
        crate::hide_tray_window(app);
        // Countdown off (DESIGN §4.6): no veil, a 50ms settle for the window
        // hide, then straight to spawn. The short beep is asked for while the
        // panel is still on screen — the tray dot is the other half of the
        // "it started" signal, and the cost (the first 0.3–0.5s of the take
        // is empty) is the documented price of turning the countdown off.
        if let Some(window) = app.get_webview_window("main") {
            let _ = app.emit_to("main", "take:beep", &BeepInfo { hz: 440, ms: 120 });
            let _ = window.hide();
        }
        crate::probe("countdown:off");
        std::thread::sleep(Duration::from_millis(50));
        // The panel is still awaiting this command: a failure is reported by
        // the rejected invoke, not by a `take:failed` event it would show a
        // second time. The UI is put back after the lock is released.
        let outcome = {
            let mut guard = state.recording.lock().unwrap();
            spawn_started(&state, &mut guard)
        };
        return match outcome {
            Ok(_) => Ok(true),
            Err(err) => {
                crate::tray::set_recording(app, false);
                crate::show_main_window(app);
                Err(err)
            }
        };
    }

    // The veil has to be up before the take is committed. Committing first and
    // discovering the veil cannot show would leave a pending take with no
    // clock, no GO and no exit except guessing Esc.
    if !crate::overlay::show(app) {
        let err = UiError::new("err.overlay");
        *state.last_error.lock().unwrap() = Some(err.to_string());
        crate::probe("countdown:overlay-missing");
        return Err(err);
    }

    *state.pending.lock().unwrap() = Some(prepared);
    // From here the take is committed: the panel steps aside, and only the
    // countdown (GO → `spawn`) or Esc (`cancel`) can follow.
    crate::tray::set_recording(app, true);
    // The popover has no business hovering over the veil — and its stop key
    // would land on a take that has not started yet.
    crate::hide_tray_window(app);

    let _ = app.emit_to(
        crate::overlay::LABEL,
        "overlay:countdown",
        &CountdownInfo { secs },
    );
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
    start_esc_watch(app, &state);
    crate::probe(&format!("countdown:begin secs={secs}"));
    Ok(false)
}

/// Watch for Esc while a take is counting down.
///
/// The veil is deliberately unfocusable — taking focus would deactivate the
/// window the user is about to record — so a keypress lands on whatever window
/// is in front, and the veil's own keydown handler would never see it. Polling
/// is the one way to watch Esc that neither steals focus nor installs a global
/// hook: 30ms is far below what reads as a delay for a cancel.
fn watch_esc(app: tauri::AppHandle, stop: Arc<AtomicBool>) {
    // Clear a press that landed before this thread existed: a tap in the last
    // few milliseconds must not cancel a take the user just asked for.
    takekit_core::source::escape_pressed();
    while !stop.load(Ordering::Relaxed) {
        if takekit_core::source::escape_pressed() {
            cancel(&app);
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Start (or restart) the Esc watcher. Only one runs at a time: the previous
/// flag is raised first, so a watcher can never outlive its countdown.
fn start_esc_watch(app: &tauri::AppHandle, state: &tauri::State<'_, AppState>) {
    stop_esc_watch(state);
    let stop = Arc::new(AtomicBool::new(false));
    std::thread::spawn({
        let stop = Arc::clone(&stop);
        let app = app.clone();
        move || watch_esc(app, stop)
    });
    *state.esc_watch.lock().unwrap() = Some(stop);
}

/// Retire the Esc watcher, if one is running. The thread exits within one
/// poll interval; nothing joins it because a sleeping watcher must never delay
/// the caller (the countdown ending is latency-sensitive).
fn stop_esc_watch(state: &tauri::State<'_, AppState>) {
    if let Some(stop) = state.esc_watch.lock().unwrap().take() {
        stop.store(true, Ordering::Relaxed);
    }
}

/// Clears a flag however its owner leaves scope.
struct StartingGuard<'a>(&'a AtomicBool);

impl Drop for StartingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Same for `finalizing`: the early returns in `stop` must clear it too.
struct FinalizeGuard<'a>(&'a AtomicBool);

impl Drop for FinalizeGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Start the pending take: the countdown reached GO. A failure puts the panel
/// back with the reason instead of leaving a veil over a take that never ran.
pub fn spawn(app: &tauri::AppHandle) -> Result<StartResult, UiError> {
    let state = app.state::<AppState>();
    // The lock covers the state transition only; the tray/overlay calls below
    // must happen with it released. They block until the main thread runs
    // them, and the main thread may be waiting on this very lock (the window's
    // close handler), which is a deadlock.
    let outcome = {
        let mut guard = state.recording.lock().unwrap();
        // The caller is the veil, which has nothing to show an error with: the
        // panel is hidden by now, so the failure has to travel as an event.
        spawn_started(&state, &mut guard)
    };
    match outcome {
        Ok(result) => {
            let _ = app.emit_to("main", "take:started", ());
            Ok(result)
        }
        Err(err) => {
            crate::tray::set_recording(app, false);
            crate::overlay::hide(app);
            crate::show_main_window(app);
            let _ = app.emit_to("main", "take:failed", &err);
            Err(err)
        }
    }
}

/// The lock-taking half of `spawn`, split out so the countdown-off path (B6)
/// can run it from inside `begin` — a `Mutex` is not reentrant.
fn spawn_started(
    state: &tauri::State<'_, AppState>,
    guard: &mut Option<Recording>,
) -> Result<StartResult, UiError> {
    let Some(prepared) = state.pending.lock().unwrap().take() else {
        // The countdown was cancelled underneath us: nothing to start, and
        // nothing to recover from either — the panel is already back.
        return Err(UiError::new("err.nothing-pending"));
    };
    // The countdown is over: Esc no longer cancels anything.
    stop_esc_watch(state);
    *state.last_error.lock().unwrap() = None;

    let engine = match prepared.backend() {
        Backend::Windows => "wgc+mf".to_string(),
        Backend::Ffmpeg => format!("{:?}", prepared.engine()),
    };
    let encoder = prepared.encoder_label().to_string();
    match prepared.start() {
        Ok(recording) => {
            let dir = recording.dir().to_string_lossy().into_owned();
            // Per-source spawn cost, in order: this is the skew the sources
            // start with before any frame is written.
            crate::probe(&format!("spawn: {:?}", recording.spawn_timings()));
            *guard = Some(recording);
            crate::probe("spawn:ok");
            // `take:started` is emitted by the caller after the lock is
            // released: every UI side effect stays out of the locked region so
            // the lock order cannot deadlock against the event loop.
            Ok(StartResult {
                dir,
                engine,
                encoder,
            })
        }
        Err(e) => {
            // The source count is the panel's business (it knows what it asked
            // for); the code plus the engine's own words are what it needs.
            let err = UiError::with_detail("err.spawn-failed", e);
            *state.last_error.lock().unwrap() = Some(err.to_string());
            crate::probe(&format!("spawn:failed {err}"));
            // Tray, veil and panel are the caller's job — doing that here would
            // run it under the recording lock.
            Err(err)
        }
    }
}

/// Throw the pending take away: Esc during the countdown. `Prepared::start` is
/// what creates the take directory, so dropping it here leaves no trace on
/// disk — which is what makes this free.
pub fn cancel(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    if state.pending.lock().unwrap().take().is_none() {
        // Either the countdown already finished (spawn took the take) or
        // nothing was ever prepared: there is nothing to cancel.
        crate::probe("cancel:nothing");
        return;
    }
    // Retire the watcher even when this call did not come from it: the hotkey
    // during the countdown and the Esc poll both land here.
    stop_esc_watch(&state);
    crate::tray::set_recording(app, false);
    crate::overlay::hide(app);
    // Tell the veil to stop its clock: hiding the window is not enough —
    // Chromium keeps feeding frames to a just-hidden window for a moment, and
    // the countdown would run on, playing the remaining beeps (and the GO
    // tone) over a take that no longer exists.
    let _ = app.emit_to(crate::overlay::LABEL, "overlay:stop", ());
    crate::show_main_window(app);
    crate::probe("cancelled");
    let _ = app.emit_to("main", "take:cancelled", ());
}

pub fn status(app: &tauri::AppHandle) -> StatusInfo {
    let state = app.state::<AppState>();
    let last_error = state.last_error.lock().unwrap().clone();
    let mut guard = state.recording.lock().unwrap();
    status_of(guard.as_mut(), last_error)
}

/// The snapshot shape without the locks, so the mapping — including the error
/// the failed state reads — can be tested without an app handle.
fn status_of(recording: Option<&mut Recording>, last_error: Option<String>) -> StatusInfo {
    match recording {
        Some(r) => StatusInfo {
            recording: true,
            elapsed: r.elapsed(),
            sources: r
                .snapshot()
                .into_iter()
                .map(|s| LiveSourceInfo {
                    id: s.id,
                    alive: s.alive,
                    usable: s.usable,
                    bytes: s.bytes,
                })
                .collect(),
            error: last_error,
        },
        None => StatusInfo {
            recording: false,
            elapsed: 0.0,
            sources: Vec::new(),
            error: last_error,
        },
    }
}

/// Stop one source mid-take. The window it points at is gone, hidden or on
/// another virtual desktop by now, so this one runs on recording whatever is
/// behind it. The rest of the take is untouched.
pub fn stop_source(app: &tauri::AppHandle, id: &str) -> bool {
    let state = app.state::<AppState>();
    let mut guard = state.recording.lock().unwrap();
    let stopped = guard.as_mut().is_some_and(|r| r.stop_source(id));
    if stopped {
        crate::probe(&format!("source:stopped {id}"));
    }
    stopped
}

/// Finish the running take. `announce` is for a stop the panel did not ask
/// for: the hotkey and the tray both end a take while the panel may be hidden,
/// so the failure has to travel as an event instead of a command response.
///
/// The stop itself is told in three broadcast events (B4): `take:finalize`
/// the moment the button is pressed (the timer freezes at `elapsed`),
/// `take:source` once per file as it lands, `take:done` when the take is
/// over. Both surfaces — the full-screen stop screen and the panel's status
/// bar — read the same stream, so a stop feels the same whoever pressed it.
pub fn stop(app: &tauri::AppHandle, announce: bool) -> Result<StopResult, UiError> {
    let state = app.state::<AppState>();
    // From here the recording is out of `recording` and the take is not
    // finished: without this flag a close during finalise looks like "idle"
    // and exits the process mid-write.
    state.finalizing.store(true, Ordering::SeqCst);
    let _finalizing = FinalizeGuard(&state.finalizing);
    let recording = state
        .recording
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| UiError::new("err.not-recording"))?;

    let dir = recording.dir().to_string_lossy().into_owned();
    // Frozen now: the stop screen shows the take's length as of the button
    // press, and the files may drain for a while after that.
    let elapsed = recording.elapsed();
    let total = recording.source_count();
    // The full-screen stop screen is a choice (`finalize_panel`); either way
    // the panel's status bar follows the same events.
    let panel = crate::config::Config::load().finalize_panel;
    if panel {
        // Cover first, then the event that paints STOP on it: the veil must
        // not come up empty and fill in a frame later.
        crate::overlay::show(app);
        let _ = app.emit_to(crate::overlay::LABEL, "take:beep", &BeepInfo { hz: 660, ms: 200 });
    }
    let _ = app.emit(
        "take:finalize",
        &FinalizeInfo {
            dir: dir.clone(),
            elapsed,
            total,
        },
    );

    // Every worker reports its own outcome the moment its file lands — that
    // is the checklist filling in, without serialising the finalise.
    let emitter = app.clone();
    let finished = match recording.stop_with(move |o| {
        let _ = emitter.emit("take:source", OutcomeInfo::from(o));
    }) {
        Ok(finished) => finished,
        Err(e) => {
            // The take did not finalise, but its folder is the only way to see
            // what went wrong — it goes in the history as a failure instead of
            // disappearing with the error.
            crate::tray::set_recording(app, false);
            crate::overlay::hide(app);
            record(&StopResult {
                dir,
                duration: None,
                sources: Vec::new(),
                failed: 1,
            });
            // Stopped by the hotkey means the recorded app is in front: bring
            // the panel back so the failure is seen, not just logged.
            crate::show_main_window(app);
            let err = UiError::with_detail("err.stop-failed", e);
            crate::probe(&format!("stop:failed {err}"));
            // The panel that pressed the button gets the error back from its
            // own command; a panel that did not has no other way to learn the
            // take is over. With the stop screen up there is no command
            // caller either, so the event goes either way.
            if announce || panel {
                let _ = app.emit_to("main", "take:failed", &err);
            }
            return Err(err);
        }
    };
    crate::tray::set_recording(app, false);
    let failed = finished.failed().count();
    let result = StopResult {
        dir: finished.dir.to_string_lossy().into_owned(),
        duration: finished.duration,
        sources: finished.sources.iter().map(OutcomeInfo::from).collect(),
        failed,
    };
    record(&result);
    // Completion travels the same way the rest of the stop did. There is
    // deliberately no "saved" banner on the veil (README B4 note): the fresh
    // history row is the success feedback, and a checklist that finished
    // needs no second ending.
    let _ = app.emit("take:done", &result);
    // The done beep belongs to the panel: it is the window coming back, and
    // the veil's own audio is going away with the veil.
    let _ = app.emit_to("main", "take:beep", &BeepInfo { hz: 880, ms: 150 });
    crate::overlay::hide(app);
    // Same reason as the failure path: the hotkey stops the take while another
    // window is in front, and "it stopped" has to be something the user sees.
    // Stopping from the panel itself is a no-op here — it is already front.
    crate::show_main_window(app);
    if announce {
        let _ = app.emit_to("main", "take:stopped", &result);
    }
    Ok(result)
}

/// Put the finished take in the history. Best effort on purpose: a journal that
/// cannot be written must never turn a finished take into an error the panel
/// reports — the take itself is already safely on disk.
fn record(result: &StopResult) {
    record_to(&crate::journal::Journal::path(), result);
}

fn record_to(path: &std::path::Path, result: &StopResult) {
    let take = crate::journal::TakeRecord {
        dir: result.dir.clone(),
        at: crate::journal::now_ms(),
        seconds: result.duration,
        bytes: result.sources.iter().map(|s| s.bytes).sum(),
        failed: result.failed,
    };
    let _ = crate::journal::Journal::update_at(path, |j| j.push_take(take));
}

/// What a request for a hotkey means, given the shortcut that is live now.
pub struct Resolution {
    /// The form to show the user.
    pub normalized: String,
    /// `Some` when a registration is needed. `None` means the request is
    /// already the live binding: re-registering it would come back as "already
    /// registered", because the OS counts our own registration as a conflict.
    pub register: Option<Shortcut>,
}

/// Decide between "register this" and "nothing changed", without touching the
/// OS. An unparseable `spec` is an error the caller has to show — never a
/// silent fallback, since a hotkey the user did not ask for is worse than none.
pub fn resolve(current: Option<Shortcut>, spec: &str) -> Result<Resolution, UiError> {
    let normalized = shortcut::normalize(spec)
        .ok_or_else(|| UiError::with_detail("err.hotkey-unknown", spec))?;
    let next: Shortcut = normalized
        .parse()
        .map_err(|e| UiError::with_detail("err.hotkey-unknown", format!("{spec} ({e})")))?;
    let register = match current {
        Some(current) if current.matches(next.mods, next.key) => None,
        _ => Some(next),
    };
    Ok(Resolution {
        normalized,
        register,
    })
}

/// Register `spec` as the global hotkey, replacing whatever was registered
/// before.
///
/// The new shortcut is registered *before* the old one is dropped: a
/// combination another program already owns fails here and leaves the previous
/// hotkey working, so a rejected change can never leave the panel with no way
/// to start or stop.
pub fn install_hotkey(app: &tauri::AppHandle, spec: &str) -> Result<String, UiError> {
    let state = app.state::<AppState>();
    let resolution = resolve(*state.hotkey.lock().unwrap(), spec)?;
    let Some(next) = resolution.register else {
        return Ok(resolution.normalized);
    };

    // `on_shortcut` is what registers: it hands the shortcut to the OS and
    // attaches the handler in one call. Registering first and then binding
    // fails, because the OS counts our own registration as a conflict.
    app.global_shortcut()
        .on_shortcut(next, |app, _shortcut, event| {
            if event.state == ShortcutState::Pressed {
                crate::probe("hotkey:pressed");
                // The panel holds the sources, the parameters and the
                // enabled/disabled rules, so the hotkey only nudges it:
                // pressing the key and clicking the button take the same path.
                let _ = app.emit_to("main", "hotkey:toggle", ());
            }
        })
        .map_err(|e| {
            // The spec is in the detail because "taken by something else" is
            // only actionable if the user knows which key that was.
            UiError::with_detail(
                "err.hotkey-conflict",
                format!("{}: {e}", resolution.normalized),
            )
        })?;

    let previous = state.hotkey.lock().unwrap().replace(next);
    if let Some(previous) = previous {
        let _ = app.global_shortcut().unregister(previous);
    }
    crate::probe(&format!("hotkey:registered {}", resolution.normalized));
    Ok(resolution.normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortcut(spec: &str) -> Shortcut {
        spec.parse().unwrap()
    }

    #[test]
    fn a_failed_start_is_visible_in_the_status() {
        let info = status_of(None, Some("ffmpeg not found on PATH".to_string()));
        assert!(!info.recording);
        assert_eq!(info.error.as_deref(), Some("ffmpeg not found on PATH"));
    }

    #[test]
    fn a_healthy_status_carries_no_error() {
        assert!(status_of(None, None).error.is_none());
    }

    #[test]
    fn a_fresh_state_has_no_pending_take_or_watcher() {
        let state = AppState::default();
        assert!(state.pending.lock().unwrap().is_none());
        assert!(state.last_error.lock().unwrap().is_none());
        assert!(state.esc_watch.lock().unwrap().is_none());
    }

    #[test]
    fn a_stopped_take_lands_in_the_history_with_its_bytes() {
        let dir = std::env::temp_dir().join("takekit-take-record");
        let path = dir.join("journal.json");
        let _ = std::fs::remove_dir_all(&dir);

        let result = StopResult {
            dir: r"D:\takes\2026-10-19-10-00-00".to_string(),
            duration: Some(5.5),
            sources: vec![
                OutcomeInfo {
                    id: "a".to_string(),
                    seconds: Some(5.5),
                    bytes: 1000,
                    error: None,
                    width: 1920,
                    height: 1080,
                },
                OutcomeInfo {
                    id: "b".to_string(),
                    seconds: None,
                    bytes: 0,
                    error: Some("boom".to_string()),
                    width: 0,
                    height: 0,
                },
            ],
            failed: 1,
        };

        record_to(&path, &result);

        let journal = crate::journal::Journal::load_from(&path);
        assert_eq!(journal.takes.len(), 1);
        let take = &journal.takes[0];
        assert_eq!(take.dir, r"D:\takes\2026-10-19-10-00-00");
        assert_eq!(take.seconds, Some(5.5));
        // The bytes of every source, including the one that produced nothing.
        assert_eq!(take.bytes, 1000);
        assert_eq!(take.failed, 1);
    }

    #[test]
    fn first_time_asks_for_a_registration() {
        let r = resolve(None, "f9").unwrap();
        assert_eq!(r.normalized, crate::shortcut::normalize("f9").unwrap());
        assert!(r.register.is_some());
    }

    #[test]
    fn same_binding_is_a_no_op() {
        let r = resolve(Some(shortcut("F9")), "f9").unwrap();
        assert_eq!(r.normalized, crate::shortcut::normalize("F9").unwrap());
        assert!(r.register.is_none());
    }

    #[test]
    fn a_different_binding_asks_for_a_registration() {
        let r = resolve(Some(shortcut("F9")), "ctrl+alt+r").unwrap();
        assert_eq!(
            r.normalized,
            crate::shortcut::normalize("ctrl+alt+r").unwrap()
        );
        assert!(r.register.is_some());
    }

    #[test]
    fn unparseable_spec_is_an_error() {
        assert!(resolve(Some(shortcut("F9")), "Ctrl+NotAKey").is_err());
        assert!(resolve(None, "").is_err());
    }
}
