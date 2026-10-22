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
use crate::source::{Backend, Capture, Engine, Source, is_recordable};
use crate::take::Take;
use takekit_media::{CameraCapture, MediaFrame, ScreenCapture, ScreenTarget, TakeClock};

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
    /// ffmpeg children, or the OS capture APIs in-process.
    pub backend: Backend,
}

/// Everything resolved before the first frame: binary locations, capture engine,
/// encoder. `start` consumes it so a prepared session can only be started once.
pub struct Prepared {
    backend: Backend,
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

    /// Which machinery this take runs on.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// What to call the encoder in a UI. The Windows backend encodes through
    /// Media Foundation, so an ffmpeg encoder name would be a lie.
    pub fn encoder_label(&self) -> &'static str {
        match self.backend {
            Backend::Windows => "mf-h264",
            Backend::Ffmpeg => self.encoder.name(),
        }
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
        if self.backend == Backend::Windows {
            return self.start_windows();
        }
        self.start_ffmpeg()
    }

    /// Start every source through the OS capture APIs. No child processes: the
    /// capture, the encoder and the muxer are all this process.
    fn start_windows(self) -> Result<Recording> {
        let marks = if self.settings.marks {
            Some(crate::marks::start().map_err(|e| Error::Marks(e.to_string()))?)
        } else {
            None
        };

        let mut take = Take::create(
            &self.settings.out_root,
            "mp4",
            self.encoder,
            &self.settings.sources,
        )?;
        // The manifest's encoder field would otherwise carry the ffmpeg pick,
        // which this take never used.
        take.manifest.encoder = "mf-h264".into();
        take.manifest.hardware = false;
        let dir = take.dir.clone();

        // One origin for every source, taken before the first capture opens:
        // a camera whose first frame arrives 300ms late must say so in its
        // own timestamps, not shift silently against the screen.
        let clock = TakeClock::start();
        // Set once when the take stops, so every file covers exactly the same
        // stretch of the take's timeline. Writers that end on their own clock
        // disagree by however long their drain took — a frame or two of skew
        // between channels that otherwise line up perfectly.
        let end_hint = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));

        let mut recording = Recording {
            take,
            running: Vec::new(),
            media: Vec::new(),
            media_clock: Some((clock, end_hint.clone())),
            spawn_ms: Vec::new(),
            marks,
            probe: self.probe,
            started: Instant::now(),
        };

        for src in &self.settings.sources {
            let out = recording.take.path_for(&src.id);
            let (tx, rx) = std::sync::mpsc::channel::<MediaFrame>();
            let fps = src.fps.max(1);
            let bitrate = bitrate_for(effective_quality(src, self.settings.quality));

            // One writer thread per source, holding the Media Foundation
            // objects: they are not `Send`, and the capture must never block
            // on an encoder. Frames are written as they arrive, each keeping
            // its own capture time — the shared take clock is what puts them
            // on one timeline (see `takekit_media::writer` for how that
            // timeline reaches the file).
            let writer_out = out.clone();
            let writer_end = end_hint.clone();
            let writer_crop = src.capture.crop();
            let handle = std::thread::spawn(move || {
                write_frames(rx, writer_out, fps, bitrate, clock, writer_end, writer_crop)
            });

            let t_spawn = Instant::now();
            // A source that cannot start ends the whole take, same as on the
            // ffmpeg path: drop the sources that did start (their `Drop` stops
            // the captures so the writers drain and finalise), then remove the
            // folder — nothing usable was recorded.
            let started = match &src.capture {
                Capture::Desktop { rect, .. } => {
                    // WGC captures one output at a time — a "whole virtual
                    // desktop" does not exist for it. A remembered rect picks
                    // the monitor holding its centre (that is what the picker
                    // stored); no rect falls back to the first monitor, which
                    // is what this used to do unconditionally (2026-10-21 多屏).
                    let index = match rect {
                        Some(r) => takekit_media::monitor_index_for(r.x, r.y, r.w, r.h),
                        None => None,
                    }
                    .unwrap_or(1);
                    ScreenCapture::start(ScreenTarget::Monitor(index), src.fps, tx.clone(), clock)
                        .map(MediaCapture::Screen)
                }
                Capture::Window { hwnd, .. } => {
                    // No raise, and none needed: WGC keeps recording a window
                    // that sits behind others. That is the whole point of this
                    // backend for a window source.
                    ScreenCapture::start(ScreenTarget::Window(*hwnd), src.fps, tx.clone(), clock)
                        .map(MediaCapture::Screen)
                }
                Capture::Camera { device, .. } => match camera_link(device) {
                    Ok(link) => CameraCapture::start(&link, src.fps, tx.clone(), clock)
                        .map(MediaCapture::Camera),
                    Err(e) => Err(e),
                },
            };
            let handle_capture = match started {
                Ok(handle) => handle,
                Err(e) => {
                    drop(recording);
                    let _ = std::fs::remove_dir_all(&dir);
                    return Err(spawn_error(e));
                }
            };

            recording
                .spawn_ms
                .push((src.id.clone(), t_spawn.elapsed().as_millis() as u64));
            recording.media.push(MediaRunning {
                id: src.id.clone(),
                capture: src.capture.clone(),
                handle: Some(handle_capture),
                sender: Some(tx),
                writer: handle,
                out,
            });
        }

        let _ = recording.take.save_manifest();
        Ok(recording)
    }

    /// Start every source as an ffmpeg child — the long-standing path.
    fn start_ffmpeg(self) -> Result<Recording> {
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
            "mkv",
            self.encoder,
            &self.settings.sources,
        )?;
        let dir = take.dir.clone();

        // Assembled before the spawn loop: if a later source fails to spawn,
        // dropping it shuts down the encoders that did start.
        let mut recording = Recording {
            take,
            running: Vec::new(),
            media: Vec::new(),
            media_clock: None,
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

/// A running Windows-API source: the capture, the channel into its writer and
/// the writer thread itself.
struct MediaRunning {
    id: String,
    /// Kept so `snapshot` can say whether this source is still worth recording
    /// — the same role `Running.capture` plays for the ffmpeg ones.
    capture: Capture,
    handle: Option<MediaCapture>,
    sender: Option<std::sync::mpsc::Sender<MediaFrame>>,
    writer: std::thread::JoinHandle<std::result::Result<WriteReport, String>>,
    out: PathBuf,
}

impl MediaRunning {
    /// Stop one source mid-take, leaving the rest running.
    fn stop(&mut self) -> bool {
        if self.handle.is_none() {
            return false;
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.stop();
        }
        // Dropping the sender is what ends the writer: it drains what is left
        // in the channel, finalises the file and exits. Not joined here — the
        // UI asked for one source to stop, not for the take to pause on it.
        self.sender.take();
        true
    }

    /// Stop everything and report. Runs on its own thread inside `stop`.
    fn finish(&mut self) -> Collected {
        if let Some(handle) = self.handle.take() {
            let _ = handle.stop();
        }
        self.sender.take();
        // `JoinHandle::join` takes ownership, so the handle leaves `self`.
        let writer = std::mem::replace(
            &mut self.writer,
            // A placeholder nobody joins twice: this struct is consumed by
            // `finish`'s caller (`stop`'s scope), so the slot is never read.
            std::thread::spawn(|| {
                Ok(WriteReport {
                    seconds: 0.0,
                    frames: 0,
                    width: 0,
                    height: 0,
                })
            }),
        );
        let outcome = writer
            .join()
            .unwrap_or_else(|_| Err("the writer thread panicked".into()));
        let bytes = std::fs::metadata(&self.out).map(|m| m.len()).unwrap_or(0);
        // A source counts as recorded only if it produced bytes, exactly like
        // an ffmpeg one that exited without writing a frame.
        let (seconds, frames, size, error) = match outcome {
            Ok(report) if bytes > 0 => (
                Some(report.seconds),
                Some(report.frames),
                Some((report.width, report.height)),
                None,
            ),
            Ok(_) => (
                None,
                Some(0),
                None,
                Some("no frames were written".to_string()),
            ),
            Err(e) => (None, None, None, Some(e)),
        };
        Collected {
            id: self.id.clone(),
            bytes,
            seconds,
            frames,
            size,
            error,
        }
    }
}

/// The two capture handles a Windows-API source can hold. They are different
/// types reaching the same `stop`, which is all a take ever does to them.
enum MediaCapture {
    Screen(ScreenCapture),
    Camera(CameraCapture),
}

impl MediaCapture {
    fn stop(self) -> takekit_media::Result<()> {
        match self {
            MediaCapture::Screen(c) => c.stop(),
            MediaCapture::Camera(c) => c.stop(),
        }
    }
}

/// Drain frames into one MP4, then finalise it.
///
/// Frames are written as they arrive and keep their capture timestamps —
/// variable frame rate, which is why a still picture costs almost nothing. The
/// returned number is the seconds the file covers, taken from the stop time
/// the caller infers rather than from a frame that never arrived.
fn write_frames(
    rx: std::sync::mpsc::Receiver<MediaFrame>,
    out: PathBuf,
    fps: u32,
    bitrate: u32,
    clock: TakeClock,
    end_hint: std::sync::Arc<std::sync::atomic::AtomicI64>,
    crop: crate::source::Crop,
) -> std::result::Result<WriteReport, String> {
    // One writer loop for every caller (`write_stream` is where a frame
    // channel becomes a file); this path writes every frame the capture
    // delivered. The CLI's --cfr sampler is the same loop with `KeepLatest`.
    let report = takekit_media::writer::write_stream(
        rx,
        &out,
        takekit_media::writer::Encode {
            fps,
            bitrate,
            // WGC hands over the whole window or monitor; the edges the editor
            // picked are cut here, so a cropped source records the same picture
            // on this backend as it does through ffmpeg's region grab.
            crop: takekit_media::writer::CropBox {
                top: crop.top,
                bottom: crop.bottom,
                left: crop.left,
                right: crop.right,
            },
            sampling: takekit_media::writer::Sampling::EveryFrame,
        },
        clock,
        end_hint,
    )
    .map_err(|e| e.to_string())?;
    Ok(WriteReport {
        seconds: report.seconds,
        frames: report.frames,
        width: report.width,
        height: report.height,
    })
}

/// What one source's writer saw. `frames` is what the capture actually
/// delivered — the output has `duration x fps` slots either way, because the
/// encoder holds a frame across a pause. The difference between the two is how
/// much of the take is a held picture rather than a fresh one, which is the
/// only visible form a dropped capture update can take.
struct WriteReport {
    seconds: f64,
    frames: u64,
    /// The size the file was written at; `0 x 0` when nothing was written.
    width: u32,
    height: u32,
}

/// A camera's Media Foundation symbolic link, matched from the name the source
/// carries. The ffmpeg backend addresses cameras by DirectShow name; Media
/// Foundation has its own naming, so the panel's pick has to be translated.
fn camera_link(device: &str) -> std::result::Result<String, takekit_media::MediaError> {
    let cameras = takekit_media::devices::cameras()?;
    cameras
        .iter()
        .find(|c| c.name == device || c.link == device)
        .map(|c| c.link.clone())
        .ok_or_else(|| {
            takekit_media::MediaError::message(format!(
                "no camera \"{device}\" visible to Media Foundation (the ffmpeg backend sees more)"
            ))
        })
}

/// CRF-flavoured quality as the fixed bitrate Media Foundation's H.264 encoder
/// takes. Rough by design: hardware encoders do not do CRF, and the steps are
/// wide enough that the difference between them is what the knob promises.
fn bitrate_for(quality: u8) -> u32 {
    match quality {
        0..=14 => 20_000,
        15..=22 => 12_000,
        23..=30 => 8_000,
        _ => 5_000,
    }
}

/// Every spawn failure is a `Source` error: the caller knows what it asked to
/// record, and the capture's own words name the device that refused.
fn spawn_error(e: takekit_media::MediaError) -> Error {
    Error::Source(e.to_string())
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
    /// Sources captured in-process through the OS APIs. Empty unless the take
    /// runs on `Backend::Windows`.
    media: Vec<MediaRunning>,
    /// The take's own timeline. Kept so `stop` can latch one end instant for
    /// every channel; `None` on the ffmpeg path, where each child files its
    /// own clock.
    media_clock: Option<(TakeClock, std::sync::Arc<std::sync::atomic::AtomicI64>)>,
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

    /// How many sources this take is writing — the denominator a stop screen
    /// counts its checklist against.
    pub fn source_count(&self) -> usize {
        self.running.len() + self.media.len()
    }

    pub fn path_for(&self, id: &str) -> PathBuf {
        self.take.path_for(id)
    }

    /// Poll every source. Cheap enough for a 4 Hz UI tick.
    pub fn snapshot(&mut self) -> Vec<LiveSource> {
        let mut out: Vec<LiveSource> = self
            .running
            .iter_mut()
            .map(|r| LiveSource {
                id: r.id.clone(),
                alive: matches!(r.child.try_wait(), Ok(None)),
                usable: is_recordable(&r.capture),
                bytes: std::fs::metadata(&r.out).map(|m| m.len()).unwrap_or(0),
            })
            .collect();
        out.extend(self.media.iter_mut().map(|m| {
            LiveSource {
                id: m.id.clone(),
                // The writer thread is the source's whole pipeline: once it has
                // exited, the source is done — usually because the capture was
                // told to stop, occasionally because writing failed.
                alive: !m.writer.is_finished(),
                usable: is_recordable(&m.capture),
                bytes: std::fs::metadata(&m.out)
                    .map(|meta| meta.len())
                    .unwrap_or(0),
            }
        }));
        out
    }

    /// Stop one source, leaving the rest of the take running.
    ///
    /// A window can close or be parked on another virtual desktop mid-take, and
    /// ffmpeg would go on recording whatever is behind it. The file is finished
    /// the same way `stop` finishes it, and is still reported in the summary —
    /// what it did capture is worth keeping. `false` means there was nothing
    /// left to stop.
    pub fn stop_source(&mut self, id: &str) -> bool {
        if let Some(r) = self.running.iter_mut().find(|r| r.id == id) {
            if !matches!(r.child.try_wait(), Ok(None)) {
                return false;
            }
            ffmpeg::stop(&mut r.child);
            return true;
        }
        if let Some(m) = self.media.iter_mut().find(|m| m.id == id) {
            return m.stop();
        }
        false
    }

    /// Finish every file properly and collect the results.
    pub fn stop(self) -> Result<Finished> {
        self.stop_with(|_| {})
    }

    /// Finish every file properly and collect the results, telling
    /// `on_source` about each one the moment its file lands.
    ///
    /// The finalise itself stays parallel — serial ffmpeg shutdown plus one
    /// ffprobe per source was the bulk of stop latency, and the Windows-API
    /// writers drain in parallel with them. The per-source checklist a stop
    /// screen wants is therefore an *event stream* (this callback), not a
    /// queue to work through (`stop_next` from the old plan would serialise
    /// the very thing that used to make stop feel slow).
    ///
    /// The callback is `Fn`, not `FnMut`: several workers finish at the same
    /// time and all of them report from their own thread.
    pub fn stop_with(mut self, on_source: impl Fn(&Outcome) + Sync) -> Result<Finished> {
        // The moment the user pressed stop is the end of the take for every
        // channel alike. Latched before anything is torn down; writers hold
        // their last frame to this instant so the files end together instead
        // of by however long each drain happened to take.
        if let Some((clock, end)) = &self.media_clock {
            end.store(clock.now_100ns(), std::sync::atomic::Ordering::SeqCst);
        }
        // Sources are independent processes and files: stop them and probe
        // their durations in parallel. Serial ffmpeg shutdown plus one ffprobe
        // per source was the bulk of stop latency.
        let probe = self.probe.clone();
        let collected: Vec<Outcome> = std::thread::scope(|scope| {
            let mut jobs: Vec<_> = self
                .running
                .iter_mut()
                .map(|r| {
                    let probe = &probe;
                    let on_source = &on_source;
                    scope.spawn(move || {
                        ffmpeg::stop(&mut r.child);

                        let bytes = std::fs::metadata(&r.out).map(|m| m.len()).unwrap_or(0);
                        let outcome = if bytes == 0 {
                            let tail = ffmpeg::tail(&r.log, 3);
                            Collected {
                                id: r.id.clone(),
                                bytes: 0,
                                seconds: None,
                                frames: None,
                                size: None,
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
                                frames: None,
                                size: None,
                                error: None,
                            }
                        }
                        .into_outcome();
                        on_source(&outcome);
                        outcome
                    })
                })
                .collect();
            // The Windows-API sources finalise in parallel with the ffmpeg
            // ones: stopping a capture waits for its frames to stop arriving,
            // and doing that serially would be stop latency the user feels.
            for m in self.media.iter_mut() {
                let on_source = &on_source;
                jobs.push(scope.spawn(move || {
                    let outcome = m.finish().into_outcome();
                    on_source(&outcome);
                    outcome
                }));
            }
            jobs.into_iter()
                .map(|j| j.join().expect("stop worker panicked"))
                .collect()
        });

        let mut sources = Vec::with_capacity(collected.len());
        for o in collected {
            if o.bytes > 0 {
                self.take.set_duration(&o.id, o.seconds);
                self.take.set_frames(&o.id, o.frames);
            }
            sources.push(o);
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
    /// Frames the capture actually delivered. `None` when the source failed
    /// before counting could start; the output's slot count is
    /// `seconds x fps` regardless, since a held frame fills the gap.
    frames: Option<u64>,
    /// The size the file was written at. `None` when nothing was written or
    /// the backend cannot say (the ffmpeg path reports through ffprobe's
    /// duration only).
    size: Option<(u32, u32)>,
    error: Option<String>,
}

impl Collected {
    fn into_outcome(self) -> Outcome {
        Outcome {
            id: self.id,
            seconds: self.seconds,
            bytes: self.bytes,
            frames: self.frames,
            size: self.size,
            error: self.error,
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        drop(self.marks.take());
        for r in &mut self.running {
            ffmpeg::stop(&mut r.child);
        }
        // The same guarantee the ffmpeg children get: a caller that forgets
        // `stop` still leaves complete files. Each writer drains its channel
        // and finalises once its capture is stopped and its sender dropped.
        for m in &mut self.media {
            if let Some(handle) = m.handle.take() {
                let _ = handle.stop();
            }
            m.sender.take();
        }
    }
}

/// One source's result after a take finishes.
#[derive(Debug)]
pub struct Outcome {
    pub id: String,
    pub seconds: Option<f64>,
    pub bytes: u64,
    /// Frames the capture delivered (`takekit-media` sources; `None` on the
    /// ffmpeg path, where the child does not report). Compare against
    /// `seconds x fps` to see how much of the take is a held picture.
    pub frames: Option<u64>,
    /// The size the file was written at (`takekit-media` sources; `None` on
    /// the ffmpeg path). What a progress checklist shows as the resolution.
    pub size: Option<(u32, u32)>,
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
        if settings.backend == Backend::Windows {
            return Self::prepare_windows(settings, t0);
        }
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
            backend: Backend::Ffmpeg,
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

impl Session {
    /// Prepare a Windows-API take. There is no ffmpeg to find, no encoder to
    /// pick and no engine to probe — the OS does all three jobs. The only real
    /// work is checking every source can still be addressed, which turns a
    /// window closed during the countdown into an error the panel shows
    /// before the veil comes up, not a take of nothing afterwards.
    fn prepare_windows(settings: Settings, t0: std::time::Instant) -> Result<Prepared> {
        for src in &settings.sources {
            match &src.capture {
                Capture::Desktop { .. } => {
                    // Accepted as-is: `start_windows` resolves a remembered
                    // `rect` to the monitor holding its centre, and falls
                    // back to the first monitor only when there is none
                    // (2026-10-21 多屏) — so the rect picks a monitor, but
                    // its extent does not apply: WGC captures one whole
                    // output, not that region. The four crop edges *do*
                    // apply — they are cut out of the captured frame by the
                    // writer — which is what the panel's editor writes.
                }
                Capture::Window { hwnd, title, .. } => {
                    if let Err(e) = crate::source::window_by_hwnd(*hwnd) {
                        return Err(Error::Source(format!("{title}: {e}")));
                    }
                }
                Capture::Camera { device, .. } => {
                    camera_link(device).map_err(spawn_error)?;
                }
            }
        }
        Ok(Prepared {
            backend: Backend::Windows,
            ffmpeg: PathBuf::new(),
            probe: PathBuf::new(),
            // Both unused on this path: the engine only steers ffmpeg input
            // flags, and the manifest names the encoder `mf-h264`.
            engine: Engine::Gdi,
            encoder: crate::encoder::Encoder::X264,
            settings,
            timings: vec![("validate", t0.elapsed().as_millis() as u64)],
            engine_probed: true,
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
            frames: None,
            size: None,
            error: None,
        }
    }

    /// The stop checklist shows resolutions, so the size a writer reported
    /// must survive the trip into the outcome (and stay `None` when there is
    /// nothing to show).
    #[test]
    fn collected_becomes_an_outcome_with_its_size() {
        let sized = Collected {
            id: "a".into(),
            bytes: 10,
            seconds: Some(1.0),
            frames: Some(30),
            size: Some((1920, 1080)),
            error: None,
        }
        .into_outcome();
        assert_eq!(sized.size, Some((1920, 1080)));

        let blank = Collected {
            id: "b".into(),
            bytes: 0,
            seconds: None,
            frames: None,
            size: None,
            error: Some("boom".into()),
        }
        .into_outcome();
        assert_eq!(blank.size, None);
        assert_eq!(blank.error.as_deref(), Some("boom"));
    }

    #[test]
    fn the_engine_override_names_the_two_engines() {
        assert_eq!(engine_from("dda"), Some(Engine::Dda));
        assert_eq!(engine_from("GDI"), Some(Engine::Gdi));
        assert_eq!(engine_from(""), None);
        assert_eq!(engine_from("cuda"), None);
    }

    fn source(id: &str) -> Source {
        Source::new(id, Capture::desktop(None), 30)
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
        let mut pinned = Source::new("win", Capture::desktop(None), 30);
        pinned.quality = Some(16);
        assert_eq!(effective_quality(&pinned, 20), 16);

        let inherited = Source::new("cam", Capture::desktop(None), 30);
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
