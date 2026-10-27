//! The full-screen prompt layer: the veil that carries the countdown and the
//! "GO" while a take starts.
//!
//! It is built hidden at startup, so its page is loaded before the first take
//! and showing it costs a visibility flip rather than a page load. It covers
//! the whole virtual screen — every monitor the capture can reach — and it is
//! never focused: the window being recorded should stay the active one.

use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow};

use takekit_core::source::virtual_screen_rect;

pub const LABEL: &str = "overlay";

pub fn create(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    // The veil has one line of prose now (the stop screen's way out), so it
    // needs the same language seed the panel gets — otherwise an English user
    // reads that line in Chinese until they switch language by hand.
    let cfg = crate::config::Config::load();
    let window =
        tauri::WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("overlay.html".into()))
            .title("TakeKit")
            // A veil, not a window: no chrome, no shadow, no place in the taskbar.
            .decorations(false)
            .resizable(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .shadow(false)
            // The page paints its own 62% black; the window around it stays clear so
            // the corners and the taskbar strip are not painted twice.
            .transparent(true)
            // It must never take the focus: taking it would deactivate the very window
            // the countdown is giving the user time to arrange. `focused(false)` alone
            // is not enough — it only applies while the window is being created, and
            // `show()` is an activating ShowWindow; `focusable(false)` is what sets
            // WS_EX_NOACTIVATE, so a later show cannot steal the active window.
            .focused(false)
            .focusable(false)
            .visible(false)
            .initialization_script(crate::ui_script(
                &cfg.effective_language(),
                &cfg.effective_theme(),
            ))
            // No `additional_browser_args` here even though B3's countdown beeps need
            // `--autoplay-policy=no-user-gesture-required`: wry already passes that
            // (its `autoplay` attribute defaults to true). Passing flags per window is
            // also actively dangerous — the setter replaces wry's whole argument
            // string, and environments that share a user data folder must have
            // identical options or the later one fails with 0x8007139F. Process-wide
            // flags go through WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS instead; see
            // main.rs and docs/webview2-agent-sandbox.md.
            .build()?;

    // Click-through, and not for the cursor's sake — for the painting.
    //
    // A full-screen topmost window at 62% opacity reads as an "occluder" to
    // every application behind it, and Chromium-based ones (Chrome, Edge,
    // VS Code, Slack, anything Electron) stop rendering while occluded. The
    // veil itself is composited live by DWM, so what the user sees during the
    // countdown is the last frame the app painted — a frozen picture that
    // gives itself away as soon as something behind it is supposed to move.
    // WS_EX_TRANSPARENT (what this sets on Windows) is the style Chromium's
    // occlusion tracker treats as "may be translucent", so the app keeps
    // painting. The veil has nothing to click, so losing hit-testing costs
    // nothing — it also lets the user keep arranging the window they are about
    // to record, which is what the countdown is for.
    if let Err(e) = window.set_ignore_cursor_events(true) {
        // Without it the veil still looks right and the backdrop still
        // freezes, which reads as a rendering bug. Say so in the log.
        crate::probe(&format!("overlay:click-through-failed {e}"));
    }

    cover_virtual_screen(&window);
    Ok(window)
}

/// Show the veil over everything. `false` means it could not be shown at all
/// (no window, or the OS refused the show), which the caller must treat as
/// "there is no countdown": committing a take without a clock would leave a
/// pending session that only guessing Esc can end.
pub fn show(app: &AppHandle) -> bool {
    let Some(window) = app.get_webview_window(LABEL) else {
        crate::probe("overlay:missing");
        return false;
    };
    // The virtual screen is not fixed: a monitor can be added or unplugged
    // between takes, so the cover is recomputed every time it comes up.
    cover_virtual_screen(&window);
    // Whatever this window last was, it comes up as the veil: click-through.
    // A stop screen may have taken the clicks (and failed to hand them back).
    let _ = window.set_ignore_cursor_events(true);
    match window.show() {
        Ok(()) => {
            crate::probe("overlay:shown");
            true
        }
        Err(e) => {
            crate::probe(&format!("overlay:show-failed {e}"));
            false
        }
    }
}

/// Take the veil down.
pub fn hide(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(LABEL) {
        if window.hide().is_ok() {
            // Back to a pure veil for the next countdown, whether or not this
            // hide was a stop screen handing the clicks back.
            let _ = window.set_ignore_cursor_events(true);
            crate::probe("overlay:hidden");
        }
    }
}

/// The stop screen is the one frame with something to click.
///
/// While a countdown runs the veil must stay click-through: a full-screen
/// topmost window that hit-tests everything is read as an occluder by Chromium
/// apps behind it and they stop painting (see `create`), and the user is busy
/// arranging the window being recorded. Once the take is over there is nothing
/// left to protect, and the summary needs a way out that is not a one-minute
/// wait — so the stop screen takes the clicks, and a click anywhere on it
/// dismisses it.
pub fn set_clickable(app: &AppHandle, clickable: bool) {
    let Some(window) = app.get_webview_window(LABEL) else {
        crate::probe("overlay:clickable-missing");
        return;
    };
    match window.set_ignore_cursor_events(!clickable) {
        Ok(()) => crate::probe(&format!("overlay:clickable={clickable}")),
        Err(e) => crate::probe(&format!("overlay:clickable-failed {e}")),
    }
}

/// Stretch the window across every monitor, in physical pixels.
fn cover_virtual_screen(window: &WebviewWindow) {
    let r = virtual_screen_rect();
    let _ = window.set_position(PhysicalPosition::new(r.x, r.y));
    let _ = window.set_size(PhysicalSize::new(r.w, r.h));
}
