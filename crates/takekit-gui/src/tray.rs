//! The notification-area icon: a filled dot that says whether a take is
//! running.
//!
//! The icon is drawn here instead of shipped as a resource file: two 32x32
//! circles that differ only in colour are not worth a build step, a file format
//! and a palette to keep in sync with the panel.

use tauri::image::Image;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

pub const ID: &str = "main";

/// Idle grey. The tray icon carries no text, so this is its own value rather
/// than a copy of a palette token — it only has to read as "neutral" against
/// both light and dark taskbars.
const IDLE: [u8; 3] = [0x8a, 0x8a, 0x8a];
/// Recording red, taken from `--tk-rec` in `ui/style.css`.
const RECORDING: [u8; 3] = [0xce, 0x1f, 0x1f];

const SIZE: u32 = 32;
const CENTER: f32 = 16.0;
const RADIUS: f32 = 11.0;

pub fn setup(app: &tauri::AppHandle) -> tauri::Result<()> {
    TrayIconBuilder::with_id(ID)
        .icon(dot(IDLE))
        .tooltip("TakeKit")
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
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

pub fn set_recording(app: &tauri::AppHandle, recording: bool) {
    let Some(tray) = app.tray_by_id(ID) else {
        crate::probe("tray:missing");
        return;
    };
    let _ = tray.set_icon(Some(dot(if recording { RECORDING } else { IDLE })));
    crate::probe(if recording {
        "tray:recording"
    } else {
        "tray:idle"
    });
}

fn dot(color: [u8; 3]) -> Image<'static> {
    Image::new_owned(dot_rgba(color), SIZE, SIZE)
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
        assert_eq!(px(&buf, 16, 16), [0xce, 0x1f, 0x1f, 255]);
        assert_eq!(px(&buf, 15, 15), [0xce, 0x1f, 0x1f, 255]);
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
}
