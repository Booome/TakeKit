//! The notification-area icon: a dot that says whether a take is running.
//!
//! The icon is drawn here instead of shipped as a resource file: two shapes
//! that differ only in colour are not worth a build step, a file format and a
//! palette to keep in sync with the panel.

use tauri::image::Image;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

pub const ID: &str = "main";

/// Idle grey. The tray icon carries no text, so this is its own value rather
/// than a copy of a palette token — it only has to read as "neutral" against
/// both light and dark taskbars.
const IDLE: [u8; 3] = [0x8a, 0x8a, 0x8a];
/// Recording red, taken from `--tk-rec` in `ui/style.css` (light theme; the
/// dark theme's `#ff5f52` is too light to read on a dark taskbar). Keep the
/// two in sync — this constant drifted a palette generation behind once.
const RECORDING: [u8; 3] = [0xdc, 0x26, 0x26];
/// Paused amber. Not `--tk-warn` itself: `#8a5300` is chosen to be readable on a
/// white card, and on a dark taskbar it is the same brightness as the idle grey
/// — the icon would read as "not recording". This is the same value in both
/// themes on purpose, like the red.
const PAUSED: [u8; 3] = [0xe0, 0xa1, 0x1e];

const SIZE: u32 = 32;
const CENTER: f32 = 16.0;
const RADIUS: f32 = 11.0;

/// Two bars, six wide, eighteen tall, six apart: the mark every player uses for
/// "paused". The shape carries this state, not the colour — a taskbar draws the
/// icon at 16 pixels, where an amber dot and a red dot are both just a dot.
const BAR_W: f32 = 6.0;
const BAR_H: f32 = 18.0;
const BAR_GAP: f32 = 6.0;

/// What the icon is saying.
///
/// Three states, not two: a paused take writes nothing, and the red dot went on
/// saying "bytes are moving" for as long as the clock was stopped. The panel
/// and the popover were saying nothing either, which is what made the pause
/// feel like it had not taken (reported 2026-10-27).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Idle,
    Recording,
    Paused,
}

pub fn setup(app: &tauri::AppHandle) -> tauri::Result<()> {
    TrayIconBuilder::with_id(ID)
        .icon(dot(IDLE))
        .tooltip(tooltip_text(Phase::Idle, uses_english()))
        // No native menu exists behind this icon, so both buttons are ours to
        // answer. An icon that opens the popover on one button and does nothing
        // on the other reads as broken — the user cannot tell "not implemented"
        // from "I mis-clicked".
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left | MouseButton::Right,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::tray_panel::toggle(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

pub fn set_phase(app: &tauri::AppHandle, phase: Phase) {
    let Some(tray) = app.tray_by_id(ID) else {
        crate::probe("tray:missing");
        return;
    };
    let icon = match phase {
        Phase::Idle => dot(IDLE),
        Phase::Recording => dot(RECORDING),
        Phase::Paused => bars(PAUSED),
    };
    let _ = tray.set_icon(Some(icon));
    let _ = tray.set_tooltip(Some(tooltip_text(phase, uses_english())));
    crate::probe(match phase {
        Phase::Idle => "tray:idle",
        Phase::Recording => "tray:recording",
        Phase::Paused => "tray:paused",
    });
}

/// The hover text is the only surface where a 16-pixel square can spell out
/// what it means, so it names the phase instead of the product. The words are
/// the popover's words (`tray.state*` in `ui/i18n.js`) — two vocabularies for
/// the same state is how "已暂停" and "暂停" end up meaning different things.
fn tooltip_text(phase: Phase, en: bool) -> String {
    let state = match (phase, en) {
        (Phase::Idle, false) => "未在录制",
        (Phase::Idle, true) => "Not recording",
        (Phase::Recording, false) => "录制中",
        (Phase::Recording, true) => "Recording",
        (Phase::Paused, false) => "已暂停",
        (Phase::Paused, true) => "Paused",
    };
    format!("TakeKit · {state}")
}

fn uses_english() -> bool {
    crate::config::Config::load().effective_language() == "en"
}

pub fn set_recording(app: &tauri::AppHandle, recording: bool) {
    set_phase(
        app,
        if recording {
            Phase::Recording
        } else {
            Phase::Idle
        },
    );
}

/// The take is still open, the clock is not. Called by pause and resume, so the
/// two entry points cannot disagree about which icon is showing.
pub fn set_paused(app: &tauri::AppHandle, paused: bool) {
    set_phase(
        app,
        if paused {
            Phase::Paused
        } else {
            Phase::Recording
        },
    );
}

fn dot(color: [u8; 3]) -> Image<'static> {
    Image::new_owned(dot_rgba(color), SIZE, SIZE)
}

fn bars(color: [u8; 3]) -> Image<'static> {
    Image::new_owned(bars_rgba(color), SIZE, SIZE)
}

/// A filled circle of `RADIUS` around the centre, with a one-pixel ramp at the
/// rim so the dot is not jagged at 100% scaling.
fn dot_rgba(color: [u8; 3]) -> Vec<u8> {
    let mut px = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 + 0.5 - CENTER;
            let dy = y as f32 + 0.5 - CENTER;
            let distance = (dx * dx + dy * dy).sqrt();
            let alpha = ((RADIUS - distance + 1.0) / 2.0).clamp(0.0, 1.0);
            let i = ((y * SIZE + x) * 4) as usize;
            px[i] = color[0];
            px[i + 1] = color[1];
            px[i + 2] = color[2];
            px[i + 3] = (alpha * 255.0).round() as u8;
        }
    }
    px
}

