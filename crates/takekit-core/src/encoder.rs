//! Encoder selection: prefer the GPU, fall back to the CPU.
//!
//! Probing is done once per run with a one-frame encode at 256x256, which costs
//! a few milliseconds and is far more reliable than guessing from driver versions
//! (NVENC in particular reports "available" and then fails at encode time).
//!
//! 256x256 rather than a token 64x64 is deliberate: current NVENC builds enforce
//! a minimum frame dimension and reject anything smaller, which would report a
//! working GPU encoder as unavailable and silently fall back to the CPU.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The quality knob's domain, in the units every encoder here takes: CRF for
/// x264, CQ/QP for the hardware ones.
///
/// Why these numbers: 51 is the top of H.264's QP range and what x264's CRF
/// stops at — above it every encoder here either refuses the value or clamps
/// it, so the file looks the same and the setting is a lie. 0 is lossless,
/// which x264 really does and the hardware encoders read as their lowest QP.
/// Values outside this range used to pass straight through to `-crf 200`,
/// where ffmpeg fails *after* the take folder exists.
pub const MIN_QUALITY: u8 = 0;
pub const MAX_QUALITY: u8 = 51;
/// What a take uses when nobody says otherwise.
pub const DEFAULT_QUALITY: u8 = 20;

/// The one message every surface shows for an out-of-range quality.
///
/// Takes the value as anything printable so the panel can report a number it
/// could not even narrow to `u8` (`{"quality": 256}`) in the same words.
pub fn quality_message(got: impl std::fmt::Display) -> String {
    format!(
        "quality must be {MIN_QUALITY}..={MAX_QUALITY} (CRF for x264, CQ/QP for the \
         hardware encoders; {MIN_QUALITY} = lossless), got {got}"
    )
}

/// `Ok(())` inside the domain, otherwise the one message every surface shows.
///
/// One function so the CLI, the panel and MCP cannot disagree about the bound
/// or about how it is worded.
pub fn check_quality(quality: u8) -> Result<(), String> {
    if (MIN_QUALITY..=MAX_QUALITY).contains(&quality) {
        Ok(())
    } else {
        Err(quality_message(quality))
    }
}

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
    let out = crate::ffmpeg::command(ffmpeg)
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=256x256:d=0.1",
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
///
/// The result is cached for the process: hardware does not change at runtime,
/// and `prepare` and the frontend both ask (a quarter second each).
pub fn pick(ffmpeg: &Path) -> Encoder {
    static PICKED: Mutex<Option<(PathBuf, Encoder)>> = Mutex::new(None);
    let mut slot = PICKED.lock().unwrap_or_else(|e| e.into_inner());
    cached_pick(&mut slot, ffmpeg, works)
}

fn cached_pick(
    slot: &mut Option<(PathBuf, Encoder)>,
    ffmpeg: &Path,
    probe: impl Fn(&Path, Encoder) -> bool,
) -> Encoder {
    if let Some((p, e)) = slot {
        if p == ffmpeg {
            return *e;
        }
    }
    let picked = [Encoder::Nvenc, Encoder::Qsv, Encoder::Amf]
        .into_iter()
        .find(|e| probe(ffmpeg, *e))
        .unwrap_or(Encoder::X264);
    *slot = Some((ffmpeg.to_path_buf(), picked));
    picked
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

    #[test]
    fn pick_probes_each_encoder_once_then_hits_the_cache() {
        let calls = std::cell::Cell::new(0);
        let probe = |_: &Path, _: Encoder| {
            calls.set(calls.get() + 1);
            false
        };
        let mut slot = None;
        let a = cached_pick(&mut slot, Path::new("ffmpeg-a"), probe);
        let second = calls.get();
        let b = cached_pick(&mut slot, Path::new("ffmpeg-a"), probe);
        assert_eq!(a, Encoder::X264);
        assert_eq!(a, b);
        assert_eq!(second, 3, "one probe per hardware candidate");
        assert_eq!(calls.get(), 3, "a cached path must not re-probe");
    }

    #[test]
    fn pick_reprobes_when_the_binary_path_changes() {
        let calls = std::cell::Cell::new(0);
        let probe = |_: &Path, _: Encoder| {
            calls.set(calls.get() + 1);
            false
        };
        let mut slot = None;
        let _ = cached_pick(&mut slot, Path::new("ffmpeg-a"), probe);
        let _ = cached_pick(&mut slot, Path::new("ffmpeg-b"), probe);
        assert_eq!(calls.get(), 6, "a different binary must be probed again");
    }

    #[test]
    fn quality_domain_is_0_to_51_and_nothing_else() {
        // The ends are included, one step outside each is not: this is the
        // gate `Session::prepare` applies to every take.
        assert!(check_quality(MIN_QUALITY).is_ok(), "0 = lossless is legal");
        assert!(check_quality(MAX_QUALITY).is_ok(), "51 is H.264's top QP");
        assert!(check_quality(DEFAULT_QUALITY).is_ok());
        assert!(check_quality(MAX_QUALITY + 1).is_err());
        assert!(check_quality(u8::MAX).is_err());

        // The message must name both ends and the value that was refused —
        // the panel and the CLI show this string as-is.
        let err = check_quality(60).unwrap_err();
        assert!(err.contains("0..=51"), "{err}");
        assert!(err.contains("got 60"), "{err}");
        assert_eq!(err, quality_message(60), "one wording, two entry points");
    }

    #[test]
    fn encoder_is_lowercase_on_the_wire_both_ways() {
        // `Encoder` rides in config.json (`encoder: Option<Encoder>`) and in
        // the control surface's PATCH body. `Backend` once went out as
        // "Windows" while the panel sent "windows": one rejected field fell
        // every field back to defaults. The same fence, for the enum that
        // had none (development.md §6.18: IPC enums are lowercase + tested).
        for value in ["nvenc", "qsv", "amf", "x264"] {
            let parsed: Encoder = serde_json::from_str(&format!("\"{value}\"")).unwrap();
            assert_eq!(
                serde_json::to_string(&parsed).unwrap(),
                format!("\"{value}\""),
                "{value} must round-trip unchanged"
            );
        }
        // And the wrong case is still a rejection, not a silent default.
        assert!(serde_json::from_str::<Encoder>("\"NVENC\"").is_err());
    }
}
