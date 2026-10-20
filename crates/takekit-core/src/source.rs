//! What to record, and how to turn it into ffmpeg input arguments.

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_HWNDNEXT, GW_OWNER, GWL_EXSTYLE, GetClientRect, GetTopWindow, GetWindow,
    GetWindowLongW, GetWindowPlacement, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, HWND_BOTTOM, HWND_NOTOPMOST, HWND_TOPMOST, IsIconic, IsWindow,
    IsWindowVisible, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPlacement,
    SetWindowPos, WINDOWPLACEMENT, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};
use windows::core::BOOL;

/// Which machinery turns a source into a file.
///
/// `Ffmpeg` drives a child process per source and is the long-standing path.
/// `Windows` captures in-process — Windows Graphics Capture for a screen or a
/// window, Media Foundation for a camera — and is the only one that keeps
/// recording a window which sits behind others. It is also the only one that
/// needs no ffmpeg on the machine.
/// Lowercase on the wire: the panel sends `"windows"` / `"ffmpeg"` as plain
/// select values, and a config file holds the same strings. Without the
/// rename, serde writes `"Windows"` but rejects the panel's own value on the
/// way back in — which poisoned the whole config file, not just the field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Ffmpeg,
    Windows,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Ffmpeg => "ffmpeg",
            Backend::Windows => "windows",
        }
    }
}

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
    /// A window, addressed by handle. Two windows can share a title, so the
    /// title is display metadata, not the identity. Its client area is used.
    Window {
        hwnd: isize,
        title: String,
        crop: Crop,
    },
    /// A DirectShow video device, e.g. a phone acting as a webcam.
    Camera { device: String, crop: Crop },
}

/// How the desktop is captured.
///
/// Desktop Duplication (ddagrab) is faster and cannot tear, but it can fail to
/// open: the session is invalidated by display changes, exclusive full-screen
/// apps, or a driver hiccup, and Windows then refuses new sessions until the
/// desktop is rebuilt. GDI (gdigrab) always works, so it is the fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Engine {
    Dda,
    Gdi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// Per-source quality override; `None` uses the session-wide quality.
    #[serde(default)]
    pub quality: Option<u8>,
}

impl Source {
    pub fn new(id: impl Into<String>, capture: Capture, fps: u32) -> Self {
        Self {
            id: id.into(),
            capture,
            fps,
            quality: None,
        }
    }

    /// The screen rectangle this source will capture, if it captures the screen
    /// at all. Needed because Desktop Duplication can succeed at one size and
    /// fail at another, so the probe has to use the real geometry.
    pub fn screen_rect(&self) -> Option<ScreenRect> {
        match &self.capture {
            Capture::Desktop { rect } => Some(rect.unwrap_or_else(virtual_screen_rect)),
            Capture::Window { hwnd, title, crop } => resolve_window(*hwnd, title)
                .ok()
                .map(|w| crop.cropped(w.rect)),
            Capture::Camera { .. } => None,
        }
    }

    /// The ffmpeg input arguments for this source, plus an optional scaling /
    /// cropping filter that runs before encoding.
    pub fn input_args(&self, engine: Engine) -> Result<(Vec<String>, Option<String>), String> {
        match &self.capture {
            Capture::Desktop { rect } => {
                let r = rect.unwrap_or_else(virtual_screen_rect);
                let mut args = screen_args(engine, r, self.fps);
                let filters: Vec<String> = download_filter(engine)
                    .map(str::to_string)
                    .into_iter()
                    .collect();
                args.extend(["-vf".into(), filter_chain(filters)]);
                Ok((args, None))
            }
            Capture::Window { hwnd, title, crop } => {
                let win = resolve_window(*hwnd, title)?;
                // Captured at the cropped rectangle, not at the window plus a
                // crop filter: `cropped` already moved the origin, so filtering
                // again would offset a second time and ask for a rectangle
                // that runs off the captured frame.
                let r = crop.cropped(win.rect);
                let mut args = screen_args(engine, r, self.fps);
                let filters: Vec<String> = download_filter(engine)
                    .map(str::to_string)
                    .into_iter()
                    .collect();
                args.extend(["-vf".into(), filter_chain(filters)]);
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
                let mut filters: Vec<String> = Vec::new();
                if !crop.is_zero() {
                    filters.push(format!(
                        "crop=iw-{}-{}:ih-{}-{}:{}:{}",
                        crop.left, crop.right, crop.top, crop.bottom, crop.left, crop.top
                    ));
                }
                // timestamps straight from the wall clock: dshow reports the
                // device's own counter, which drifts against real time.
                args.insert(0, "-use_wallclock_as_timestamps".into());
                args.insert(1, "1".into());
                Ok((args, Some(filter_chain(filters))))
            }
        }
    }
}

/// H.264 — and every hardware encoder that shares its format — wants both
/// dimensions even: a 4:2:0 frame cannot carry half a pixel, and libx264
/// abandons the whole take rather than round ("height not divisible by 2").
/// Window client areas are routinely odd, so every source trims its frames by at
/// most one pixel on the way to the encoder. On a size that was already even the
/// trim is a no-op.
const EVEN_DIMENSIONS: &str = "crop=trunc(iw/2)*2:trunc(ih/2)*2";

