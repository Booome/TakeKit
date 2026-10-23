//! Screen and window capture through Windows Graphics Capture.
//!
//! WGC is the only capture path that can record a window which is behind others
//! but not minimized — Desktop Duplication and GDI both capture the desktop, so
//! anything covering the window covers the take. It also hands over a timestamp
//! per frame, which is what lets several sources share one clock.
//!
//! Frames arrive only when something on screen changes. A still desktop
//! produces no frames at all, which is why stopping has to come from outside
//! the frame callback: the callback may simply not be running.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use windows_capture::capture::{CaptureControl, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::clock::TakeClock;
use crate::error::{MediaError, Result};
use crate::frame::{MediaFrame, PixelFormat, VideoFrame};

/// What to point the capture at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenTarget {
    /// A monitor, by the one-based index `Monitor` uses.
    Monitor(usize),
    /// A window, by handle.
    Window(isize),
}

/// A running screen capture. Dropping it stops it.
pub struct ScreenCapture {
    control: Option<CaptureControl<Handler, MediaError>>,
}

impl ScreenCapture {
    /// Start capturing into `out`.
    ///
    /// `fps` is a ceiling, not a rate: Windows Graphics Capture delivers a frame
    /// when the picture changes, so a take of a still screen is one frame and a
    /// long pause. The interval passed here caps how often an update is
    /// *eligible* — without it a 144 Hz display hands over far more frames than
    /// the take needs.
    pub fn start(
        target: ScreenTarget,
        fps: u32,
        out: SyncSender<MediaFrame>,
        clock: TakeClock,
    ) -> Result<Self> {
        let interval =
            MinimumUpdateIntervalSettings::Custom(Duration::from_secs_f64(1.0 / fps.max(1) as f64));
        let flags = (std::sync::Arc::new(AtomicBool::new(false)), out, clock);

        let control = match target {
            ScreenTarget::Monitor(index) => {
                let monitor =
                    Monitor::from_index(index).map_err(|e| MediaError::message(e.to_string()))?;
                launch(monitor, interval, flags)?
            }
            ScreenTarget::Window(hwnd) => {
                let window = Window::from_raw_hwnd(hwnd as *mut core::ffi::c_void);
                launch(window, interval, flags)?
            }
        };

        Ok(Self {
            control: Some(control),
        })
    }

    /// Stop the capture and wait for its frames to stop arriving.
    pub fn stop(mut self) -> Result<()> {
        self.take()
            .stop()
            .map_err(|e| MediaError::message(e.to_string()))
    }

    fn take(&mut self) -> CaptureControl<Handler, MediaError> {
        self.control
            .take()
            .expect("stop and drop both take the control at most once")
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
    }
}

/// Start a capture of one item. Split out because a monitor and a window are
/// two different types reaching the same generic call.
fn launch<T>(
    item: T,
    interval: MinimumUpdateIntervalSettings,
    flags: HandlerFlags,
) -> Result<CaptureControl<Handler, MediaError>>
where
    T: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
    Handler::start_free_threaded(Settings::new(
        item,
        // `WithCursor`, not `Default`: the ffmpeg backend records with
        // `draw_mouse=1`, and a tutorial recording without a pointer is a
        // different product (2026-10-21 对齐). `Default` leaves visibility to
        // the system — for a monitor that has meant no cursor at all.
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        interval,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    ))
    .map_err(|e| MediaError::message(e.to_string()))
}

/// One display, as the picker lists them. `index` is the one-based number
/// `ScreenTarget::Monitor` takes — the position in `Monitor::enumerate`,
/// which is the same order `Monitor::from_index` uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorRect {
    pub index: usize,
    /// The monitor's rectangle in desktop coordinates (physical pixels).
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Every display currently attached, in `Monitor` index order.
pub fn monitors() -> Result<Vec<MonitorRect>> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};

    Monitor::enumerate()
        .map_err(|e| MediaError::message(e.to_string()))?
        .into_iter()
        .enumerate()
        .map(|(i, m)| {
            // GetMonitorInfoW needs the raw handle the wrapper hides; the
            // rectangle is the one thing the wrapper does not expose.
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let ok = unsafe {
                GetMonitorInfoW(
                    windows::Win32::Graphics::Gdi::HMONITOR(m.as_raw_hmonitor() as _),
                    &mut info,
                )
            };
            if !ok.as_bool() {
                return Err(MediaError::message(format!(
                    "monitor {}: GetMonitorInfoW failed",
                    i + 1
                )));
            }
            let RECT {
                left,
                top,
                right,
                bottom,
            } = info.rcMonitor;
            Ok(MonitorRect {
                index: i + 1,
                x: left,
                y: top,
                w: (right - left) as u32,
                h: (bottom - top) as u32,
            })
        })
        .collect()
}

/// Which monitor a desktop rectangle belongs to: the one holding its centre.
/// `None` when nothing does (a stale rect for a display that went away) —
/// the caller falls back to the first monitor rather than failing the take.
pub fn monitor_index_for(x: i32, y: i32, w: u32, h: u32) -> Option<usize> {
    let cx = x + (w as i32 / 2);
    let cy = y + (h as i32 / 2);
    monitors()
        .ok()?
        .into_iter()
        .find(|m| cx >= m.x && cx < m.x + m.w as i32 && cy >= m.y && cy < m.y + m.h as i32)
        .map(|m| m.index)
}

type HandlerFlags = (
    std::sync::Arc<AtomicBool>,
    SyncSender<MediaFrame>,
    TakeClock,
);

struct Handler {
    stop: std::sync::Arc<AtomicBool>,
    out: SyncSender<MediaFrame>,
    clock: TakeClock,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = HandlerFlags;
    type Error = MediaError;

    fn new(
        ctx: windows_capture::capture::Context<Self::Flags>,
    ) -> std::result::Result<Self, Self::Error> {
        let (stop, out, clock) = ctx.flags;
        Ok(Self { stop, out, clock })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame<'_>,
        control: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        // A second way out besides the outside `stop()`: the item is gone, so
        // there is nothing left to capture.
        if self.stop.load(std::sync::atomic::Ordering::SeqCst) {
            control.stop();
            return Ok(());
        }

        // The frame's own clock, not a stopwatch started here: several sources
        // in the same take have to agree on when a moment was, and only the
        // capture timestamps do that. WGC stamps in the system clock, so the
        // take's shared origin is one subtraction away — exact, unlike a
        // device clock which has to be bridged through the wall.
        let time = self.clock.from_system(frame.timestamp()?.Duration);

        let buffer = frame
            .buffer()
            .map_err(|e| MediaError::message(e.to_string()))?;
        let width = buffer.width();
        let height = buffer.height();
        let mut unpadded = Vec::new();
        let data = buffer.as_nopadding_buffer(&mut unpadded).to_vec();

        // A full queue means the writer is behind; `offer` drops the frame —
        // keeping the take going at the cost of one frame rather than blocking
        // the capture — and says no only when the writer is gone.
        crate::frame::offer(
            &self.out,
            MediaFrame::Video(VideoFrame {
                width,
                height,
                format: PixelFormat::Bgra,
                data,
                time,
            }),
        );
        Ok(())
    }
}
