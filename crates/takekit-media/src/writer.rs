//! Writing a take through Media Foundation's sink writer.
//!
//! The alternative to handing frames to an ffmpeg child is to hand them to the
//! OS, and that is what makes the capture timestamps worth having: a sample
//! carries the time it was captured, so a take can be variable-frame-rate
//! without drifting, and two sources written by the same process share one
//! clock instead of each child guessing from when it happened to start.
//!
//! Media Foundation inserts the encoder itself: the sink is told it is writing
//! H.264 into an MP4 and is given uncompressed frames, and it loads the
//! encoder transform in between.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSample, IMFSinkWriter, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_AVG_BITRATE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MFAudioFormat_AAC, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Audio,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoFormat_RGB32,
};
use windows::core::PCWSTR;

use crate::devices::ensure_media_foundation;
use crate::error::Result;
use crate::frame::{AudioChunk, PixelFormat, VideoFrame};
use crate::mediatype::{pcm_type, video_type};

/// A frame held until the next one says how long it was on screen.
struct Pending {
    time: i64,
    bytes: Vec<u8>,
}

/// A video file being written, one uncompressed frame at a time.
pub struct VideoWriter {
    writer: IMFSinkWriter,
    stream: u32,
    /// Frames arrive top-down, which is the opposite of what an RGB32 media
    /// type means by default, so each frame is flipped on its way in.
    flip: bool,
    /// The frame waiting for its duration. It cannot be written as it arrives:
    /// how long a frame was on screen is only known once the next one shows up.
    pending: Option<Pending>,
    /// The buffer the previous frame owned, kept so a take recycles one
    /// allocation instead of making one per frame.
    spare: Vec<u8>,
    /// The gap before the last frame, reused for the final one — nothing ever
    /// arrives to say how long that one lasted.
    last_duration: i64,
}

impl VideoWriter {
    /// Start writing `path` as H.264 in an MP4, from top-down BGRA frames.
    pub fn create(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        Self::open(path, PixelFormat::Bgra, width, height, fps, bitrate_kbps)
    }

    /// Start writing `path` from a camera, whose frames are already NV12 and
    /// already top-down — the one case nothing has to be flipped.
    pub fn create_nv12(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        Self::open(path, PixelFormat::Nv12, width, height, fps, bitrate_kbps)
    }

    fn open(
        path: &Path,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        // SAFETY: every call below takes objects owned here; the stream index
        // is the only thing that outlives the call and it is a plain integer.
        unsafe {
            let writer = sink(path)?;
            let output = h264_type(width, height, fps, bitrate_kbps)?;
            let stream = writer.AddStream(&output)?;
            let input = input_type(format, width, height, fps)?;
            writer.SetInputMediaType(stream, &input, None)?;
            writer.BeginWriting()?;
            Ok(Self {
                writer,
                stream,
                flip: matches!(format, PixelFormat::Bgra),
                pending: None,
                spare: Vec::new(),
                last_duration: 0,
            })
        }
    }

    /// Hand over one frame.
    ///
    /// A screen that does not change produces no frames at all, so the picture
    /// has to be held for as long as the pause really was. That gap is only
    /// knowable once the next frame arrives, which is why a frame is kept back
    /// rather than written straight away.
    pub fn write(&mut self, frame: &VideoFrame) -> Result<()> {
        let mut bytes = std::mem::take(&mut self.spare);
        copy_frame(&mut bytes, frame, self.flip);

        if let Some(previous) = self.pending.take() {
            let duration = (frame.time - previous.time).max(0);
            // SAFETY: `previous.bytes` is owned here and the writer copies what
            // it needs before `WriteSample` returns.
            unsafe {
                write_sample(
                    &self.writer,
                    self.stream,
                    &previous.bytes,
                    previous.time,
                    duration,
                )?;
            }
            self.last_duration = duration;
            self.spare = previous.bytes;
        }
        self.pending = Some(Pending {
            time: frame.time,
            bytes,
        });
        Ok(())
    }

    /// Close the file. A take without this is not playable.
    pub fn finish(mut self) -> Result<()> {
        if let Some(last) = self.pending.take() {
            // SAFETY: owned buffer, as above.
            unsafe {
                write_sample(
                    &self.writer,
                    self.stream,
                    &last.bytes,
                    last.time,
                    self.last_duration,
                )?;
            }
        }
        // SAFETY: Finalize flushes and closes; the writer is consumed so it
        // cannot be written to afterwards.
        unsafe { self.writer.Finalize() }.map_err(Into::into)
    }
}