/// The `-vf` chain a source hands ffmpeg: whatever the engine needs first, the
/// even-size trim last.
fn filter_chain(mut filters: Vec<String>) -> String {
    filters.push(EVEN_DIMENSIONS.to_string());
    filters.join(",")
}

/// The filter that brings a captured frame off the GPU, for the engines that
/// hand one over: Desktop Duplication yields D3D11 frames, while the encoder
/// wants system memory. GDI already hands over system-memory frames, and asking
/// it for `hwdownload` stops the take before its first frame.
fn download_filter(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::Dda => Some("hwdownload,format=bgra"),
        Engine::Gdi => None,
    }
}

fn screen_args(engine: Engine, r: ScreenRect, fps: u32) -> Vec<String> {
    match engine {
        Engine::Dda => vec![
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!(
                "ddagrab=output_idx=0:framerate={fps}:draw_mouse=1:offset_x={}:offset_y={}:video_size={}x{}",
                r.x, r.y, r.w, r.h
            ),
        ],
        Engine::Gdi => vec![
            "-f".into(),
            "gdigrab".into(),
            "-framerate".into(),
            fps.to_string(),
            "-draw_mouse".into(),
            "1".into(),
            "-offset_x".into(),
            r.x.to_string(),
            "-offset_y".into(),
            r.y.to_string(),
            "-video_size".into(),
            format!("{}x{}", r.w, r.h),
            "-i".into(),
            "desktop".into(),
        ],
    }
}

impl Crop {
    /// The part of `r` this crop keeps.
    ///
    /// Both edges move: `left`/`top` shift the origin, `right`/`bottom` take
    /// from the far side. Callers capture this rectangle directly, so the
    /// origin has to move with the crop — a size-only shrink would keep the
    /// window's top-left corner and silently cut the wrong edges.
    pub fn cropped(&self, r: ScreenRect) -> ScreenRect {
        // ScreenRect's origin is signed (a monitor left of the primary has a
        // negative x) while the crop is not; an absurd crop clamps instead of
        // wrapping, so a hand-edited config yields an empty region.
        let left = i32::try_from(self.left).unwrap_or(i32::MAX);
        let top = i32::try_from(self.top).unwrap_or(i32::MAX);
        ScreenRect {
            x: r.x.saturating_add(left),
            y: r.y.saturating_add(top),
            // Saturating adds: a hand-edited config with absurd values must
            // yield an empty rectangle, not an overflow panic in debug builds.
            w: r.w.saturating_sub(self.left.saturating_add(self.right)),
            h: r.h.saturating_sub(self.top.saturating_add(self.bottom)),
        }
    }
}

/// A visible top-level window: its identity is `hwnd`, everything else is what
/// the picker shows and what the take record keeps for reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowItem {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub rect: ScreenRect,
}

/// Screen bounds of the whole virtual screen (all monitors).
pub fn virtual_screen_rect() -> ScreenRect {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    // SAFETY: GetSystemMetrics only reads system metrics and takes no
    // pointers.
    unsafe {
        ScreenRect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN) as u32,
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN) as u32,
        }
    }
}

/// The windows the taskbar would show, whose title contains `needle`.
///
/// `exclude_pid` drops one process's own windows. The panel is a window too, and
/// offering "record the panel" can only ever produce a blank take: by the time
/// recording starts, the panel is hidden. A caller that is not itself a window
/// (the CLI) passes `None`.
pub fn list_windows(needle: &str, exclude_pid: Option<u32>) -> Vec<WindowItem> {
    let lower = needle.to_lowercase();
    let mut out: Vec<WindowItem> = Vec::new();
    let ptr = &mut out as *mut Vec<WindowItem>;

    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        // SAFETY: `lp` is the `&mut out` above. `EnumWindows` runs the callback
        // synchronously, so the reference cannot outlive `list_windows`'s
        // stack frame, and the callback is only ever entered from that call.
        let out = unsafe { &mut *(lp.0 as *mut Vec<WindowItem>) };
        if let Some(item) = capture_target(hwnd) {
            out.push(item);
        }
        BOOL(1)
    }

    // SAFETY: the callback only dereferences a pointer that is valid for the
    // duration of this call; a non-zero return keeps the enumeration going to
    // completion.
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(ptr as isize));
    }

    out.into_iter()
        .filter(|w| is_pickable(w, &lower, exclude_pid))
        .collect()
}

/// Whether a window belongs in the picker. `lower_needle` is already lowercased
/// by `list_windows` because it is applied to every window in turn.
fn is_pickable(w: &WindowItem, lower_needle: &str, exclude_pid: Option<u32>) -> bool {
    let titled = lower_needle.is_empty() || w.title.to_lowercase().contains(lower_needle);
    titled && Some(w.pid) != exclude_pid
}

