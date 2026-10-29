//! Writing a take through Media Foundation's sink writer.
//!
//! The alternative to handing frames to an ffmpeg child is to hand them to the
//! OS, and that is what makes the capture timestamps worth having: a sample
//! carries when it was captured, and two sources written by the same process
//! share one clock instead of each child guessing from when it happened to
//! start.
//!
//! Media Foundation inserts the encoder itself: the sink is told it is writing
//! H.264 into an MP4 and is given uncompressed frames, and it loads the
//! encoder transform in between.
//!
//! One measured property of that pipeline governs the rest of this module: the
//! encoder runs at a fixed frame rate and expands every sample's duration into
//! that many repeated frames. Sample times never reach the container as-is
//! (the sink rebases the track to its first sample), but *placement* does: a
//! sample written for [1.0s, 1.5s) comes out as frames 10..14 at 10 fps. That
//! is what lets several files line up frame for frame — the shared origin is
//! carried by a lead-in sample (black video, silent audio) filling [0, first
//! frame), and everything after it lands where the take's clock put it.
//!
//! In other words this path cannot produce true variable-frame-rate files, and
//! does not need to: a held frame is worth the same as a repeated one, and the
//! repeats are what makes the files alignable.

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
use crate::error::{MediaError, Result};
use crate::frame::{AudioChunk, MediaFrame, PixelFormat, VideoFrame};
use crate::mediatype::{pcm_type, video_type};

/// How far a sound chunk's timestamp may disagree with the byte-derived
/// timeline before it stops being jitter and becomes an event, in 100 ns units.
///
/// 40 ms is about one device buffer: long enough that ordinary scheduling
/// absorbs into the chained writes, short enough that a real stall or a
/// re-delivered buffer is caught while it is still worth correcting. See
/// `AudioWriter::write`.
const SKEW_TOLERANCE: i64 = 400_000;

/// Where a chunk goes, given the device's timestamp and where the file's
/// timeline currently ends.
///
/// Pulled out of `AudioWriter::write` so the three cases can be tested without
/// a Media Foundation sink — an untestable branch here is a branch that stays
/// wrong, and this one stays wrong *silently*.
#[derive(Debug, PartialEq, Eq)]
enum Placement {
    /// Write it where the last chunk ended, absorbing the difference.
    Chain,
    /// Write it at its own timestamp, with silence filling the hole first.
    AfterHole,
    /// Drop it: the device re-sent what it had already sent.
    Skip,
}

/// `skew_base` is state: the first measurement becomes the baseline, and the
/// answer is judged against the *change* since it. Without that, a device whose
/// clock sits 200 ms off would look like a 200 ms hole on every chunk.
fn place(arrival: i64, next: i64, skew_base: &mut Option<i64>) -> Placement {
    let skew = arrival - next;
    let base = *skew_base.get_or_insert(skew);
    let relative = skew - base;
    if relative < -SKEW_TOLERANCE {
        Placement::Skip
    } else if relative > SKEW_TOLERANCE {
        Placement::AfterHole
    } else {
        Placement::Chain
    }
}

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
    /// The duration it is finally given is the gap to the next frame — the
    /// truth — which the encoder then expands into repeated CFR frames.
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
        let writer = sink(path)?;
        let stream = add_video_stream(&writer, format, width, height, fps, bitrate_kbps)?;
        // SAFETY: the sink is owned here and every stream it will carry has
        // been set up; after this it accepts samples.
        unsafe { writer.BeginWriting() }.map_err(MediaError::from)?;
        Ok(Self::attach(&writer, stream, format))
    }

    /// Wrap a picture stream that already exists on a sink someone else owns.
    ///
    /// That is the window-with-its-own-sound case: one file carries a picture
    /// and a sound, and Media Foundation has to be told about both streams
    /// before it starts writing — so `MuxSink` adds them and the writers here
    /// only stamp samples into them.
    pub fn attach(writer: &IMFSinkWriter, stream: u32, format: PixelFormat) -> Self {
        Self {
            writer: writer.clone(),
            stream,
            flip: matches!(format, PixelFormat::Bgra),
            pending: None,
            spare: Vec::new(),
            last_duration: 0,
        }
    }

    /// Hand over one frame.
    ///
    /// A screen that does not change produces no frames at all, so the picture
    /// has to be held for as long as the pause really was. That gap is only
    /// knowable once the next frame arrives, which is why a frame is kept back
    /// rather than written straight away.
    pub fn write(&mut self, frame: &VideoFrame) -> Result<()> {
        self.write_timed(frame, frame.time)
    }

    /// Write the same picture at a chosen time on the take's timeline. The
    /// keep-latest sampler stamps one buffer into several fps slots this way
    /// instead of cloning the pixels just to change the timestamp.
    pub fn write_at(&mut self, frame: &VideoFrame, time: i64) -> Result<()> {
        self.write_timed(frame, time)
    }

    fn write_timed(&mut self, frame: &VideoFrame, time: i64) -> Result<()> {
        let mut bytes = std::mem::take(&mut self.spare);
        copy_frame(&mut bytes, frame, self.flip);

        // The first frame arrives when its capture woke up, not when the take
        // started — a camera opening 300ms late still has to land 300ms into
        // the take for the files to line up. The MP4 sink rebases every track
        // to its first sample, so the gap is filled with a black sample at
        // time 0 and the real frames keep their offsets behind it.
        if self.pending.is_none() && time > 0 {
            let lead = vec![0u8; bytes.len()];
            // SAFETY: owned zeros, as below.
            unsafe {
                write_sample(&self.writer, self.stream, &lead, 0, time)?;
            }
        }

        if let Some(previous) = self.pending.take() {
            let duration = (time - previous.time).max(0);
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
        self.pending = Some(Pending { time, bytes });
        Ok(())
    }

    /// Close the file. A take without this is not playable.
    ///
    /// The frame still held back is written with the same duration as the gap
    /// before it, which is a guess: nothing ever arrived to say how long it
    /// lasted. A caller that knows when the take was stopped should use
    /// `finish_at` instead — with a screen that never changed, the guess is
    /// zero and the file would be zero seconds long.
    pub fn finish(self) -> Result<()> {
        self.finish_at(None)
    }

    /// Close the file, holding the last frame until `end`.
    ///
    /// `end` is when the capture was told to stop, in the same 100 ns units as
    /// a frame's time. It is what makes a still picture a five-second file
    /// rather than a zero-second one: under variable frame rate a screen that
    /// did not change produced one frame, and only the stop time says how long
    /// that frame was on screen.
    pub fn finish_at(mut self, end: Option<i64>) -> Result<()> {
        self.flush(end)?;
        // SAFETY: Finalize flushes and closes; the writer is consumed so it
        // cannot be written to afterwards.
        unsafe { self.writer.Finalize() }.map_err(Into::into)
    }

    /// Write the frame still held back, ending it at `end`.
    ///
    /// Leaves the file open: a sink shared with a sound stream is finalised by
    /// whoever owns it, not by either stream's writer.
    pub fn flush(&mut self, end: Option<i64>) -> Result<()> {
        if let Some(last) = self.pending.take() {
            let duration = match end {
                // The frame ends before the cut: hold it until there.
                Some(end) if end > last.time => end - last.time,
                // The frame is stamped *past* the cut, so it belongs to the
                // pause and not to this file. Writing it — or giving it its
                // usual slot, which is what the `last_duration` fallback did —
                // runs the file past the length the manifest records, and that
                // is the second face of the 2026-10-27 `len.mismatch`: a 1.8 s
                // segment measured 2.07 s because the frame after the freeze
                // was held for another fifth of a second.
                Some(_) => return Ok(()),
                None => self.last_duration,
            };
            // SAFETY: owned buffer, as above.
            unsafe {
                write_sample(
                    &self.writer,
                    self.stream,
                    &last.bytes,
                    last.time,
                    duration.max(0),
                )?;
            }
        }
        Ok(())
    }
}

