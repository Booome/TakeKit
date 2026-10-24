//! Camera capture through Media Foundation.
//!
//! A camera is addressed by its symbolic link, which is what survives a
//! re-plug and a restart, and is asked for NV12 at its own resolution: the
//! format it produces natively, and the one an encoder takes without a
//! conversion.
//!
//! The thread, the stop flag and the failure slot are `runner::DeviceCapture`;
//! what is left here is what a camera does differently — how it opens and how
//! its samples are turned into frames.

use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;

use windows::Win32::Media::MediaFoundation::{
    IMFMediaSource, IMFSourceReader, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
};

use crate::clock::TakeClock;
use crate::devices::activate;
use crate::error::{MediaError, Result};
use crate::frame::{MediaFrame, PixelFormat, VideoFrame};
use crate::mediatype::{camera_type, frame_size};
use crate::reader::{VIDEO_STREAM, create_reader, native_type, next_sample, sample_bytes};
use crate::runner::{CaptureCtx, DeviceCapture, Pump};

/// What a camera turned out to produce, which the writer needs before it can
/// size a single frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraFormat {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// A running camera capture. Dropping it stops it.
pub type CameraCapture = DeviceCapture<CameraFormat>;

impl DeviceCapture<CameraFormat> {
    /// Start capturing `link` into `out`.
    pub fn start(
        link: &str,
        fps: u32,
        out: SyncSender<MediaFrame>,
        clock: TakeClock,
    ) -> Result<Self> {
        let link = link.to_string();
        Self::spawn(
            "the camera did not open within ten seconds",
            out,
            clock,
            move || {
                let (reader, width, height) = open(&link, fps)?;
                Ok((
                    CameraFormat { width, height, fps },
                    Box::new(move |ctx| pump(&reader, width, height, ctx)) as Pump,
                ))
            },
        )
    }
}

/// Open the camera and ask it for NV12.
fn open(link: &str, fps: u32) -> Result<(IMFSourceReader, u32, u32)> {
    crate::devices::ensure_com();
    let source: IMFMediaSource = activate(
        MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
        link,
    )?;

    // The resolution is the device's, not ours: a camera has a handful of modes
    // and asking for one it does not have means a black take.
    //
    // One reader does both jobs: the native size is read off it, then the type
    // we want is set on it. A throwaway reader for the peek would shut the
    // source down when it was dropped (see `reader::create_reader`).
    let reader = create_reader(&source)?;
    let native = native_type(&reader, VIDEO_STREAM)?;
    let (width, height) = frame_size(&native)?;
    if width == 0 || height == 0 {
        return Err(MediaError::message("the camera reports no picture size"));
    }
    // SAFETY: the reader is owned here and returned to the caller still live.
    unsafe {
        reader.SetCurrentMediaType(VIDEO_STREAM, None, &camera_type(width, height, fps)?)?;
    }
    Ok((reader, width, height))
}

fn pump(reader: &IMFSourceReader, width: u32, height: u32, ctx: CaptureCtx) -> Result<()> {
    // Bridged once, at the first sample: the camera's clock counts from its
    // own origin and knows nothing about the take's.
    let mut anchor: Option<i64> = None;
    loop {
        if ctx.stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some((sample, time)) = next_sample(reader, VIDEO_STREAM)? else {
            return Ok(());
        };
        let anchor = *anchor.get_or_insert_with(|| ctx.clock.device_anchor(time));
        let data = sample_bytes(&sample)?;
        if !crate::frame::offer(
            &ctx.out,
            MediaFrame::Video(VideoFrame {
                width,
                height,
                format: PixelFormat::Nv12,
                data,
                time: time - anchor,
            }),
        ) {
            // The writer is gone; there is nowhere to put the frames.
            return Ok(());
        }
    }
}
