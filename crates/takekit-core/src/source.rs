//! What to record, and how to turn it into ffmpeg input arguments.

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::core::BOOL;
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crop {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

impl Crop {
    pub fn is_zero(&self) -> bool {
        self.top == 0 && self.bottom == 0 && self.left == 0 && self.right == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capture {
    /// The whole desktop, or a region of it.
    Desktop { rect: Option<ScreenRect> },
    /// A window, matched by a substring of its title. Its client area is used.
    Window { title: String, crop: Crop },
    /// A DirectShow video device, e.g. a phone acting as a webcam.
    Camera { device: String, crop: Crop },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// Used for the output file name, e.g. `win` -> `win.mkv`.
    pub id: String,
    pub capture: Capture,
    pub fps: u32,
}

impl Source {
    pub fn new(id: impl Into<String>, capture: Capture) -> Self {
        Self {
            id: id.into(),
            capture,
            fps: 30,
        }
    }

    /// The ffmpeg input arguments for this source, plus an optional scaling /
    /// cropping filter that runs before encoding.
    pub fn input_args(&self) -> Result<(Vec<String>, Option<String>), String> {
        match &self.capture {
            Capture::Desktop { rect } => {
                let r = rect.unwrap_or_else(virtual_screen_rect);
                let mut args = ddagrab_args(r, self.fps);
                args.extend(["-vf".into(), "hwdownload,format=bgra".into()]);
                Ok((args, None))
            }
            Capture::Window { title, crop } => {
                let win = find_window(title)?;
                let r = crop.cropped(win.rect);
                let mut args = ddagrab_args(r, self.fps);
                let mut filters = vec!["hwdownload,format=bgra".to_string()];
                if !crop.is_zero() {
                    filters.push(format!(
                        "crop={}:{}:{}:{}",
                        r.w, r.h, crop.left, crop.top
                    ));
                }
                args.extend(["-vf".into(), filters.join(",")]);
                Ok((args, None))
            }
            Capture::Camera { device, crop } => {
                let mut args: Vec<String> = vec![
                    "-f".into(),
                    "dshow".into(),
                    "-framerate".into(),
                    self.fps.to_string(),
                    "-i".into(),
                    format!("video={device}"),
                ];
                let filter = if crop.is_zero() {
                    None
                } else {
                    Some(format!(
                        "crop=iw-{}-{}:ih-{}-{}:{}:{}",
                        crop.left, crop.right, crop.top, crop.bottom, crop.left, crop.top
                    ))
                };
                // timestamps straight from the wall clock: dshow reports the
                // device's own counter, which drifts against real time.
                args.insert(0, "-use_wallclock_as_timestamps".into());
                args.insert(1, "1".into());
                if filter.is_some() {
                    // keep the stream CFR after cropping
                }
                Ok((args, filter))
            }
        }
    }
}

fn ddagrab_args(r: ScreenRect, fps: u32) -> Vec<String> {
    vec![
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!(
            "ddagrab=output_idx=0:framerate={fps}:draw_mouse=1:offset_x={}:offset_y={}:video_size={}x{}",
            r.x, r.y, r.w, r.h
        ),
    ]
}

impl Crop {
    fn cropped(&self, r: ScreenRect) -> ScreenRect {
        ScreenRect {
            x: r.x,
            y: r.y,
            w: r.w.saturating_sub(self.left + self.right),
            h: r.h.saturating_sub(self.top + self.bottom),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct WindowInfo {
    rect: ScreenRect,
}

/// Screen bounds of the whole virtual screen (all monitors).
pub fn virtual_screen_rect() -> ScreenRect {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    unsafe {
        ScreenRect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN) as u32,
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN) as u32,
        }
    }
}

/// Visible top-level windows whose title contains `needle`.
pub fn list_windows(needle: &str) -> Vec<(String, ScreenRect)> {
    let needle = needle.to_lowercase();
    let mut out: Vec<(String, ScreenRect)> = Vec::new();
    let ptr = &mut out as *mut Vec<(String, ScreenRect)>;

    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lp.0 as *mut Vec<(String, ScreenRect)>) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1);
            }
            let len = GetWindowTextLengthW(hwnd);
            if len == 0 {
                return BOOL(1);
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            out.push((title, client_rect(hwnd)));
        }
        BOOL(1)
    }

    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(ptr as isize));
    }

    out.into_iter()
        .filter(|(t, _)| needle.is_empty() || t.to_lowercase().contains(&needle))
        .filter(|(_, r)| r.w > 0 && r.h > 0)
        .collect()
}

fn find_window(needle: &str) -> Result<WindowInfo, String> {
    let mut hits = list_windows(needle);
    if hits.is_empty() {
        return Err(format!("no visible window matches \"{needle}\""));
    }
    if hits.len() > 1 {
        let list = hits
            .iter()
            .map(|(t, _)| format!("  {t}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "\"{needle}\" matches {} windows, be more specific:\n{list}",
            hits.len()
        ));
    }
    Ok(WindowInfo {
        rect: hits.remove(0).1,
    })
}

/// Client area of a window, in screen coordinates (physical pixels).
fn client_rect(hwnd: HWND) -> ScreenRect {
    unsafe {
        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc).is_err() {
            return ScreenRect { x: 0, y: 0, w: 0, h: 0 };
        }
        let mut origin = windows::Win32::Foundation::POINT { x: 0, y: 0 };
        let _ = ClientToScreen(hwnd, &mut origin);
        ScreenRect {
            x: origin.x,
            y: origin.y,
            w: (rc.right - rc.left).max(0) as u32,
            h: (rc.bottom - rc.top).max(0) as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_reduces_the_region() {
        let c = Crop { top: 33, bottom: 0, left: 0, right: 0 };
        let r = c.cropped(ScreenRect { x: 8, y: 66, w: 1920, h: 1134 });
        assert_eq!(r.h, 1101);
        assert_eq!(r.w, 1920);
    }
}