/// An audio file being written, one PCM chunk at a time.
pub struct AudioWriter {
    writer: IMFSinkWriter,
    stream: u32,
    bytes_per_second: u32,
    /// Where the next chunk starts.
    next: Option<i64>,
}

impl AudioWriter {
    pub fn create(path: &Path, sample_rate: u32, channels: u32, bitrate_kbps: u32) -> Result<Self> {
        // SAFETY: owned objects, integer stream index, as above.
        unsafe {
            let writer = sink(path)?;
            let output = aac_type(sample_rate, channels, bitrate_kbps)?;
            let stream = writer.AddStream(&output)?;
            let input = pcm_type(sample_rate, channels)?;
            writer.SetInputMediaType(stream, &input, None)?;
            writer.BeginWriting()?;
            Ok(Self {
                writer,
                stream,
                bytes_per_second: sample_rate * channels * 2,
                next: None,
            })
        }
    }

    /// Write one chunk.
    ///
    /// A capture device hands audio over whenever it has some, in pieces whose
    /// length has nothing to do with the piece before, so a chunk starts where
    /// the previous one ended. Using the timestamp it arrived with instead would
    /// leave a hole in the container every time the device stalled.
    pub fn write(&mut self, chunk: &AudioChunk) -> Result<()> {
        if chunk.data.is_empty() {
            return Ok(());
        }
        let duration = duration_of(chunk.data.len(), self.bytes_per_second);
        let start = self.next.unwrap_or(chunk.time);
        // SAFETY: the chunk is borrowed for the duration of the call, which
        // copies it into the sample's own buffer.
        unsafe {
            write_sample(&self.writer, self.stream, &chunk.data, start, duration)?;
        }
        self.next = Some(start + duration);
        Ok(())
    }

    pub fn finish(self) -> Result<()> {
        // SAFETY: as `VideoWriter::finish`.
        unsafe { self.writer.Finalize() }.map_err(Into::into)
    }
}

/// Put one buffer into the sink at `time` for `duration`.
///
/// # Safety
/// `bytes` must be readable for its whole length, and `writer` must have been
/// told about a stream at `stream`.
unsafe fn write_sample(
    writer: &IMFSinkWriter,
    stream: u32,
    bytes: &[u8],
    time: i64,
    duration: i64,
) -> Result<()> {
    // SAFETY: the caller guarantees `bytes`; the sample and the buffer it owns
    // live until `WriteSample` has taken what it needs.
    unsafe {
        let sample: IMFSample = MFCreateSample()?;
        let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
        let mut target = std::ptr::null_mut::<u8>();
        buffer.Lock(&mut target, None, None)?;
        if !target.is_null() {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
        }
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(duration)?;
        writer.WriteSample(stream, &sample)?;
    }
    Ok(())
}

/// Trim a frame to even dimensions.
///
/// H.264 stores chroma at half resolution, so a frame with an odd width or
/// height cannot be encoded at all — and a monitor or a window with an odd
/// size is entirely ordinary. The far row and column are dropped rather than
/// scaled, which is what the ffmpeg backend does too.
pub fn even_frame(frame: &VideoFrame) -> VideoFrame {
    let width = frame.width & !1;
    let height = frame.height & !1;
    if width == frame.width && height == frame.height {
        return frame.clone();
    }
    let bytes_per_pixel = match frame.format {
        PixelFormat::Bgra => 4usize,
        PixelFormat::Nv12 => 1usize,
    };
    let stride = frame.width as usize * bytes_per_pixel;
    let kept = width as usize * bytes_per_pixel;
    let mut out = Vec::with_capacity(kept * height as usize);
    for row in 0..height as usize {
        let at = row * stride;
        if at + kept <= frame.data.len() {
            out.extend_from_slice(&frame.data[at..at + kept]);
        }
    }
    if matches!(frame.format, PixelFormat::Nv12) {
        // The chroma plane of an NV12 frame is a second plane at half height,
        // so it is trimmed on its own: cutting the luma rows is not enough to
        // make the plane line up.
        let chroma_at = stride * frame.height as usize;
        for row in 0..(height as usize / 2) {
            let at = chroma_at + row * stride;
            if at + kept <= frame.data.len() {
                out.extend_from_slice(&frame.data[at..at + kept]);
            }
        }
    }
    VideoFrame {
        width,
        height,
        format: frame.format,
        data: out,
        time: frame.time,
    }
}

