//! Uncompressed media on its way from a capture to a writer.
//!
//! A frame carries the time it was captured rather than the time it was
//! written: the two differ by however long the queue and the encoder take, and
//! only the capture time keeps a take in step with the wall clock.

/// Windows counts media time in 100 ns units.
pub const TIME_UNITS_PER_SECOND: i64 = 10_000_000;

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
}
