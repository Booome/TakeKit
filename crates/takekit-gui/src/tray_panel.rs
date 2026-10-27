//! The popover behind the tray icon: the way back to a running take once the
//! panel has hidden itself.
//!
//! It is built hidden and only ever shown through `toggle`, so its page is
//! already loaded when the icon is clicked and it never appears at a spot it
//! then has to jump away from.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{
    AppHandle, LogicalSize, Manager, PhysicalPosition, Position, Size, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};

use crate::place;

pub const LABEL: &str = "tray";

const WIDTH: f64 = 220.0;
/// The height the window is built with, and the guess `fit` starts from before
/// the page has measured itself. Only a starting point: the real height is the
/// card's, see `fit`.
const HEIGHT: f64 = 152.0;
/// Bounds on a page-reported height. Damage control for a page that reports
/// nonsense — never a layout decision.
const MIN_HEIGHT: f64 = 64.0;
const MAX_HEIGHT: f64 = 480.0;
/// Gap kept between the panel and the icon, and between the panel and the edge
/// of the screen it lands on.
const GAP: f64 = 8.0;
/// A focus loss within this long of the icon being clicked is that click's own
/// doing — the shell took the focus to handle it — so it must not be read as a
/// request to open the panel.
const REOPEN_GUARD: Duration = Duration::from_millis(250);

static LAST_HIDE: Mutex<Option<Instant>> = Mutex::new(None);

/// The height the page last measured for its own card, in CSS pixels. Kept here
/// and not only in the page so that the next `toggle` can open the window at
/// the right size straight away, instead of opening at the previous shape and
/// snapping a tick later.
static LAST_FIT: Mutex<f64> = Mutex::new(HEIGHT);

/// A rectangle in physical pixels: the tray icon, or a screen.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn center_x(&self) -> f64 {
        self.x + self.w / 2.0
    }
}

pub fn create(app: &AppHandle, language: &str, theme: &str) -> tauri::Result<WebviewWindow> {
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("tray.html".into()))
        .title("TakeKit")
        .inner_size(WIDTH, HEIGHT)
        .decorations(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(true)
        // Transparent so the page's own surface can carry the 12px radius and
        // the native shadow; an opaque window would show square corners.
        .transparent(true)
        .visible(false)
        // The card is its own document, so it needs the language and palette
        // seeded the same way the panel's are; `ui:settings` keeps it current
        // afterwards.
        .initialization_script(crate::ui_script(language, theme))
        .build()?;

    let handle = window.clone();
    window.on_window_event(move |event| {
        // Clicking anywhere else — the icon included — is what closes it.
        if let WindowEvent::Focused(false) = event {
            mark_hidden();
            hide(&handle);
        }
    });
    if let Ok(size) = window.outer_size() {
        let inner = window
            .inner_size()
            .map(|s| format!("{}x{}", s.width, s.height))
            .unwrap_or_else(|_| "?".into());
        crate::probe(&format!(
            "tray:built inner={inner} outer={}x{} scale={:?}",
            size.width,
            size.height,
            window.scale_factor()
        ));
    }
    Ok(window)
}

/// Show the panel above the icon, or hide it when it is already up.
pub fn toggle(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL) else {
        crate::probe("tray:no-panel");
        return;
    };
    if window.is_visible().unwrap_or(false) {
        hide(&window);
        return;
    }
    if just_hidden() {
        crate::probe("tray:reopen-guarded");
        return;
    }
    match open_at_fit(app, &window) {
        Ok(()) => crate::probe("tray:placed"),
        Err(e) => crate::probe(&format!("tray:place-failed {e}")),
    }
    let _ = window.show();
    let _ = window.set_focus();
}