/// The window `hwnd` points at, if it is still something we can capture.
///
/// The handle is checked rather than trusted: between picking a window in the
/// panel and pressing record, it can close, and a stale handle must fail loudly
/// instead of quietly capturing whatever inherited the number. The picker's
/// taskbar rule is deliberately not applied here — a remembered source must
/// still resolve when its app is minimized.
///
/// A minimized window is brought back on the way, without the keyboard focus:
/// DWM stops composing a minimized window entirely, so there is nothing to
/// capture and nothing to measure (its client area reads 0x0). This is part of
/// resolving the window *for capture*, not a Z-order preference — that is
/// `raise_sources`'s job, and the two are independent.
pub fn window_by_hwnd(hwnd: isize) -> Result<WindowItem, String> {
    let handle = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: both calls take a window handle and read state; a stale handle
    // is exactly what they are asked about and comes back as `false`.
    unsafe {
        if !IsWindow(Some(handle)).as_bool() {
            return Err(format!("window {hwnd} is gone"));
        }
        if !IsWindowVisible(handle).as_bool() {
            return Err(format!("window {hwnd} is not visible"));
        }
    }
    restore(handle);
    let rect = client_rect(handle);
    if rect.w == 0 || rect.h == 0 {
        return Err(format!("window {hwnd} has no client area"));
    }
    Ok(WindowItem {
        hwnd,
        pid: window_pid(handle),
        title: window_title(handle),
        rect,
    })
}