/// The same centre, two bars instead of a disc — same soft edge, so the two
/// shapes look like they came from the same hand.
fn bars_rgba(color: [u8; 3]) -> Vec<u8> {
    let mut px = vec![0u8; (SIZE * SIZE * 4) as usize];
    let left = CENTER - (BAR_W * 2.0 + BAR_GAP) / 2.0;
    let right = left + BAR_W + BAR_GAP;
    let top = CENTER - BAR_H / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            let vertical = edge(fy - top) * edge(top + BAR_H - fy);
            let in_left = edge(fx - left) * edge(left + BAR_W - fx);
            let in_right = edge(fx - right) * edge(right + BAR_W - fx);
            let alpha = vertical * in_left.max(in_right);
            let i = ((y * SIZE + x) * 4) as usize;
            px[i] = color[0];
            px[i + 1] = color[1];
            px[i + 2] = color[2];
            px[i + 3] = (alpha * 255.0).round() as u8;
        }
    }
    px
}

/// 1 well inside the shape, 0 well outside, linear across the boundary pixel.
fn edge(d: f32) -> f32 {
    (d + 0.5).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(buf: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * SIZE + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn buffer_matches_the_declared_size() {
        assert_eq!(dot_rgba(IDLE).len(), (SIZE * SIZE * 4) as usize);
    }

    #[test]
    fn centre_is_opaque_and_takes_the_colour() {
        let buf = dot_rgba(RECORDING);
        assert_eq!(px(&buf, 16, 16), [0xdc, 0x26, 0x26, 255]);
        assert_eq!(px(&buf, 15, 15), [0xdc, 0x26, 0x26, 255]);
    }

    #[test]
    fn corners_stay_transparent() {
        let buf = dot_rgba(IDLE);
        assert_eq!(px(&buf, 0, 0)[3], 0);
        assert_eq!(px(&buf, SIZE - 1, SIZE - 1)[3], 0);
    }

    #[test]
    fn the_rim_is_soft() {
        let buf = dot_rgba(IDLE);
        let partial = (0..SIZE)
            .flat_map(|y| (0..SIZE).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let a = px(&buf, x, y)[3];
                a > 0 && a < 255
            })
            .count();
        assert!(partial > 0, "expected a soft rim, found a hard edge");
    }

    #[test]
    fn paused_is_two_bars_and_not_a_dot() {
        let buf = bars_rgba(PAUSED);
        // The middle is empty — that gap is the only thing separating this from
        // the recording dot at 16 pixels, where colour alone is not enough.
        assert_eq!(px(&buf, 16, 16)[3], 0, "the bars ran together");
        // …and both bars are solid, centred, and the pause colour.
        for x in [9, 21] {
            assert_eq!(px(&buf, x, 16), [0xe0, 0xa1, 0x1e, 255], "no bar at x={x}");
        }
    }

    #[test]
    fn the_bars_sit_inside_the_same_square_as_the_dot() {
        // A pause mark is a square: two bars as tall as the pair is wide. Not
        // "inside the circle" — the corners of a square never are, and the
        // 32×32 canvas is what the shell actually scales.
        let buf = bars_rgba(PAUSED);
        let (mut x0, mut x1, mut y0, mut y1) = (SIZE, 0u32, SIZE, 0u32);
        for y in 0..SIZE {
            for x in 0..SIZE {
                if px(&buf, x, y)[3] > 0 {
                    x0 = x0.min(x);
                    x1 = x1.max(x);
                    y0 = y0.min(y);
                    y1 = y1.max(y);
                }
            }
        }
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        assert!(
            (w as i32 - h as i32).abs() <= 1,
            "not square: {w}x{h} at {x0},{y0}"
        );
        // Nor much smaller than the dot it replaces: the tray would look like
        // it had shrunk the moment a take paused.
        assert!(w as f32 >= RADIUS, "too narrow to read: {w}px");
    }

    #[test]
    fn every_phase_has_its_own_colour() {
        // A silent copy/paste would make the icon lie, and the icon is the only
        // signal left once the panel has hidden itself.
        assert_ne!(IDLE, RECORDING);
        assert_ne!(RECORDING, PAUSED);
        assert_ne!(IDLE, PAUSED);
    }

    #[test]
    fn the_tooltip_names_the_phase_in_the_panels_language() {
        // Three phases must give three texts: a tooltip that only says the
        // product name was the whole complaint. And it has to follow the same
        // language switch as everything else, or a bilingual user gets a
        // Chinese tooltip over an English panel.
        let zh: Vec<String> = [Phase::Idle, Phase::Recording, Phase::Paused]
            .map(|p| tooltip_text(p, false))
            .to_vec();
        assert_eq!(
            zh,
            ["TakeKit · 未在录制", "TakeKit · 录制中", "TakeKit · 已暂停"]
        );

        let en: Vec<String> = [Phase::Idle, Phase::Recording, Phase::Paused]
            .map(|p| tooltip_text(p, true))
            .to_vec();
        assert_eq!(
            en,
            [
                "TakeKit · Not recording",
                "TakeKit · Recording",
                "TakeKit · Paused"
            ]
        );
    }
}
