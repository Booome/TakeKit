//! Encoder selection: prefer the GPU, fall back to the CPU.
//!
//! Probing is done once per run with a one-frame encode at 64x64, which costs a
//! few milliseconds and is far more reliable than guessing from driver versions
//! (NVENC in particular reports "available" and then fails at encode time).

use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoder {
    /// NVIDIA NVENC — hardware.
    Nvenc,
    /// Intel Quick Sync — hardware.
    Qsv,
    /// AMD AMF — hardware.
    Amf,
    /// libx264 — CPU, always available.
    X264,
}

impl Encoder {
    pub fn name(self) -> &'static str {
        match self {
            Encoder::Nvenc => "h264_nvenc",
            Encoder::Qsv => "h264_qsv",
            Encoder::Amf => "h264_amf",
            Encoder::X264 => "libx264",
        }
    }

    pub fn is_hardware(self) -> bool {
        !matches!(self, Encoder::X264)
    }

    /// Encoder-specific arguments. `quality` is roughly CRF for x264 and CQ for
    /// the hardware encoders, so the same number means comparable visual quality.
    pub fn args(self, quality: u8) -> Vec<String> {
        let q = quality.to_string();
        match self {
            Encoder::X264 => vec![
                "-c:v".into(),
                "libx264".into(),
                "-preset".into(),
                "veryfast".into(),
                "-crf".into(),
                q,
                "-pix_fmt".into(),
                "yuv420p".into(),
            ],
            Encoder::Nvenc => vec![
                "-c:v".into(),
                "h264_nvenc".into(),
                "-preset".into(),
                "p5".into(),
                "-rc".into(),
                "vbr".into(),
                "-cq".into(),
                q,
                "-pix_fmt".into(),
                "yuv420p".into(),
            ],
            Encoder::Qsv => vec![
                "-c:v".into(),
                "h264_qsv".into(),
                "-global_quality".into(),
                q,
                "-pix_fmt".into(),
                "nv12".into(),
            ],
            Encoder::Amf => vec![
                "-c:v".into(),
                "h264_amf".into(),
                "-quality".into(),
                "speed".into(),
                "-qp_i".into(),
                q.clone(),
                "-qp_p".into(),
                q,
                "-pix_fmt".into(),
                "yuv420p".into(),
            ],
        }
    }
}

/// Returns true if `ffmpeg` can actually encode with this encoder.
pub fn works(ffmpeg: &Path, enc: Encoder) -> bool {
    let out = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:d=0.1",
            "-frames:v",
            "1",
            "-c:v",
            enc.name(),
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    matches!(out, Ok(s) if s.success())
}

/// Pick the best encoder available on this machine.
///
/// Order is deliberate: NVENC, then QSV, then AMF — the encoders people who
/// record their desktop actually have — and x264 last.
pub fn pick(ffmpeg: &Path) -> Encoder {
    for enc in [Encoder::Nvenc, Encoder::Qsv, Encoder::Amf] {
        if works(ffmpeg, enc) {
            return enc;
        }
    }
    Encoder::X264
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x264_args_carry_the_codec() {
        let a = Encoder::X264.args(20);
        assert!(a.contains(&"libx264".to_string()));
        assert!(a.contains(&"20".to_string()));
    }

    #[test]
    fn hardware_flag_is_right() {
        assert!(Encoder::Nvenc.is_hardware());
        assert!(!Encoder::X264.is_hardware());
    }
}