/// Bring a window to the front of the Z order, restoring it first if it is
/// minimized. `true` means it is on screen and worth pointing a capture at.
///
/// `SetWindowPos(HWND_TOP)` is the obvious move and the wrong one on Windows 11:
/// when the caller is not the foreground process the raise is dropped silently —
/// the call reports success and the Z order does not move. That is exactly the
/// case here, because the panel is a background window by the time a take
/// starts. Entering the topmost group is not gated that way, so the window is
/// pushed up and then — unless it was topmost to begin with, which is somebody
/// else's setting to keep — taken straight back out.
///
/// The keyboard focus is left where the user put it: `SWP_NOACTIVATE`
/// throughout, because a game that loses focus stops painting the very frames
/// being recorded. Undoing a minimize is the one exception — a window that
/// comes back from minimized is activated by Windows, which is what the user
/// asked for by picking it.
///
/// A window DWM is not drawing — another virtual desktop, a suspended UWP app —
/// cannot be captured even though this succeeds, so it is reported as a failure
/// rather than left to record the desktop behind it.
pub fn bring_to_front(hwnd: isize) -> bool {
    let handle = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: every call in this block takes the window handle (or returns
    // one) and none of them dereferences a pointer we own.
    unsafe {
        if !IsWindow(Some(handle)).as_bool() {
            return false;
        }
        let before = z_rank(handle);
        restore(handle);

        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
        let was_topmost = is_topmost(handle);
        let _ = SetWindowPos(handle, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
        if !was_topmost {
            let _ = SetWindowPos(handle, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
        }

        !cloaked(handle) && matches!((before, z_rank(handle)), (Some(b), Some(a)) if a <= b)
    }
}

/// Bring a minimized window back so it can be recorded. `true` when it *was*
/// minimized and is now up.
///
/// A recording prerequisite, not a preference: WGC delivers zero frames for a
/// minimized window (DWM does not compose it at all), so a take of one would
/// come back empty. Callers that want the window out of the way afterwards can
/// `push_to_back`; whether it is raised at all is `raise_sources`.
pub fn unminimize(hwnd: isize) -> bool {
    let handle = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: a state query and a placement restore on one handle; no pointers.
    let was_minimized = unsafe { IsIconic(handle).as_bool() };
    restore(handle);
    was_minimized
}

/// Put a window behind the others without activating anything — the
/// un-minimize escape hatch for `raise_sources: off`, where the window must be
/// back on screen to be captured but must not land on top of the user's work.
pub fn push_to_back(hwnd: isize) {
    let handle = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: a Z-order move on one handle; no pointers.
    unsafe {
        let _ = SetWindowPos(
            handle,
            Some(HWND_BOTTOM),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Undo a minimize so the window has something to capture again.
///
/// `ShowWindow(SW_RESTORE)` would do it in one line; `SW_SHOWNOACTIVATE` says
/// what this wants — a window that comes back without reaching for the keyboard
/// focus on the way.
fn restore(handle: HWND) {
    let mut placement = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: `placement` lives on this stack frame and `length` is set to its
    // size, which is what `GetWindowPlacement` requires.
    unsafe {
        if !IsIconic(handle).as_bool() || GetWindowPlacement(handle, &mut placement).is_err() {
            return;
        }
    }
    placement.showCmd = SW_SHOWNOACTIVATE.0 as u32;
    // SAFETY: same stack-owned struct, still valid, still carrying its size.
    let _ = unsafe { SetWindowPlacement(handle, &placement) };
}

fn is_topmost(handle: HWND) -> bool {
    // SAFETY: reads an extended style bit for a handle; no pointers.
    unsafe { (GetWindowLongW(handle, GWL_EXSTYLE) as u32) & WS_EX_TOPMOST.0 != 0 }
}

/// The top-level windows in Z order, front to back.
fn z_order() -> Vec<HWND> {
    let mut out = Vec::new();
    // SAFETY: walks the top-level window chain by handle only; the null check
    // is the loop's own termination condition, and no pointer is dereferenced.
    unsafe {
        let mut current = match GetTopWindow(None) {
            Ok(h) => h,
            Err(_) => return out,
        };
        while !current.0.is_null() {
            out.push(current);
            current = match GetWindow(current, GW_HWNDNEXT) {
                Ok(h) => h,
                Err(_) => break,
            };
        }
    }
    out
}

/// Where `handle` sits in the top-level Z order; `None` if it is not there.
fn z_rank(handle: HWND) -> Option<usize> {
    z_order().iter().position(|h| *h == handle)
}

/// Whether Esc was pressed since the last call to this function.
///
/// The low bit is the edge that survives a poll loop; the high bit alone would
/// miss a tap shorter than the interval. This is how the GUI sees Esc while a
/// take counts down: the veil is deliberately unfocusable (focusing it would
/// deactivate the window the user is about to record), so the keypress belongs
/// to whatever window is in front and a keydown handler would never see it.
pub fn escape_pressed() -> bool {
    // SAFETY: GetAsyncKeyState takes a virtual-key code and reads thread
    // input state; it has no pointer or lifetime preconditions. This windows
    // crate takes the raw i32, not the VIRTUAL_KEY newtype.
    unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 1 != 0 }
}

/// Where a window sits in the top-level Z order, front to back; `None` if the
/// handle is not a visible top-level window.
///
/// A way for callers to tell a raise that worked from one Windows quietly
/// dropped: `SetWindowPos` reports success either way.
pub fn z_rank_of(hwnd: isize) -> Option<usize> {
    z_rank(HWND(hwnd as *mut core::ffi::c_void))
}

/// Resolve a title substring to exactly one window.
///
/// Our own process is excluded for the same reason the picker excludes it: a
/// remembered source that falls back to a title (its handle died) must never
/// resolve to the panel that is running the recording.
pub fn find_window(needle: &str) -> Result<WindowItem, String> {
    pick_only(list_windows(needle, Some(std::process::id())), needle)
}

/// The window a source points at.
///
/// The handle is the identity, because two windows can share a title. A live
/// handle wins even when the title has changed — browser tabs, editors adding
/// an unsaved marker and every app that rewrites its caption would otherwise
/// stop resolving, and the take would report a window it can see as missing.
///
/// The title is only a fallback for the one case the handle cannot cover: the
/// window is gone (or not capturable), which is also what a remembered source
/// looks like after its app restarted.
pub fn resolve_window(hwnd: isize, title: &str) -> Result<WindowItem, String> {
    if let Ok(w) = window_by_hwnd(hwnd) {
        return Ok(w);
    }
    find_window(title).map_err(|e| format!("window {hwnd} \"{title}\": {e}"))
}

/// Whether this capture can still be recorded for real.
///
/// A window can close, be moved to another virtual desktop or have its app
/// suspended — a take of it would record whatever is behind instead. Callers
/// drop such a source rather than record the wrong thing.
///
/// The handle is the identity, so a window that has closed is not followed to a
/// look-alike by title. A minimized window stays a source: the take brings it
/// back before capture (WGC cannot see a minimized window at all), and this
/// stays `window_by_hwnd`-free because status polling must never move the
/// user's windows around. Cameras and the desktop are taken on trust; the take
/// itself reports a device that is gone.
pub fn is_recordable(capture: &Capture) -> bool {
    match capture {
        Capture::Window { hwnd, .. } => live_window(HWND(*hwnd as *mut core::ffi::c_void)),
        Capture::Desktop { .. } | Capture::Camera { .. } => true,
    }
}

/// A window a take could still be pointed at: it exists, the shell would draw
/// it, and DWM is drawing it.
fn live_window(hwnd: HWND) -> bool {
    // SAFETY: two handle queries, no pointers; a dead handle answers `false`.
    let shown = unsafe { IsWindow(Some(hwnd)).as_bool() && IsWindowVisible(hwnd).as_bool() };
    shown && !cloaked(hwnd)
}

fn pick_only(mut hits: Vec<WindowItem>, needle: &str) -> Result<WindowItem, String> {
    if hits.is_empty() {
        return Err(format!("no visible window matches \"{needle}\""));
    }
    if hits.len() > 1 {
        let list = hits
            .iter()
            .map(|w| format!("  [{}] {}", w.hwnd, w.title))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "\"{needle}\" matches {} windows; pass a handle (#<hwnd>) or a longer title:\n{list}",
            hits.len()
        ));
    }
    Ok(hits.remove(0))
}

/// The window flags the shell's taskbar rule reads.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ShellFlags {
    tool: bool,
    app: bool,
    owned: bool,
    cloaked: bool,
}

/// Whether the taskbar would give this window a button, which is what makes it
/// a window the user can pick on purpose.
///
/// Tool windows (the NVIDIA overlay, the desktop's Program Manager), windows
/// DWM has cloaked (a suspended UWP app, one on another virtual desktop) and
/// owned dialogs are all left out. Measured against this machine's taskbar and
/// its Alt+Tab list, which agree on exactly the applications that appear there.
fn is_switchable(f: ShellFlags) -> bool {
    !f.cloaked && !f.tool && (f.app || !f.owned)
}

fn shell_flags(hwnd: HWND) -> ShellFlags {
    // SAFETY: reads an extended style and the owner handle for `hwnd`; both
    // are handle-based queries with no pointer or lifetime preconditions.
    unsafe {
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        ShellFlags {
            tool: ex & WS_EX_TOOLWINDOW.0 != 0,
            app: ex & WS_EX_APPWINDOW.0 != 0,
            owned: GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.0.is_null()),
            cloaked: cloaked(hwnd),
        }
    }
}

/// A cloaked window is one DWM is not drawing: another virtual desktop, or a
/// suspended UWP app. It is "visible" to `IsWindowVisible`, and it is not on
/// the screen.
fn cloaked(hwnd: HWND) -> bool {
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};

    let mut value = 0u32;
    let size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `value` is a live `u32` on this frame and `size` is its exact
    // size, which is the documented contract for DwmGetWindowAttribute.
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            std::ptr::addr_of_mut!(value).cast(),
            size,
        )
    };
    ok.is_ok() && value != 0
}

