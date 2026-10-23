//! Microphone capture through Media Foundation.
//!
//! A microphone is addressed by its WASAPI endpoint id and asked for 16-bit
//! PCM at the rate the device already runs at — asking for a different one
//! makes Media Foundation insert a resampler, which costs quality and latency
//! for nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{
    IMFMediaSource, IMFSourceReader, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_ENDPOINT_ID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_GUID, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

use crate::clock::TakeClock;
use crate::devices::activate;
use crate::error::{MediaError, Result};
use crate::frame::{AudioChunk, MediaFrame};
use crate::mediatype::pcm_type;
use crate::reader::{AUDIO_STREAM, create_reader, native_type, next_sample, sample_bytes};

/// How long a microphone is given to open before the take is called off.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// What a microphone turned out to produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicFormat {
    pub sample_rate: u32,
    pub channels: u32,
}

/// A running microphone capture. Dropping it stops it.
pub struct MicCapture {
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
    format: MicFormat,
}

impl MicCapture {
    pub fn start(endpoint: &str, out: SyncSender<MediaFrame>, clock: TakeClock) -> Result<Self> {
        let (ready, opened) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));

        let endpoint = endpoint.to_string();
        let thread_stop = stop.clone();
        let thread_failure = failure.clone();
        // Created on the thread that reads it, for the reason given in
        // `camera::open`: Media Foundation objects are not `Send`.
        let thread = std::thread::spawn(move || match open(&endpoint) {
            Ok((reader, sample_rate, channels)) => {
                let _ = ready.send(Ok(MicFormat {
                    sample_rate,
                    channels,
                }));
                if let Err(e) = pump(&reader, thread_stop, out, clock)
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

    pub fn format(&self) -> MicFormat {
        self.format
    }

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

impl Drop for MicCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Wait for the microphone to report its format.
fn wait_for_open(
    opened: Receiver<std::result::Result<MicFormat, String>>,
    thread: JoinHandle<()>,
) -> Result<(MicFormat, JoinHandle<()>)> {
    match opened.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(format)) => Ok((format, thread)),
        Ok(Err(e)) => Err(MediaError::message(e)),
        Err(_) => {
            let _ = thread.join();
            Err(MediaError::message(
                "the microphone did not open within ten seconds",
            ))
        }
    }
}

/// Open the microphone and ask it for the PCM it already produces.
fn open(endpoint: &str) -> Result<(IMFSourceReader, u32, u32)> {
    // SAFETY: COM has to be initialised on this thread before Media Foundation
    // is used from it; a thread that already did it answers with a benign error.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
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

fn pump(
    reader: &IMFSourceReader,
    stop: Arc<AtomicBool>,
    out: SyncSender<MediaFrame>,
    clock: TakeClock,
) -> Result<()> {
    // Same bridge as the camera: one offset at the first sample, then the
    // device's own spacing comes through untouched.
    let mut anchor: Option<i64> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some((sample, time)) = next_sample(reader, AUDIO_STREAM)? else {
            return Ok(());
        };
        let anchor = *anchor.get_or_insert_with(|| clock.device_anchor(time));
        let data = sample_bytes(&sample)?;
        if !crate::frame::offer(
            &out,
            MediaFrame::Audio(AudioChunk {
                data,
                time: time - anchor,
            }),
        ) {
            return Ok(());
        }
    }
}
