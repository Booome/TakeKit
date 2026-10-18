//! Thin wrapper around the ffmpeg binaries.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Find ffmpeg on PATH. Returns `None` if it is not there.
pub fn locate() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(exe))
            .find(|p| p.is_file())
    })
}

pub fn ffprobe(ffmpeg: &Path) -> PathBuf {
    ffmpeg.with_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" })
}

/// Spawn one ffmpeg process. `args` should not include the binary itself.
///
/// One process per source is deliberate: feeding several inputs to a single
/// ffmpeg makes its demuxer pick inputs by timestamp, which starves the desktop
/// grabber down to roughly half the requested frame rate.
/// `log` receives ffmpeg's stderr. Piping it and never reading would eventually
/// block the encoder once the pipe buffer fills, so it goes to a file — which is
/// also what you need when a source fails to open.
pub fn spawn(ffmpeg: &Path, args: &[String], log: &Path) -> std::io::Result<Child> {
    let err = std::fs::File::create(log)?;
    Command::new(ffmpeg)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
}

/// Last `n` lines of a text file, for surfacing ffmpeg's error.
pub fn tail(path: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Ask ffmpeg to finish the file properly, then wait for it.
///
/// ffmpeg finalises the container when it receives `q` on stdin; killing it
/// leaves the file truncated (MKV survives this, MP4 does not).
pub fn stop(child: &mut Child) {
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(b"q");
        let _ = stdin.flush();
    }
    let _ = child.wait();
}

/// Duration of a media file in seconds, via ffprobe.
pub fn duration(probe: &Path, file: &Path) -> Option<f64> {
    let out = Command::new(probe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(file)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Devices that ffmpeg can capture from (DirectShow on Windows).
///
/// Parsing this is less obvious than it looks: ffmpeg builds differ. Some print
/// `DirectShow video devices` headers and tag entries as `(video)`; others (the
/// full builds on Windows) print only the name followed by an `Alternative name`
/// line whose class GUID says whether it is a video or an audio device. Both
/// shapes are handled.
pub fn list_capture_devices(ffmpeg: &Path) -> Vec<String> {
    /// DirectShow class GUIDs.
    const VIDEO: &str = "860BB310-5D01-11D0-BD3B-00A0C911CE86";
    const AUDIO: &str = "33D9A762-90C8-11D0-BD43-00A0C911CE86";

    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-f", "dshow", "-list_devices", "true", "-i", "dummy"])
        .stdin(Stdio::null())
        .output();

    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stderr);

    let mut devices: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;
    let mut in_audio_section = false;

    for line in text.lines() {
        if line.contains("DirectShow video devices") {
            in_audio_section = false;
            continue;
        }
        if line.contains("DirectShow audio devices") {
            in_audio_section = true;
            continue;
        }

        let quoted = first_quoted(line);

        if line.contains("Alternative name") {
            if let (Some(name), Some(guid)) = (pending.take(), last_guid(line)) {
                if guid.eq_ignore_ascii_case(VIDEO) {
                    devices.push(name);
                } else if guid.eq_ignore_ascii_case(AUDIO) {
                    // audio device: ignored for now
                }
            }
            continue;
        }

        if let Some(name) = quoted {
            // Builds that tag the entry inline are unambiguous.
            if line.contains("(video)") {
                devices.push(name);
                pending = None;
            } else if in_audio_section {
                pending = None;
            } else {
                pending = Some(name);
            }
        }
    }

    devices
}

fn first_quoted(line: &str) -> Option<String> {
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_string())
}

/// The class GUID embedded in `@device_sw_{GUID}\...`.
fn last_guid(line: &str) -> Option<String> {
    let open = line.rfind('{')?;
    let close = line[open..].find('}')? + open;
    Some(line[open + 1..close].to_string())
}

#[cfg(test)]
mod tests {
    use super::{first_quoted, last_guid};

    #[test]
    fn pulls_the_quoted_name() {
        assert_eq!(
            first_quoted(r#"[in#0 @ 0x1] "Mikey Cam" (none)"#).as_deref(),
            Some("Mikey Cam")
        );
        assert_eq!(first_quoted("no quotes here"), None);
    }

    #[test]
    fn pulls_the_class_guid() {
        assert_eq!(
            last_guid(
                r#"  Alternative name "@device_sw_{860BB310-5D01-11D0-BD3B-00A0C911CE86}\DirectShow Softcam""#
            )
            .as_deref(),
            Some("860BB310-5D01-11D0-BD3B-00A0C911CE86")
        );
    }
}
