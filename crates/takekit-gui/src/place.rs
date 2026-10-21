//! Where the panel opens, and remembering it between runs.
//!
//! The remembered position goes to the window builder instead of being applied
//! after the window exists: a `set_position` afterwards is visible as the panel
//! appearing at the default spot and then jumping.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, WebviewWindow, WindowEvent};

use crate::config::{Config, WindowPos};

/// A drag writes the position at most this often; the close handler writes the
/// spot it ended on.
const SAVE_EVERY: Duration = Duration::from_millis(400);
/// How much of the panel has to stay on a monitor when the remembered point is
/// off every screen.
const KEEP_VISIBLE: f64 = 80.0;
/// Windows parks a minimized window at `-32000,-32000` and reports that as its
/// position. No monitor reaches that far, so the pair is the sentinel.
const MINIMIZED_SENTINEL: f64 = -32000.0;

/// Whether a reported position is somewhere the user could have put the panel.
/// A minimized window is not: its sentinel position would put the panel at the
/// top-left corner of the primary monitor on every later start, with every copy
/// the user opens landing on top of the last one.
fn rememberable(x: f64, y: f64) -> bool {
    x > MINIMIZED_SENTINEL || y > MINIMIZED_SENTINEL
}

/// A monitor as the OS reports it: physical pixels, plus the scale factor that
/// turns them back into the logical units the builder takes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonitorBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub scale: f64,
}

impl MonitorBox {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// Squared distance from the point to the rectangle, zero when inside.
    fn distance(&self, x: f64, y: f64) -> f64 {
        let dx = (self.x - x).max(x - (self.x + self.w)).max(0.0);
        let dy = (self.y - y).max(y - (self.y + self.h)).max(0.0);
        dx * dx + dy * dy
    }
}

