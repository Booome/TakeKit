//! Reading samples out of a Media Foundation source.
//!
//! One source reader drives either a camera or a microphone: the only thing
//! that differs is which stream is read and which media type is asked for.

use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_ENDOFSTREAM, MFCreateAttributes,
    MFCreateSourceReaderFromMediaSource,
};
use windows::Win32::System::Com::COINIT_MULTITHREADED;
use windows::Win32::System::Com::CoInitializeEx;

use crate::error::{MediaError, Result};

/// The stream index Media Foundation gives a device's video. It is not 0: it is
/// a sentinel (`0xFFFFFFFE`) standing for "the first video stream", so the
/// constant has to be used rather than a number.
pub const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// Likewise for audio (`0xFFFFFFFD`).
pub const AUDIO_STREAM: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
/// The bit set on a sample that says the stream has ended.
const END_OF_STREAM: u32 = MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;

/// Create a source reader without choosing a media type yet.
///
/// A source reader shuts down the media source it was created from when it is
/// dropped, so exactly one reader may exist per source: a throwaway reader
/// used to peek at the native type would leave the real one reading a shut
/// down source (`MF_E_SHUTDOWN`).
pub fn create_reader(
    source: &windows::Win32::Media::MediaFoundation::IMFMediaSource,
) -> Result<IMFSourceReader> {
    // COM has to be initialised on the thread that touches Media Foundation,
    // and a thread that already did it answers with a benign error.
    // SAFETY: `CoInitializeEx` is process-global and idempotent per thread.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    // SAFETY: `attributes` and the reader are owned by this frame and the
    // function's return value respectively; no pointer outlives either.
    unsafe {
        let mut attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attributes, 0)?;
        let attributes = attributes.ok_or_else(|| MediaError::message("no attribute store"))?;
        let reader = MFCreateSourceReaderFromMediaSource(source, &attributes)?;
        Ok(reader)
    }
}

/// The type `stream` produces before any conversion is asked for.
pub fn native_type(reader: &IMFSourceReader, stream: u32) -> Result<IMFMediaType> {
    // SAFETY: the reader is borrowed and the returned type is reference-counted.
    unsafe { reader.GetNativeMediaType(stream, 0).map_err(Into::into) }
}

/// The next sample and the device timestamp it carries.
///
/// `None` means the stream ended — a camera that was unplugged, a microphone
/// that was disabled — which is a stop, not an error: the take so far is still
/// worth keeping.
///
/// A read can also come back empty without the stream having ended: the type
/// changed, or the device had nothing new yet. Those just read again — only
/// the end-of-stream flag means stop.
pub fn next_sample(reader: &IMFSourceReader, stream: u32) -> Result<Option<(IMFSample, i64)>> {
    loop {
        // SAFETY: every out-parameter is a live local on this frame, and the
        // sample comes back reference-counted.
        unsafe {
            let mut sample: Option<IMFSample> = None;
            let mut flags = 0u32;
            let mut time = 0i64;
            reader.ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut time),
                Some(&mut sample),
            )?;

            if flags & END_OF_STREAM != 0 {
                return Ok(None);
            }
            if let Some(sample) = sample {
                return Ok(Some((sample, time)));
            }
        }
    }
}

/// The bytes of a sample, whatever planes it has.
pub fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    // SAFETY: the buffer is locked and unlocked on this frame; the bytes are
    // copied out before the unlock, so nothing borrowed from the buffer escapes.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut data = std::ptr::null_mut::<u8>();
        let mut length = 0u32;
        buffer.Lock(&mut data, None, Some(&mut length))?;
        let bytes = if data.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data, length as usize).to_vec()
        };
        buffer.Unlock()?;
        Ok(bytes)
    }
}
