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
        if let Some(last) = self.pending.take() {
            let duration = match end {
                Some(end) => (end - last.time).max(self.last_duration),
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
    /// Whether any chunk has been written yet, so the lead-in silence the
    /// video writer's black sample plays against is written exactly once.
    started: bool,
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
                started: false,
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
        if !self.started && start > 0 {
            // The microphone woke up after the take did: fill the gap with
            // silence so the file's timeline keeps the shared origin, exactly
            // like the video writer's black lead-in.
            let silence =
                vec![0u8; (start as u128 * self.bytes_per_second as u128 / 10_000_000) as usize];
            // SAFETY: owned zeros, as below.
            unsafe {
                write_sample(&self.writer, self.stream, &silence, 0, start)?;
            }
        }
        self.started = true;
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

/// How one source's frames reach the encoder.
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
    pub seconds: f64,
    pub frames: u64,
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
    fps: u32,
    bitrate: u32,
    clock: crate::clock::TakeClock,
    end_hint: std::sync::Arc<std::sync::atomic::AtomicI64>,
    sampling: Sampling,
) -> Result<StreamReport> {
    use crate::frame::MediaFrame;
    use std::sync::atomic::Ordering;

    let fps = fps.max(1);
    let interval = std::time::Duration::from_secs_f64(1.0 / fps as f64);
    let mut video: Option<VideoWriter> = None;
    let mut frames = 0u64;
    let mut latest: Option<VideoFrame> = None;
    let mut due_slot = 0u64;

    loop {
        match sampling {
            Sampling::EveryFrame => match rx.recv() {
                Ok(MediaFrame::Video(frame)) => {
                    frames += 1;
                    let frame = even_frame(frame);
                    if video.is_none() {
                        video = Some(open_for(&frame, out, fps, bitrate)?);
                    }
                    video.as_mut().expect("opened above").write(&frame)?;
                }
                Ok(_) => {}
                Err(_) => break,
            },
            Sampling::KeepLatest => {
                match rx.recv_timeout(interval) {
                    Ok(MediaFrame::Video(frame)) => {
                        frames += 1;
                        latest = Some(even_frame(frame));
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
                        if video.is_none() {
                            video = Some(open_for(frame, out, fps, bitrate)?);
                        }
                        let slot_time = (due_slot as i64 * 10_000_000) / i64::from(fps);
                        video
                            .as_mut()
                            .expect("opened above")
                            .write_at(frame, slot_time)?;
                    }
                    due_slot += 1;
                }
            }
        }
    }

    let Some(w) = video.take() else {
        return Ok(StreamReport {
            seconds: 0.0,
            frames,
        });
    };
    // How long the last frame was on screen is something only the caller
    // knows: nothing arrives after it. Without this a take of a still screen
    // is a zero-second file. The latched end wins so all channels stop
    // together.
    let end = match end_hint.load(Ordering::SeqCst) {
        0 => clock.now_100ns(),
        t => t,
    };
    w.finish_at(Some(end))?;
    Ok(StreamReport {
        seconds: end as f64 / 10_000_000.0,
        frames,
    })
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
