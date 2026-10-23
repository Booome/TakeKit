//! Uncompressed media on its way from a capture to a writer.
//!
//! A frame carries the time it was captured rather than the time it was
//! written: the two differ by however long the queue and the encoder take, and
//! only the capture time keeps a take in step with the wall clock.
//!
//! The queue between the two ends is deliberately small and bounded (see
//! [`VIDEO_QUEUE`] / [`AUDIO_QUEUE`]) and is fed through [`offer`], never
//! blocking `send`: the capture must not wait on an encoder, and an unbounded
//! queue would turn "the writer is behind" into unbounded memory growth.

use std::sync::mpsc::{SyncSender, TrySendError};

/// Windows counts media time in 100 ns units.
pub const TIME_UNITS_PER_SECOND: i64 = 10_000_000;

/// How many video frames may wait between a capture and its writer.
///
/// Small on purpose: every queued frame owns its full pixels (a 4K BGRA frame
/// is about 33 MB) and the writer only falls behind by an encoder hiccup. A
/// queue this deep rides those out; a longer stall starts dropping frames,
/// which the take is built to survive — the encoder holds the previous picture
/// across the gap.
pub const VIDEO_QUEUE: usize = 3;

/// The same for audio chunks. A chunk is tiny — 10 ms of 48 kHz stereo 16-bit
/// is under 2 KB — so a deep queue is nearly free and only a writer stuck for
/// whole seconds ever fills it. That headroom matters because a dropped chunk
/// is worse than a dropped frame: chunks are written back to back, so a missing
/// one pulls the rest of the file earlier.
pub const AUDIO_QUEUE: usize = 64;

/// How the bytes of a [`VideoFrame`] are laid out.
///
/// Each capture path has a format it can produce without a conversion: Windows
/// Graphics Capture hands over BGRA, a camera through Media Foundation is
/// native NV12. Converting either one here would cost a full-frame copy per
/// frame, so the writer is told what it is being given instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4 bytes per pixel, blue first, rows top-down.
    Bgra,
    /// Y plane then an interleaved UV plane, both full width.
    Nv12,
}

/// One video frame, [`PixelFormat`] bytes, no row padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    /// Capture time in 100 ns units since this take started.
    pub time: i64,
}

/// One slice of audio: interleaved 16-bit PCM, the format every Windows
/// capture path can produce without a conversion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioChunk {
    pub data: Vec<u8>,
    /// Capture time in 100 ns units since this take started.
    pub time: i64,
}

/// What a capture hands its writer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaFrame {
    Video(VideoFrame),
    Audio(AudioChunk),
}

impl MediaFrame {
    pub fn time(&self) -> i64 {
        match self {
            Self::Video(f) => f.time,
            Self::Audio(a) => a.time,
        }
    }
}

/// Hand one frame to a writer's queue without ever blocking the capture.
///
/// A full queue means the writer is behind; dropping the frame keeps the take
/// going at the cost of one frame rather than blocking the capture (which would
/// stall the whole device). `false` comes back only when the writer is gone —
/// its receiver was dropped — and the capture should stop.
pub fn offer(out: &SyncSender<MediaFrame>, frame: MediaFrame) -> bool {
    match out.try_send(frame) {
        Ok(()) => true,
        // Deliberate: see the doc above. One dropped frame reads as a held
        // picture in the file, which the writer's gap handling already covers.
        Err(TrySendError::Full(_)) => true,
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// Seconds as media time.
pub fn seconds_to_time(seconds: f64) -> i64 {
    (seconds * TIME_UNITS_PER_SECOND as f64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_is_ten_million_units() {
        assert_eq!(seconds_to_time(1.5), 15_000_000);
    }

    #[test]
    fn both_kinds_report_their_own_time() {
        let video = MediaFrame::Video(VideoFrame {
            width: 2,
            height: 2,
            format: PixelFormat::Bgra,
            data: vec![0; 16],
            time: 7,
        });
        let audio = MediaFrame::Audio(AudioChunk {
            data: vec![0; 4],
            time: 9,
        });
        assert_eq!(video.time(), 7);
        assert_eq!(audio.time(), 9);
    }

    fn video(time: i64) -> MediaFrame {
        MediaFrame::Video(VideoFrame {
            width: 1,
            height: 1,
            format: PixelFormat::Bgra,
            data: vec![0; 4],
            time,
        })
    }

    /// A full queue is the writer falling behind, not a failure: the frame is
    /// dropped and the capture keeps going.
    #[test]
    fn a_full_queue_drops_the_frame_and_keeps_the_capture_going() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<MediaFrame>(1);
        assert!(offer(&tx, video(0)));
        assert!(
            offer(&tx, video(1)),
            "the second frame is dropped, not an error"
        );
        assert_eq!(rx.recv().unwrap().time(), 0);
        assert!(offer(&tx, video(2)), "the queue is free again");
        assert_eq!(rx.recv().unwrap().time(), 2);
    }

    /// A dropped receiver is the only real failure: the capture has nowhere to
    /// put frames any more and should stop.
    #[test]
    fn a_gone_writer_is_the_only_failure() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<MediaFrame>(1);
        drop(rx);
        assert!(!offer(&tx, video(0)));
    }
}