/// Bring the popover up for keeps — the second launch's "show me what is
/// already running" while a take owns the screen. Unlike `toggle` this never
/// hides an already-visible popover (a focus request is not a close request)
/// and skips the reopen guard (that guard is for the tray click's own focus
/// juggling, not for an explicit raise).
pub fn raise(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL) else {
        crate::probe("tray:no-panel");
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.set_focus();
        return;
    }
    match open_at_fit(app, &window) {
        Ok(()) => crate::probe("tray:placed"),
        Err(e) => crate::probe(&format!("tray:place-failed {e}")),
    }
    let _ = window.show();
    let _ = window.set_focus();
    crate::probe("tray:raised");
}

pub fn hide(window: &WebviewWindow) {
    if window.hide().is_ok() {
        crate::probe("tray:hidden");
    }
}

/// Resize the popover to the height its own content measured.
///
/// The card is not a fixed shape. An idle take is one line of text, a take with
/// three sources is three rows, and a translated button label wraps to two
/// lines — so a window sized once at build time is always the wrong height for
/// something. At 132px the second row of keys was already half cut off by the
/// taskbar, because the card needed 152 and got 132.
///
/// `tray.js` measures the card (`fit`) and calls this with CSS pixels; the
/// window is the only half of that pair that a webview cannot resize itself.
pub fn fit(app: &AppHandle, height: f64) -> tauri::Result<()> {
    let height = clamp_height(height);
    let changed = {
        let mut last = LAST_FIT.lock().unwrap_or_else(|e| e.into_inner());
        // Sub-pixel churn is not worth a resize: a fractional height comes from
        // line-height rounding, and it would otherwise fire on every tick.
        let changed = (*last - height).abs() >= 0.5;
        *last = height;
        changed
    };
    let Some(window) = app.get_webview_window(LABEL) else {
        return Ok(());
    };
    // Not just `changed`: the window can be the wrong size while the page's
    // number is the same one it always asked for. The first `set_size` on a
    // fresh window lands 30px tall (see `resize`), and skipping the resize
    // because "nothing changed" is what let that 30px stay there.
    let scale = window.scale_factor()?;
    let wrong = (f64::from(window.inner_size()?.height) - height * scale).abs() > 1.0;
    if !changed && !wrong {
        return Ok(());
    }
    resize(&window, height, scale)?;
    // Growing upwards from the icon: without this a taller popover keeps its
    // top edge and pushes its own bottom row into the taskbar.
    if window.is_visible().unwrap_or(false) {
        place(app, &window)?;
    }
    crate::probe(&format!("tray:fitted {height:.0}"));
    Ok(())
}

/// Open the popover at the height it last measured.
///
/// The page cannot measure while it is hidden — its rows are only filled in by
/// a tick that runs when it is on screen — so the window is opened at the last
/// measurement and corrected within a tick. Opening at the built-in height
/// instead would make that correction the normal case rather than the rare one.
fn open_at_fit(app: &AppHandle, window: &WebviewWindow) -> tauri::Result<()> {
    let height = clamp_height(*LAST_FIT.lock().unwrap_or_else(|e| e.into_inner()));
    resize(window, height, window.scale_factor()?)?;
    place(app, window)
}

/// Give the window a client area `height` CSS pixels tall.
///
/// On Windows the only door is `set_size`, which tao implements as an
/// undecorated-with-shadow resize: it measures this window's frame offset
/// (window rect minus client rect) and adds it, then `SetWindowPos`es with the
/// caption and border styles masked out. Measured on a real desktop, a fresh
/// popover reports an offset of 39px — a hidden 30px caption on top of the real
/// 9px frame — and only 9px once it has been resized once. So the first request
/// for 152 lands a 182px client: 30 too tall, with the card's keys already
/// inside the window and 30px of empty surface under them.
///
/// The offset settles after that first resize, so asking again with the same
/// height lands exactly. Hence: ask, measure, ask again while it is wrong —
/// rather than modelling a frame that `decorations(false)` says is not there.
fn resize(window: &WebviewWindow, height: f64, scale: f64) -> tauri::Result<()> {
    let want = height * scale;
    let mut inner = 0.0;
    for attempt in 0..3 {
        window.set_size(LogicalSize::new(WIDTH, height))?;
        inner = f64::from(window.inner_size()?.height);
        if (inner - want).abs() <= 1.0 {
            if attempt > 0 {
                crate::probe(&format!("tray:resize-settled {attempt}"));
            }
            return Ok(());
        }
    }
    // Not fatal: a popover a few pixels out still works, and the next tick asks
    // again. Worth a line in the log because nothing else would say why a card
    // has a strip of empty surface under it.
    crate::probe(&format!("tray:resize-off want={want:.0} got={inner:.0}"));
    Ok(())
}