/// A window worth offering: one the taskbar would show, visible, titled, and
/// with a client area.
fn capture_target(hwnd: HWND) -> Option<WindowItem> {
    // SAFETY: a visibility query on a handle; a closed window answers `false`.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
    }
    if !is_switchable(shell_flags(hwnd)) {
        return None;
    }
    let title = window_title(hwnd);
    if title.is_empty() {
        return None;
    }
    let rect = client_rect(hwnd);
    if rect.w == 0 || rect.h == 0 {
        return None;
    }
    Some(WindowItem {
        hwnd: hwnd.0 as isize,
        pid: window_pid(hwnd),
        title,
        rect,
    })
}

/// The process that owns a window.
fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: `pid` is a live local and the call writes exactly one u32 into
    // it; a failed lookup leaves it at zero.
    unsafe {
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

fn window_title(hwnd: HWND) -> String {
    // SAFETY: the buffer is sized from the window's own title length plus the
    // terminator, and `GetWindowTextW` writes at most that many UTF-16 units.
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// Client area of a window, in screen coordinates (physical pixels).
fn client_rect(hwnd: HWND) -> ScreenRect {
    // SAFETY: `rc` is a stack-owned RECT that the call fills in; it is also
    // passed to ClientToScreen below while still borrowed by this frame.
    unsafe {
        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc).is_err() {
            return ScreenRect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            };
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
    use std::sync::Mutex;
    use windows::Win32::Foundation::{HINSTANCE, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::HWND_BOTTOM;

    #[test]
    fn crop_reduces_the_region() {
        let c = Crop {
            top: 33,
            bottom: 0,
            left: 0,
            right: 0,
        };
        let r = c.cropped(ScreenRect {
            x: 8,
            y: 66,
            w: 1920,
            h: 1134,
        });
        assert_eq!(r.h, 1101);
        assert_eq!(r.w, 1920);
    }

    #[test]
    fn crop_moves_the_origin_of_the_kept_region() {
        // Left/top cut from the near side, so the captured rectangle has to
        // start there — a size-only shrink would keep the old corner and cut
        // the wrong edges.
        let c = Crop {
            top: 10,
            bottom: 20,
            left: 30,
            right: 40,
        };
        let r = c.cropped(ScreenRect {
            x: 100,
            y: 200,
            w: 1000,
            h: 800,
        });
        assert_eq!(
            r,
            ScreenRect {
                x: 130,
                y: 210,
                w: 930,
                h: 770
            }
        );
    }

    #[test]
    fn an_absurd_crop_yields_an_empty_region_instead_of_panicking() {
        let c = Crop {
            top: u32::MAX,
            bottom: u32::MAX,
            left: u32::MAX,
            right: u32::MAX,
        };
        let r = c.cropped(ScreenRect {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        });
        assert_eq!((r.w, r.h), (0, 0));
    }

    fn window(hwnd: isize) -> WindowItem {
        window_of(hwnd, 0)
    }

    fn window_of(hwnd: isize, pid: u32) -> WindowItem {
        WindowItem {
            hwnd,
            pid,
            title: "Editor".to_string(),
            rect: ScreenRect {
                x: 0,
                y: 0,
                w: 640,
                h: 480,
            },
        }
    }

    #[test]
    fn only_windows_the_taskbar_would_show_are_offered() {
        let plain = ShellFlags {
            tool: false,
            app: false,
            owned: false,
            cloaked: false,
        };
        assert!(is_switchable(plain));

        // Program Manager and the NVIDIA overlay are tool windows.
        assert!(!is_switchable(ShellFlags {
            tool: true,
            ..plain
        }));
        // A suspended UWP app, or one on another virtual desktop, is cloaked.
        assert!(!is_switchable(ShellFlags {
            cloaked: true,
            ..plain
        }));
        // An owned window is a dialog belonging to another window...
        assert!(!is_switchable(ShellFlags {
            owned: true,
            ..plain
        }));
        // ...unless its app says it belongs on the taskbar.
        assert!(is_switchable(ShellFlags {
            owned: true,
            app: true,
            ..plain
        }));
        // The tool bit wins over the app bit, as it does in the shell.
        assert!(!is_switchable(ShellFlags {
            tool: true,
            app: true,
            ..plain
        }));
    }

    #[test]
    fn our_own_windows_are_not_offered() {
        let mine = window_of(7, 100);
        let theirs = window_of(8, 200);

        assert!(is_pickable(&theirs, "", Some(100)));
        assert!(!is_pickable(&mine, "", Some(100)));
        assert!(
            is_pickable(&mine, "", None),
            "the CLI is not a window; it has nothing to hide"
        );
        assert!(
            is_pickable(&theirs, "edit", None),
            "the title filter still lets a match through"
        );
        assert!(
            !is_pickable(&theirs, "firefox", None),
            "and it still drops a window that does not match"
        );
    }

    #[test]
    fn only_one_window_may_answer_a_title() {
        let one = window(7);
        assert_eq!(pick_only(vec![one.clone()], "edit").unwrap(), one);

        let err = pick_only(vec![one.clone(), window(8)], "edit").unwrap_err();
        assert!(err.contains("matches 2 windows"), "{err}");
        assert!(err.contains("[8]"), "handles are what disambiguates: {err}");

        let err = pick_only(Vec::new(), "edit").unwrap_err();
        assert!(err.contains("no visible window matches"), "{err}");
    }

    #[test]
    fn a_dead_handle_is_an_error() {
        let err = window_by_hwnd(0).unwrap_err();
        assert!(err.contains("is gone"), "{err}");
    }

    #[test]
    fn a_window_that_is_gone_cannot_be_brought_to_front() {
        assert!(!bring_to_front(0));
        // Un-minimizing and Z-order moves are just as inert on a dead handle:
        // status paths must be able to call them without a window to move.
        assert!(!unminimize(0));
        push_to_back(0);
    }

    #[test]
    fn a_window_that_is_gone_is_not_recordable() {
        let gone = Capture::Window {
            hwnd: 0,
            title: "TakeKit no such window".to_string(),
            crop: Crop::default(),
        };
        assert!(!is_recordable(&gone));
    }

    /// There is no window to watch, so there is nothing that can turn
    /// unavailable behind the user's back. A camera that has been unplugged is
    /// found out by the take itself, which reports the device properly.
    #[test]
    fn a_desktop_or_camera_source_is_taken_on_trust() {
        assert!(is_recordable(&Capture::Desktop { rect: None }));
        assert!(is_recordable(&Capture::Camera {
            device: "unplugged anyway".to_string(),
            crop: Crop::default(),
        }));
    }

    /// The two tests below drive real windows; they take turns so that one
    /// raising its window cannot be mistaken for the other's.
    static INTERACTIVE: Mutex<()> = Mutex::new(());

    /// Needs an interactive desktop: puts a window at the back and checks that
    /// `bring_to_front` moves it up the Z order — the thing the unit test above
    /// cannot see.
    ///
    /// `cargo test -p takekit-core --ignored --nocapture z_order`.
    #[test]
    #[ignore = "shows a real window; run by hand on an interactive desktop"]
    fn a_window_at_the_back_comes_to_the_front() {
        let _turn = INTERACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        let window = TestWindow::open();

        // A new window is already in front, which would make the assertion
        // below meaningless.
        window.push_to_back();
        let rank_before = z_rank(window.handle).expect("still in the Z order");

        assert!(bring_to_front(window.hwnd()));

        assert!(
            z_rank(window.handle).expect("still in the Z order") < rank_before,
            "the window did not move up the Z order"
        );
    }

    /// With `raise_sources` off the window must still come back — WGC
    /// delivers zero frames for a minimized one — but it must come back
    /// *behind* the other windows, not on top of the work it interrupted.
    ///
    /// `cargo test -p takekit-core --ignored --nocapture quietly`.
    #[test]
    #[ignore = "shows a real window; run by hand on an interactive desktop"]
    fn a_minimized_window_comes_back_quietly_when_not_raised() {
        use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SW_MINIMIZE, ShowWindow};

        let _turn = INTERACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        let window = TestWindow::open();
        let rank_before = z_rank(window.handle).expect("still in the Z order");

        unsafe {
            let _ = ShowWindow(window.handle, SW_MINIMIZE);
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
        assert!(unsafe { IsIconic(window.handle).as_bool() });

        assert!(
            unminimize(window.hwnd()),
            "it was minimized, so this is a restore"
        );
        assert!(
            !unsafe { IsIconic(window.handle).as_bool() },
            "still minimized"
        );
        // Restoring alone can leave it on top of the user's work…
        push_to_back(window.hwnd());
        // …and this is the point: it lands behind, not in front.
        assert!(
            z_rank(window.handle).expect("still in the Z order") > rank_before,
            "the window came back in front of the user's work"
        );
        // A second call is honest about having nothing to do.
        assert!(!unminimize(window.hwnd()));
    }

    /// A minimized window is still the one the user picked, and a take of it
    /// must not come back empty: `bring_to_front` has to undo the minimize
    /// before the first frame. Windows activates a window as it restores it, so
    /// unlike a plain raise this does move the focus — nothing that can be
    /// helped, and the user asked for that window in the first place.
    ///
    /// `cargo test -p takekit-core --ignored --nocapture minimized`.
    #[test]
    #[ignore = "shows a real window; run by hand on an interactive desktop"]
    fn a_minimized_window_comes_back_before_it_is_captured() {
        use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SW_MINIMIZE, ShowWindow};

        let _turn = INTERACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        let window = TestWindow::open();

        // From the back, so that "came back in front" means something.
        window.push_to_back();
        let rank_before = z_rank(window.handle).expect("still in the Z order");

        unsafe {
            let _ = ShowWindow(window.handle, SW_MINIMIZE);
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
        assert!(
            unsafe { IsIconic(window.handle).as_bool() },
            "the window is not minimized"
        );

        assert!(bring_to_front(window.hwnd()));

        assert!(
            !unsafe { IsIconic(window.handle).as_bool() },
            "the window came back still minimized"
        );
        assert!(
            z_rank(window.handle).expect("still in the Z order") < rank_before,
            "the window came back behind where it was"
        );
    }

    /// The suspended-UWP / other-virtual-desktop case, made real: the test
    /// window cloaks itself exactly the way Windows cloaks those, and has to
    /// drop out of the picker and out of the recordable set at the same time.
    ///
    /// `cargo test -p takekit-core --ignored --nocapture cloaked`.
    #[test]
    #[ignore = "shows a real window; run by hand on an interactive desktop"]
    fn a_cloaked_window_is_neither_offered_nor_recordable() {
        use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

        let _turn = INTERACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        let window = TestWindow::open();
        let capture = Capture::Window {
            hwnd: window.hwnd(),
            title: "TakeKit test window".to_string(),
            crop: Crop::default(),
        };

        assert!(
            is_recordable(&capture),
            "a plain window is recordable to begin with"
        );
        assert!(offered(&window), "and it is in the picker to begin with");

        window.cloak();
        assert!(waits_until_cloaked(window.handle), "the window cloaked");
        assert!(
            unsafe { IsWindowVisible(window.handle).as_bool() },
            "and it is still \"visible\" to the shell, which is why the cloak bit has to be read"
        );

        assert!(!is_recordable(&capture));
        assert!(!offered(&window));
    }

    /// Minimized is not gone: the window is still on the desktop and
    /// `bring_to_front` puts it back, so the source stays where it is.
    ///
    /// `cargo test -p takekit-core --ignored --nocapture minimized`.
    #[test]
    #[ignore = "shows a real window; run by hand on an interactive desktop"]
    fn a_minimized_window_is_still_recordable() {
        use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SW_MINIMIZE, ShowWindow};

        let _turn = INTERACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        let window = TestWindow::open();
        let capture = Capture::Window {
            hwnd: window.hwnd(),
            title: "TakeKit test window".to_string(),
            crop: Crop::default(),
        };

        unsafe {
            let _ = ShowWindow(window.handle, SW_MINIMIZE);
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
        assert!(
            unsafe { IsIconic(window.handle).as_bool() },
            "the window is not minimized"
        );

        assert!(
            is_recordable(&capture),
            "a minimized window is recovered by bring_to_front, not dropped"
        );
    }

    /// Whether the picker would still offer the test window.
    fn offered(window: &TestWindow) -> bool {
        list_windows("TakeKit test window", None)
            .iter()
            .any(|w| w.hwnd == window.hwnd())
    }

    /// DWM applies the cloak asynchronously, so the bit needs a moment to read
    /// back. `false` after two seconds means it never took.
    fn waits_until_cloaked(handle: HWND) -> bool {
        for _ in 0..40 {
            if cloaked(handle) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    /// A plain window of the test's own.
    ///
    /// The obvious choice — Notepad — is a single process with several windows
    /// that restores its previous session on launch: the tests could not tell
    /// which window they had got, and would leave the others on the desktop for
    /// whoever is watching. This one is created, known and closed here.
    struct TestWindow {
        handle: HWND,
        pump: Option<std::thread::JoinHandle<()>>,
    }

    impl TestWindow {
        /// Shows the window and returns it once its message loop is running.
        fn open() -> Self {
            use windows::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, DispatchMessageW, GetMessageW, MSG, RegisterClassW,
                TranslateMessage, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
            };
            use windows::core::w;

            let (sender, receiver) = std::sync::mpsc::channel();
            let pump = std::thread::spawn(move || unsafe {
                let instance = GetModuleHandleW(None).expect("this module");
                let class = WNDCLASSW {
                    lpfnWndProc: Some(window_proc),
                    hInstance: HINSTANCE(instance.0),
                    lpszClassName: w!("TakeKitTestWindow"),
                    ..Default::default()
                };
                let _ = RegisterClassW(&class);
                let handle = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("TakeKitTestWindow"),
                    w!("TakeKit test window"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    400,
                    300,
                    620,
                    420,
                    None,
                    None,
                    Some(HINSTANCE(instance.0)),
                    None,
                )
                .expect("a window");
                // `HWND` holds a raw pointer and does not cross threads; the
                // address does.
                let _ = sender.send(handle.0 as isize);

                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            });

            let handle = receiver.recv().expect("the window thread");
            Self {
                handle: HWND(handle as *mut core::ffi::c_void),
                pump: Some(pump),
            }
        }

        fn hwnd(&self) -> isize {
            self.handle.0 as isize
        }

        fn push_to_back(&self) {
            unsafe {
                let _ = SetWindowPos(
                    self.handle,
                    Some(HWND_BOTTOM),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }

        /// Stop DWM drawing the window, the way another virtual desktop or a
        /// suspended UWP app does it. Only an app's own window may be cloaked
        /// this way, which is why the test makes its own.
        fn cloak(&self) {
            use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
            use windows::core::BOOL;

            let on = BOOL(1);
            unsafe {
                DwmSetWindowAttribute(
                    self.handle,
                    DWMWA_CLOAK,
                    std::ptr::addr_of!(on).cast(),
                    std::mem::size_of::<BOOL>() as u32,
                )
                .expect("cloak our own window");
            }
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            use windows::Win32::Foundation::{LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

            unsafe {
                let _ = PostMessageW(Some(self.handle), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            // `window_proc` ends the message loop with the window, so this
            // returns as soon as it is gone.
            if let Some(pump) = self.pump.take() {
                let _ = pump.join();
            }
        }
    }

    /// Closes the window when asked, and ends the loop with it.
    unsafe extern "system" fn window_proc(
        handle: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        use windows::Win32::UI::WindowsAndMessaging::{
            DefWindowProcW, DestroyWindow, PostQuitMessage, WM_CLOSE, WM_DESTROY,
        };

        unsafe {
            match message {
                WM_CLOSE => {
                    let _ = DestroyWindow(handle);
                    LRESULT(0)
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(handle, message, wparam, lparam),
            }
        }
    }

    #[test]
    fn a_window_capture_keeps_its_handle() {
        let capture = Capture::Window {
            hwnd: 4660,
            title: "Editor".to_string(),
            crop: Crop::default(),
        };
        let json = serde_json::to_string(&capture).unwrap();
        assert!(json.contains("\"hwnd\":4660"), "{json}");
        assert_eq!(serde_json::from_str::<Capture>(&json).unwrap(), capture);
    }

    /// `hwdownload` is how a Desktop Duplication frame leaves the GPU, and only
    /// that engine produces one: asking GDI for it stops the take before the
    /// first frame ("Impossible to convert between the formats").
    #[test]
    fn only_the_desktop_duplication_engine_asks_for_a_d3d_frame() {
        let source = Source::new(
            "screen",
            Capture::Desktop {
                rect: Some(ScreenRect {
                    x: 0,
                    y: 0,
                    w: 640,
                    h: 480,
                }),
            },
            30,
        );

        let (args, _) = source.input_args(Engine::Gdi).unwrap();
        assert!(args.iter().any(|a| a == "gdigrab"), "{args:?}");
        assert!(!args.iter().any(|a| a.contains("hwdownload")), "{args:?}");

        let (args, _) = source.input_args(Engine::Dda).unwrap();
        assert!(args.iter().any(|a| a.contains("hwdownload")), "{args:?}");
    }

    /// H.264 needs both dimensions even and libx264 abandons the whole take when
    /// they are not ("height not divisible by 2"). A window's client area is
    /// routinely odd, so every source ends its filter chain with the trim — and
    /// the engine's own filter still comes first.
    #[test]
    fn every_source_trims_its_frames_to_even_dimensions() {
        let source = Source::new(
            "screen",
            Capture::Desktop {
                rect: Some(ScreenRect {
                    x: 0,
                    y: 0,
                    w: 640,
                    h: 561,
                }),
            },
            30,
        );

        let (args, _) = source.input_args(Engine::Gdi).unwrap();
        let vf = filter_arg(&args);
        assert_eq!(vf, "crop=trunc(iw/2)*2:trunc(ih/2)*2", "{args:?}");

        let (args, _) = source.input_args(Engine::Dda).unwrap();
        let vf = filter_arg(&args);
        assert_eq!(
            vf, "hwdownload,format=bgra,crop=trunc(iw/2)*2:trunc(ih/2)*2",
            "{args:?}"
        );
    }

    /// A camera has no window geometry, but its crop can still leave an odd
    /// size behind, so it gets the same trim.
    #[test]
    fn a_camera_source_trims_its_frames_to_even_dimensions() {
        let source = Source::new(
            "cam",
            Capture::Camera {
                device: "phone".to_string(),
                crop: Crop {
                    top: 1,
                    bottom: 0,
                    left: 0,
                    right: 0,
                },
            },
            30,
        );

        let (_, filter) = source.input_args(Engine::Gdi).unwrap();
        let filter = filter.expect("a filter chain");
        assert!(
            filter.ends_with(",crop=trunc(iw/2)*2:trunc(ih/2)*2"),
            "{filter}"
        );
    }

    fn filter_arg(args: &[String]) -> String {
        let at = args.iter().position(|a| a == "-vf").expect("-vf");
        args[at + 1].clone()
    }

    #[test]
    fn backend_is_lowercase_on_the_wire_both_ways() {
        // The panel sends `"windows"`; serde must accept it, and the config
        // it writes must read back. PascalCase here once poisoned the whole
        // config: one rejected field fell every field back to defaults.
        let from_panel: Backend = serde_json::from_str("\"windows\"").unwrap();
        assert_eq!(from_panel, Backend::Windows);
        let from_panel: Backend = serde_json::from_str("\"ffmpeg\"").unwrap();
        assert_eq!(from_panel, Backend::Ffmpeg);
        assert_eq!(
            serde_json::to_string(&Backend::Windows).unwrap(),
            "\"windows\""
        );
    }
}