/// How long a PCM chunk lasts, in 100 ns units.
fn duration_of(bytes: usize, bytes_per_second: u32) -> i64 {
    if bytes_per_second == 0 {
        return 0;
    }
    (bytes as i64 * 10_000_000) / i64::from(bytes_per_second)
}

/// Fill `out` with the bytes of `frame`, bottom row first when `flip`.
///
/// An RGB32 media type is bottom-up unless it says otherwise, and a captured
/// frame is top-down, so without the flip a take plays upside down.
fn copy_frame(out: &mut Vec<u8>, frame: &VideoFrame, flip: bool) {
    out.clear();
    if !flip {
        out.extend_from_slice(&frame.data);
        return;
    }
    let stride = frame.width as usize * 4;
    if stride == 0 {
        return;
    }
    let rows = frame.data.len() / stride;
    out.reserve(frame.data.len());
    for row in (0..rows).rev() {
        let at = row * stride;
        out.extend_from_slice(&frame.data[at..at + stride]);
    }
}

fn sink(path: &Path) -> Result<IMFSinkWriter> {
    ensure_media_foundation()?;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` outlives the call, which copies the path, and the writer
    // it returns is reference-counted.
    unsafe { MFCreateSinkWriterFromURL(PCWSTR(wide.as_ptr()), None, None) }.map_err(Into::into)
}

fn h264_type(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<IMFMediaType> {
    let media_type = video_type(MFVideoFormat_H264, width, height, fps)?;
    // SAFETY: one scalar attribute write on an owned object.
    unsafe { media_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps * 1000)? };
    Ok(media_type)
}

fn input_type(format: PixelFormat, width: u32, height: u32, fps: u32) -> Result<IMFMediaType> {
    let subtype = match format {
        PixelFormat::Bgra => MFVideoFormat_RGB32,
        PixelFormat::Nv12 => MFVideoFormat_NV12,
    };
    video_type(subtype, width, height, fps)
}

fn aac_type(sample_rate: u32, channels: u32, bitrate_kbps: u32) -> Result<IMFMediaType> {
    // SAFETY: scalar attribute writes on an owned object.
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
        media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bitrate_kbps * 1000 / 8)?;
        Ok(media_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bottom-up RGB32 media type is the default, so a top-down frame has to
    /// come out with its rows reversed — and a frame only ever has to be
    /// flipped once, however many times it is written.
    #[test]
    fn a_bgra_frame_is_flipped_on_its_way_in() {
        let frame = VideoFrame {
            width: 2,
            height: 2,
            format: PixelFormat::Bgra,
            data: vec![1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4],
            time: 0,
        };
        let mut out = Vec::new();
        copy_frame(&mut out, &frame, true);
        assert_eq!(out[0], 3, "the bottom row goes first");
        assert_eq!(out[15], 2);

        let mut again = Vec::new();
        copy_frame(&mut again, &frame, false);
        assert_eq!(again, frame.data, "an NV12 frame is left alone");
    }

    /// NV12 with odd dimensions is trimmed plane-wise, so the chroma plane
    /// still lines up with the luma plane afterwards.
    #[test]
    fn an_odd_nv12_frame_is_trimmed_plane_wise() {
        let frame = VideoFrame {
            width: 5,
            height: 3,
            format: PixelFormat::Nv12,
            // 5x3 luma + a 5x1 chroma plane at half height.
            data: vec![
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, // luma
                20, 21, 22, 23, 24, // chroma
            ],
            time: 0,
        };
        let trimmed = even_frame(&frame);
        assert_eq!((trimmed.width, trimmed.height), (4, 2));
        assert_eq!(&trimmed.data[..8], &[1, 2, 3, 4, 6, 7, 8, 9]);
        assert_eq!(trimmed.data.len(), 12, "eight luma bytes plus four chroma");
        assert_eq!(&trimmed.data[8..], &[20, 21, 22, 23]);
    }

    /// 48 kHz stereo 16-bit is 192,000 bytes a second, so a 1920-byte chunk is
    /// ten milliseconds — the number the container needs to keep audio in step
    /// with the wall clock.
    #[test]
    fn a_pcm_chunk_duration_follows_from_its_size() {
        assert_eq!(duration_of(1920, 192_000), 100_000);
        assert_eq!(duration_of(0, 192_000), 0);
        assert_eq!(
            duration_of(100, 0),
            0,
            "an unknown rate must not divide by zero"
        );
    }
}