/// An audio file being written, one PCM chunk at a time.
pub struct AudioWriter {
    writer: IMFSinkWriter,
    stream: u32,
    bytes_per_second: u32,
    /// One frame: every channel's sample, side by side. Written silence is
    /// rounded to this, because a partial frame shifts the whole stream that
    /// follows (`write_silence`).
    bytes_per_frame: u32,
    /// Where the next chunk starts.
    next: Option<i64>,
    /// Whether any chunk has been written yet, so the lead-in silence the
    /// video writer's black sample plays against is written exactly once.
    started: bool,
    /// The take-time this file treats as zero. `0` for a source that was there
    /// from the start; a source added mid-take sets its own first sample, so
    /// its file opens where its sound begins (see `Origin`).
    origin: i64,
    /// The standing difference between the device's clock and this file's
    /// byte-derived timeline, learned from the first chunk that can measure it.
    ///
    /// WASAPI timestamps and a byte count are not guaranteed to agree in
    /// absolute terms; a device whose clock runs 100 ms ahead would otherwise
    /// read as a 100 ms hole on every single chunk, and the file would grow
    /// silence forever. What is worth acting on is the *change*, so the first
    /// measurement becomes the baseline and everything after is judged against
    /// it. See `write`.
    skew_base: Option<i64>,
}

impl AudioWriter {
    pub fn create(path: &Path, sample_rate: u32, channels: u32, bitrate_kbps: u32) -> Result<Self> {
        let writer = sink(path).map_err(|e| e.context("AudioWriter::sink"))?;
        let stream = add_audio_stream(&writer, sample_rate, channels, bitrate_kbps)?;
        // SAFETY: the sink is owned here and its one stream is set up.
        unsafe { writer.BeginWriting() }
            .map_err(|e| MediaError::from(e).context("AudioWriter::BeginWriting"))?;
        Ok(Self::attach(&writer, stream, sample_rate, channels))
    }

    /// Wrap a sound stream that already exists on a sink someone else owns —
    /// the window-with-its-own-sound case, exactly as `VideoWriter::attach`.
    pub fn attach(writer: &IMFSinkWriter, stream: u32, sample_rate: u32, channels: u32) -> Self {
        Self {
            writer: writer.clone(),
            stream,
            bytes_per_second: sample_rate * channels * 2,
            bytes_per_frame: channels * 2,
            next: None,
            started: false,
            origin: 0,
            skew_base: None,
        }
    }

    /// Start this file's timeline at `origin` instead of at zero.
    ///
    /// Called before the first chunk is written. A source added mid-take sets
    /// its own first sample here, which is what keeps its file from opening
    /// with minutes of silence: the manifest's `start` records where it belongs
    /// on the take's timeline instead.
    pub fn set_origin(&mut self, origin: i64) {
        self.origin = origin;
    }

    /// How far the file's timeline reaches, in 100 ns units from the take's
    /// origin — readable before finalising, which is what a shared sink needs.
    pub fn end(&self) -> i64 {
        self.next.unwrap_or(0)
    }

    /// Write one chunk.
    ///
    /// Three things can be true of the chunk's own timestamp, and they need
    /// different answers (2026-10-27, `docs/api-recording.md` §11.2):
    ///
    /// * It agrees with where the last chunk ended → write it there. The
    ///   difference is absorbed, so ordinary jitter never reaches the file.
    /// * It is *later* than that by more than a buffer → the device really did
    ///   stall. Silence fills the hole, so the sound after it keeps the
    ///   picture's timeline.
    /// * It is *earlier* → the device handed over a buffer it had already
    ///   handed over. WASAPI does this after a device change or a long stall,
    ///   and writing it pushes everything after it later by exactly the repeat.
    ///   Nothing later wins that time back, so the rest of the file drifts out
    ///   of sync with its picture — permanently and silently. Dropping the copy
    ///   is the cheaper loss: the same audio twice, at worst.
    ///
    /// Before this, the timestamp was ignored past the first chunk and every
    /// chunk was chained by byte count, which made the third case a one-way
    /// error no later measurement could see.
    pub fn write(&mut self, chunk: &AudioChunk) -> Result<()> {
        if chunk.data.is_empty() {
            return Ok(());
        }
        let duration = duration_of(chunk.data.len(), self.bytes_per_second);
        // Measured from this file's own origin. A chunk that arrived before
        // that origin (sound can arrive before the picture it belongs to) is
        // pinned to zero rather than written at a negative time, which MP4
        // cannot express.
        let arrival = (chunk.time - self.origin).max(0);
        let mut start = match self.next {
            None => arrival,
            Some(next) => match place(arrival, next, &mut self.skew_base) {
                Placement::Skip => return Ok(()),
                Placement::AfterHole => {
                    let filled = self.write_silence(next, arrival)?;
                    if filled > next { filled } else { arrival }
                }
                Placement::Chain => next,
            },
        };
        if !self.started && start > 0 {
            // The microphone woke up after the take did: fill the gap with
            // silence so the file's timeline keeps the shared origin, exactly
            // like the video writer's black lead-in.
            let filled = self.write_silence(0, start)?;
            if filled > 0 {
                start = filled;
            }
        }
        self.started = true;
        // SAFETY: the chunk is borrowed for the duration of the call, which
        // copies it into the sample's own buffer.
        unsafe {
            write_sample(&self.writer, self.stream, &chunk.data, start, duration)
                .map_err(|e| e.context("AudioWriter::write"))?;
        }
        self.next = Some(start + duration);
        Ok(())
    }

    /// Zeroes on this stream, from `from` up to `to`.
    ///
    /// One shape for two callers: the lead-in (0 → the first sample) and a hole
    /// in the middle (0 → the first sample is the same operation, and so is
    /// next → arrival after a stall).
    ///
    /// The byte count is rounded to a whole number of frames, and that is not
    /// cosmetic (2026-10-29). PCM handed to the sink is one continuous stream:
    /// a single frame's worth of stray bytes shifts every byte after it by one
    /// or two, so each 16-bit sample ends up assembled from half of itself and
    /// half of its neighbour — byte-swapped, channel-swapped, and loud. A take
    /// whose sound arrived 17 ms after its picture was inserted as 3295 bytes,
    /// and all 6 runs that did so wrote a track that measured `-5.2 dB` of
    /// broadband noise at `diff/abs 1.63`; the runs whose silence happened to
    /// land on a frame boundary were clean. The take's own audio was never at
    /// fault — the padding in front of it was.
    ///
    /// Returns where the silence actually ends, so the caller can put the next
    /// chunk against it instead of against the time it asked for.
    fn write_silence(&self, from: i64, to: i64) -> Result<i64> {
        if to <= from {
            return Ok(from);
        }
        let bytes = silence_bytes(to - from, self.bytes_per_second, self.bytes_per_frame);
        if bytes == 0 {
            return Ok(from);
        }
        let duration = duration_of(bytes, self.bytes_per_second);
        let silence = vec![0u8; bytes];
        // SAFETY: owned zeros; the call copies them out.
        unsafe { write_sample(&self.writer, self.stream, &silence, from, duration)? };
        Ok(from + duration)
    }

    /// Close the file and report how far its timeline reached, in 100 ns units
    /// from the take's origin.
    ///
    /// That is the position after the last sample written, lead-in silence
    /// included — which the sink cannot say for itself, because it knows the
    /// samples and not the take. Counting the bytes that arrived instead
    /// leaves the silence out, and a microphone that woke up half a second
    /// late then looks half a second short: exactly the error a drift check
    /// must not have.
    pub fn finish(self) -> Result<i64> {
        let end = self.end();
        // SAFETY: as `VideoWriter::finish`.
        unsafe { self.writer.Finalize() }
            .map_err(|e| MediaError::from(e).context("AudioWriter::finish"))?;
        Ok(end)
    }
}

/// One sink writer carrying a picture and its sound.
///
/// A window can be recorded together with the sound its own process tree
/// makes, and the two belong in one file. Media Foundation takes a stream only
/// before the sink starts writing, so both streams are added here and `begin`
/// comes after — which is why the picture's size has to be known first, and
/// why the sound that arrives before the first frame waits in its channel
/// instead of being written.
pub struct MuxSink {
    writer: IMFSinkWriter,
}

impl MuxSink {
    pub fn create(path: &Path) -> Result<Self> {
        Ok(Self {
            writer: sink(path)?,
        })
    }