/// A page-reported height the window can live with.
fn clamp_height(height: f64) -> f64 {
    // `NAN` fails both comparisons and would sail through `clamp`, so it is
    // turned away first.
    if height.is_nan() {
        return HEIGHT;
    }
    height.clamp(MIN_HEIGHT, MAX_HEIGHT)
}

/// Put the popover above the tray icon, on the screen the icon is on.
///
/// The size comes from the window, not from the height the page asked for: this
/// window carries a 9px frame the page knows nothing about, and placing by the
/// client height puts that 9px straight into the taskbar.
fn place(app: &AppHandle, window: &WebviewWindow) -> tauri::Result<()> {
    let scale = window.scale_factor()?;
    let size = window.outer_size()?;
    let panel = (f64::from(size.width), f64::from(size.height));
    let screens = place::live_monitors(app);

    let (icon, screen) = match icon_rect(app, scale) {
        Some(icon) => {
            // The screen under the icon decides the bounds, so a taskbar on a
            // second monitor does not pull the panel onto the first.
            let screen = place::monitor_at(icon.center_x(), icon.y + icon.h / 2.0, &screens)
                .or_else(|| screens.first().copied())
                .map(|m| Rect {
                    x: m.x,
                    y: m.y,
                    w: m.w,
                    h: m.h,
                });
            (icon, screen)
        }
        // The shell did not say where the icon is: the bottom-right corner of
        // the primary screen is where a tray popover is expected. A zero-size
        // icon in the corner expresses that through the same placement rule.
        None => match app.primary_monitor().ok().flatten() {
            Some(m) => {
                let p = m.position();
                let s = m.size();
                let corner = Rect {
                    x: p.x as f64 + s.width as f64,
                    y: p.y as f64 + s.height as f64,
                    w: 0.0,
                    h: 0.0,
                };
                (
                    corner,
                    Some(Rect {
                        x: p.x as f64,
                        y: p.y as f64,
                        w: s.width as f64,
                        h: s.height as f64,
                    }),
                )
            }
            None => return Ok(()),
        },
    };

    let Some(screen) = screen else {
        return Ok(());
    };
    let (x, y) = place_near(icon, panel, screen);
    window.set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32))
}

/// Where the icon is, in physical pixels — `None` when the shell will not say.
fn icon_rect(app: &AppHandle, scale: f64) -> Option<Rect> {
    let rect = app.tray_by_id(crate::tray::ID)?.rect().ok().flatten()?;
    let (x, y) = match rect.position {
        Position::Physical(p) => (f64::from(p.x), f64::from(p.y)),
        Position::Logical(p) => (p.x * scale, p.y * scale),
    };
    let (w, h) = match rect.size {
        Size::Physical(s) => (f64::from(s.width), f64::from(s.height)),
        Size::Logical(s) => (s.width * scale, s.height * scale),
    };
    Some(Rect { x, y, w, h })
}

