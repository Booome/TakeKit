//! The popover behind the tray icon: the way back to a running take once the
//! panel has hidden itself.
//!
//! It is built hidden and only ever shown through `toggle`, so its page is
//! already loaded when the icon is clicked and it never appears at a spot it
//! then has to jump away from.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{
    AppHandle, Manager, PhysicalPosition, Position, Size, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};

use crate::place;

pub const LABEL: &str = "tray";

const WIDTH: f64 = 220.0;
const HEIGHT: f64 = 132.0;
/// Gap kept between the panel and the icon, and between the panel and the edge
/// of the screen it lands on.
const GAP: f64 = 8.0;
/// A focus loss within this long of the icon being clicked is that click's own
/// doing — the shell took the focus to handle it — so it must not be read as a
/// request to open the panel.
const REOPEN_GUARD: Duration = Duration::from_millis(250);

static LAST_HIDE: Mutex<Option<Instant>> = Mutex::new(None);

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
    match position_near_tray(app, &window) {
        Ok(()) => crate::probe("tray:placed"),
        Err(e) => crate::probe(&format!("tray:place-failed {e}")),
    }
    let _ = window.show();
    let _ = window.set_focus();
}

pub fn hide(window: &WebviewWindow) {
    if window.hide().is_ok() {
        crate::probe("tray:hidden");
    }
}

/// Put the panel above the tray icon, on the screen the icon is on.
fn position_near_tray(app: &AppHandle, window: &WebviewWindow) -> tauri::Result<()> {
    let size = window.outer_size()?;
    let panel = (f64::from(size.width), f64::from(size.height));
    let screens = place::live_monitors(app);

    let (icon, screen) = match icon_rect(app, window.scale_factor()?) {
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
}
