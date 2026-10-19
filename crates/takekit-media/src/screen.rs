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
use std::sync::mpsc::Sender;
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
    pub fn start(target: ScreenTarget, fps: u32, out: Sender<MediaFrame>) -> Result<Self> {
        let interval =
            MinimumUpdateIntervalSettings::Custom(Duration::from_secs_f64(1.0 / fps.max(1) as f64));
        let flags = (std::sync::Arc::new(AtomicBool::new(false)), out);

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
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        interval,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    ))
    .map_err(|e| MediaError::message(e.to_string()))
}

type HandlerFlags = (std::sync::Arc<AtomicBool>, Sender<MediaFrame>);

struct Handler {
    stop: std::sync::Arc<AtomicBool>,
    out: Sender<MediaFrame>,
    first: Option<i64>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = HandlerFlags;
    type Error = MediaError;

    fn new(
        ctx: windows_capture::capture::Context<Self::Flags>,
    ) -> std::result::Result<Self, Self::Error> {
        let (stop, out) = ctx.flags;
        Ok(Self {
            stop,
            out,
            first: None,
        })
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
        // capture timestamps do that.
        let time = frame.timestamp()?.Duration;
        let start = *self.first.get_or_insert(time);

        let buffer = frame
            .buffer()
            .map_err(|e| MediaError::message(e.to_string()))?;
        let width = buffer.width();
        let height = buffer.height();
        let mut unpadded = Vec::new();
        let data = buffer.as_nopadding_buffer(&mut unpadded).to_vec();

        // A full queue means the writer is behind; dropping the frame keeps the
        // take going at the cost of one frame rather than blocking the capture.
        let _ = self.out.send(MediaFrame::Video(VideoFrame {
            width,
            height,
            format: PixelFormat::Bgra,
            data,
            time: time - start,
        }));
        Ok(())
    }
}