/// Where the panel opens: centred on the icon, above it, and inside the screen.
fn place_near(icon: Rect, panel: (f64, f64), screen: Rect) -> (f64, f64) {
    let x = clamp(
        icon.center_x() - panel.0 / 2.0,
        screen.x + GAP,
        screen.x + screen.w - panel.0 - GAP,
    );
    // Above the icon, unless the taskbar is on top: the icon is already at the
    // top there, and above it there is nothing.
    let mut y = icon.y - panel.1 - GAP;
    if y < screen.y + GAP {
        y = icon.y + icon.h + GAP;
    }
    let y = clamp(y, screen.y + GAP, screen.y + screen.h - panel.1 - GAP);
    (x, y)
}

/// A clamp that copes with bounds that cross over — a screen narrower than the
/// panel — by keeping the lower one.
fn clamp(value: f64, min: f64, max: f64) -> f64 {
    if max < min {
        min
    } else {
        value.clamp(min, max)
    }
}

fn mark_hidden() {
    let mut last = LAST_HIDE.lock().unwrap_or_else(|e| e.into_inner());
    *last = Some(Instant::now());
}

fn just_hidden() -> bool {
    LAST_HIDE
        .lock()
        .map(|last| last.is_some_and(|t| t.elapsed() < REOPEN_GUARD))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANEL: (f64, f64) = (220.0, 132.0);

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn the_panel_sits_above_the_icon_centred_on_it() {
        let screen = rect(0.0, 0.0, 1920.0, 1080.0);
        let icon = rect(950.0, 1066.0, 24.0, 24.0);
        let (x, y) = place_near(icon, PANEL, screen);
        assert_eq!(y, icon.y - PANEL.1 - GAP, "above the icon");
        assert_eq!(x, icon.center_x() - PANEL.0 / 2.0, "centred on the icon");
    }

    #[test]
    fn the_panel_stays_inside_the_screen_at_the_edges() {
        let screen = rect(0.0, 0.0, 1920.0, 1080.0);
        let left = place_near(rect(0.0, 1066.0, 24.0, 24.0), PANEL, screen);
        assert_eq!(left.0, screen.x + GAP);
        let right = place_near(rect(1896.0, 1066.0, 24.0, 24.0), PANEL, screen);
        assert_eq!(right.0, screen.x + screen.w - PANEL.0 - GAP);
    }

    #[test]
    fn a_taskbar_on_top_opens_the_panel_downwards() {
        let screen = rect(0.0, 0.0, 1920.0, 1080.0);
        let icon = rect(950.0, 4.0, 24.0, 24.0);
        let (_, y) = place_near(icon, PANEL, screen);
        assert_eq!(y, icon.y + icon.h + GAP);
    }

    #[test]
    fn the_panel_is_kept_on_the_screen_the_icon_is_on() {
        let second = rect(1920.0, 0.0, 2560.0, 1440.0);
        let (x, _) = place_near(rect(1924.0, 1426.0, 24.0, 24.0), PANEL, second);
        assert_eq!(x, second.x + GAP, "it must not slide onto the first screen");
    }

    #[test]
    fn a_screen_smaller_than_the_panel_still_gets_a_corner() {
        let screen = rect(0.0, 0.0, 200.0, 100.0);
        let (x, y) = place_near(rect(190.0, 90.0, 8.0, 8.0), PANEL, screen);
        assert_eq!((x, y), (screen.x + GAP, screen.y + GAP));
    }

    #[test]
    fn the_built_in_height_is_one_the_window_may_open_at() {
        assert!((MIN_HEIGHT..=MAX_HEIGHT).contains(&HEIGHT));
    }

    #[test]
    fn a_reported_height_is_kept_inside_the_window_limits() {
        assert_eq!(clamp_height(180.0), 180.0, "a measurement is taken as is");
        assert_eq!(
            clamp_height(0.0),
            MIN_HEIGHT,
            "a collapsed card is no window"
        );
        assert_eq!(
            clamp_height(9000.0),
            MAX_HEIGHT,
            "a log of sources is no window either"
        );
        assert_eq!(
            clamp_height(f64::NAN),
            HEIGHT,
            "a page bug must not become a zero-height window"
        );
    }
}
