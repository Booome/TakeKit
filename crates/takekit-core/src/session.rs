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

use crate::diagnose;
use crate::encoder::{self, Encoder};
use crate::ffmpeg;
use crate::marks::Marks;
use crate::source::{Backend, Capture, Engine, Source, is_recordable};
use crate::take::Take;
use takekit_media::writer::Origin;
use takekit_media::{
    CameraCapture, LoopbackCapture, LoopbackTarget, MediaFrame, MicCapture, MuxSink, ScreenCapture,
    ScreenTarget, TakeClock,
};

pub type Result<T> = std::result::Result<T, Error>;

/// `Manifest::start_clock` values — what the offsets in a take were measured
/// against. Two strings rather than an enum because a manifest is read by
/// people and by other programs, and both benefit from a word they can match
/// on without a schema.
const START_BY_FRAMES: &str = "frames";
const START_BY_SPAWN: &str = "spawn";

/// How long a mid-take add waits for its first sample before reporting
/// `start: null` and letting the file say so when it closes.
///
/// One frame is ~33ms; a camera can take the better part of a second to open.
/// Waiting here is what lets `take.json` name the offset immediately, and it
/// is bounded so a device that never delivers cannot hold the panel's
/// recording lock for longer than this.
const ADD_FIRST_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

#[derive(Debug)]
pub enum Error {
    NoFfmpeg,
    /// The requested hardware encoder is present but cannot encode.
    Encoder(String),
    /// A source could not be resolved (window not found, bad crop, …).
    Source(String),
    /// A quality value outside the domain every encoder shares — refused here
    /// because ffmpeg only says so after the take folder exists.
    Quality(String),
    Marks(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoFfmpeg => write!(f, "ffmpeg not found on PATH"),
            Error::Encoder(m) | Error::Source(m) | Error::Quality(m) => write!(f, "{m}"),
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
    /// Mouse effects: draw click ripples and drag trails (MouseShow).
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
        // Every `start` in this take comes from a file's own first sample, read
        // off the one clock below — the most exact answer a manifest can hold.
        take.manifest.start_clock = Some(START_BY_FRAMES.into());
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
            clock,
            end_hint,
            backend: Backend::Windows,
            // Unused on this path: the OS captures and encodes, so there is no
            // binary to spawn. `probe` is still worth having — it is what
            // verifies every finished file's length against the manifest.
            ffmpeg: PathBuf::new(),
            probe: self.probe,
            encoder: self.encoder,
            quality: self.settings.quality,
            engine: self.engine,
            spawn_ms: Vec::new(),
            marks,
        };

        for src in &self.settings.sources {
            let t_spawn = Instant::now();
            // A source that cannot start ends the whole take and leaves no
            // folder behind: nothing usable was recorded. `start_media_source`
            // starts every channel of this source; a failure there is the same
            // failure the spawn loop used to report inline.
            if let Err(e) = recording.start_media_source(src, Origin::Take) {
                recording.shutdown_writers();
                drop(recording);
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
            recording
                .spawn_ms
                .push((src.id.clone(), t_spawn.elapsed().as_millis() as u64));
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

        let mut take = Take::create(
            &self.settings.out_root,
            "mkv",
            self.encoder,
            &self.settings.sources,
        )?;
        // Each child files its own clock, so the best this backend can say
        // about a source's place on the timeline is when the child was
        // launched — see `Manifest::start_clock`.
        take.manifest.start_clock = Some(START_BY_SPAWN.into());
        let dir = take.dir.clone();

        // The take's clock. Sound sources are recorded in-process even here
        // (ffmpeg has no WASAPI loopback), and this is what puts their
        // timestamps on the same timeline as everything else.
        let clock = TakeClock::start();
        let end_hint = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));

        // Assembled before the spawn loop: if a later source fails to spawn,
        // dropping it shuts down the encoders that did start.
        let mut recording = Recording {
            take,
            running: Vec::new(),
            media: Vec::new(),
            clock,
            end_hint,
            backend: Backend::Ffmpeg,
            ffmpeg: self.ffmpeg,
            probe: self.probe,
            encoder: self.encoder,
            quality: self.settings.quality,
            engine: self.engine,
            spawn_ms: Vec::new(),
            marks,
        };

