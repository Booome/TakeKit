//! Camera capture through Media Foundation.
//!
//! A camera is addressed by its symbolic link, which is what survives a
//! re-plug and a restart, and is asked for NV12 at its own resolution: the
//! format it produces natively, and the one an encoder takes without a
//! conversion.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaSource, IMFMediaType, IMFSourceReader,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MFCreateAttributes,
    MFCreateSourceReaderFromMediaSource,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

use crate::clock::TakeClock;
use crate::devices::activate;
use crate::error::{MediaError, Result};
use crate::frame::{MediaFrame, PixelFormat, VideoFrame};
use crate::mediatype::{camera_type, frame_size};
use crate::reader::{VIDEO_STREAM, next_sample, open_reader, sample_bytes};

/// How long a camera is given to open before the take is called off.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// What a camera turned out to produce, which the writer needs before it can
/// size a single frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraFormat {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// A running camera capture. Dropping it stops it.
pub struct CameraCapture {
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
    format: CameraFormat,
}

impl CameraCapture {
    /// Start capturing `link` into `out`.
    pub fn start(link: &str, fps: u32, out: Sender<MediaFrame>, clock: TakeClock) -> Result<Self> {
        let (ready, opened) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));

        let link = link.to_string();
        let thread_stop = stop.clone();
        let thread_failure = failure.clone();
        // Everything Media Foundation has to be created on the thread that
        // reads it: its objects are not `Send`, and COM has to be initialised
        // on that thread anyway.
        let thread = std::thread::spawn(move || match open(&link, fps) {
            Ok((reader, width, height)) => {
                let _ = ready.send(Ok(CameraFormat { width, height, fps }));
                if let Err(e) = pump(&reader, width, height, thread_stop, out, clock)
                    && let Ok(mut slot) = thread_failure.lock()
                {
                    *slot = Some(e.to_string());
                }
            }
            Err(e) => {
                let _ = ready.send(Err(e.to_string()));
            }
        });

        let (format, thread) = wait_for_open(opened, thread)?;
        Ok(Self {
            stop,
            failure,
            thread: Some(thread),
            format,
        })
    }

    pub fn format(&self) -> CameraFormat {
        self.format
    }

    /// Stop the capture and wait for the read loop to leave its blocking call.
    pub fn stop(mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        self.join()
    }

    fn join(&mut self) -> Result<()> {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let failure = self
            .failure
            .lock()
            .map_err(|e| MediaError::message(e.to_string()))?
            .take();
        match failure {
            Some(e) => Err(MediaError::message(e)),
            None => Ok(()),
        }
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Wait for the camera to report its format, turning a camera that never opens
/// into an error instead of a take that records nothing and says so at the end.
fn wait_for_open(
    opened: Receiver<std::result::Result<CameraFormat, String>>,
    thread: JoinHandle<()>,
) -> Result<(CameraFormat, JoinHandle<()>)> {
    match opened.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(format)) => Ok((format, thread)),
        Ok(Err(e)) => Err(MediaError::message(e)),
        Err(_) => {
            let _ = thread.join();
            Err(MediaError::message(
                "the camera did not open within ten seconds",
            ))
        }
    }
}

/// Open the camera and ask it for NV12.
fn open(link: &str, fps: u32) -> Result<(IMFSourceReader, u32, u32)> {
    // SAFETY: COM has to be initialised on this thread before Media Foundation
    // is used from it; a thread that already did it answers with a benign error.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let source: IMFMediaSource = activate(
        MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
        link,
    )?;

    // The resolution is the device's, not ours: a camera has a handful of modes
    // and asking for one it does not have means a black take.
    let native = native_type(&source)?;
    let (width, height) = frame_size(&native)?;
    if width == 0 || height == 0 {
        return Err(MediaError::message("the camera reports no picture size"));
    }
    let reader = open_reader(&source, VIDEO_STREAM, &camera_type(width, height, fps)?)?;
    Ok((reader, width, height))
}

fn native_type(source: &IMFMediaSource) -> Result<IMFMediaType> {
    // SAFETY: the attributes and the reader are owned here; only the media type
    // leaves, and it is reference-counted.
    unsafe {
        let mut attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attributes, 0)?;
        let attributes = attributes.ok_or_else(|| MediaError::message("no attribute store"))?;
        let reader = MFCreateSourceReaderFromMediaSource(source, &attributes)?;
        reader
            .GetNativeMediaType(VIDEO_STREAM, 0)
            .map_err(Into::into)
    }
}

fn pump(
    reader: &IMFSourceReader,
    width: u32,
    height: u32,
    stop: Arc<AtomicBool>,
    out: Sender<MediaFrame>,
    clock: TakeClock,
) -> Result<()> {
    // Bridged once, at the first sample: the camera's clock counts from its
    // own origin and knows nothing about the take's.
    let mut anchor: Option<i64> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some((sample, time)) = next_sample(reader, VIDEO_STREAM)? else {
            return Ok(());
        };
        let anchor = *anchor.get_or_insert_with(|| clock.device_anchor(time));
        let data = sample_bytes(&sample)?;
        if out
            .send(MediaFrame::Video(VideoFrame {
                width,
                height,
                format: PixelFormat::Nv12,
                data,
                time: time - anchor,
            }))
            .is_err()
        {
            // The writer is gone; there is nowhere to put the frames.
            return Ok(());
        }
    }
}