    /// Add the picture stream. The first frame is what says how big the file
    /// is, so this is called once one has arrived.
    pub fn add_video(
        &self,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<VideoWriter> {
        let stream = add_video_stream(&self.writer, format, width, height, fps, bitrate_kbps)?;
        Ok(VideoWriter::attach(&self.writer, stream, format))
    }

    /// Add the sound stream.
    pub fn add_audio(
        &self,
        sample_rate: u32,
        channels: u32,
        bitrate_kbps: u32,
    ) -> Result<AudioWriter> {
        let stream = add_audio_stream(&self.writer, sample_rate, channels, bitrate_kbps)?;
        Ok(AudioWriter::attach(
            &self.writer,
            stream,
            sample_rate,
            channels,
        ))
    }

    pub fn begin(&self) -> Result<()> {
        // SAFETY: every stream this file will carry was added through the two
        // calls above, which is what the sink insists on.
        unsafe { self.writer.BeginWriting() }.map_err(MediaError::from)
    }

    /// Close the file. Both writers must have flushed their last sample first.
    pub fn finalize(&self) -> Result<()> {
        // SAFETY: flushes and closes; the sink is still owned here, so nothing
        // can be written through it afterwards.
        unsafe { self.writer.Finalize() }.map_err(MediaError::from)
    }
}

/// Tell a sink about one picture stream, and what will be handed to it.
fn add_video_stream(
    writer: &IMFSinkWriter,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<u32> {
    let output = h264_type(width, height, fps, bitrate_kbps)?;
    let input = input_type(format, width, height, fps)?;
    // SAFETY: both types are owned here, and the stream index that outlives
    // the call is a plain integer.
    unsafe {
        let stream = writer.AddStream(&output)?;
        writer.SetInputMediaType(stream, &input, None)?;
        Ok(stream)
    }
}

/// The same for one sound stream: AAC out, the PCM every capture produces in.
fn add_audio_stream(
    writer: &IMFSinkWriter,
    sample_rate: u32,
    channels: u32,
    bitrate_kbps: u32,
) -> Result<u32> {
    let output = aac_type(sample_rate, channels, bitrate_kbps)?;
    let input = pcm_type(sample_rate, channels)?;
    // SAFETY: as `add_video_stream`.
    unsafe {
        let stream = writer
            .AddStream(&output)
            .map_err(|e| MediaError::from(e).context("AudioWriter::AddStream"))?;
        writer
            .SetInputMediaType(stream, &input, None)
            .map_err(|e| MediaError::from(e).context("AudioWriter::SetInputMediaType"))?;
        Ok(stream)
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

/// Which rectangle of a frame to keep, in source pixels.
///
/// Four edges rather than an origin and a size: that is how a user drags a
/// crop, and it is the same shape `takekit_core::source::Crop` serialises, so
/// the panel's numbers travel here unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CropBox {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

impl CropBox {
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }

    /// Where the kept rectangle sits inside a `width x height` frame, or
    /// `None` when nothing is cropped.
    ///
    /// A crop that no longer fits — the window was resized since the edges
    /// were picked — is dropped instead of clamped to a sliver: recording the
    /// whole picture is the useful answer, and a two-pixel file is not.
    pub fn rect_in(&self, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
        if self.is_zero() {
            return None;
        }
        let kept_w = width.checked_sub(self.left.checked_add(self.right)?)?;
        let kept_h = height.checked_sub(self.top.checked_add(self.bottom)?)?;
        if kept_w < 2 || kept_h < 2 {
            return None;
        }
        Some((self.left, self.top, kept_w, kept_h))
    }
}

/// Keep one rectangle of a frame, in the frame's own pixels.
///
/// Takes the frame by value and hands the same buffer straight back when
/// there is nothing to cut, so the common case allocates nothing. A cropped
/// frame is rebuilt row by row — no scaling, no resampling: a crop has to move
/// pixel boundaries and nothing else.
pub fn crop_frame(frame: VideoFrame, crop: CropBox) -> VideoFrame {
    let Some((x, y, kept_w, kept_h)) = crop.rect_in(frame.width, frame.height) else {
        return frame;
    };
    let (data, width, height) = match frame.format {
        PixelFormat::Bgra => {
            let stride = frame.width as usize * 4;
            let row_bytes = kept_w as usize * 4;
            let mut out = Vec::with_capacity(row_bytes * kept_h as usize);
            for row in y..y + kept_h {
                let at = row as usize * stride + x as usize * 4;
                match frame.data.get(at..at + row_bytes) {
                    Some(slice) => out.extend_from_slice(slice),
                    None => return frame, // torn buffer: the whole frame beats half of one
                }
            }
            (out, kept_w, kept_h)
        }
        PixelFormat::Nv12 => {
            // Chroma is stored at half resolution in both directions, so every
            // edge has to land on an even pixel or the two planes disagree
            // about which block they are describing.
            let (x, y) = (x & !1, y & !1);
            let (w, h) = (kept_w & !1, kept_h & !1);
            if w == 0 || h == 0 {
                return frame;
            }
            let stride = frame.width as usize;
            let row_bytes = w as usize;
            let luma_at = y as usize * stride;
            let chroma_at = stride * frame.height as usize;
            let mut out = Vec::with_capacity(row_bytes * (h as usize + h as usize / 2));
            for row in 0..h {
                let at = luma_at + row as usize * stride + x as usize;
                match frame.data.get(at..at + row_bytes) {
                    Some(slice) => out.extend_from_slice(slice),
                    None => return frame,
                }
            }
            for row in 0..h / 2 {
                let at = chroma_at + (y as usize / 2 + row as usize) * stride + x as usize;
                match frame.data.get(at..at + row_bytes) {
                    Some(slice) => out.extend_from_slice(slice),
                    None => return frame,
                }
            }
            (out, w, h)
        }
    };
    VideoFrame {
        width,
        height,
        format: frame.format,
        data,
        time: frame.time,
    }
}

/// Trim a frame to even dimensions.
///
/// H.264 stores chroma at half resolution, so a frame with an odd width or
/// height cannot be encoded at all — and a monitor or a window with an odd
/// size is entirely ordinary. The far row and column are dropped rather than
/// scaled, which is what the ffmpeg backend does too.
///
/// Takes the frame by value: on the common path (already even) it hands the
/// same buffer back, where a `&VideoFrame` signature meant cloning the whole
/// pixel buffer every frame — ~8 MB a frame at 1080p BGRA, just to feed the
/// encoder.
pub fn even_frame(frame: VideoFrame) -> VideoFrame {
    let width = frame.width & !1;
    let height = frame.height & !1;
    if width == frame.width && height == frame.height {
        return frame;
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
        // Same rule as `crop_frame`: a torn buffer gets the whole frame back,
        // never a rebuild out of the rows that happened to fit — that one
        // would carry a height its pixels no longer describe.
        match frame.data.get(at..at + kept) {
            Some(row_bytes) => out.extend_from_slice(row_bytes),
            None => return frame,
        }
    }
    if matches!(frame.format, PixelFormat::Nv12) {
        // The chroma plane of an NV12 frame is a second plane at half height,
        // so it is trimmed on its own: cutting the luma rows is not enough to
        // make the plane line up.
        let chroma_at = stride * frame.height as usize;
        for row in 0..(height as usize / 2) {
            let at = chroma_at + row * stride;
            match frame.data.get(at..at + kept) {
                Some(row_bytes) => out.extend_from_slice(row_bytes),
                None => return frame,
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

/// How many bytes of silence cover `span` (100 ns units) without cutting a
/// frame in half.
///
/// Rounded to the nearest whole frame rather than truncated: PCM handed to the
/// sink is one stream, so half a frame of padding re-assembles every sample
/// after it out of two neighbours. `write_silence` has the story.
fn silence_bytes(span: i64, bytes_per_second: u32, bytes_per_frame: u32) -> usize {
    let align = u128::from(bytes_per_frame);
    if align == 0 || span <= 0 || bytes_per_second == 0 {
        return 0;
    }
    let wanted = span as u128 * u128::from(bytes_per_second) / 10_000_000;
    (((wanted + align / 2) / align) * align) as usize
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

/// How one source's frames reach the encoder.
#[derive(Clone, Copy, Debug)]
pub enum Sampling {
    /// Every frame as it arrives, with the gaps it really had (the take's own
    /// timeline). The encoder expands a held frame across a pause either way;
    /// this just never skips a fresh picture.
    EveryFrame,
    /// Fixed rate: each 1/fps slot on the clock gets the latest picture and
    /// older ones are dropped rather than queued — same frames out, different
    /// sampling in (`takekit media --cfr`).
    KeepLatest,
}

/// What one source's writer saw. `frames` is what the capture actually
/// delivered — the output has `duration x fps` slots either way, because the
/// encoder holds a frame across a pause. The difference between the two is how
/// much of the take is a held picture rather than a fresh one, which is the
/// only visible form a dropped capture update can take.
pub struct StreamReport {
    /// How long the file is, in seconds.
    pub seconds: f64,
    pub frames: u64,
    /// The size the file was written at (crop applied, even-aligned) — the
    /// first frame decides it, since that is when the writer opens.
    /// `0 x 0` when no frame ever arrived.
    pub width: u32,
    pub height: u32,
    /// Take-time of the sample the file begins with, in 100 ns units. `None`
    /// when nothing was ever written. This is the number a manifest's `start`
    /// is made of: it says where the file belongs on the take's timeline, and
    /// `seconds` says how much of it the file covers.
    pub start: Option<i64>,
    /// One entry per file, in order. A source that was never paused has exactly
    /// one; every pause starts the next, so the files really do begin and end
    /// where the user did.
    pub segments: Vec<SegmentReport>,
}

/// One file of one source.
#[derive(Clone, Debug)]
pub struct SegmentReport {
    pub file: std::path::PathBuf,
    /// Take-time of its first sample, in 100 ns units.
    pub start: i64,
    /// How much of the take it covers — the same span in seconds, measured from
    /// that first sample rather than from the file's own zero.
    pub seconds: f64,
    /// Frames written into this file.
    pub frames: u64,
}

/// Where a file's own timeline begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Zero is the take's origin. A source whose device wakes up late is
    /// padded with black to keep its content where the take clock put it, so
    /// the file really does start with the take (see `VideoWriter::write_timed`).
    Take,
    /// Zero is this file's own first sample.
    ///
    /// For a source that joined a take already running: padding it back to the
    /// take's origin would mean encoding every second it missed as black
    /// frames — a minute into a take that is eighteen hundred frames of encode
    /// before the first real one, and the live frames arriving meanwhile queue
    /// behind it. The manifest's `start` says where the file belongs instead.
    FirstSample,
}

/// What one file is supposed to look like: the four numbers that used to be
/// four more positional arguments to `write_stream`.
#[derive(Clone, Debug)]
pub struct Encode {
    pub fps: u32,
    pub bitrate: u32,
    /// The rectangle to keep, in the frames' own pixels.
    pub crop: CropBox,
    pub sampling: Sampling,
    pub origin: Origin,
    /// Told the take-time of the file's first sample the moment it is known.
    ///
    /// A source added mid-take has to write that number into the manifest
    /// straight away — waiting for the file to close would mean a take on disk
    /// that cannot say where its newest file belongs. `try_send` on the writer
    /// side, so a caller that has already given up never stalls the encoder.
    pub first: Option<std::sync::mpsc::SyncSender<i64>>,
}

/// Latch a file's origin against the first sample seen, and report it once.
fn latch_origin(
    base: &mut Option<i64>,
    reported: &mut bool,
    first: Option<&std::sync::mpsc::SyncSender<i64>>,
    t: i64,
) -> i64 {
    let origin = *base.get_or_insert(t);
    if !*reported {
        *reported = true;
        if let Some(tx) = first {
            // Never blocks: the receiver may have timed out and gone away.
            let _ = tx.try_send(origin);
        }
    }
    origin
}

/// `win-part{n}.mp4` — a part's number, suffix and all.
///
/// The suffix spells `part` out (2026-10-27) because a bare number was already
/// taken: `-2` is how a *second source of the same app* is named (`chrome`,
/// `chrome-2`), so one mic paused once and two mics never paused wrote the same
/// two file names. Two different takes, two different meanings, one folder
/// shape — which is the kind of collision that only shows up when someone tries
/// to line the parts up again.
fn numbered_path(base: &std::path::Path, n: usize) -> std::path::PathBuf {
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "take".into());
    match base.extension().map(|e| e.to_string_lossy().into_owned()) {
        Some(ext) if !ext.is_empty() => base.with_file_name(format!("{stem}-part{n}.{ext}")),
        _ => base.with_file_name(format!("{stem}-part{n}")),
    }
}

/// The path a writer opens for segment `n` while the take is still running.
///
/// A separate file per pause is the only way to cut a video exactly. MP4 can
/// only be split between keyframes, and nothing here sets a GOP — the encoder's
/// default is seconds long — so a split after the fact would land wherever the
/// last IDR happened to be. Starting a new file puts the boundary at frame
/// zero, where it is exact by construction.
///
/// The first one is plain `base` because nobody knows yet whether a second will
/// follow — numbering it up front would mean renaming back on every take that
/// was never paused. [`number_all_parts`] settles the names once the count is
/// known.
pub fn segment_path(base: &std::path::Path, n: usize) -> std::path::PathBuf {
    if n <= 1 {
        base.to_path_buf()
    } else {
        numbered_path(base, n)
    }
}

/// Number every part once a source turns out to have more than one.
///
/// A source with one file keeps its plain name, because there `win.mp4` really
/// is the whole thing. The moment a second file exists that reading breaks:
/// `win.mp4` sitting next to `win-part2.mp4` looks like the first one holds
/// everything and the rest are extras — which is exactly backwards, and it is
/// what the user read off the folder (2026-10-27). So the whole set gets
/// numbered together, or none of them does.
///
/// `base` is passed in rather than read off `files[0]`: a second call is then a
/// no-op instead of renaming `win-part1.mp4` to `win-part1-part1.mp4`. Entries
/// are only changed when their rename actually succeeded, so a caller that
/// writes the list back cannot point at a file that is not there.
pub fn number_all_parts(
    base: &std::path::Path,
    files: &mut [std::path::PathBuf],
) -> std::io::Result<()> {
    if files.len() < 2 {
        return Ok(());
    }
    for (i, file) in files.iter_mut().enumerate() {
        let want = numbered_path(base, i + 1);
        if *file == want {
            continue;
        }
        std::fs::rename(&*file, &want)?;
        *file = want;
    }
    Ok(())
}

/// Apply [`number_all_parts`] to a report's segments.
///
/// A rename that fails leaves the original name in place: the files are
/// complete either way and the manifest stays truthful about what is on disk
/// (it just says `win.mp4` + `win-part2.mp4`, the pre-2026-10-27 shape). Losing
/// the take over a naming nicety would be the worse trade.
fn name_parts(base: &std::path::Path, segments: &mut [SegmentReport]) {
    let mut files: Vec<std::path::PathBuf> = segments.iter().map(|s| s.file.clone()).collect();
    if let Err(e) = number_all_parts(base, &mut files) {
        eprintln!(
            "takekit: could not number the parts of {}: {e}",
            base.display()
        );
    }
    for (seg, file) in segments.iter_mut().zip(files) {
        seg.file = file;
    }
}

/// Close the file a source has open and record what it covered. A no-op when
/// nothing is open, so callers do not have to ask.
///
/// `end` is take-time, not file-time: the file's own length is measured from
/// the sample it starts with.
fn close_segment(
    video: &mut Option<VideoWriter>,
    segments: &mut Vec<SegmentReport>,
    base: Option<i64>,
    frames: u64,
    out: &std::path::Path,
    end: i64,
) -> Result<()> {
    let Some(w) = video.take() else {
        return Ok(());
    };
    let at = base.unwrap_or(end);
    let span = (end - at).max(0);
    w.finish_at(Some(span))?;
    segments.push(SegmentReport {
        file: segment_path(out, segments.len() + 1),
        start: at,
        seconds: span as f64 / 10_000_000.0,
        frames,
    });
    Ok(())
}

/// Drain one source's frame channel into its own MP4. The writer opens on the
/// first frame (that is when the real dimensions are known) and closes with
/// `end_hint` — latched when the take stops — so every channel covers the same
/// stretch of the take.
///
/// This is the one place that turns a frame channel into a file: the session
/// writes takes with it and the CLI's device harness samples with it. A second
/// copy of this loop once cost a fix two call sites (`even_frame`).
pub fn write_stream(
    rx: std::sync::mpsc::Receiver<crate::frame::MediaFrame>,
    out: &std::path::Path,
    encode: Encode,
    clock: crate::clock::TakeClock,
    end_hint: std::sync::Arc<std::sync::atomic::AtomicI64>,
) -> Result<StreamReport> {
    use std::sync::atomic::Ordering;

    let Encode {
        fps,
        bitrate,
        crop,
        sampling,
        origin,
        first,
    } = encode;
    let fps = fps.max(1);
    let interval = std::time::Duration::from_secs_f64(1.0 / fps as f64);
    let mut video: Option<VideoWriter> = None;
    // The output's size: whatever the first kept frame came out as.
    let mut size = (0u32, 0u32);
    let mut frames = 0u64;
    // Frames written into the segment that is open now. Reset at every cut, so
    // each file reports its own share of the delivery.
    let mut seg_frames = 0u64;
    let mut segments: Vec<SegmentReport> = Vec::new();
    let mut latest: Option<VideoFrame> = None;
    // The last picture written, kept so a resume has something to open with.
    // A window that never repaints delivers no frame at all — its first frame
    // came when the capture started, and a pause does not restart the capture —
    // so without this the file after a pause would only appear once the user
    // happened to touch that window.
    let mut held: Option<VideoFrame> = None;
    // The instant the last segment was cut at. The file after a pause starts
    // there, not at "whenever the writer noticed the clock running again" —
    // the loop sleeps through the pause, so those two differ by up to a poll
    // interval and would leave a sliver of timeline belonging to no file.
    let mut cut_at: Option<i64> = None;
    let mut due_slot = 0u64;
    // Where the open file's timeline begins. Zero for a source that was there
    // from the start — but only for its first file: after a cut the file must
    // start at its own first sample, or every resume would pad back to the
    // take's origin with black.
    let mut base = match origin {
        Origin::Take => Some(0),
        Origin::FirstSample => None,
    };
    let mut reported = false;

    loop {
        if clock.is_paused() {
            // The clock was stopped before the cut reached here, so whatever is
            // already queued with an earlier stamp was captured before the
            // pause and belongs in this file. Write it, then close the file
            // exactly at the frozen instant. A frame still in a capture's hand
            // is the only thing this can lose: one frame, at the cut, and the
            // pause was the user's own doing.
            let frozen = clock.now_100ns();
            if let Sampling::EveryFrame = sampling {
                while let Ok(msg) = rx.try_recv() {
                    if let MediaFrame::Video(frame) = msg {
                        // The capture never stops, so a window that keeps
                        // painting keeps delivering — and every one of those
                        // frames is stamped past the freeze. Writing them was
                        // the 2026-10-27 bug: with nothing open the loop opened
                        // the *first* file again (that is the path `out` names),
                        // overwrote the take with one frame, and recorded a
                        // zero-second segment for each pass — sixteen of them in
                        // a 5-second pause, and a `len.mismatch` for the file
                        // that really did lose its content.
                        if frame.time > frozen {
                            continue;
                        }
                        frames += 1;
                        seg_frames += 1;
                        let frame = even_frame(crop_frame(frame, crop));
                        let at = latch_origin(&mut base, &mut reported, first.as_ref(), frame.time);
                        if video.is_none() {
                            size = (frame.width, frame.height);
                            let path = segment_path(out, segments.len() + 1);
                            video = Some(open_for(&frame, &path, fps, bitrate)?);
                        }
                        video
                            .as_mut()
                            .expect("opened above")
                            .write_at(&frame, frame.time - at)?;
                    }
                }
            }
            if video.is_some() {
                cut_at = Some(frozen);
            }
            close_segment(&mut video, &mut segments, base, seg_frames, out, frozen)?;
            seg_frames = 0;
            base = None;
            // Nothing more to write until the clock runs again. The capture
            // keeps running throughout, which is what makes a resume instant
            // instead of a device warming up from cold.
            let _ = rx.recv_timeout(std::time::Duration::from_millis(20));
            continue;
        }

        // Back from a pause with nothing new to show. The next file opens on
        // the picture the last one ended with, at the instant the clock started
        // again — which is what the encoder would have done with it anyway, had
        // the file not been closed in between.
        if video.is_none() {
            if let Some(frame) = held.as_ref() {
                let at = cut_at.take().unwrap_or_else(|| clock.now_100ns());
                base = Some(at);
                size = (frame.width, frame.height);
                // `segment_path`, not `out`: the first file is on disk and must
                // not be truncated by the second one opening over it.
                let path = segment_path(out, segments.len() + 1);
                video = Some(open_for(frame, &path, fps, bitrate)?);
                video.as_mut().expect("opened above").write_at(frame, 0)?;
                seg_frames = 1;
            }
        }

        match sampling {
            Sampling::EveryFrame => match rx.recv_timeout(interval) {
                Ok(MediaFrame::Video(frame)) => {
                    // A pause can land between the check at the top of the loop
                    // and this frame arriving — the writer is parked in this
                    // `recv` when the user presses pause. The frame is then
                    // stamped past the freeze and belongs to the pause, exactly
                    // like the ones the paused branch drops (measured: this is
                    // where the first post-pause frame used to slip in).
                    if clock.is_paused() && frame.time > clock.now_100ns() {
                        continue;
                    }
                    frames += 1;
                    seg_frames += 1;
                    let frame = even_frame(crop_frame(frame, crop));
                    let at = latch_origin(&mut base, &mut reported, first.as_ref(), frame.time);
                    if video.is_none() {
                        size = (frame.width, frame.height);
                        // Same rule as above: never `out`, which may already
                        // hold a closed segment.
                        let path = segment_path(out, segments.len() + 1);
                        video = Some(open_for(&frame, &path, fps, bitrate)?);
                    }
                    video
                        .as_mut()
                        .expect("opened above")
                        .write_at(&frame, frame.time - at)?;
                    held = Some(frame);
                }
                Ok(_) => {}
                // Idle is not the end: a paused clock advances nothing, so the
                // loop has to come back around and look.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            },
            Sampling::KeepLatest => {
                match rx.recv_timeout(interval) {
                    Ok(MediaFrame::Video(frame)) => {
                        // Keep latest means the newest picture, not the newest
                        // that happened to be at the queue's head: after a
                        // stall the queue holds a burst, and stamping the
                        // burst's *oldest* frame into the gap the stall left
                        // behind is exactly backwards.
                        let (frame, seen) = newest_of_burst(&rx, frame);
                        frames += seen;
                        latest = Some(even_frame(crop_frame(frame, crop)));
                    }
                    Ok(_) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                // Advance to "now" one slot at a time; each slot gets the held
                // picture stamped to the slot, not to its capture time.
                let end_slot = (clock.now_100ns() as u64 * fps as u64) / 10_000_000;
                while due_slot < end_slot {
                    if let Some(frame) = latest.as_ref() {
                        let at = latch_origin(&mut base, &mut reported, first.as_ref(), frame.time);
                        if video.is_none() {
                            size = (frame.width, frame.height);
                            video = Some(open_for(frame, out, fps, bitrate)?);
                        }
                        let slot_time = (due_slot as i64 * 10_000_000) / i64::from(fps);
                        video
                            .as_mut()
                            .expect("opened above")
                            .write_at(frame, slot_time - at)?;
                        seg_frames += 1;
                    }
                    due_slot += 1;
                }
            }
        }
    }

    // How long the last frame was on screen is something only the caller
    // knows: nothing arrives after it. Without this a take of a still screen
    // is a zero-second file. The latched end wins so all channels stop
    // together.
    let end = match end_hint.load(Ordering::SeqCst) {
        0 => clock.now_100ns(),
        t => t,
    };
    close_segment(&mut video, &mut segments, base, seg_frames, out, end)?;

    // The file count is finally known, so a source that was cut gets its parts
    // numbered from one instead of leaving the first file looking like the
    // whole recording.
    name_parts(out, &mut segments);

    // The source's own span: its files back to back. Because a pause removes
    // time from the timeline instead of recording it, those files really are
    // contiguous — `start + seconds` of one segment is the next one's `start`,
    // and the sum is the stretch of the take this source covers.
    Ok(StreamReport {
        seconds: segments.iter().map(|s| s.seconds).sum(),
        frames,
        width: size.0,
        height: size.1,
        start: segments.first().map(|s| s.start),
        segments,
    })
}

/// Everything the queue is holding, collapsed to its newest frame.
///
/// `first` is the one the caller just received; whatever piled up behind it is
/// older by definition (the queue is FIFO), so only the last one is kept. The
/// count comes back as "frames the capture delivered" for the report — the
/// intermediate ones were delivered too, just never written.
fn newest_of_burst(
    rx: &std::sync::mpsc::Receiver<MediaFrame>,
    first: VideoFrame,
) -> (VideoFrame, u64) {
    let mut frame = first;
    let mut seen = 1;
    while let Ok(next) = rx.try_recv() {
        if let MediaFrame::Video(next) = next {
            frame = next;
            seen += 1;
        }
    }
    (frame, seen)
}

fn open_for(
    frame: &VideoFrame,
    out: &std::path::Path,
    fps: u32,
    bitrate: u32,
) -> Result<VideoWriter> {
    match frame.format {
        PixelFormat::Nv12 => VideoWriter::create_nv12(out, frame.width, frame.height, fps, bitrate),
        PixelFormat::Bgra => VideoWriter::create(out, frame.width, frame.height, fps, bitrate),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three answers a chunk's timestamp can deserve, and the reason the
    /// baseline exists.
    ///
    /// Units are 100 ns throughout; 100 ms is `1_000_000` and the tolerance is
    /// 40 ms, so the numbers below are deliberately far from the line.
    #[test]
    fn a_redelivered_chunk_is_dropped_and_a_real_hole_is_filled() {
        const MS: i64 = 10_000;

        // Chained: the timestamp agrees with where the last chunk ended.
        let mut base = None;
        assert_eq!(place(1_000 * MS, 1_000 * MS, &mut base), Placement::Chain);
        assert_eq!(place(1_005 * MS, 1_004 * MS, &mut base), Placement::Chain);

        // A redelivery: the device hands over audio it already handed over.
        // Writing it would push everything after it later by the repeat, and
        // nothing later can win that time back.
        assert_eq!(
            place(900 * MS, 1_010 * MS, &mut base),
            Placement::Skip,
            "a second and a half of re-sent audio must not become permanent drift"
        );

        // A real stall: the device stopped and came back. Silence holds the
        // timeline so the sound after it still matches its picture.
        let mut base = None;
        assert_eq!(place(1_000 * MS, 1_000 * MS, &mut base), Placement::Chain);
        assert_eq!(
            place(3_000 * MS, 1_005 * MS, &mut base),
            Placement::AfterHole,
            "two seconds of silence must land as two seconds of silence, not as \
             two seconds of drift"
        );
    }

    /// A device whose clock is simply *wrong* must not be treated as stalling
    /// on every chunk — that is what the baseline is for, and getting it wrong
    /// would grow the file's silence without bound.
    #[test]
    fn a_standing_clock_offset_is_learned_not_repaired() {
        const MS: i64 = 10_000;
        let mut base = None;
        // Every timestamp is 200 ms later than the byte count says it should
        // be, chunk after chunk: a clock offset, not a hole.
        for i in 0..50 {
            let next = 1_000 * MS + i * 10 * MS;
            let arrival = next + 200 * MS;
            assert_eq!(
                place(arrival, next, &mut base),
                Placement::Chain,
                "chunk {i}: a standing offset is not fifty holes"
            );
        }
        // And a genuine event on top of that offset is still caught.
        assert_eq!(
            place(
                1_000 * MS + 50 * 10 * MS + 200 * MS - 2_000 * MS,
                1_000 * MS + 50 * 10 * MS,
                &mut base
            ),
            Placement::Skip
        );
    }

    /// Padding must be a whole number of frames, always.
    ///
    /// A take whose sound woke up 17.16 ms after its picture inserted 3295
    /// bytes of silence — three bytes into a four-byte stereo frame — and every
    /// sample behind it was then assembled from two neighbours. The track
    /// measured -5.2 dB of broadband noise in all six runs that landed on that
    /// offset, and was clean whenever the same sum happened to be divisible by
    /// four (2026-10-29).
    #[test]
    fn inserted_silence_never_cuts_a_frame_in_half() {
        // The exact span that started this: 48 kHz stereo, 17.16 ms late.
        assert_eq!(silence_bytes(171_616, 192_000, 4), 3_296);
        assert_eq!(3295 % 4, 3, "this is the figure that used to be written");

        // Whatever the span and whatever the format, whole frames only.
        for span in [1, 7, 999, 171_616, 216_643, 182_598] {
            for (rate, channels) in [(48_000u32, 2u32), (48_000, 1), (96_000, 2)] {
                let bytes = silence_bytes(span, rate * channels * 2, channels * 2);
                assert_eq!(
                    bytes % (channels as usize * 2),
                    0,
                    "span {span}, {rate} Hz, {channels} ch"
                );
            }
        }
        // Nothing to fill is nothing written.
        assert_eq!(silence_bytes(0, 192_000, 4), 0);
        assert_eq!(silence_bytes(-5, 192_000, 4), 0);
    }

    /// A pause names its files `-part2`, never `-2`: a bare number is already
    /// how a second source of the same app is named, and one mic paused once
    /// must not produce the same two names as two mics never paused.
    #[test]
    fn a_segment_is_named_apart_from_a_second_source() {
        use std::path::Path;
        assert_eq!(
            segment_path(Path::new("/t/chrome.mp4"), 1),
            Path::new("/t/chrome.mp4")
        );
        assert_eq!(
            segment_path(Path::new("/t/chrome.mp4"), 2),
            Path::new("/t/chrome-part2.mp4")
        );
        assert_eq!(
            segment_path(Path::new("/t/chrome-2.mp4"), 3),
            Path::new("/t/chrome-2-part3.mp4")
        );
        // A source id and a part number can never be confused for each other:
        // the second source of an app still ends in a bare `-2`.
        assert_ne!(
            segment_path(Path::new("/t/mic.mp4"), 2),
            Path::new("/t/mic-2.mp4")
        );
        // No extension at all still gets a name.
        assert_eq!(
            segment_path(Path::new("/t/raw"), 2),
            Path::new("/t/raw-part2")
        );
    }

    /// A split numbers **every** file, the first one included: leaving the
    /// first file plain next to `-part2` reads as "the one without a number
    /// holds it all" — the opposite of what a split means (2026-10-27, the
    /// user's reading of the folder). A take that was never cut keeps the plain
    /// name, so the rule only kicks in when there is something to line up.
    #[test]
    fn a_split_numbers_every_part_including_the_first() {
        // Stamped, not just pid-keyed: a recycled pid would land in a directory
        // a previous run left behind, and then `win.mp4 → win-part1.mp4` fails
        // on Windows because the target already exists — a failure that says
        // nothing about the code under test.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("tk-parts-{stamp}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("win.mp4");
        let two = dir.join("win-part2.mp4");

        // One part: untouched, and nothing renamed on disk.
        std::fs::write(&base, b"a").unwrap();
        let mut lone = vec![base.clone()];
        number_all_parts(&base, &mut lone).unwrap();
        assert_eq!(lone, vec![base.clone()]);
        assert!(base.is_file());

        // Two parts: the first gains its number, the second keeps the one it
        // already had.
        std::fs::write(&two, b"b").unwrap();
        let mut pair = vec![base.clone(), two.clone()];
        number_all_parts(&base, &mut pair).unwrap();
        let first = dir.join("win-part1.mp4");
        assert_eq!(pair, vec![first.clone(), two.clone()]);
        assert!(
            first.is_file() && !base.exists(),
            "the plain name must be gone"
        );
        assert_eq!(std::fs::read(&first).unwrap(), b"a");

        // Running it again changes nothing — `base` is passed in rather than
        // read off the first entry, which is the whole reason it is a parameter.
        let mut again = pair.clone();
        number_all_parts(&base, &mut again).unwrap();
        assert_eq!(again, pair);
        assert!(!dir.join("win-part1-part1.mp4").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

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
        let trimmed = even_frame(frame);
        assert_eq!((trimmed.width, trimmed.height), (4, 2));
        assert_eq!(&trimmed.data[..8], &[1, 2, 3, 4, 6, 7, 8, 9]);
        assert_eq!(trimmed.data.len(), 12, "eight luma bytes plus four chroma");
        assert_eq!(&trimmed.data[8..], &[20, 21, 22, 23]);
    }

    /// A torn buffer gets the whole frame back from both halves of the
    /// pipeline: rebuilding it out of the rows that happened to fit would
    /// hand the encoder a height its pixels no longer describe (the policy
    /// `crop_frame` states — the whole frame beats half of one).
    #[test]
    fn a_torn_buffer_is_never_rebuilt_row_by_row() {
        // 5x3 BGRA where the data stops 10 bytes into the second row: the
        // 4x2 rebuild needs row bytes at 20..36 and they are not there.
        let torn = VideoFrame {
            width: 5,
            height: 3,
            format: PixelFormat::Bgra,
            data: vec![7; 30],
            time: 0,
        };
        let again = even_frame(torn);
        assert_eq!((again.width, again.height), (5, 3), "whole frame, not 4x2");
        assert_eq!(again.data.len(), 30);

        // Luma complete but the chroma plane short: same answer.
        let torn_nv12 = VideoFrame {
            width: 5,
            height: 3,
            format: PixelFormat::Nv12,
            data: vec![7; 5 * 3 + 2],
            time: 0,
        };
        let again = even_frame(torn_nv12);
        assert_eq!((again.width, again.height), (5, 3));
        assert_eq!(again.data.len(), 5 * 3 + 2);
    }

    /// A crop moves pixel boundaries and nothing else: the kept rectangle has
    /// to be exactly the source pixels inside the four edges, at the original
    /// scale, top-down.
    #[test]
    fn a_crop_keeps_the_pixels_inside_its_edges() {
        // 4x4 BGRA, each pixel's four bytes carry its index, so a misplaced row
        // or column shows up as the wrong number rather than a plausible
        // picture.
        let data: Vec<u8> = (0..4 * 4 * 4).map(|i| (i / 4) as u8).collect();
        let frame = VideoFrame {
            width: 4,
            height: 4,
            format: PixelFormat::Bgra,
            data,
            time: 7,
        };
        let cropped = crop_frame(
            frame,
            CropBox {
                top: 1,
                bottom: 1,
                left: 1,
                right: 1,
            },
        );
        assert_eq!((cropped.width, cropped.height), (2, 2));
        assert_eq!(cropped.data.len(), 2 * 2 * 4);
        // Rows 1 and 2, columns 1 and 2 — pixels 5, 6, 9, 10 of the grid.
        assert_eq!(&cropped.data[..4], &[5, 5, 5, 5]);
        assert_eq!(&cropped.data[4..8], &[6, 6, 6, 6]);
        assert_eq!(&cropped.data[8..12], &[9, 9, 9, 9]);
        assert_eq!(&cropped.data[12..16], &[10, 10, 10, 10]);
        assert_eq!(cropped.time, 7, "the timestamp travels with the pixels");
    }

    /// A burst behind the queue's head is one picture to write: the newest.
    /// Stamping the burst's oldest frame into the gap a stall leaves behind
    /// would rewind the picture right where the viewer sees the hiccup.
    #[test]
    fn a_burst_collapses_to_its_newest_frame() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<MediaFrame>(8);
        for time in [0i64, 1, 2, 3] {
            tx.send(MediaFrame::Video(VideoFrame {
                width: 2,
                height: 2,
                format: PixelFormat::Bgra,
                data: vec![time as u8; 16],
                time,
            }))
            .unwrap();
        }
        let MediaFrame::Video(first) = rx.recv().unwrap() else {
            panic!("a video frame went in");
        };
        let (winner, seen) = newest_of_burst(&rx, first);
        assert_eq!(winner.time, 3, "the last frame in wins");
        assert_eq!(&winner.data[..], &[3u8; 16]);
        assert_eq!(seen, 4, "every delivered frame is still counted");
    }

    /// An empty queue behind the head is the common case: one frame in, the
    /// same frame out, one frame counted.
    #[test]
    fn a_lone_frame_survives_the_burst_check() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<MediaFrame>(8);
        tx.send(MediaFrame::Video(VideoFrame {
            width: 1,
            height: 1,
            format: PixelFormat::Bgra,
            data: vec![7; 4],
            time: 5,
        }))
        .unwrap();
        let MediaFrame::Video(first) = rx.recv().unwrap() else {
            panic!("a video frame went in");
        };
        let (winner, seen) = newest_of_burst(&rx, first);
        assert_eq!(winner.time, 5);
        assert_eq!(seen, 1);
    }

    /// Nothing to cut must cost nothing: the very same buffer comes back.
    /// The stop checklist shows each file's resolution, so the report must
    /// carry the size the writer actually opened at — crop applied and made
    /// even — not the size the capture delivered.
    #[test]
    fn the_report_carries_the_size_the_file_was_written_at() {
        let dir = std::env::temp_dir().join("takekit-writer-size");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.mp4");
        let (tx, rx) = std::sync::mpsc::channel();
        for t in [0i64, 333_333] {
            tx.send(crate::frame::MediaFrame::Video(VideoFrame {
                width: 100,
                height: 50,
                format: PixelFormat::Bgra,
                data: vec![0; 100 * 50 * 4],
                time: t,
            }))
            .unwrap();
        }
        drop(tx);
        let report = write_stream(
            rx,
            &path,
            Encode {
                fps: 30,
                bitrate: 2_000,
                crop: CropBox {
                    top: 0,
                    bottom: 0,
                    left: 0,
                    right: 4,
                },
                sampling: Sampling::EveryFrame,
                origin: Origin::Take,
                first: None,
            },
            crate::clock::TakeClock::start(),
            std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        )
        .unwrap();
        assert_eq!((report.width, report.height), (96, 50), "crop right=4 kept");
        assert_eq!(report.frames, 2);
    }

    /// A file that starts with its own first sample says so, and reports the
    /// instant once — this is what a manifest writes down as `start`.
    #[test]
    fn a_first_sample_origin_is_reported_once() {
        let dir = std::env::temp_dir().join("takekit-writer-origin");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("late.mp4");
        let (tx, rx) = std::sync::mpsc::channel();
        // 12 s into the take, the way a source added mid-recording arrives.
        for t in [120_000_000i64, 120_333_333] {
            tx.send(crate::frame::MediaFrame::Video(VideoFrame {
                width: 128,
                height: 64,
                format: PixelFormat::Bgra,
                data: vec![0; 128 * 64 * 4],
                time: t,
            }))
            .unwrap();
        }
        drop(tx);
        let (watch_tx, watch_rx) = std::sync::mpsc::sync_channel(4);
        let report = write_stream(
            rx,
            &path,
            Encode {
                fps: 30,
                bitrate: 2_000,
                crop: CropBox::default(),
                sampling: Sampling::EveryFrame,
                origin: Origin::FirstSample,
                first: Some(watch_tx),
            },
            crate::clock::TakeClock::start(),
            std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        )
        .unwrap();
        assert_eq!(report.start, Some(120_000_000));
        assert_eq!(watch_rx.try_recv().ok(), Some(120_000_000));
        assert!(watch_rx.try_recv().is_err(), "reported once, not per frame");
        // The file covers what it recorded, not the twelve seconds it missed.
        assert!(report.seconds < 1.0, "got {}", report.seconds);
    }

    /// A paused take of a window that keeps painting. The capture never stops
    /// during a pause, so a busy window keeps handing over frames stamped past
    /// the freeze; writing them opened a fresh file per frame, each one over
    /// the take's *first* file, and left sixteen zero-second segments in the
    /// manifest for a five-second pause (measured 2026-10-27). Only the two
    /// real cuts may survive.
    #[test]
    fn frames_that_arrive_during_a_pause_are_dropped() {
        let dir = std::env::temp_dir().join("takekit-writer-paused-frames");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chrome.mp4");
        let (tx, rx) = std::sync::mpsc::channel();
        let clock = crate::clock::TakeClock::start();

        let frame = |t: i64| {
            crate::frame::MediaFrame::Video(VideoFrame {
                width: 100,
                height: 50,
                format: PixelFormat::Bgra,
                data: vec![0; 100 * 50 * 4],
                time: t,
            })
        };

        let _ = tx.send(frame(0));
        let _ = tx.send(frame(33_333_333));

        let writer_clock = clock.clone();
        let handle = std::thread::spawn(move || {
            write_stream(
                rx,
                &path,
                Encode {
                    fps: 30,
                    bitrate: 2_000,
                    crop: CropBox::default(),
                    sampling: Sampling::EveryFrame,
                    origin: Origin::Take,
                    first: None,
                },
                writer_clock,
                std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
            )
        });

        // Long enough for the encoder to open and take both frames; it opens
        // lazily on the first one, which is the slow part.
        std::thread::sleep(std::time::Duration::from_millis(400));
        clock.pause();
        let frozen = clock.now_100ns();
        for i in 1..=10 {
            let _ = tx.send(frame(frozen + i * 33_333_333));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        clock.resume();
        let _ = tx.send(frame(clock.now_100ns() + 33_333_333));
        drop(tx);

        let report = handle.join().unwrap().unwrap();
        let shape: Vec<(u64, f64)> = report
            .segments
            .iter()
            .map(|s| (s.frames, s.seconds))
            .collect();
        assert_eq!(
            report.segments.len(),
            2,
            "one file per real cut; got {shape:?}"
        );
        assert_eq!(report.segments[0].frames, 2, "before the pause: {shape:?}");
    }

    #[test]
    fn a_zero_crop_hands_the_frame_back_untouched() {
        let frame = VideoFrame {
            width: 2,
            height: 2,
            format: PixelFormat::Bgra,
            data: vec![9; 16],
            time: 1,
        };
        let same = crop_frame(frame, CropBox::default());
        assert_eq!((same.width, same.height), (2, 2));
        assert_eq!(same.data.len(), 16);
    }

    /// A window that shrank after its edges were picked would leave a crop
    /// wider than the frame: the whole picture is the useful answer, and the
    /// alternative is a two-pixel file or a panic.
    #[test]
    fn a_crop_wider_than_the_frame_is_dropped() {
        let box_too_big = CropBox {
            left: 100,
            right: 100,
            ..CropBox::default()
        };
        assert_eq!(box_too_big.rect_in(120, 80), None);
        let sliver = CropBox {
            left: 0,
            right: 79,
            top: 0,
            bottom: 0,
        };
        assert_eq!(sliver.rect_in(80, 80), None, "one pixel is not a crop");
        let fits = CropBox {
            left: 10,
            right: 20,
            top: 5,
            bottom: 15,
        };
        assert_eq!(fits.rect_in(100, 80), Some((10, 5, 70, 60)));
    }

    /// NV12 keeps chroma at half resolution, so an odd edge would put the two
    /// planes half a block apart — every edge lands on an even pixel instead.
    #[test]
    fn an_nv12_crop_aligns_both_planes() {
        let (w, h) = (6u32, 4u32);
        let mut data = vec![0u8; (w * h + w * h / 2) as usize];
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }
        let frame = VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Nv12,
            data,
            time: 0,
        };
        let cropped = crop_frame(
            frame,
            CropBox {
                top: 1,
                bottom: 1,
                left: 1,
                right: 1,
            },
        );
        // Edges rounded down to even: the 4x2 rectangle starting at (0, 0).
        assert_eq!((cropped.width, cropped.height), (4, 2));
        assert_eq!(
            cropped.data.len(),
            4 * 2 + 4,
            "luma plus a half-height chroma"
        );
        assert_eq!(&cropped.data[..4], &[0, 1, 2, 3]);
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

    /// Where each frame's content lands on the take's timeline.
    ///
    /// The MP4 sink runs the H.264 encoder at a fixed frame rate and expands
    /// every sample's duration into that many repeated frames — input sample
    /// times never reach the container as-is. What does survive is *placement*:
    /// a frame written for [1.0s, 1.5s) must appear as output frames 10..14
    /// at 10 fps. Multi-source sync is exactly this placement, so this test
    /// decodes the file and checks the picture, not the timestamps.
    #[test]
    fn frames_land_where_the_take_timeline_puts_them() {
        let dir = std::env::temp_dir().join("takekit-writer-places");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.mp4");

        let mut w = VideoWriter::create(&path, 64, 64, 10, 2_000).unwrap();
        // White from 1.0s, grey from 1.5s, and nothing before 1.0s — the
        // writer owes a black lead-in for that gap on its own.
        for (t, level) in [(10_000_000i64, 255u8), (15_000_000, 64)] {
            let frame = VideoFrame {
                width: 64,
                height: 64,
                format: PixelFormat::Bgra,
                data: vec![level; 64 * 64 * 4],
                time: t,
            };
            w.write(&frame).unwrap();
        }
        w.finish_at(Some(25_000_000)).unwrap();

        // Decode to raw grey and read the frames back.
        let out = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-i",
                path.to_str().unwrap(),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "gray",
                "-",
            ])
            .output()
            .expect("ffmpeg is on the PATH of a machine that can build this crate");
        let frame_len = 64 * 64;
        let frames: Vec<&[u8]> = out.stdout.chunks_exact(frame_len).collect();
        assert_eq!(frames.len(), 25, "2.5s at 10fps is 25 output frames");

        let level_of = |i: usize| -> u32 {
            frames[i].iter().map(|&b| b as u32).sum::<u32>() / frame_len as u32
        };
        // [0, 1.0s) black, [1.0s, 1.5s) white, [1.5s, 2.5s) grey — ten, five
        // and ten frames. If the lead-in were missing, white would start at
        // frame 0 and every file of a multi-source take would slide against
        // the others by however late its capture woke up.
        assert_eq!(level_of(0), 0, "frames 0..10 are the lead-in");
        assert_eq!(level_of(9), 0);
        assert_eq!(level_of(10), 255, "white starts exactly at 1.0s");
        assert_eq!(level_of(14), 255);
        assert_eq!(level_of(15), 64, "grey starts exactly at 1.5s");
        assert_eq!(level_of(24), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where content lands when the capture delivered frames between slots.
    ///
    /// Real captures arrive whenever they arrive: 37ms here, 51ms there. Each
    /// sample's duration is expanded into `round(duration x fps)` output
    /// frames, so if that rounding accumulated, a long take's picture would
    /// drift against its own timeline and against the other channels. Content
    /// must land within half a slot of where it was captured, all the way to
    /// the end.
    #[test]
    fn between_slot_capture_times_do_not_drift() {
        let dir = std::env::temp_dir().join("takekit-writer-drift");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.mp4");

        // 30 fps, frames at 0.37s / 1.02s / 1.53s / 1.97s — none on a slot
        // boundary (slots are 33.3ms). Grey levels 255 / 192 / 128 / 64.
        let mut w = VideoWriter::create(&path, 64, 64, 30, 2_000).unwrap();
        for (t, level) in [
            (3_700_000i64, 255u8),
            (10_200_000, 192),
            (15_300_000, 128),
            (19_700_000, 64),
        ] {
            let frame = VideoFrame {
                width: 64,
                height: 64,
                format: PixelFormat::Bgra,
                data: vec![level; 64 * 64 * 4],
                time: t,
            };
            w.write(&frame).unwrap();
        }
        w.finish_at(Some(24_000_000)).unwrap();

        let out = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-i",
                path.to_str().unwrap(),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "gray",
                "-",
            ])
            .output()
            .expect("ffmpeg is on the PATH of a machine that can build this crate");
        let frame_len = 64 * 64;
        let frames: Vec<&[u8]> = out.stdout.chunks_exact(frame_len).collect();
        // 2.4s at 30fps is 72 slots. The tail may round +/-1; the content
        // positions are what must not drift.
        assert!(
            (70..=73).contains(&frames.len()),
            "72 slots expected, got {}",
            frames.len()
        );

        let level_of = |i: usize| -> u32 {
            frames[i].iter().map(|&b| b as u32).sum::<u32>() / frame_len as u32
        };
        // The slot for time t is round(t x fps): 0.37s -> slot 11, 1.02s ->
        // slot 31, 1.53s -> slot 46, 1.97s -> slot 59. Each must land within
        // one slot of that; anything more is the expansion rounding piling up.
        // H.264 is lossy: a flat grey comes back within a couple of steps,
        // never exactly. Nearest level within tolerance counts as a hit.
        let where_level = |level: u32| -> usize {
            frames
                .iter()
                .position(|f| {
                    let v = f.iter().map(|&b| b as u32).sum::<u32>() / frame_len as u32;
                    v.abs_diff(level) <= 4
                })
                .unwrap_or(usize::MAX)
        };
        let first = |level: u32, expected: usize, name: &str| {
            let at = where_level(level);
            assert!(
                at.abs_diff(expected) <= 1,
                "{name} at slot {at}, expected within 1 of {expected} — expansion rounding drifted"
            );
        };
        first(255, 11, "frame at 0.37s");
        first(192, 31, "frame at 1.02s");
        first(128, 46, "frame at 1.53s");
        first(64, 59, "frame at 1.97s");
        // And nothing before its slot: the lead-in really is black.
        assert_eq!(level_of(0), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
