//! Microphone capture through Media Foundation.
//!
//! A microphone is addressed by its WASAPI endpoint id and asked for 16-bit
//! PCM at the rate the device already runs at — asking for a different one
//! makes Media Foundation insert a resampler, which costs quality and latency
//! for nothing.
//!
//! The thread, the stop flag and the failure slot are `runner::DeviceCapture`;
//! what is left here is what a microphone does differently.

use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;

use windows::Win32::Media::MediaFoundation::{
    IMFMediaSource, IMFSourceReader, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_ENDPOINT_ID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_GUID, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND,
};

use crate::clock::TakeClock;
use crate::devices::activate;
use crate::error::{MediaError, Result};
use crate::frame::{AudioChunk, MediaFrame};
use crate::mediatype::pcm_type;
use crate::reader::{AUDIO_STREAM, create_reader, native_type, next_sample, sample_bytes};
use crate::runner::{CaptureCtx, DeviceCapture, Pump};

/// What a microphone turned out to produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicFormat {
    pub sample_rate: u32,
    pub channels: u32,
}

/// A running microphone capture. Dropping it stops it.
pub type MicCapture = DeviceCapture<MicFormat>;

impl DeviceCapture<MicFormat> {
    pub fn start(endpoint: &str, out: SyncSender<MediaFrame>, clock: TakeClock) -> Result<Self> {
        let endpoint = endpoint.to_string();
        Self::spawn(
            "the microphone did not open within ten seconds",
            out,
            clock,
            move || {
                let (reader, sample_rate, channels) = open(&endpoint)?;
                Ok((
                    MicFormat {
                        sample_rate,
                        channels,
                    },
                    Box::new(move |ctx| pump(&reader, ctx)) as Pump,
                ))
            },
        )
    }
}

/// Open the microphone and ask it for the PCM it already produces.
fn open(endpoint: &str) -> Result<(IMFSourceReader, u32, u32)> {
    crate::devices::ensure_com();
    let source: IMFMediaSource = activate(
        MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_GUID,
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_ENDPOINT_ID,
        endpoint,
    )?;

    // One reader does both jobs: the native type is read off it, then the
    // type we want is set on it. A throwaway reader for the peek would shut
    // the source down when it was dropped (see `reader::create_reader`).
    let reader = create_reader(&source)?;
    let native = native_type(&reader, AUDIO_STREAM)?;
    // SAFETY: two scalar attribute reads on an owned object.
    let (sample_rate, channels) = unsafe {
        (
            native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?,
            native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)?,
        )
    };
    if sample_rate == 0 || channels == 0 {
        return Err(MediaError::message("the microphone reports no format"));
    }
    // SAFETY: the reader is owned here and returned to the caller still live.
    unsafe {
        reader.SetCurrentMediaType(AUDIO_STREAM, None, &pcm_type(sample_rate, channels)?)?;
    }
    Ok((reader, sample_rate, channels))
}

fn pump(reader: &IMFSourceReader, ctx: CaptureCtx) -> Result<()> {
    // Same bridge as the camera: one offset at the first sample, then the
    // device's own spacing comes through untouched.
    let mut anchor: Option<i64> = None;
    loop {
        if ctx.stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some((sample, time)) = next_sample(reader, AUDIO_STREAM)? else {
            return Ok(());
        };
        let anchor = *anchor.get_or_insert_with(|| ctx.clock.device_anchor(time));
        let data = sample_bytes(&sample)?;
        if !crate::frame::offer(
            &ctx.out,
            MediaFrame::Audio(AudioChunk {
                data,
                time: time - anchor,
            }),
        ) {
            return Ok(());
        }
    }
}
