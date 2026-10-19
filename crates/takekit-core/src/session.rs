//! One recording session: prepare → start → stop.
//!
//! Both the CLI and the GUI drive this. The orchestration used to live in the
//! CLI's `record()`; if it stays there the GUI has to write a second copy, and
//! the two will drift — the ordering rules below are too easy to get wrong
//! twice.
//!
//! The ordering that matters: every window the recorder owns must exist *before*
//! the first capture starts. Creating or showing a topmost window while Desktop
//! Duplication is live invalidates the session, which truncates the output to
//! under a second.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Instant;

use crate::encoder::{self, Encoder};
use crate::ffmpeg;
use crate::marks::Marks;
use crate::source::{Capture, Engine, Source, is_recordable};
use crate::take::Take;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    NoFfmpeg,
    /// The requested hardware encoder is present but cannot encode.
    Encoder(String),
    /// A source could not be resolved (window not found, bad crop, …).
    Source(String),
    Marks(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoFfmpeg => write!(f, "ffmpeg not found on PATH"),
            Error::Encoder(m) | Error::Source(m) => write!(f, "{m}"),
            Error::Marks(m) => write!(f, "cannot start MouseShow: {m}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// What to record. Everything the caller decides before a session exists.
#[derive(Debug)]
pub struct Settings {
    pub sources: Vec<Source>,
    pub out_root: PathBuf,
    /// Session-wide quality; a source with `quality: Some(q)` overrides it.
    pub quality: u8,
    /// `None` picks the best available encoder.
    pub encoder: Option<Encoder>,
    /// Draw click ripples and drag trails (MouseShow).
    pub marks: bool,
}

/// Everything resolved before the first frame: binary locations, capture engine,
/// encoder. `start` consumes it so a prepared session can only be started once.
pub struct Prepared {
    ffmpeg: PathBuf,
    probe: PathBuf,
    engine: Engine,
    encoder: Encoder,
    settings: Settings,
    /// Milliseconds per prepare phase (`locate`, `encoder`, `engine`). The
    /// click-to-countdown gap is judged by these numbers, not by feel.
    timings: Vec<(&'static str, u64)>,
    /// False when the engine was picked without a cached DDA verdict: the take
    /// runs on GDI while the probe warms up in the background.
    engine_probed: bool,
}

impl Prepared {
    pub fn engine(&self) -> Engine {
        self.engine
    }

    pub fn timings(&self) -> &[(&'static str, u64)] {
        &self.timings
    }

    pub fn engine_probed(&self) -> bool {
        self.engine_probed
    }

    pub fn encoder(&self) -> Encoder {
        self.encoder
    }

    /// Start every source. Returns once all encoders have been spawned — the
    /// first frame may still be a few hundred milliseconds away.
    pub fn start(self) -> Result<Recording> {
        // Overlay before capture (see the module comment) and before the take
        // folder: an overlay that cannot start must not leave an empty take
        // behind (DESIGN §8 recovery table).
        let marks = if self.settings.marks {
            Some(crate::marks::start().map_err(|e| Error::Marks(e.to_string()))?)
        } else {
            None
        };

        let take = Take::create(
            &self.settings.out_root,
            self.encoder,
            &self.settings.sources,
        )?;
        let dir = take.dir.clone();

        // Assembled before the spawn loop: if a later source fails to spawn,
        // dropping it shuts down the encoders that did start.
        let mut recording = Recording {
            take,
            running: Vec::new(),
            spawn_ms: Vec::new(),
            marks,
            probe: self.probe,
            started: Instant::now(),
        };

        for src in &self.settings.sources {
            // A source can stop resolving between prepare and start (a window
            // closed during the countdown). Same cleanup as a failed spawn:
            // nothing was recorded, so the folder must not survive.
            let (input, extra_filter) = match src.input_args(self.engine) {
                Ok(pair) => pair,
                Err(e) => {
                    drop(recording);
                    let _ = std::fs::remove_dir_all(&dir);
                    return Err(Error::Source(e));
                }
            };
            let out = recording.take.path_for(&src.id);
            let log = recording.take.dir.join(format!("{}.log", src.id));

            let mut args: Vec<String> = vec!["-hide_banner".into(), "-y".into()];
            args.extend(input);
            if let Some(filter) = extra_filter {
                args.extend(["-vf".into(), filter]);
            }
            args.extend(
                self.encoder
                    .args(effective_quality(src, self.settings.quality)),
            );
            args.push(out.to_string_lossy().into_owned());

            let t = std::time::Instant::now();
            let child = match ffmpeg::spawn(&self.ffmpeg, &args, &log) {
                Ok(child) => child,
                Err(e) => {
                    // Nothing usable was recorded: stop the encoders that did
                    // start, then remove the folder so a failed start does not
                    // leave a take with no manifest and no frames.
                    drop(recording);
                    let _ = std::fs::remove_dir_all(&dir);
                    return Err(e.into());
                }
            };
            recording
                .spawn_ms
                .push((src.id.clone(), t.elapsed().as_millis() as u64));
            recording.running.push(Running {
                id: src.id.clone(),
                capture: src.capture.clone(),
                child,
                log,
                out,
            });
        }

        // Best-effort so a crash mid-take still leaves a manifest on disk;
        // stop() rewrites it with the final durations and reports errors.
        let _ = recording.take.save_manifest();

        Ok(recording)
    }
}

/// A source's own quality when pinned, the session-wide default otherwise.
fn effective_quality(src: &Source, global: u8) -> u8 {
    src.quality.unwrap_or(global)
}

struct Running {
    id: String,
    /// Kept so `snapshot` can say whether this source is still worth recording
    /// at all — see `LiveSource::usable`.
    capture: Capture,
    child: Child,
    log: PathBuf,
    out: PathBuf,
}

/// A live recording. Dropping it always shuts the encoders down gracefully, so
/// a caller that forgets `stop` (or a window that closes mid-take) still gets
/// complete files instead of orphaned ffmpeg processes. Call `stop` to collect
/// the results.
pub struct Recording {
    take: Take,
    running: Vec<Running>,
    /// Milliseconds each encoder took to be spawned, in source order. This is
    /// the skew the capture starts with before any frame is written.
    spawn_ms: Vec<(String, u64)>,
    marks: Option<Marks>,
    probe: PathBuf,
    started: Instant,
}

/// Per-source snapshot for a UI that polls while recording.
#[derive(Debug)]
pub struct LiveSource {
    pub id: String,
    /// `false` once ffmpeg has exited — usually a dead device or a full disk.
    pub alive: bool,
    /// `false` once the window behind this source is gone, hidden or cloaked:
    /// what it is recording is no longer what the user picked.
    pub usable: bool,
    pub bytes: u64,
}

impl Recording {
    pub fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    pub fn spawn_timings(&self) -> &[(String, u64)] {
        &self.spawn_ms
    }

    pub fn dir(&self) -> &Path {
        &self.take.dir
    }

    pub fn path_for(&self, id: &str) -> PathBuf {
        self.take.path_for(id)
    }

    /// Poll every source. Cheap enough for a 4 Hz UI tick.
    pub fn snapshot(&mut self) -> Vec<LiveSource> {
        self.running
            .iter_mut()
            .map(|r| LiveSource {
                id: r.id.clone(),
                alive: matches!(r.child.try_wait(), Ok(None)),
                usable: is_recordable(&r.capture),
                bytes: std::fs::metadata(&r.out).map(|m| m.len()).unwrap_or(0),
            })
            .collect()
    }

    /// Stop one source, leaving the rest of the take running.
    ///
    /// A window can close or be parked on another virtual desktop mid-take, and
    /// ffmpeg would go on recording whatever is behind it. The file is finished
    /// the same way `stop` finishes it, and is still reported in the summary —
    /// what it did capture is worth keeping. `false` means there was nothing
    /// left to stop.
    pub fn stop_source(&mut self, id: &str) -> bool {
        let Some(r) = self.running.iter_mut().find(|r| r.id == id) else {
            return false;
        };
        if !matches!(r.child.try_wait(), Ok(None)) {
            return false;
        }
        ffmpeg::stop(&mut r.child);
        true
    }

    /// Finish every file properly and collect the results.
    pub fn stop(mut self) -> Result<Finished> {
        // Sources are independent processes and files: stop them and probe
        // their durations in parallel. Serial ffmpeg shutdown plus one ffprobe
        // per source was the bulk of stop latency.
        let probe = self.probe.clone();
        let collected: Vec<Collected> = std::thread::scope(|scope| {
            let jobs: Vec<_> = self
                .running
                .iter_mut()
                .map(|r| {
                    let probe = &probe;
                    scope.spawn(move || {
                        ffmpeg::stop(&mut r.child);

                        let bytes = std::fs::metadata(&r.out).map(|m| m.len()).unwrap_or(0);
                        if bytes == 0 {
                            let tail = ffmpeg::tail(&r.log, 3);
                            Collected {
                                id: r.id.clone(),
                                bytes: 0,
                                seconds: None,
                                error: Some(if tail.is_empty() {
                                    "ffmpeg exited without writing a frame or a log line".into()
                                } else {
                                    tail
                                }),
                            }
                        } else {
                            Collected {
                                id: r.id.clone(),
                                bytes,
                                seconds: ffmpeg::duration(probe, &r.out),
                                error: None,
                            }
                        }
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|j| j.join().expect("stop worker panicked"))
                .collect()
        });

        let mut sources = Vec::with_capacity(collected.len());
        for c in collected {
            if c.bytes > 0 {
                self.take.set_duration(&c.id, c.seconds);
            }
            sources.push(Outcome {
                id: c.id,
                seconds: c.seconds,
                bytes: c.bytes,
                error: c.error,
            });
        }

        self.take.manifest.duration = self
            .take
            .manifest
            .sources
            .iter()
            .filter_map(|s| s.duration)
            .reduce(f64::max);
        // Best effort: the .mkv files are complete and the journal is what the
        // panel lists, so a disk hiccup writing metadata must not turn a
        // finished take into a failed one (the caller would then record it as
        // `failed:1 · 0 bytes`, and the user might delete good footage).
        let _ = self.take.save_manifest();

        Ok(Finished {
            dir: self.take.dir.clone(),
            duration: self.take.manifest.duration,
            sources,
        })
    }
}

struct Collected {
    id: String,
    bytes: u64,
    seconds: Option<f64>,
    error: Option<String>,
}

impl Drop for Recording {
    fn drop(&mut self) {
        drop(self.marks.take());
        for r in &mut self.running {
            ffmpeg::stop(&mut r.child);
        }
    }
}

/// One source's result after a take finishes.
#[derive(Debug)]
pub struct Outcome {
    pub id: String,
    pub seconds: Option<f64>,
    pub bytes: u64,
    /// Why nothing was written: the source's log tail, or a stand-in line when
    /// even the log came back empty.
    pub error: Option<String>,
}

impl Outcome {
    /// A source counts as recorded only if it produced bytes. A zero-length file
    /// means ffmpeg exited before the first frame, which is a failure even when
    /// the log tail came back empty.
    pub fn ok(&self) -> bool {
        self.error.is_none() && self.bytes > 0
    }

    pub fn megabytes(&self) -> f64 {
        self.bytes as f64 / (1024.0 * 1024.0)
    }
}

/// A finished take, ready to be reported or shown in a UI.
#[derive(Debug)]
pub struct Finished {
    pub dir: PathBuf,
    pub sources: Vec<Outcome>,
    /// Longest source, so the take's length is the safe one to cut against.
    pub duration: Option<f64>,
}

impl Finished {
    pub fn failed(&self) -> impl Iterator<Item = &Outcome> {
        self.sources.iter().filter(|s| !s.ok())
    }

    /// Difference between the longest and shortest source.
    ///
    /// This is the number that matters for editing: two files with the same
    /// container duration can still cover different real time. Anything above
    /// ~0.2s means the tracks will slide against each other.
    pub fn delta(&self) -> Option<f64> {
        let mut ds = self.sources.iter().filter_map(|s| s.seconds);
        let first = ds.next()?;
        let (mut min, mut max) = (first, first);
        for d in ds {
            min = min.min(d);
            max = max.max(d);
        }
        Some(max - min)
    }
}

/// Resolve the toolchain and the capture engine.
pub struct Session;

impl Session {
    pub fn prepare(settings: Settings) -> Result<Prepared> {
        let t0 = std::time::Instant::now();
        validate_sources(&settings.sources)?;
        let ffmpeg = ffmpeg::locate().ok_or(Error::NoFfmpeg)?;
        let probe = ffmpeg::ffprobe(&ffmpeg);
        let locate_ms = t0.elapsed().as_millis() as u64;

        let t1 = std::time::Instant::now();
        let encoder = match settings.encoder {
            Some(e) => {
                if e.is_hardware() && !encoder::works(&ffmpeg, e) {
                    return Err(Error::Encoder(format!(
                        "{} does not work on this machine",
                        e.name()
                    )));
                }
                e
            }
            None => encoder::pick(&ffmpeg),
        };
        let encoder_ms = t1.elapsed().as_millis() as u64;

        let t2 = std::time::Instant::now();

        // Probe with the geometry that will actually be captured: Desktop
        // Duplication can open at one size and be refused at another. Every
        // source is checked, not just the first — the engine is session-wide,
        // so one unsupported rectangle would fail the whole take at spawn.
        // Verdicts are cached per rectangle, so the preheat pays for this.
        let rects: Vec<crate::source::ScreenRect> = settings
            .sources
            .iter()
            .filter_map(|s| s.screen_rect())
            .collect();
        // The DDA probe is ~2.6s of D3D init and cannot be made cheaper (a
        // 64×64 probe costs the same as 4K), so it never runs here. Cached
        // verdicts decide the engine; anything still unknown starts a
        // background probe and this take uses GDI instead of freezing the
        // click. The next take picks up the verdict.
        let mut engine_probed = true;
        let engine = engine_override().unwrap_or_else(|| {
            if rects.is_empty() {
                // Camera-only takes never consult the engine.
                return Engine::Gdi;
            }
            let mut verdicts = Vec::with_capacity(rects.len());
            for r in &rects {
                match ffmpeg::dda_verdict(&ffmpeg, *r) {
                    Some(v) => verdicts.push(v),
                    None => {
                        engine_probed = false;
                        verdicts.push(false);
                        let ff = ffmpeg.clone();
                        let rect = *r;
                        std::thread::spawn(move || {
                            ffmpeg::dda_available(&ff, rect);
                        });
                    }
                }
            }
            if verdicts.iter().all(|v| *v) {
                Engine::Dda
            } else {
                Engine::Gdi
            }
        });

        Ok(Prepared {
            ffmpeg,
            probe,
            engine,
            encoder,
            settings,
            timings: vec![
                ("locate", locate_ms),
                ("encoder", encoder_ms),
                ("engine", t2.elapsed().as_millis() as u64),
            ],
            engine_probed,
        })
    }
}

/// Reject source lists that would misbehave on disk before anything starts.
///
/// `id` becomes a file name (`<id>.mkv`, `<id>.log`) and is unique per take:
/// a separator or `..` would let a hand-edited config write outside the take
/// folder, and a duplicate would point two encoders at one file.
fn validate_sources(sources: &[Source]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for s in sources {
        let safe = !s.id.is_empty()
            && s.id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !safe {
            return Err(Error::Source(format!(
                "invalid source id {:?}: use letters, digits, '-' or '_'",
                s.id
            )));
        }
        if s.fps == 0 {
            return Err(Error::Source(format!("source {:?} has fps 0", s.id)));
        }
        if !seen.insert(s.id.as_str()) {
            return Err(Error::Source(format!("duplicate source id {:?}", s.id)));
        }
    }
    Ok(())
}

/// `TAKEKIT_ENGINE=dda|gdi` forces the desktop capture engine.
///
/// A diagnostic: the probe above picks Desktop Duplication wherever it works,
/// so on such a machine the GDI fallback would otherwise never be exercised.
/// An unknown value is ignored, like an unset variable.
fn engine_override() -> Option<Engine> {
    engine_from(&std::env::var("TAKEKIT_ENGINE").ok()?)
}

fn engine_from(spec: &str) -> Option<Engine> {
    match spec.to_ascii_lowercase().as_str() {
        "dda" => Some(Engine::Dda),
        "gdi" => Some(Engine::Gdi),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Capture;

    fn outcome(id: &str, seconds: Option<f64>, bytes: u64) -> Outcome {
        Outcome {
            id: id.into(),
            seconds,
            bytes,
            error: None,
        }
    }

    #[test]
    fn the_engine_override_names_the_two_engines() {
        assert_eq!(engine_from("dda"), Some(Engine::Dda));
        assert_eq!(engine_from("GDI"), Some(Engine::Gdi));
        assert_eq!(engine_from(""), None);
        assert_eq!(engine_from("cuda"), None);
    }

    fn source(id: &str) -> Source {
        Source::new(id, Capture::Desktop { rect: None }, 30)
    }

    #[test]
    fn source_ids_have_to_be_usable_as_file_names() {
        assert!(validate_sources(&[source("screen")]).is_ok());
        assert!(validate_sources(&[source("win-2")]).is_ok());
        // A separator or dot-dot would write outside the take folder.
        assert!(validate_sources(&[source("../escape")]).is_err());
        assert!(validate_sources(&[source("a/b")]).is_err());
        assert!(validate_sources(&[source("a\\b")]).is_err());
        assert!(validate_sources(&[source("")]).is_err());
    }

    #[test]
    fn two_sources_cannot_share_an_id() {
        let err = validate_sources(&[source("screen"), source("screen")]).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn a_zero_frame_rate_is_rejected() {
        let mut s = source("screen");
        s.fps = 0;
        assert!(validate_sources(&[s]).is_err());
    }

    #[test]
    fn delta_is_zero_for_a_single_source() {
        let f = Finished {
            dir: PathBuf::new(),
            sources: vec![outcome("win", Some(60.0), 1)],
            duration: Some(60.0),
        };
        assert_eq!(f.delta(), Some(0.0));
    }

    #[test]
    fn delta_reports_the_widest_gap() {
        let f = Finished {
            dir: PathBuf::new(),
            sources: vec![
                outcome("win", Some(60.0), 1),
                outcome("cam", Some(60.033), 1),
                outcome("mic", Some(59.7), 1),
            ],
            duration: Some(60.033),
        };
        let d = f.delta().unwrap();
        assert!((d - 0.333).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn quality_prefers_the_source_override() {
        let mut pinned = Source::new("win", Capture::Desktop { rect: None }, 30);
        pinned.quality = Some(16);
        assert_eq!(effective_quality(&pinned, 20), 16);

        let inherited = Source::new("cam", Capture::Desktop { rect: None }, 30);
        assert_eq!(effective_quality(&inherited, 20), 20);
    }

    #[test]
    fn delta_ignores_sources_that_never_wrote() {
        let f = Finished {
            dir: PathBuf::new(),
            sources: vec![outcome("win", Some(60.0), 1), outcome("cam", None, 0)],
            duration: Some(60.0),
        };
        assert_eq!(f.delta(), Some(0.0));
        assert_eq!(f.failed().count(), 1);
    }
}
