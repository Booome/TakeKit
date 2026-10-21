//! One-shot frame grabs for the panel's preview.
//!
//! A preview is not a recording: it is one still picture, taken when the
//! editor opens, used as the canvas for choosing a crop. GDI is the right tool
//! here — it answers immediately, while Windows Graphics Capture only delivers
//! a frame when the picture *changes*, so a still window can deliver nothing at
//! all. A still does not need to be fast.

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HDC, HGDIOBJ, MONITORINFO, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HMONITOR};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;
use windows::core::BOOL;

use crate::{MediaError, Result};

/// `PW_RENDERFULLCONTENT` (0x0002) is absent from the bindings — the SDK
/// documents it in `winuser.h` only for Windows 8.1+, and the generated crate
/// exports `PW_CLIENTONLY` alone. Spelling it here keeps the dependency
/// honest: any other value would render a blank rectangle for WebView2.
const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

/// A still frame in top-down BGRA rows — the layout a BMP or a canvas wants.
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Where a window sits on screen and how big its picture is, in physical
/// pixels — the rectangle a window source records.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Grab a window at the frame the caller says the take will record — the whole
/// window, title bar included.
///
/// The caller measures that frame (and knows things this module must not
/// repeat: which rectangle WGC calls "the window", and what a minimized window
/// will be restored to). This only fills it with pixels.
///
/// `PrintWindow` with `PW_RENDERFULLCONTENT` copies the entire window — title
/// bar and visible borders included — into the DC at (0,0), which is the
/// picture a window source records. `BitBlt` from the *screen* at `frame`'s
/// top-left is the fallback for windows that refuse PrintWindow: the window DC
/// starts at the client area and would shift the picture by the frame, and
/// `GetWindowRect` is not the frame either (Windows 11's invisible resize
/// borders, 7px left/right/bottom, are inside it).
pub fn window(hwnd: isize, frame: Frame) -> Result<Snapshot> {
    if hwnd == 0 {
        return Err(MediaError::message("window handle is 0"));
    }
    if frame.width == 0 || frame.height == 0 {
        return Err(MediaError::message("window has no area"));
    }
    let handle = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: a state query on a handle the caller owns; a dead handle is
    // exactly what this asks about and comes back as `false`.
    if !unsafe { IsWindow(Some(handle)).as_bool() } {
        return Err(MediaError::message(format!("window {hwnd} is gone")));
    }
    let (width, height) = (frame.width as i32, frame.height as i32);

    // SAFETY (whole block): every handle below is created here, released here,
    // and never outlives this function. PrintWindow/BitBlt render into a memory
    // DC the caller sized, so they cannot write past it.
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, width, height);
        let old = SelectObject(mem, HGDIOBJ(bmp.0));

        let painted = PrintWindow(handle, mem, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();
        if !painted {
            let _ = BitBlt(
                mem,
                0,
                0,
                width,
                height,
                Some(screen),
                frame.x,
                frame.y,
                SRCCOPY,
            );
        }

        // GetDIBits wants the bitmap out of any DC.
        SelectObject(mem, old);
        let snap = read_pixels(mem, bmp, width, height);

        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        snap
    }
}

/// Grab one monitor by its one-based index — the same enumeration
/// `windows_capture::monitor::Monitor::from_index` uses for recording, so the
/// preview is the screen the take would capture.
pub fn monitor(index: usize) -> Result<Snapshot> {
    let index = index.max(1);
    let mut rects: Vec<(i32, i32, i32, i32)> = Vec::new();
    let ptr = &mut rects as *mut Vec<(i32, i32, i32, i32)>;

    unsafe extern "system" fn collect(
        hmon: HMONITOR,
        _hdc: HDC,
        _clip: *mut RECT,
        lp: LPARAM,
    ) -> BOOL {
        // SAFETY: `lp` is the `&mut rects` above. EnumDisplayMonitors runs the
        // callback synchronously, so the reference cannot outlive it.
        let list = unsafe { &mut *(lp.0 as *mut Vec<(i32, i32, i32, i32)>) };
        let mut info = MONITORINFO {
            cbSize: core::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a stack struct with `cbSize` set, as the API requires.
        if unsafe { GetMonitorInfoW(hmon, &mut info) }.as_bool() {
            let r = info.rcMonitor;
            list.push((r.left, r.top, r.right - r.left, r.bottom - r.top));
        }
        BOOL(1)
    }

    // SAFETY: the callback is synchronous; see its own SAFETY note.
    let ok = unsafe { EnumDisplayMonitors(None, None, Some(collect), LPARAM(ptr as isize)) };
    if !ok.as_bool() || rects.is_empty() {
        return Err(MediaError::message("no monitors to capture"));
    }
    let (x, y, width, height) = rects
        .get(index - 1)
        .copied()
        .ok_or_else(|| MediaError::message(format!("monitor {index} does not exist")))?;
    if width <= 0 || height <= 0 {
        return Err(MediaError::message("monitor has no area"));
    }

    // SAFETY: the memory DC and bitmap are created and released here; the blit
    // is bounded by the monitor rect it was told about.
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, width, height);
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        let _ = BitBlt(mem, 0, 0, width, height, Some(screen), x, y, SRCCOPY);
        SelectObject(mem, old);
        let snap = read_pixels(mem, bmp, width, height);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        snap
    }
}

/// Copy a DIB section's pixels out as top-down BGRA.
///
/// SAFETY: the caller passes a bitmap it owns, a DC compatible with it, and
/// the dimensions the bitmap was created with.
unsafe fn read_pixels(
    dc: HDC,
    bmp: windows::Win32::Graphics::Gdi::HBITMAP,
    width: i32,
    height: i32,
) -> Result<Snapshot> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative height asks for top-down rows, which is what the BMP writer
            // and the panel both expect.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bytes = vec![0u8; width as usize * height as usize * 4];
    // SAFETY: the caller contract above covers this call.
    let copied = unsafe {
        GetDIBits(
            dc,
            bmp,
            0,
            height as u32,
            Some(bytes.as_mut_ptr() as *mut core::ffi::c_void),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if copied == 0 {
        return Err(MediaError::message("GetDIBits returned no rows"));
    }
    Ok(Snapshot {
        width: width as u32,
        height: height as u32,
        bgra: bytes,
    })
}