pub(crate) fn live_monitors(app: &AppHandle) -> Vec<MonitorBox> {
    app.available_monitors()
        .map(|list| {
            list.iter()
                .map(|m| {
                    let p = m.position();
                    let s = m.size();
                    MonitorBox {
                        x: p.x as f64,
                        y: p.y as f64,
                        w: s.width as f64,
                        h: s.height as f64,
                        scale: m.scale_factor(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The logical position to build the window at; `None` lets the OS place it.
pub fn restore(app: &AppHandle, saved: Option<WindowPos>) -> Option<(f64, f64)> {
    let saved = saved.filter(|p| rememberable(p.x, p.y))?;
    let monitors = live_monitors(app);
    if monitors.is_empty() {
        return None;
    }
    let (x, y, scale) = onto_a_monitor(saved.x, saved.y, &monitors);
    if scale <= 0.0 {
        return None;
    }
    Some((x / scale, y / scale))
}

/// Remember the panel's position as it moves, and once more on the way out.
pub fn watch_moves(window: &WebviewWindow) {
    let handle = window.clone();
    let last = Mutex::new(Instant::now() - SAVE_EVERY);
    window.on_window_event(move |event| match event {
        WindowEvent::Moved(pos) => {
            // The crop workspace grows the panel on purpose; that place is
            // not where the user keeps the strip and must not be remembered.
            if in_crop_mode() {
                return;
            }
            let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
            if last.elapsed() < SAVE_EVERY {
                return;
            }
            *last = Instant::now();
            drop(last);
            // A read-modify-write plus a rename on the UI thread stalls the
            // drag it is trying to record; the config lock keeps concurrent
            // saves sane.
            let (x, y) = (pos.x as f64, pos.y as f64);
            std::thread::spawn(move || save(x, y));
        }
        // Closing does not move the window, so this only catches a move made
        // inside the throttle window above.
        WindowEvent::CloseRequested { .. } => {
            if !in_crop_mode() && let Ok(pos) = handle.outer_position() {
                save(pos.x as f64, pos.y as f64);
            }
        }
        _ => {}
    });
}

/// A position the user could have chosen is worth keeping; a minimized window's
/// sentinel is not.
fn save(x: f64, y: f64) {
    if !rememberable(x, y) {
        return;
    }
    let _ = Config::update(|cfg| cfg.window_pos = Some(WindowPos { x, y }));
}

/// The monitor a point falls on, or the nearest one when it is off every screen.
pub(crate) fn monitor_at(x: f64, y: f64, monitors: &[MonitorBox]) -> Option<MonitorBox> {
    monitors
        .iter()
        .find(|m| m.contains(x, y))
        .or_else(|| {
            monitors
                .iter()
                .min_by(|a, b| a.distance(x, y).total_cmp(&b.distance(x, y)))
        })
        .copied()
}

/// The remembered point when some monitor still covers it, otherwise the same
/// point pulled onto the nearest monitor: an unplugged screen must not leave the
/// panel out of reach.
fn onto_a_monitor(x: f64, y: f64, monitors: &[MonitorBox]) -> (f64, f64, f64) {
    let m = monitor_at(x, y, monitors).expect("callers pass a non-empty list");
    if m.contains(x, y) {
        return (x, y, m.scale);
    }
    (
        x.clamp(m.x, (m.x + m.w - KEEP_VISIBLE).max(m.x)),
        y.clamp(m.y, (m.y + m.h - KEEP_VISIBLE).max(m.y)),
        m.scale,
    )
}

// ── The crop workspace's window ─────────────────────────────────────────────
//
// The crop wants as much of the screen as it can get: a 4K frame trimmed to
// the pixel is not a job for a 900x640 control strip. So the panel grows to
// 90% of the screen it is on while the workspace is open, and goes back to
// being the strip afterwards. The remembered position is not touched by the
// detour (see `CROP_MODE` in `watch_moves`).

/// True while the crop workspace owns the screen.
static CROP_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the panel was before the workspace grew it: where it sat and how big
/// its client area was — `((x, y), (w, h))` in physical pixels.
type PanelGeometry = ((i32, i32), (u32, u32));

static PANEL_GEOMETRY: Mutex<Option<PanelGeometry>> = Mutex::new(None);

pub fn in_crop_mode() -> bool {
    CROP_MODE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Grow the panel to 90% of the current monitor's work area, centred.
///
/// The work area, not the whole screen: the taskbar is not part of the picture
/// and a workspace that ends up under it is not 90% of anything.
pub fn enter_crop_mode(window: &WebviewWindow) -> Result<(), String> {
    if in_crop_mode() {
        return Ok(());
    }
    let mut saved = PANEL_GEOMETRY.lock().unwrap_or_else(|e| e.into_inner());
    if saved.is_some() {
        return Ok(());
    }
    let pos = window
        .outer_position()
        .map_err(|e| format!("panel position: {e}"))?;
    let size = window
        .inner_size()
        .map_err(|e| format!("panel size: {e}"))?;
    *saved = Some(((pos.x, pos.y), (size.width, size.height)));

    let monitor = window
        .current_monitor()
        .map_err(|e| format!("monitor: {e}"))?
        .ok_or_else(|| "no monitor under the panel".to_string())?;
    let work = monitor.work_area();
    let (wx, wy) = (work.position.x, work.position.y);
    let (ww, wh) = (work.size.width, work.size.height);
    // 90% of the work area.
    let inner = tauri::PhysicalSize::new(((ww as f64) * 0.9) as u32, ((wh as f64) * 0.9) as u32);
    window
        .set_size(inner)
        .map_err(|e| format!("resize: {e}"))?;
    // Centred on the work area, using the size the window actually grew to
    // (the title bar is outside `inner_size` and would otherwise push the
    // window half a title bar low).
    let outer = window
        .outer_size()
        .map_err(|e| format!("outer size: {e}"))?;
    let x = wx + ((ww.saturating_sub(outer.width)) / 2) as i32;
    let y = wy + ((wh.saturating_sub(outer.height)) / 2) as i32;
    window
        .set_position(tauri::PhysicalPosition::new(x, y))
        .map_err(|e| format!("place: {e}"))?;
    CROP_MODE.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// Put the panel back where it was before the workspace grew it.
pub fn exit_crop_mode(window: &WebviewWindow) -> Result<(), String> {
    CROP_MODE.store(false, std::sync::atomic::Ordering::SeqCst);
    let taken = PANEL_GEOMETRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let Some(((x, y), (w, h))) = taken else {
        return Ok(());
    };
    window
        .set_size(tauri::PhysicalSize::new(w, h))
        .map_err(|e| format!("resize back: {e}"))?;
    window
        .set_position(tauri::PhysicalPosition::new(x, y))
        .map_err(|e| format!("place back: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(x: f64, y: f64, w: f64, h: f64, scale: f64) -> MonitorBox {
        MonitorBox { x, y, w, h, scale }
    }

    #[test]
    fn a_position_on_a_live_monitor_is_kept_as_is() {
        let monitors = [monitor(0.0, 0.0, 1920.0, 1080.0, 1.0)];
        let (x, y, _) = onto_a_monitor(300.0, 400.0, &monitors);
        assert_eq!((x, y), (300.0, 400.0));
    }

    #[test]
    fn the_scale_of_the_monitor_it_landed_on_comes_back() {
        let monitors = [
            monitor(0.0, 0.0, 1920.0, 1080.0, 1.0),
            monitor(1920.0, 0.0, 2560.0, 1440.0, 1.25),
        ];
        let (_, _, scale) = onto_a_monitor(2400.0, 200.0, &monitors);
        assert_eq!(scale, 1.25);
    }

    #[test]
    fn a_position_on_an_unplugged_monitor_lands_on_the_nearest_one() {
        let monitors = [monitor(0.0, 0.0, 1920.0, 1080.0, 1.0)];
        let (x, y, _) = onto_a_monitor(4000.0, 300.0, &monitors);
        assert_eq!(y, 300.0, "the y axis still fits and should not move");
        assert!(x < 1920.0 && x > 0.0, "pulled onto the live monitor: {x}");
    }

    #[test]
    fn a_position_left_of_every_monitor_is_pulled_back_onto_one() {
        let monitors = [monitor(0.0, 0.0, 1920.0, 1080.0, 1.0)];
        let (x, y, _) = onto_a_monitor(-2400.0, -500.0, &monitors);
        assert_eq!((x, y), (0.0, 0.0));
    }

    #[test]
    fn a_point_picks_the_monitor_that_covers_it() {
        let monitors = [
            monitor(0.0, 0.0, 1920.0, 1080.0, 1.0),
            monitor(1920.0, 0.0, 2560.0, 1440.0, 1.5),
        ];
        assert_eq!(monitor_at(300.0, 300.0, &monitors).unwrap().scale, 1.0);
        assert_eq!(monitor_at(2400.0, 300.0, &monitors).unwrap().scale, 1.5);
    }

    /// Windows parks a minimized window at -32000,-32000 and reports that as
    /// its position. Remembering it would put the panel at the top-left corner
    /// of the primary monitor on every later start, and every copy of the panel
    /// the user opens would land on top of the last one.
    #[test]
    fn the_minimized_sentinel_is_not_a_position() {
        assert!(!rememberable(-32000.0, -32000.0));
        assert!(rememberable(0.0, 0.0));
        assert!(
            rememberable(-2400.0, -500.0),
            "a monitor left of the primary is a real place"
        );
        assert!(
            rememberable(-2400.0, -32000.0),
            "only the pair is the sentinel, not one far axis"
        );
    }

    #[test]
    fn a_point_off_every_monitor_picks_the_nearest() {
        let monitors = [
            monitor(0.0, 0.0, 1920.0, 1080.0, 1.0),
            monitor(1920.0, 0.0, 2560.0, 1440.0, 1.5),
        ];
        assert_eq!(monitor_at(9000.0, 300.0, &monitors).unwrap().scale, 1.5);
        assert_eq!(monitor_at(-9000.0, 300.0, &monitors).unwrap().scale, 1.0);
        assert!(monitor_at(300.0, 300.0, &[]).is_none());
    }
}
