//! Thin wrapper around the ffmpeg binaries.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long ffmpeg gets to write its trailer after being asked to quit.
///
/// A direct-show device that has stopped responding can leave ffmpeg blocked
/// on `q`, and an unbounded `wait()` would hang the stop (and `Recording`'s
/// drop, and the app's exit) with it. Five seconds is far beyond the cost of
/// finalising a normal file.
const FINALISE_TIMEOUT: Duration = Duration::from_secs(5);

/// Find ffmpeg on PATH. Returns `None` if it is not there.
pub fn locate() -> Option<PathBuf> {
    // Windows-only crate (the source module calls Win32 unconditionally), so
    // the binary name is not selected at runtime.
    let exe = "ffmpeg.exe";
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(exe))
            .find(|p| p.is_file())
    })
}

pub fn ffprobe(ffmpeg: &Path) -> PathBuf {
    ffmpeg.with_file_name("ffprobe.exe")
}

/// Build a command for an ffmpeg-family binary.
///
/// Windows gives a console child of a GUI-subsystem parent its own console
/// window, so every probe would flash a terminal over the panel.
/// `CREATE_NO_WINDOW` suppresses it; stdout and stderr are piped or captured
/// at every call site anyway.
pub(crate) fn command(exe: &Path) -> Command {
    use std::os::windows::process::CommandExt;
    use windows::Win32::System::Threading::CREATE_NO_WINDOW;

    let mut cmd = Command::new(exe);
    cmd.creation_flags(CREATE_NO_WINDOW.0);
    cmd
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
    command(ffmpeg)
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
/// leaves the file truncated (MKV survives this, MP4 does not). A process that
/// ignores `q` is killed after [`FINALISE_TIMEOUT`] — a truncated file is bad,
/// but an app that can never exit is worse.
pub fn stop(child: &mut Child) {
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(b"q");
        let _ = stdin.flush();
    }
    let deadline = Instant::now() + FINALISE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            // Either the timeout passed or `try_wait` itself failed: there is
            // nothing left to wait for.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

/// Duration of a media file in seconds, via ffprobe.
pub fn duration(probe: &Path, file: &Path) -> Option<f64> {
    let out = command(probe)
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

/// The cache of DDA verdicts, keyed by (ffmpeg binary, rect).
fn dda_cache() -> &'static Mutex<HashMap<(PathBuf, crate::source::ScreenRect), bool>> {
    static CACHE: OnceLock<Mutex<HashMap<(PathBuf, crate::source::ScreenRect), bool>>> =
        OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The cached verdict, if one has been reached — **never probes**.
///
/// The probe opens a D3D11 device and a duplication session and costs ~2.6s
/// regardless of the rectangle size, so it must never sit on the click path.
/// `Session::prepare` reads this and falls back to GDI for one take when the
/// answer is not known yet; the probe runs on a background thread and the next
/// take uses its verdict.
pub fn dda_verdict(ffmpeg: &Path, r: crate::source::ScreenRect) -> Option<bool> {
    let slot = dda_cache().lock().unwrap_or_else(|e| e.into_inner());
    slot.get(&(ffmpeg.to_path_buf(), r)).copied()
}

/// Probe now (or read the cache). Call this from a background thread.
///
/// Only one probe per rect runs at a time: the startup warm-up and the
/// frontend's per-source preheat ask for the same rectangle, and two
/// concurrent D3D11 duplications cost 2.6s each for one answer.
pub fn dda_available(ffmpeg: &Path, r: crate::source::ScreenRect) -> bool {
    if let Some(known) = dda_verdict(ffmpeg, r) {
        return known;
    }
    let key = (ffmpeg.to_path_buf(), r);
    let mine = {
        let mut inflight = in_flight().lock().unwrap_or_else(|e| e.into_inner());
        inflight.insert(key.clone())
    };
    if !mine {
        // Someone else owns the probe: wait for their verdict instead of
        // running a second one. Bounded, because this is a background thread.
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(known) = dda_verdict(ffmpeg, r) {
                return known;
            }
        }
        return false;
    }
    // The probe runs outside the lock: holding it across 2.6s of D3D init also
    // blocks every other reader, including the one on the click path.
    let ok = dda_probe(ffmpeg, r);
    dda_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone(), ok);
    in_flight()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    ok
}

fn in_flight() -> &'static Mutex<std::collections::HashSet<(PathBuf, crate::source::ScreenRect)>> {
    static SET: OnceLock<Mutex<std::collections::HashSet<(PathBuf, crate::source::ScreenRect)>>> =
        OnceLock::new();
    SET.get_or_init(Default::default)
}

fn dda_probe(ffmpeg: &Path, r: crate::source::ScreenRect) -> bool {
    let out = command(ffmpeg)
        .args([
            "-hide_banner", "-v", "error",
            "-f", "lavfi", "-i",
            &format!(
                "ddagrab=output_idx=0:framerate=1:draw_mouse=0:offset_x={}:offset_y={}:video_size={}x{}",
                r.x, r.y, r.w, r.h
            ),
            "-frames:v", "1", "-vf", "hwdownload,format=bgra", "-f", "null", "-",
        ])
        .stdin(Stdio::null())
        .output();
    matches!(out, Ok(o) if o.status.success())
}

#[cfg(test)]
fn cached_dda(
    cache: &mut HashMap<(PathBuf, crate::source::ScreenRect), bool>,
    ffmpeg: &Path,
    rect: crate::source::ScreenRect,
    probe: impl Fn(&Path, crate::source::ScreenRect) -> bool,
) -> bool {
    let key = (ffmpeg.to_path_buf(), rect);
    if let Some(&known) = cache.get(&key) {
        return known;
    }
    let ok = probe(ffmpeg, rect);
    cache.insert(key, ok);
    ok
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

    let out = command(ffmpeg)
        .args([
            "-hide_banner",
            "-f",
            "dshow",
            "-list_devices",
            "true",
            "-i",
            "dummy",
        ])
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
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use super::{cached_dda, first_quoted, last_guid};
    use crate::source::ScreenRect;

    #[test]
    fn dda_probe_is_cached_per_path_and_rect() {
        let calls = std::cell::Cell::new(0);
        let probe = |_: &Path, _: ScreenRect| {
            calls.set(calls.get() + 1);
            false
        };
        let mut cache: HashMap<(PathBuf, ScreenRect), bool> = HashMap::new();
        let r1 = ScreenRect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        let r2 = ScreenRect {
            x: 100,
            y: 0,
            w: 1280,
            h: 720,
        };
        let ff = Path::new("ffmpeg-a");

        let first = cached_dda(&mut cache, ff, r1, probe);
        assert!(!first);
        assert_eq!(calls.get(), 1);

        let again = cached_dda(&mut cache, ff, r1, probe);
        assert_eq!(again, first, "a cached rect must keep its verdict");
        assert_eq!(calls.get(), 1, "same path and rect must not re-probe");

        let _ = cached_dda(&mut cache, ff, r2, probe);
        assert_eq!(calls.get(), 2, "a different rect must probe again");

        let other_ff = Path::new("ffmpeg-b");
        let _ = cached_dda(&mut cache, other_ff, r1, probe);
        assert_eq!(calls.get(), 3, "a different binary must probe again");
    }

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