        for src in &self.settings.sources {
            let t = std::time::Instant::now();
            // Sound is never a child process (ffmpeg has no WASAPI loopback);
            // everything with a picture is.
            let started = if src.capture.is_audio() {
                recording.start_media_source(src, Origin::Take).map(|_| ())
            } else {
                recording.start_ffmpeg_source(src).map(|_| ())
            };
            if let Err(e) = started {
                // A take that *chose* DDA and then could not start a desktop
                // source is the one signal that distinguishes a stale verdict
                // from a good one. Nothing else ever re-asks: the rectangle is
                // unchanged, so the cache keeps saying yes and every later take
                // fails the same way. Forgetting it costs 2.6s on a background
                // thread and one take on GDI.
                if recording.engine == Engine::Dda
                    && let Some(r) = src.screen_rect()
                {
                    ffmpeg::dda_forget(&recording.ffmpeg, r);
                }
                // Nothing usable was recorded: stop the encoders that did
                // start, then remove the folder so a failed start does not
                // leave a take with no manifest and no frames.
                recording.shutdown_writers();
                drop(recording);
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
            recording
                .spawn_ms
                .push((src.id.clone(), t.elapsed().as_millis() as u64));
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

/// Starting one source against a take that is already under way.
///
/// Both the take's first frame and a source added ten minutes in come through
/// here, which is the point: the ordering rules (capture before writer, writer
/// before capture, who reports the first sample) are stated once.
impl Recording {
    fn now_secs(&self) -> f64 {
        self.clock.now_100ns() as f64 / 10_000_000.0
    }

    /// Start one source through the OS capture APIs.
    ///
    /// Sound takes a different road than the picture: the capture reports its
    /// format and the writer opens an AAC file behind it. A source that records
    /// sound with its picture gets one file with both tracks — the picture
    /// through WGC, the sound through WASAPI's loopback, one sink writer behind
    /// the pair (see `start_av`).
    ///
    /// Returns the channel a `FirstSample` origin announces its first take-time
    /// on, and `None` for the `Take` origin, whose file begins at zero by
    /// definition. Nothing here decides what a failure means — the take's start
    /// throws the folder away, a mid-take add rolls back one row — so the caller
    /// does.
    fn start_media_source(
        &mut self,
        src: &Source,
        origin: Origin,
    ) -> Result<Option<std::sync::mpsc::Receiver<i64>>> {
        let out = self.take.path_for(&src.id);
        let fps = src.fps.max(1);
        let quality = effective_quality(src, self.quality);
        let bitrate = bitrate_for(quality);
        let audio_kbps = audio_bitrate_for(quality);

        // Only a source that joins a running take has a first sample worth
        // announcing: one that starts with the take already knows it begins at
        // zero. `sync_channel(1)` so the writer's `try_send` never queues.
        let (first, watch) = match origin {
            Origin::Take => (None, None),
            Origin::FirstSample => {
                let (tx, rx) = std::sync::mpsc::sync_channel::<i64>(1);
                (Some(tx), Some(rx))
            }
        };

        if src.capture.is_audio() {
            let (handle_capture, sender, writer) = start_audio_media(
                src,
                out.clone(),
                self.clock.clone(),
                audio_kbps,
                first.clone(),
            )?;
            self.remember(src, handle_capture, sender, None, writer, out, origin);
            return Ok(watch);
        }

        if src.audio {
            let (handle_capture, sender, sound, writer) = start_av(
                src,
                out.clone(),
                self.clock.clone(),
                self.end_hint.clone(),
                bitrate,
                audio_kbps,
                origin,
                first.clone(),
            )?;
            self.remember(
                src,
                handle_capture,
                sender,
                Some(sound),
                writer,
                out,
                origin,
            );
            return Ok(watch);
        }

        // Bounded, and fed through `offer` by the captures: the writer
        // must never make the capture wait, and an unbounded queue would
        // turn "the writer is behind" into unbounded memory growth (a 4K
        // BGRA frame is about 33 MB). A full queue drops the frame — the
        // encoder holds the previous picture across the gap.
        let (tx, rx) = std::sync::mpsc::sync_channel::<MediaFrame>(takekit_media::VIDEO_QUEUE);

        // One writer thread per source, holding the Media Foundation
        // objects: they are not `Send`, and the capture must never block
        // on an encoder. Frames are written as they arrive, each keeping
        // its own capture time — the shared take clock is what puts them
        // on one timeline (see `takekit_media::writer` for how that
        // timeline reaches the file).
        let job = FrameJob {
            out: out.clone(),
            fps,
            bitrate,
            clock: self.clock.clone(),
            end_hint: self.end_hint.clone(),
            crop: src.capture.crop(),
            origin,
            first,
        };
        let handle = std::thread::spawn(move || write_frames(rx, job));

        let started = match &src.capture {
            Capture::Desktop { rect, .. } => ScreenCapture::start(
                ScreenTarget::Monitor(monitor_index(*rect)),
                src.fps,
                tx.clone(),
                self.clock.clone(),
            )
            .map(MediaCapture::Screen),
            Capture::Window { hwnd, .. } => {
                // No raise, and none needed: WGC keeps recording a window
                // that sits behind others. That is the whole point of this
                // backend for a window source.
                ScreenCapture::start(
                    ScreenTarget::Window(*hwnd),
                    src.fps,
                    tx.clone(),
                    self.clock.clone(),
                )
                .map(MediaCapture::Screen)
            }
            Capture::Camera { device, .. } => match camera_link(device) {
                Ok(link) => CameraCapture::start(&link, src.fps, tx.clone(), self.clock.clone())
                    .map(MediaCapture::Camera),
                Err(e) => Err(e),
            },
            // Audio sources were started above and returned through
            // `start_audio_media` — ffmpeg has no WASAPI loopback, so there
            // is nothing left for this video-only match to do.
            Capture::Mic { .. } | Capture::SystemAudio { .. } | Capture::WindowAudio { .. } => {
                unreachable!("audio sources take the branch above")
            }
        };
        let handle_capture = started.map_err(spawn_error)?;
        self.remember(src, handle_capture, tx, None, handle, out, origin);
        Ok(watch)
    }

    /// Start one source as its own ffmpeg child, and return the take-time the
    /// child was launched at — this backend never sees a frame, so that is the
    /// only evidence it has of where the file belongs.
    fn start_ffmpeg_source(&mut self, src: &Source) -> Result<i64> {
        // A source can stop resolving between prepare and start (a window
        // closed during the countdown), or between two adds. Nothing was
        // recorded, so the caller decides what to throw away.
        let (input, extra_filter) = src.input_args(self.engine).map_err(Error::Source)?;
        let log = self.take.dir.join(format!("{}.log", src.id));
        let out = self.take.path_for(&src.id);

        let mut args: Vec<String> = vec!["-hide_banner".into(), "-y".into()];
        args.extend(input);
        if let Some(filter) = extra_filter {
            args.extend(["-vf".into(), filter]);
        }
        args.extend(self.encoder.args(effective_quality(src, self.quality)));
        args.push(out.to_string_lossy().into_owned());

        let started_at = self.clock.now_100ns();
        let child = ffmpeg::spawn(&self.ffmpeg, &args, &log)?;
        self.running.push(Running {
            id: src.id.clone(),
            capture: src.capture.clone(),
            child,
            log,
            out,
            fps: src.fps,
            start: Some(started_at),
        });
        self.take
            .set_start(&src.id, Some(started_at as f64 / 10_000_000.0));
        Ok(started_at)
    }

    /// Put a started source in the running list, and write down where its file
    /// begins.
    ///
    /// A `Take` origin is known to be zero before a single frame arrives, so
    /// the manifest is complete from the moment the source starts — a take that
    /// crashes keeps saying the right thing. A `FirstSample` origin is only
    /// known once a frame lands: `add_source` waits for it and fills this in.
    #[allow(clippy::too_many_arguments)]
    fn remember(
        &mut self,
        src: &Source,
        handle: MediaCapture,
        sender: std::sync::mpsc::SyncSender<MediaFrame>,
        sound: Option<SoundLane>,
        writer: std::thread::JoinHandle<std::result::Result<WriteReport, String>>,
        out: PathBuf,
        origin: Origin,
    ) {
        let start = match origin {
            Origin::Take => {
                self.take.set_start(&src.id, Some(0.0));
                Some(0)
            }
            Origin::FirstSample => None,
        };
        self.media.push(MediaRunning {
            id: src.id.clone(),
            capture: src.capture.clone(),
            handle: Some(handle),
            sender: Some(sender),
            sound,
            writer,
            out,
            fps: src.fps,
            start,
        });
    }

    /// Record a source that joined the take after it began.
    ///
    /// Returns once the file knows where it starts — a couple of frames, or the
    /// time the device took to wake up — so the manifest is correct while the
    /// take is still running rather than only after it closes.
    ///
    /// A failure only rolls back this one source: the rest of the take is
    /// running, and a camera that is busy is no reason to lose it. That is the
    /// opposite of what a failure at start time means (nothing was recorded, so
    /// the folder goes), and it is why the rollback lives here and not in the
    /// shared start path.
    pub fn add_source(&mut self, src: &Source) -> Result<Added> {
        validate_sources(std::slice::from_ref(src))?;
        if self.take.manifest.sources.iter().any(|s| s.id == src.id) {
            return Err(Error::Source(format!("duplicate source id {:?}", src.id)));
        }
        // Sound inside a picture's file is a Media Foundation mux; ffmpeg has
        // no WASAPI loopback to feed it. Refused by name rather than recording
        // a silent file (see `Session::prepare`).
        if self.backend != Backend::Windows && src.audio {
            return Err(Error::Source(format!(
                "{}: recording its sound into its own file needs the Windows backend (WGC + MF)",
                src.capture.device_name().unwrap_or("the desktop")
            )));
        }

        // On disk before the capture opens: a take that dies between the two
        // must still say this source was asked for and never arrived.
        self.take.add_source(src, self.now_secs());
        let _ = self.take.save_manifest();

        let t_spawn = Instant::now();
        let started: Result<Option<i64>> = match self.backend {
            Backend::Windows => self
                .start_media_source(src, Origin::FirstSample)
                .map(|watch| watch.and_then(|rx| rx.recv_timeout(ADD_FIRST_TIMEOUT).ok())),
            Backend::Ffmpeg if src.capture.is_audio() => self
                .start_media_source(src, Origin::FirstSample)
                .map(|watch| watch.and_then(|rx| rx.recv_timeout(ADD_FIRST_TIMEOUT).ok())),
            Backend::Ffmpeg => self.start_ffmpeg_source(src).map(Some),
        };
        let start = match started {
            Ok(start) => start,
            Err(e) => {
                self.take.remove_source(&src.id);
                let _ = self.take.save_manifest();
                return Err(e);
            }
        };
        self.spawn_ms
            .push((src.id.clone(), t_spawn.elapsed().as_millis() as u64));

        let seconds = start.map(|t| t as f64 / 10_000_000.0);
        if let Some(secs) = seconds {
            self.take.set_start(&src.id, Some(secs));
            // Keep the running copy in step, so `snapshot` reports the same
            // offset the manifest does.
            if let Some(m) = self.media.iter_mut().find(|m| m.id == src.id) {
                m.start = start;
            }
            let _ = self.take.save_manifest();
        }
        Ok(Added {
            id: src.id.clone(),
            start: seconds,
        })
    }
}

/// A source that joined a take already running, and where its file begins.
#[derive(Debug, Clone)]
pub struct Added {
    pub id: String,
    /// Take-time of the file's first sample, in seconds. `None` when nothing
    /// arrived within `ADD_FIRST_TIMEOUT` — the manifest then fills it in when
    /// the file closes, or leaves it null if the source never wrote a frame.
    pub start: Option<f64>,
}

/// A running Windows-API source: the capture, the channel into its writer and
/// the writer thread itself.
struct MediaRunning {
    id: String,
    /// Kept so `snapshot` can say whether this source is still worth recording
    /// — the same role `Running.capture` plays for the ffmpeg ones.
    capture: Capture,
    handle: Option<MediaCapture>,
    sender: Option<std::sync::mpsc::SyncSender<MediaFrame>>,
    /// The sound that goes into this source's own file, when it asked for one
    /// (a window recording its own process tree). It has its own capture and
    /// its own channel, but shares the one writer thread — a sink writer is
    /// not something to hand to two threads.
    sound: Option<SoundLane>,
    writer: std::thread::JoinHandle<std::result::Result<WriteReport, String>>,
    out: PathBuf,
    /// The rate the file was asked for; `length_mismatch` judges ffprobe's
    /// reading against it.
    fps: u32,
    /// Take-time its file begins at, once the first sample has said so. `Some(0)`
    /// the moment a from-the-start source is registered; `None` for a source
    /// added mid-take until a frame actually arrives.
    start: Option<i64>,
}

/// One extra capture feeding an existing source's writer.
struct SoundLane {
    capture: Option<MediaCapture>,
    sender: Option<std::sync::mpsc::SyncSender<MediaFrame>>,
}

impl SoundLane {
    /// Stop the capture, then drop the channel so the writer drains and the
    /// file closes. Same order the picture path uses, for the same reason.
    /// A capture that reports a failure while stopping says so: the caller
    /// attaches it to the source's outcome, so a device that died mid-take
    /// cannot pass as a short successful one.
    fn stop(&mut self) -> std::result::Result<(), String> {
        let mut failed = None;
        if let Some(capture) = self.capture.take() {
            if let Err(e) = capture.stop() {
                failed = Some(e.to_string());
            }
        }
        self.sender.take();
        match failed {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
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
        // The sound goes with its picture; a writer waiting on two channels
        // only ends when both of them are closed.
        if let Some(mut lane) = self.sound.take() {
            let _ = lane.stop();
        }
        true
    }

    /// Stop everything and report. Runs on its own thread inside `stop`.
    fn finish(&mut self) -> Collected {
        // A capture that failed while stopping is the only witness that the
        // device died mid-take: dropping the error here made that look like
        // a short, successful recording. It travels with the source's
        // outcome, bytes or not — the file is short for a reason.
        let mut stop_error = None;
        if let Some(handle) = self.handle.take() {
            if let Err(e) = handle.stop() {
                stop_error = Some(e.to_string());
            }
        }
        self.sender.take();
        if let Some(mut lane) = self.sound.take() {
            if let Err(e) = lane.stop() {
                stop_error = stop_error.or(Some(e));
            }
        }
        // `JoinHandle::join` takes ownership, so the handle leaves `self`.
        let writer = std::mem::replace(
            &mut self.writer,
            // A placeholder nobody joins twice: this struct is consumed by
            // `finish`'s caller (`stop`'s scope), so the slot is never read.
            std::thread::spawn(|| {
                Ok(WriteReport {
                    seconds: 0.0,
                    frames: 0,
                    segments: Vec::new(),
                    width: 0,
                    height: 0,
                    start: None,
                })
            }),
        );
        let outcome = writer
            .join()
            .unwrap_or_else(|_| Err("the writer thread panicked".into()));
        // Every file this source produced, not just `out`: a paused take splits
        // the source, and a size that counted only the first one would report a
        // tenth of what is on disk.
        let (start, seconds, frames, size, segments, bytes, error) = match outcome {
            Ok(report) if !report.segments.is_empty() => {
                let bytes: u64 = report
                    .segments
                    .iter()
                    .map(|s| std::fs::metadata(&s.file).map(|m| m.len()).unwrap_or(0))
                    .sum();
                (
                    // The writer's own measurement wins over the optimistic
                    // value a from-the-start source was registered with: it is
                    // the one that came from a sample.
                    report.start.or(self.start),
                    Some(report.seconds),
                    Some(report.frames),
                    Some((report.width, report.height)),
                    report.segments.iter().map(Seg::from).collect(),
                    bytes,
                    None,
                )
            }
            // The writer never opened a file.
            Ok(_) => (
                None,
                None,
                Some(0),
                None,
                Vec::new(),
                std::fs::metadata(&self.out).map(|m| m.len()).unwrap_or(0),
                Some(diagnose::FFMPEG_DIED.to_string()),
            ),
            Err(e) => (None, None, None, None, Vec::new(), 0, Some(e)),
        };
        // The writer's own failure is the more specific story; a device that
        // also failed to stop is appended rather than replacing it. Either
        // one alone means this source did not record cleanly, no matter how
        // many bytes landed on disk.
        let error = match (error, stop_error) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (a, b) => a.or(b),
        };
        Collected {
            id: self.id.clone(),
            bytes,
            start,
            seconds,
            frames,
            size,
            probed: None,
            fps: self.fps,
            segments,
            error,
        }
    }
}

/// The capture handles a Windows-API source can hold. They are different
/// types reaching the same `stop`, which is all a take ever does to them.
enum MediaCapture {
    Screen(ScreenCapture),
    Camera(CameraCapture),
    Mic(MicCapture),
    Loopback(LoopbackCapture),
}

impl MediaCapture {
    fn stop(self) -> takekit_media::Result<()> {
        match self {
            MediaCapture::Screen(c) => c.stop(),
            MediaCapture::Camera(c) => c.stop(),
            MediaCapture::Mic(c) => c.stop(),
            MediaCapture::Loopback(c) => c.stop(),
        }
    }
}

/// Drain frames into one MP4, then finalise it.
///
/// Frames are written as they arrive and keep their capture timestamps —
/// variable frame rate, which is why a still picture costs almost nothing. The
/// returned number is the seconds the file covers, taken from the stop time
/// the caller infers rather than from a frame that never arrived.
/// Everything one picture file is written with — the frame channel stays a
/// separate argument, because it is the only one that is not a setting.
struct FrameJob {
    out: PathBuf,
    fps: u32,
    bitrate: u32,
    clock: TakeClock,
    end_hint: std::sync::Arc<std::sync::atomic::AtomicI64>,
    crop: crate::source::Crop,
    origin: Origin,
    /// Told the file's first take-time, for a source added mid-take.
    first: Option<std::sync::mpsc::SyncSender<i64>>,
}

fn write_frames(
    rx: std::sync::mpsc::Receiver<MediaFrame>,
    job: FrameJob,
) -> std::result::Result<WriteReport, String> {
    // One writer loop for every caller (`write_stream` is where a frame
    // channel becomes a file); this path writes every frame the capture
    // delivered. The CLI's --cfr sampler is the same loop with `KeepLatest`.
    let report = takekit_media::writer::write_stream(
        rx,
        &job.out,
        takekit_media::writer::Encode {
            fps: job.fps,
            bitrate: job.bitrate,
            // WGC hands over the whole window or monitor; the edges the editor
            // picked are cut here, so a cropped source records the same picture
            // on this backend as it does through ffmpeg's region grab.
            crop: crop_box(job.crop),
            sampling: takekit_media::writer::Sampling::EveryFrame,
            origin: job.origin,
            first: job.first,
        },
        job.clock,
        job.end_hint,
    )
    .map_err(|e| e.to_string())?;
    Ok(WriteReport {
        seconds: report.seconds,
        frames: report.frames,
        segments: report.segments,
        width: report.width,
        height: report.height,
        start: report.start,
    })
}

/// The edges the editor picked, in the writer's own vocabulary.
fn crop_box(crop: crate::source::Crop) -> takekit_media::writer::CropBox {
    takekit_media::writer::CropBox {
        top: crop.top,
        bottom: crop.bottom,
        left: crop.left,
        right: crop.right,
    }
}

/// One started picture-with-sound source: the picture's capture, the channel
/// into its writer, the sound capture with its own channel, and the thread
/// that drains both into one file.
type StartedAv = (
    MediaCapture,
    std::sync::mpsc::SyncSender<MediaFrame>,
    SoundLane,
    std::thread::JoinHandle<std::result::Result<WriteReport, String>>,
);

/// Which monitor a desktop source records.
///
/// WGC captures one output at a time — a "whole virtual desktop" does not exist
/// for it. A remembered rect picks the monitor holding its centre (that is what
/// the picker stored); no rect falls back to the first monitor, which is what
/// this used to do unconditionally (2026-10-21 多屏).
fn monitor_index(rect: Option<crate::source::ScreenRect>) -> usize {
    match rect {
        Some(r) => takekit_media::monitor_index_for(r.x, r.y, r.w, r.h),
        None => None,
    }
    .unwrap_or(1)
}

/// Start a picture and the sound that belongs with it, as one source.
///
/// Two captures, one writer: a sink writer is not something to hand to two
/// threads, so both channels are drained by the thread that owns it (see
/// `write_av`).
///
/// * A **window** brings its own sound: the process tree, which is what a
///   window really is to a user — the process and its children, so a browser
///   playing music in another process still counts as this window.
/// * The **desktop** brings everyone's: everything the speakers play, which is
///   the same WASAPI loopback the `SystemAudio` source records. There is no
///   per-monitor sound on Windows, so "this screen's sound" and "this
///   computer's sound" are the same thing — the option lives on the desktop
///   source instead of on a monitor.
#[allow(clippy::too_many_arguments)]
fn start_av(
    src: &Source,
    out: PathBuf,
    clock: TakeClock,
    end_hint: std::sync::Arc<std::sync::atomic::AtomicI64>,
    bitrate: u32,
    audio_bitrate_kbps: u32,
    origin: Origin,
    first: Option<std::sync::mpsc::SyncSender<i64>>,
) -> Result<StartedAv> {
    // What to capture, and what to listen to while capturing it. Both are
    // settled before anything opens: an unreachable target must fail before a
    // second capture is left running.
    let (video_target, audio_target) = match &src.capture {
        Capture::Window {
            hwnd, pid: owner, ..
        } => {
            // A window that is gone has no process tree left to listen to, and
            // no picture either — say so before anything opens. The handle must
            // still be the same process's: another app's window inheriting the
            // number would put *its* music in this file.
            let pid = takekit_media::loopback::window_pid(*hwnd)
                .ok_or_else(|| Error::Source(format!("no process owns window {hwnd:#x}")))?;
            if *owner != 0 && pid != *owner {
                return Err(Error::Source(format!(
                    "window {hwnd:#x} is now pid {pid}'s, not pid {owner}'s"
                )));
            }
            (
                ScreenTarget::Window(*hwnd),
                LoopbackTarget::ProcessTree(pid),
            )
        }
        Capture::Desktop { rect, .. } => (
            ScreenTarget::Monitor(monitor_index(*rect)),
            LoopbackTarget::All,
        ),
        other => {
            return Err(Error::Source(format!(
                "{} has no sound of its own to carry; only a window or the desktop does",
                other.device_name().unwrap_or("this source")
            )));
        }
    };

    let fps = src.fps.max(1);
    let crop = crop_box(src.capture.crop());
    let (vtx, vrx) = std::sync::mpsc::sync_channel::<MediaFrame>(takekit_media::VIDEO_QUEUE);
    let (atx, arx) = std::sync::mpsc::sync_channel::<MediaFrame>(takekit_media::AUDIO_QUEUE);
    let (format_tx, format_rx) = std::sync::mpsc::channel();
    // Owned by the writer thread; the captures below keep the other copy. A
    // `move` closure would take the only one and leave nothing to capture with.
    let writer_clock = clock.clone();
    let handle = std::thread::spawn(move || {
        write_av(
            vrx,
            arx,
            format_rx,
            out,
            fps,
            bitrate,
            audio_bitrate_kbps,
            writer_clock,
            end_hint,
            crop,
            origin,
            first,
        )
    });

    // The picture first: if it cannot be captured there is nothing to put the
    // sound into, and the loopback is the more expensive open.
    let screen =
        ScreenCapture::start(video_target, fps, vtx.clone(), clock.clone()).map_err(spawn_error)?;
    let loopback = LoopbackCapture::start(audio_target, atx.clone(), clock).map_err(spawn_error)?;
    let f = loopback.format();
    format_tx
        .send((f.sample_rate, f.channels))
        .map_err(|_| Error::Source("the sound writer is gone".into()))?;

    Ok((
        MediaCapture::Screen(screen),
        vtx,
        SoundLane {
            capture: Some(MediaCapture::Loopback(loopback)),
            sender: Some(atx),
        },
        handle,
    ))
}

/// Drain a picture and the sound that belongs with it into one MP4.
///
/// The sound usually starts talking before the first frame has arrived, and
/// Media Foundation will not take a stream for a sink that has already begun
/// writing. So the sound waits in its channel — a second or two of it fits —
/// and goes in right behind the first frame, which is what says how big the
/// file is.
///
/// The returned length is whichever stream ended later: the container's own
/// duration is the longer of the two, and a take's manifest should say the
/// same thing the file does.
#[allow(clippy::too_many_arguments)]
fn write_av(
    vrx: std::sync::mpsc::Receiver<MediaFrame>,
    arx: std::sync::mpsc::Receiver<MediaFrame>,
    format_rx: std::sync::mpsc::Receiver<(u32, u32)>,
    out: PathBuf,
    fps: u32,
    bitrate: u32,
    audio_bitrate_kbps: u32,
    clock: TakeClock,
    end_hint: std::sync::Arc<std::sync::atomic::AtomicI64>,
    crop: takekit_media::writer::CropBox,
    origin: Origin,
    first: Option<std::sync::mpsc::SyncSender<i64>>,
) -> std::result::Result<WriteReport, String> {
    use std::sync::atomic::Ordering;
    use takekit_media::writer::{SegmentReport, crop_frame, even_frame, segment_path};

    let (rate, channels) = format_rx
        .recv()
        .map_err(|_| "the sound capture never reported a format".to_string())?;

    // One segment at a time: a pause closes this file and the next picture
    // opens the following one, so the cut lands on a keyframe by construction.
    let mut sink: Option<MuxSink> = None;
    let mut video: Option<takekit_media::writer::VideoWriter> = None;
    let mut audio: Option<takekit_media::AudioWriter> = None;
    let mut segments: Vec<SegmentReport> = Vec::new();
    let mut base: Option<i64> = None;
    let mut seg_frames = 0u64;
    let mut frames = 0u64;
    let mut reported = false;
    let mut size = (0u32, 0u32);
    // The last picture written — what a resume with nothing new opens with.
    let mut held: Option<takekit_media::VideoFrame> = None;

    loop {
        if clock.is_paused() {
            // Sound first and pictures second, the same order as the live loop:
            // what is already queued was captured before the cut.
            let frozen = clock.now_100ns();
            while let Ok(frame) = arx.try_recv() {
                if let MediaFrame::Audio(chunk) = frame {
                    if let Some(a) = audio.as_mut() {
                        a.write(&chunk).map_err(|e| e.to_string())?;
                    }
                }
            }
            while let Ok(frame) = vrx.try_recv() {
                if let MediaFrame::Video(frame) = frame {
                    if let (Some(v), Some(at)) = (video.as_mut(), base) {
                        frames += 1;
                        seg_frames += 1;
                        let frame = even_frame(crop_frame(frame, crop));
                        v.write_at(&frame, frame.time - at)
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
            close_av(
                &mut sink,
                &mut video,
                &mut audio,
                &mut segments,
                base,
                seg_frames,
                &out,
                frozen,
            )?;
            seg_frames = 0;
            base = None;
            // The capture keeps running through the pause; that is what makes
            // "继续" instant instead of a window warming up again.
            let _ = vrx.recv_timeout(std::time::Duration::from_millis(20));
            continue;
        }

        // Sound first in every turn: a chunk is a couple of kilobytes and must
        // not queue behind a picture the encoder is busy with.
        while let Ok(frame) = arx.try_recv() {
            if let MediaFrame::Audio(chunk) = frame {
                if let Some(a) = audio.as_mut() {
                    a.write(&chunk).map_err(|e| e.to_string())?;
                }
            }
        }
        // One picture per turn: a frame that arrived, or — if the clock just
        // started again and the window has not repainted — the picture the
        // previous file ended with. Without the second case a resumed segment
        // would never open on a still window, because the capture never stopped
        // and so never sends a fresh first frame.
        let arrived = match vrx.recv_timeout(std::time::Duration::from_millis(2)) {
            Ok(MediaFrame::Video(frame)) => Some((frame, true)),
            Ok(_) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let picked = match arrived {
            Some((frame, true)) => {
                let at = frame.time;
                Some((frame, at, false))
            }
            _ if sink.is_none() => held
                .as_ref()
                .map(|frame| (frame.clone(), clock.now_100ns(), true)),
            _ => None,
        };
        if let Some((frame, at_time, seeded)) = picked {
            if sink.is_none() {
                // A new segment starts here. Its own zero is its first sample,
                // never the take's: the manifest's `start` places it, and
                // padding every resume back to the take's origin would be a
                // wall of black. The one exception is the very first file of a
                // from-the-start source, which pays for a late device with
                // black lead-in (see `Origin`).
                let at = match origin {
                    Origin::Take if segments.is_empty() && !seeded => 0,
                    _ => at_time,
                };
                if !reported {
                    reported = true;
                    if let Some(tx) = first.as_ref() {
                        let _ = tx.try_send(at);
                    }
                }
                base = Some(at);
                let first_frame = even_frame(crop_frame(frame, crop));
                size = (first_frame.width, first_frame.height);
                let path = segment_path(&out, segments.len() + 1);
                let s = MuxSink::create(&path).map_err(|e| e.to_string())?;
                let mut v = s
                    .add_video(
                        first_frame.format,
                        first_frame.width,
                        first_frame.height,
                        fps,
                        bitrate,
                    )
                    .map_err(|e| e.to_string())?;
                let mut a = s
                    .add_audio(rate, channels, audio_bitrate_kbps)
                    .map_err(|e| e.to_string())?;
                // Sound buffered before this picture arrived belongs to this
                // file, on its timeline.
                a.set_origin(at);
                s.begin().map_err(|e| e.to_string())?;
                v.write_at(&first_frame, first_frame.time - at)
                    .map_err(|e| e.to_string())?;
                // A seeded file holds a picture that was already counted.
                if !seeded {
                    seg_frames = 1;
                    frames += 1;
                }
                held = Some(first_frame);
                sink = Some(s);
                video = Some(v);
                audio = Some(a);
            } else if let (Some(v), Some(at)) = (video.as_mut(), base) {
                let frame = even_frame(crop_frame(frame, crop));
                v.write_at(&frame, at_time - at)
                    .map_err(|e| e.to_string())?;
                frames += 1;
                seg_frames += 1;
                held = Some(frame);
            }
        }
    }

    // What the sound still held, then the picture's last frame held until the
    // stop time — the same end every other file in the take gets.
    while let Ok(frame) = arx.try_recv() {
        if let MediaFrame::Audio(chunk) = frame {
            if let Some(a) = audio.as_mut() {
                a.write(&chunk).map_err(|e| e.to_string())?;
            }
        }
    }
    let end = match end_hint.load(Ordering::SeqCst) {
        0 => clock.now_100ns(),
        t => t,
    };
    close_av(
        &mut sink,
        &mut video,
        &mut audio,
        &mut segments,
        base,
        seg_frames,
        &out,
        end,
    )?;

    let total: f64 = segments.iter().map(|s| s.seconds).sum();
    Ok(WriteReport {
        seconds: total,
        frames,
        width: size.0,
        height: size.1,
        start: segments.first().map(|s| s.start),
        segments,
    })
}

/// Close the file a picture-with-sound source has open, at take-time `end`.
///
/// The picture is held to `end` so the two tracks finish together; the sound
/// cannot be held (there is no such thing as a frame of silence to stretch), so
/// the longer of the two decides how long the file claims to be.
// Same shape as `write_av`, and for the same reason: a file being closed needs
// its three handles plus where it started, and packaging those into a struct
// only to pass it once would not make the call easier to read.
#[allow(clippy::too_many_arguments)]
fn close_av(
    sink: &mut Option<MuxSink>,
    video: &mut Option<takekit_media::writer::VideoWriter>,
    audio: &mut Option<takekit_media::AudioWriter>,
    segments: &mut Vec<takekit_media::writer::SegmentReport>,
    base: Option<i64>,
    frames: u64,
    out: &Path,
    end: i64,
) -> std::result::Result<(), String> {
    let Some(s) = sink.take() else {
        return Ok(());
    };
    let at = base.unwrap_or(end);
    let span = (end - at).max(0);
    if let Some(mut v) = video.take() {
        v.flush(Some(span)).map_err(|e| e.to_string())?;
    }
    let audio_end = audio.take().map(|a| a.end()).unwrap_or(0);
    s.finalize().map_err(|e| e.to_string())?;
    segments.push(takekit_media::writer::SegmentReport {
        file: takekit_media::writer::segment_path(out, segments.len() + 1),
        start: at,
        seconds: span.max(audio_end) as f64 / 10_000_000.0,
        frames,
    });
    Ok(())
}

/// What one source's writer saw. `frames` is what the capture actually
/// delivered — the output has `duration x fps` slots either way, because the
/// encoder holds a frame across a pause. The difference between the two is how
/// much of the take is a held picture rather than a fresh one, which is the
/// only visible form a dropped capture update can take.
struct WriteReport {
    seconds: f64,
    frames: u64,
    /// One entry per file the writer produced. A pause closes the current one
    /// and the next sample opens the next, so this is where a take that was
    /// paused becomes several clips instead of one.
    segments: Vec<takekit_media::writer::SegmentReport>,
    /// The size the file was written at; `0 x 0` when nothing was written.
    width: u32,
    height: u32,
    /// Take-time the file begins at, in 100 ns units. `None` when nothing was
    /// written at all.
    start: Option<i64>,
}

/// The camera's Media Foundation symbolic link, matched from the name the
/// source carries. The ffmpeg backend addresses cameras by DirectShow name;
/// Media Foundation has its own naming, so the panel's pick has to be
/// translated.
fn camera_link(device: &str) -> std::result::Result<String, takekit_media::MediaError> {
    let cameras = takekit_media::devices::cameras()?;
    cameras
        .iter()
        .find(|c| c.name == device || c.link == device)
        .map(|c| c.link.clone())
        .ok_or_else(|| {
            takekit_media::MediaError::message(format!("{}|{device}", diagnose::CAM_NOT_MF))
        })
}

/// A microphone's WASAPI endpoint id, matched the same way — by its listed
/// name or by the endpoint id itself.
fn mic_endpoint(device: &str) -> std::result::Result<String, takekit_media::MediaError> {
    let mics = takekit_media::devices::microphones()?;
    mics.iter()
        .find(|m| m.name == device || m.endpoint == device)
        .map(|m| m.endpoint.clone())
        .ok_or_else(|| {
            takekit_media::MediaError::message(format!("{}|{device}", diagnose::DEVICE_GONE))
        })
}

/// One started audio source: its capture handle, the channel feeding its
/// writer thread, and the writer thread itself.
type StartedAudio = (
    MediaCapture,
    std::sync::mpsc::SyncSender<MediaFrame>,
    std::thread::JoinHandle<std::result::Result<WriteReport, String>>,
);

/// Start one audio source through the OS capture APIs and put a writer behind
/// it. Both backends land here — ffmpeg has no WASAPI loopback at all, so a
/// sound source is never a child process.
///
/// The writer thread comes first and waits for the capture's format (sample
/// rate and channel count) over a channel, the same shape the CLI's `media`
/// command uses: `AudioWriter` cannot open its file until it knows them.
fn start_audio_media(
    src: &Source,
    out: PathBuf,
    clock: TakeClock,
    bitrate_kbps: u32,
    first: Option<std::sync::mpsc::SyncSender<i64>>,
) -> Result<StartedAudio> {
    // Bounded like the video one, but much deeper: chunks are tiny, and a
    // dropped chunk pulls the rest of the file earlier (audio is written back
    // to back) — only a writer stuck for whole seconds should ever fill it.
    let (tx, rx) = std::sync::mpsc::sync_channel::<MediaFrame>(takekit_media::AUDIO_QUEUE);
    let (format_tx, format_rx) = std::sync::mpsc::channel();
    // The writer thread owns one copy, the capture below keeps the other.
    let writer_clock = clock.clone();
    let handle = std::thread::spawn(move || {
        write_audio(rx, format_rx, out, bitrate_kbps, writer_clock, first)
    });

    let (capture, format) = match &src.capture {
        Capture::Mic { device } => {
            let endpoint = mic_endpoint(device).map_err(spawn_error)?;
            let capture = MicCapture::start(&endpoint, tx.clone(), clock).map_err(spawn_error)?;
            let f = capture.format();
            (MediaCapture::Mic(capture), (f.sample_rate, f.channels))
        }
        Capture::SystemAudio {} => {
            let capture = LoopbackCapture::start(LoopbackTarget::All, tx.clone(), clock)
                .map_err(spawn_error)?;
            let f = capture.format();
            (MediaCapture::Loopback(capture), (f.sample_rate, f.channels))
        }
        Capture::WindowAudio {
            hwnd, pid: owner, ..
        } => {
            // The filter is the process tree of the window's owner. A window
            // that is gone has no tree left to listen to, and a recycled
            // handle would filter on a stranger's tree.
            let pid = takekit_media::loopback::window_pid(*hwnd)
                .ok_or_else(|| Error::Source(format!("no process owns window {hwnd:#x}")))?;
            if *owner != 0 && pid != *owner {
                return Err(Error::Source(format!(
                    "window {hwnd:#x} is now pid {pid}'s, not pid {owner}'s"
                )));
            }
            let capture =
                LoopbackCapture::start(LoopbackTarget::ProcessTree(pid), tx.clone(), clock)
                    .map_err(spawn_error)?;
            let f = capture.format();
            (MediaCapture::Loopback(capture), (f.sample_rate, f.channels))
        }
        _ => {
            return Err(Error::Source(
                "start_audio_media was given a video source".into(),
            ));
        }
    };
    format_tx
        .send(format)
        .map_err(|_| Error::Source("the audio writer is gone".into()))?;
    Ok((capture, tx, handle))
}

/// Drain an audio capture into one MP4 of AAC, then finalise it.
///
/// The file's length is where its timeline stopped rather than how many bytes
/// arrived: chunks are written back to back, so there is no end hint to latch
/// (unlike the video writer's held frame), but the lead-in silence that pads a
/// late-waking device does belong to the take and does count.
///
/// The first chunk sets the file's origin, which is what a source added
/// mid-take needs: its file opens where its sound begins instead of with
/// however many minutes of silence it missed.
fn write_audio(
    rx: std::sync::mpsc::Receiver<MediaFrame>,
    format_rx: std::sync::mpsc::Receiver<(u32, u32)>,
    out: PathBuf,
    bitrate_kbps: u32,
    clock: TakeClock,
    first: Option<std::sync::mpsc::SyncSender<i64>>,
) -> std::result::Result<WriteReport, String> {
    use takekit_media::AudioWriter;
    use takekit_media::writer::{SegmentReport, segment_path};

    let (rate, channels) = format_rx
        .recv()
        .map_err(|_| "the audio capture never reported a format".to_string())?;
    let mut writer: Option<AudioWriter> = None;
    let mut segments: Vec<SegmentReport> = Vec::new();
    // Take-time of the chunk the open file starts with.
    let mut origin: Option<i64> = None;
    let mut seg_chunks = 0u64;
    let mut chunks = 0u64;
    let mut reported = false;

    loop {
        if clock.is_paused() {
            // Drain first: these chunks were captured before the cut.
            let mut at = origin;
            while let Ok(frame) = rx.try_recv() {
                if let MediaFrame::Audio(chunk) = frame {
                    if let Some(w) = writer.as_mut() {
                        chunks += 1;
                        seg_chunks += 1;
                        if at.is_none() {
                            at = Some(chunk.time);
                            w.set_origin(chunk.time);
                        }
                        w.write(&chunk).map_err(|e| e.to_string())?;
                    }
                }
            }
            if let Some(w) = writer.take() {
                let end = w.finish().map_err(|e| e.to_string())?;
                segments.push(SegmentReport {
                    file: segment_path(&out, segments.len() + 1),
                    start: at.unwrap_or(end),
                    seconds: end as f64 / 10_000_000.0,
                    frames: seg_chunks,
                });
            }
            origin = None;
            seg_chunks = 0;
            // The device keeps running through the pause; nothing of it is kept.
            let _ = rx.recv_timeout(std::time::Duration::from_millis(20));
            continue;
        }

        match rx.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(MediaFrame::Audio(chunk)) => {
                if writer.is_none() {
                    writer = Some(
                        AudioWriter::create(
                            &segment_path(&out, segments.len() + 1),
                            rate,
                            channels,
                            bitrate_kbps,
                        )
                        .map_err(|e| e.to_string())?,
                    );
                    // A chunk's own instant is its file's zero: the manifest's
                    // per-segment `start` says where it belongs on the take.
                    origin = Some(chunk.time);
                    writer
                        .as_mut()
                        .expect("opened above")
                        .set_origin(chunk.time);
                    if !reported {
                        reported = true;
                        if let Some(tx) = first.as_ref() {
                            let _ = tx.try_send(chunk.time);
                        }
                    }
                }
                chunks += 1;
                seg_chunks += 1;
                writer
                    .as_mut()
                    .expect("opened above")
                    .write(&chunk)
                    .map_err(|e| e.to_string())?;
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if let Some(w) = writer.take() {
        let end = w.finish().map_err(|e| e.to_string())?;
        segments.push(SegmentReport {
            file: segment_path(&out, segments.len() + 1),
            start: origin.unwrap_or(end),
            seconds: end as f64 / 10_000_000.0,
            frames: seg_chunks,
        });
    }
    let total: f64 = segments.iter().map(|s| s.seconds).sum();
    let start = segments.first().map(|s| s.start);
    Ok(WriteReport {
        seconds: total,
        frames: chunks,
        segments,
        width: 0,
        height: 0,
        start,
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

/// The same knob as AAC bitrate. Much smaller numbers than the picture's —
/// 256 kbps is transparent for stereo speech and music, and the top of the
/// range the CLI's `media` command caps at.
fn audio_bitrate_for(quality: u8) -> u32 {
    match quality {
        0..=14 => 256,
        15..=22 => 192,
        23..=30 => 128,
        _ => 96,
    }
}

/// Every spawn failure is a `Source` error: the caller knows what it asked to
/// record, and the capture's own words name the device that refused. Known
/// failures come back as `err.*|device` codes (see `diagnose`) so every face
/// can say the same thing in its own words.
fn spawn_error(e: takekit_media::MediaError) -> Error {
    Error::Source(diagnose::diagnose(&e.to_string(), None))
}

struct Running {
    id: String,
    /// Kept so `snapshot` can say whether this source is still worth recording
    /// at all — see `LiveSource::usable`.
    capture: Capture,
    child: Child,
    log: PathBuf,
    out: PathBuf,
    fps: u32,
    /// Take-time the child was launched at. The only offset an ffmpeg child can
    /// offer: it files its own clock, and nothing here sees its frames.
    start: Option<i64>,
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
    /// The take's own timeline: one origin, taken before the first capture
    /// opened, that every source's `start` is measured against — including a
    /// source that joins ten minutes in. A clock per source would make each
    /// one's offset true only to itself.
    clock: TakeClock,
    /// The instant the take ended, latched when `stop` begins so every file
    /// covers exactly the same stretch of the timeline.
    end_hint: std::sync::Arc<std::sync::atomic::AtomicI64>,
    /// What a source added later has to be started with. Kept on the recording
    /// because a take that can still grow has to remember how it was started.
    backend: Backend,
    ffmpeg: PathBuf,
    probe: PathBuf,
    encoder: Encoder,
    /// Session-wide quality; a source may override it.
    quality: u8,
    engine: Engine,
    /// Milliseconds each encoder took to be spawned, in source order. This is
    /// the skew the capture starts with before any frame is written.
    spawn_ms: Vec<(String, u64)>,
    marks: Option<Marks>,
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
    /// Where its file begins on the take's timeline, in seconds. `None` until
    /// the first sample (a source added mid-take) or when it never wrote one.
    pub start: Option<f64>,
}

impl Recording {
    /// Recorded time, not wall time: a take paused for a minute reads a minute
    /// less, which is exactly what its files will add up to.
    pub fn elapsed(&self) -> f64 {
        self.clock.now_100ns() as f64 / 10_000_000.0
    }

    /// Stop the clock and close every source's current file.
    ///
    /// The clock stops first, so anything stamped after this instant belongs to
    /// the pause and gets dropped. Then each writer sees the frozen clock,
    /// drains what was already queued for this file, and closes it exactly
    /// there. Because they all read one clock, they cut on the same instant —
    /// and the next sample after `resume` opens the next file, which is how a
    /// take that was paused comes out as several clips with exact boundaries
    /// instead of one file that has to be split at a keyframe later.
    pub fn pause(&mut self) -> Result<()> {
        // ffmpeg records each source in a child process with a clock of its own;
        // there is no way to stop it that does not also corrupt its timestamps
        // (a suspended decoder keeps the device queue filling and drains it in a
        // burst afterwards). Refused by name rather than silently ignored.
        if self.backend != Backend::Windows {
            return Err(Error::Source(
                "pausing a take needs the Windows backend (WGC + MF); this take is recording \
                 through ffmpeg children, which each file their own clock"
                    .into(),
            ));
        }
        self.clock.pause();
        Ok(())
    }

    /// Start the clock again. Every source opens its next file on the first
    /// sample after this, with no device warm-up: the captures never stopped.
    pub fn resume(&mut self) -> Result<()> {
        self.clock.resume();
        Ok(())
    }

    pub fn is_paused(&self) -> bool {
        self.clock.is_paused()
    }

    /// How long the pause in progress has lasted, or 0 while running.
    pub fn pause_len(&self) -> f64 {
        self.clock.pause_len_100ns() as f64 / 10_000_000.0
    }

    /// Paused time banked by pauses that have already ended.
    pub fn paused_total(&self) -> f64 {
        self.clock.paused_100ns() as f64 / 10_000_000.0
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

    /// What this take is recording, in the order it started.
    ///
    /// A caller that wants to add a source mid-take needs it: ids name the
    /// output files, so a new one has to be checked against what is already
    /// there rather than against whatever list the request came from.
    pub fn sources(&self) -> Vec<Source> {
        self.take
            .manifest
            .sources
            .iter()
            .map(|s| s.source.clone())
            .collect()
    }

    /// Poll every source. Cheap enough for a 4 Hz UI tick.
    pub fn snapshot(&mut self) -> Vec<LiveSource> {
        let secs = |t: Option<i64>| t.map(|t| t as f64 / 10_000_000.0);
        let mut out: Vec<LiveSource> = self
            .running
            .iter_mut()
            .map(|r| LiveSource {
                id: r.id.clone(),
                alive: matches!(r.child.try_wait(), Ok(None)),
                usable: is_recordable(&r.capture),
                bytes: std::fs::metadata(&r.out).map(|m| m.len()).unwrap_or(0),
                start: secs(r.start),
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
                start: secs(m.start),
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
        self.end_hint
            .store(self.clock.now_100ns(), std::sync::atomic::Ordering::SeqCst);
        // Where each source's file is, resolved once: the workers below
        // finalise and verify in parallel and must not borrow the take to ask.
        let files: Vec<(String, PathBuf)> = self
            .take
            .manifest
            .sources
            .iter()
            .map(|s| (s.id.clone(), self.take.dir.join(&s.file)))
            .collect();
        let probe = self.probe.clone();

        // Sources are independent processes and files: stop them, finalise them
        // and verify them in parallel. Serial ffmpeg shutdown plus one ffprobe
        // per source was the bulk of stop latency — and the per-source checklist
        // a stop screen shows is this same event stream, which is why every
        // worker reports its own outcome the moment its file lands instead of
        // queueing results for the caller to walk through.
        let collected: Vec<Collected> = std::thread::scope(|scope| {
            let mut jobs: Vec<_> = self
                .running
                .iter_mut()
                .map(|r| {
                    let probe = &probe;
                    let files = &files;
                    let on_source = &on_source;
                    scope.spawn(move || {
                        ffmpeg::stop(&mut r.child);
                        let mut c = collect_ffmpeg(r);
                        c.verify(probe, files);
                        on_source(&c.outcome());
                        c
                    })
                })
                .collect();
            // The Windows-API sources finalise in parallel with the ffmpeg
            // ones: stopping a capture waits for its frames to stop arriving,
            // and doing that serially would be stop latency the user feels.
            for m in self.media.iter_mut() {
                let probe = &probe;
                let files = &files;
                let on_source = &on_source;
                jobs.push(scope.spawn(move || {
                    let mut c = m.finish();
                    c.verify(probe, files);
                    on_source(&c.outcome());
                    c
                }));
            }
            jobs.into_iter()
                .map(|j| j.join().expect("stop worker panicked"))
                .collect()
        });

        let mut sources = Vec::with_capacity(collected.len());
        for mut c in collected {
            if c.bytes > 0 {
                // A source that was cut while paused has several files, and the
                // first one must not keep the plain name: `win.mp4` beside
                // `win-part2.mp4` reads as "the one without a number holds it
                // all". The count is only known now, so the names are settled
                // here — before the manifest is written, so what it records is
                // what is on disk (2026-10-27).
                if let Some((_, base)) = files.iter().find(|(id, _)| *id == c.id) {
                    number_parts(base, &mut c.segments);
                }
                self.take.set_duration(&c.id, c.seconds);
                self.take.set_frames(&c.id, c.frames);
                self.take
                    .set_start(&c.id, c.start.map(|t| t as f64 / 10_000_000.0));
                self.take.set_probed(&c.id, c.probed);
                let files: Vec<crate::take::SegmentRecord> = c
                    .segments
                    .iter()
                    .map(|s| crate::take::SegmentRecord {
                        file: s
                            .file
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        start: s.start as f64 / 10_000_000.0,
                        duration: s.seconds,
                        probed: s.probed,
                        frames: s.frames,
                    })
                    .collect();
                self.take.set_segments(&c.id, files);
            }
            sources.push(c.outcome());
        }

        self.take.manifest.duration = self
            .take
            .manifest
            .sources
            .iter()
            .filter_map(|s| s.duration)
            .reduce(f64::max);
        // Only when it happened: a take that was never paused should not carry
        // a `paused: 0.0` that invites the question of what it means.
        let paused = self.paused_total();
        if paused > 0.0 {
            self.take.manifest.paused = Some(paused);
        }
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

/// Rename a cut source's files so every part carries its number, then write
/// the settled names back into the report.
///
/// Same rule as `takekit_media::writer::number_all_parts`, applied here too
/// because the ffmpeg-backed sources build their segment list inside this
/// module and never go through the writer's own finaliser. It is a no-op when
/// the names are already right, so a path that did both is not double-numbered.
fn number_parts(base: &Path, segments: &mut [Seg]) {
    let mut files: Vec<PathBuf> = segments.iter().map(|s| s.file.clone()).collect();
    let moved = takekit_media::writer::number_all_parts(base, &mut files);
    // Assign back whatever the rename managed: an entry only changes when the
    // rename really happened, so the report cannot name a file that is absent.
    for (seg, file) in segments.iter_mut().zip(files) {
        seg.file = file;
    }
    if moved.is_err() && segments.len() > 1 {
        // The take is still complete, so this must not fail it — the manifest
        // will just keep the older `win.mp4` + `win-part2.mp4` shape.
        eprintln!("takekit: could not number the parts of {}", base.display());
    }
}

/// Read one finished ffmpeg child: its size on disk and, when it wrote
/// nothing, why not. The duration needs ffprobe and comes in `verify`.
fn collect_ffmpeg(r: &Running) -> Collected {
    let bytes = std::fs::metadata(&r.out).map(|m| m.len()).unwrap_or(0);
    if bytes == 0 {
        // The signature that names the failure (e.g. "Could not find output
        // pin") need not be in the last lines, so classification reads a wide
        // window; only the verbatim fallback quotes the tail.
        let wide = ffmpeg::tail(&r.log, 20);
        let device = r.capture.device_name();
        return Collected {
            id: r.id.clone(),
            bytes: 0,
            // Nothing was written, so its file has no place on the timeline to
            // claim.
            start: None,
            seconds: None,
            frames: None,
            size: None,
            probed: None,
            fps: r.fps,
            segments: Vec::new(),
            error: Some(match diagnose::classify(&wide, device) {
                Some(code) => code,
                None => {
                    let tail = ffmpeg::tail(&r.log, 3);
                    if tail.is_empty() {
                        diagnose::FFMPEG_DIED.to_string()
                    } else {
                        tail
                    }
                }
            }),
        };
    }
    Collected {
        id: r.id.clone(),
        bytes,
        start: r.start,
        seconds: None,
        frames: None,
        size: None,
        probed: None,
        fps: r.fps,
        segments: Vec::new(),
        error: None,
    }
}

struct Collected {
    id: String,
    bytes: u64,
    /// Take-time its file begins at, in 100 ns units.
    start: Option<i64>,
    seconds: Option<f64>,
    /// Frames the capture actually delivered. `None` when the source failed
    /// before counting could start; the output's slot count is
    /// `seconds x fps` regardless, since a held frame fills the gap.
    frames: Option<u64>,
    /// The size the file was written at. `None` when nothing was written or
    /// the backend cannot say (the ffmpeg path reports through ffprobe's
    /// duration only).
    size: Option<(u32, u32)>,
    /// ffprobe's own reading of the finished file. `None` when no binary is
    /// installed or ffprobe could not read the container.
    probed: Option<f64>,
    fps: u32,
    /// One entry per file the source produced. Empty on the ffmpeg path (a
    /// child writes one file and never splits); a single entry otherwise when
    /// the take was never paused.
    segments: Vec<Seg>,
    error: Option<String>,
}

/// One file of one source, with what ffprobe made of it.
struct Seg {
    file: PathBuf,
    start: i64,
    seconds: f64,
    frames: u64,
    probed: Option<f64>,
}

impl From<&takekit_media::writer::SegmentReport> for Seg {
    fn from(r: &takekit_media::writer::SegmentReport) -> Self {
        Self {
            file: r.file.clone(),
            start: r.start,
            seconds: r.seconds,
            frames: r.frames,
            probed: None,
        }
    }
}

impl Collected {
    /// Read the finished file with ffprobe — the second authority.
    ///
    /// The writer's own length comes from the clock it wrote with, so it cannot
    /// disagree with itself. The container is a different authority, and the
    /// only one that can say the file really is as long as the manifest claims.
    /// Skipped when ffmpeg is not installed (the Windows backend needs no
    /// binary), and then `probed` stays null: saying nothing is honest,
    /// guessing is not.
    fn verify(&mut self, probe: &Path, files: &[(String, PathBuf)]) {
        if self.bytes == 0 {
            return;
        }
        if self.segments.is_empty() {
            // The ffmpeg path: a child writes one file and never splits, so
            // ffprobe's reading is the only length this source will ever have.
            let Some(file) = files.iter().find(|(id, _)| *id == self.id).map(|(_, p)| p) else {
                return;
            };
            let probed = if probe.as_os_str().is_empty() {
                None
            } else {
                ffmpeg::duration(probe, file)
            };
            if self.seconds.is_none() {
                self.seconds = probed;
            }
            self.probed = probed;
            if let (Some(start), Some(seconds)) = (self.start, self.seconds) {
                self.segments.push(Seg {
                    file: file.clone(),
                    start,
                    seconds,
                    frames: self.frames.unwrap_or(0),
                    probed,
                });
            }
            return;
        }
        // In-process writer: every file gets its own reading. Checking only the
        // total would let a short segment hide behind a long one, and a paused
        // take is exactly where that happens.
        let mut total = 0.0;
        let mut complete = true;
        for seg in &mut self.segments {
            seg.probed = if probe.as_os_str().is_empty() {
                None
            } else {
                ffmpeg::duration(probe, &seg.file)
            };
            match seg.probed {
                Some(p) => total += p,
                None => complete = false,
            }
        }
        self.probed = complete.then_some(total);
    }

    /// Whether any one file disagrees with the manifest about its length.
    ///
    /// Per file rather than summed: a take that was paused has several, and a
    /// segment that came out short is invisible in the total.
    fn length_off(&self) -> bool {
        if self.segments.is_empty() {
            return crate::take::length_mismatch(self.seconds, self.probed, self.fps);
        }
        self.segments
            .iter()
            .any(|s| crate::take::length_mismatch(Some(s.seconds), s.probed, self.fps))
    }

    fn outcome(&self) -> Outcome {
        Outcome {
            id: self.id.clone(),
            start: self.start.map(|t| t as f64 / 10_000_000.0),
            seconds: self.seconds,
            bytes: self.bytes,
            frames: self.frames,
            size: self.size,
            probed: self.probed,
            error: self.error.clone(),
            warning: self.length_off().then(|| "len.mismatch".to_string()),
        }
    }
}

impl Recording {
    /// Stop every capture, drop every channel and join every writer — the
    /// order a failed start needs before the take folder is removed. A writer
    /// holds its file open while it drains and finalises, and on Windows an
    /// open file cannot be deleted: without the join the folder survives as a
    /// take with no manifest and a half-written source in it. `Drop` stops the
    /// captures but must not join (it cannot panic), which is why this lives
    /// beside it.
    ///
    /// `drain` is what makes the join possible: `JoinHandle::join` takes
    /// ownership, and after a failed start nothing is left to report, so the
    /// list is emptied rather than patched in place. The ffmpeg children go
    /// first — they hold the folder's files just as a writer does.
    fn shutdown_writers(&mut self) {
        for r in &mut self.running {
            ffmpeg::stop(&mut r.child);
        }
        for mut m in self.media.drain(..) {
            if let Some(handle) = m.handle.take() {
                let _ = handle.stop();
            }
            m.sender.take();
            if let Some(mut lane) = m.sound.take() {
                let _ = lane.stop();
            }
            let _ = m.writer.join();
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
    /// Where its file begins on the take's timeline, in seconds. `None` when
    /// nothing was written.
    pub start: Option<f64>,
    pub seconds: Option<f64>,
    pub bytes: u64,
    /// Frames the capture delivered (`takekit-media` sources; `None` on the
    /// ffmpeg path, where the child does not report). Compare against
    /// `seconds x fps` to see how much of the take is a held picture.
    pub frames: Option<u64>,
    /// The size the file was written at (`takekit-media` sources; `None` on
    /// the ffmpeg path). What a progress checklist shows as the resolution.
    pub size: Option<(u32, u32)>,
    /// What ffprobe measured in the finished file. Beside `seconds` rather
    /// than instead of it, so a reader can see the two authorities agree.
    pub probed: Option<f64>,
    /// Why nothing was written: the source's log tail, or a stand-in line when
    /// even the log came back empty.
    pub error: Option<String>,
    /// Recorded, but not quite what the manifest says — a code like
    /// `len.mismatch`, which the panel puts into words. Separate from `error`
    /// because the file is usable: it is the bookkeeping that is off.
    pub warning: Option<String>,
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
        validate_quality(&settings)?;
        validate_sources(&settings.sources)?;
        // Sound inside a picture's file is a Media Foundation mux: ffmpeg has
        // no WASAPI loopback and no way to put one stream's bytes into another
        // stream's container here. Refused by name rather than silently
        // recording a silent file — "the speaker was on and the file is mute"
        // is indistinguishable from a broken microphone.
        if settings.backend != Backend::Windows {
            if let Some(bad) = settings.sources.iter().find(|s| s.audio) {
                return Err(Error::Source(format!(
                    "{}: recording its sound into its own file needs the Windows backend \
                     (WGC + MF); ffmpeg has no WASAPI loopback — add --windows, or record \
                     the sound as a source of its own (--system-audio)",
                    bad.capture.device_name().unwrap_or("the desktop")
                )));
            }
        }
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
                Capture::Window {
                    hwnd, pid, title, ..
                } => {
                    if let Err(e) = crate::source::resolve_window(*hwnd, *pid) {
                        return Err(Error::Source(format!("{title}: {e}")));
                    }
                }
                Capture::Camera { device, .. } => {
                    camera_link(device).map_err(spawn_error)?;
                }
                Capture::Mic { device, .. } => {
                    // Same check the capture itself will make, surfaced here
                    // so an unplugged mic fails before the veil, not as a
                    // half-recorded take.
                    mic_endpoint(device).map_err(spawn_error)?;
                }
                Capture::SystemAudio { .. } => {
                    // Always addressable: loopback on the default render
                    // device, which Windows guarantees exists while any
                    // audio endpoint is present.
                }
                Capture::WindowAudio {
                    hwnd, pid, title, ..
                } => {
                    // Sound survives the window being covered or minimised
                    // (that is the point of per-process loopback) — only the
                    // window going away, or its handle being recycled by
                    // another process, ends it. No client area is required and
                    // nothing is restored: a sound source must not move the
                    // user's screen just to be checked.
                    if !crate::source::window_is_alive(*hwnd, *pid) {
                        return Err(Error::Source(format!(
                            "{title}: window {hwnd:#x} is gone or belongs to another process"
                        )));
                    }
                }
            }
        }
        Ok(Prepared {
            backend: Backend::Windows,
            ffmpeg: PathBuf::new(),
            // Not needed to record, but worth finding: it is what checks every
            // finished file's length against the manifest afterwards. Without
            // it `probed` stays null and the take says so, which is better
            // than a number nobody verified.
            probe: ffmpeg::locate()
                .map(|f| ffmpeg::ffprobe(&f))
                .unwrap_or_default(),
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

/// Refuse a quality the encoders cannot honour, before anything is created.
///
/// The bound lives in `encoder` (the module that turns the number into flags);
/// this is the gate every caller passes — CLI, panel and MCP alike — because
/// `-crf 200` is only rejected by ffmpeg *after* the take folder exists, and
/// the in-process path would just clamp it silently.
fn validate_quality(settings: &Settings) -> Result<()> {
    encoder::check_quality(settings.quality).map_err(Error::Quality)?;
    for src in &settings.sources {
        if let Some(q) = src.quality {
            encoder::check_quality(q)
                .map_err(|m| Error::Quality(format!("source {:?}: {m}", src.id)))?;
        }
    }
    Ok(())
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
            start: None,
            seconds,
            bytes,
            frames: None,
            size: None,
            probed: None,
            error: None,
            warning: None,
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
            start: Some(0),
            seconds: Some(1.0),
            frames: Some(30),
            size: Some((1920, 1080)),
            probed: Some(1.02),
            fps: 30,
            segments: Vec::new(),
            error: None,
        }
        .outcome();
        assert_eq!(sized.size, Some((1920, 1080)));
        assert_eq!(sized.start, Some(0.0));
        assert_eq!(sized.probed, Some(1.02));
        // 20 ms apart at 30 fps is the same number said twice.
        assert_eq!(sized.warning, None);

        let blank = Collected {
            id: "b".into(),
            bytes: 0,
            start: None,
            seconds: None,
            frames: None,
            size: None,
            probed: None,
            fps: 30,
            segments: Vec::new(),
            error: Some("boom".into()),
        }
        .outcome();
        assert_eq!(blank.size, None);
        assert_eq!(blank.error.as_deref(), Some("boom"));
    }

    /// A file that really is a different length from the manifest is flagged,
    /// not rounded away — the panel turns the code into a sentence and the
    /// manifest keeps both numbers.
    #[test]
    fn a_container_that_disagrees_is_flagged() {
        let c = Collected {
            id: "a".into(),
            bytes: 10,
            start: Some(0),
            seconds: Some(10.0),
            frames: Some(300),
            size: Some((1920, 1080)),
            probed: Some(10.6),
            fps: 30,
            segments: Vec::new(),
            error: None,
        };
        assert_eq!(c.outcome().warning.as_deref(), Some("len.mismatch"));
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

    /// Sound and picture in one file is a Media Foundation mux. The ffmpeg
    /// backend has no WASAPI loopback to feed it, so a source asking for both
    /// has to be refused by name — silently recording a mute file looks
    /// exactly like a broken microphone.
    #[test]
    fn sound_inside_the_picture_needs_the_windows_backend() {
        let mut desk = source("screen");
        desk.audio = true;
        let settings = Settings {
            sources: vec![desk],
            out_root: std::env::temp_dir(),
            quality: 20,
            encoder: None,
            marks: false,
            backend: Backend::Ffmpeg,
        };
        let err = match Session::prepare(settings) {
            Ok(_) => panic!("the ffmpeg backend accepted a source with sound in its file"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("Windows backend"), "{err}");

        // The same source on the Windows backend is not refused here — it is
        // what the panel records every day.
        let mut desk = source("screen");
        desk.audio = true;
        let settings = Settings {
            sources: vec![desk],
            out_root: std::env::temp_dir(),
            quality: 20,
            encoder: None,
            marks: false,
            backend: Backend::Windows,
        };
        match Session::prepare(settings) {
            Ok(_) => {}
            // A desktop source resolves without touching any hardware, so a
            // failure here would be a real regression and must not pass as
            // "expected in CI".
            Err(e) => panic!("the Windows backend refused a desktop take: {e}"),
        }
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

    /// A quality the encoders cannot honour must be refused by `prepare`, not
    /// by ffmpeg after the take folder exists — and the source override is
    /// held to the same domain as the session-wide value.
    #[test]
    fn an_out_of_range_quality_stops_the_take_before_it_starts() {
        let settings = |quality: u8, pinned: Option<u8>| {
            let mut s = source("screen");
            s.quality = pinned;
            Settings {
                sources: vec![s],
                out_root: std::env::temp_dir(),
                quality,
                encoder: None,
                marks: false,
                backend: Backend::Windows,
            }
        };

        let err = validate_quality(&settings(52, None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("0..=51") && err.contains("got 52"), "{err}");

        let err = validate_quality(&settings(20, Some(200)))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("\"screen\"") && err.contains("got 200"),
            "{err}"
        );

        // Both ends of the domain stay legal, and `None` never fails.
        assert!(validate_quality(&settings(0, None)).is_ok());
        assert!(validate_quality(&settings(51, Some(51))).is_ok());
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
