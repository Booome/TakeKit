//! `takekit` — command-line front end.
//!
//! The GUI will drive `takekit-core` directly; this binary exists so the core
//! can be exercised without any UI.

use std::io::BufRead;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use takekit_core::encoder::Encoder;
use takekit_core::ffmpeg;
use takekit_core::session::{Session, Settings};
use takekit_core::source::{Backend, Capture, Crop, Engine, Source, WindowItem};
use takekit_media::{
    AudioWriter, CameraCapture, DeviceList, LoopbackCapture, LoopbackTarget, MediaFrame,
    MicCapture, ScreenCapture, ScreenTarget, TakeClock, writer,
};

mod client;
mod ctl;
mod mcp;

#[derive(Parser)]
#[command(
    name = "takekit",
    version,
    about = "Record several sources at once into one take"
)]
struct Cli {
    /// Point at a specific ffmpeg instead of the PATH lookup
    #[arg(long, global = true, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List windows and capture devices
    List {
        /// Only windows whose title contains this
        #[arg(default_value = "")]
        filter: String,
    },
    /// Record one take
    Record(RecordArgs),
    /// Capture through the Windows APIs instead of ffmpeg (experimental)
    Media(MediaArgs),
    /// Drive the running panel (docs/agent-control.md)
    #[command(subcommand)]
    Ctl(ctl::Ctl),
    /// Speak MCP over stdio, for AI agents
    Mcp,
}

#[derive(clap::Args)]
struct RecordArgs {
    /// Record a window: a substring of its title, or "#12345" for a window
    /// handle (which survives two windows sharing a title)
    #[arg(long, value_name = "TITLE_OR_#HWND")]
    window: Option<String>,

    /// Record the whole desktop instead of a window
    #[arg(long, conflicts_with = "window")]
    desktop: bool,

    /// Capture through the Windows APIs (WGC + Media Foundation) instead of
    /// ffmpeg children. Keeps recording a window that sits behind others.
    /// Records one monitor: the first, or the one a --crop region sits on
    #[arg(long)]
    windows: bool,

    /// Crop the captured area: "top,bottom,left,right" in pixels
    #[arg(long, value_name = "T,B,L,R", default_value = "0,0,0,0")]
    crop: CropArg,

    /// Also record a video device (see `takekit list`)
    #[arg(long, value_name = "DEVICE")]
    cam: Option<String>,

    /// Also record a microphone, by name or endpoint id (see
    /// `takekit media --list` for the names)
    #[arg(long, value_name = "DEVICE")]
    mic: Option<String>,

    /// Also record what the speakers play (system sound, WASAPI loopback)
    #[arg(long)]
    system_audio: bool,

    /// Write the window's own sound into the same file as its picture
    /// (needs --window; per-process WASAPI loopback)
    #[arg(long)]
    window_sound: bool,

    /// Write the desktop's sound into the same file as its picture (system
    /// sound, WASAPI loopback — the whole machine, since Windows has no
    /// per-monitor audio; not with --window)
    #[arg(long)]
    desktop_sound: bool,

    /// Stop after this many seconds (otherwise press Enter)
    #[arg(short, long, default_value_t = 0.0)]
    duration: f64,

    /// Output root; a timestamped directory is created inside
    #[arg(short, long, default_value = "takes")]
    out: PathBuf,

    /// Capture rate in frames per second
    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// auto | x264 | nvenc | qsv | amf — auto probes each and picks the
    /// best that actually works
    #[arg(long, value_enum, default_value_t = EncoderArg::Auto)]
    encoder: EncoderArg,

    /// Quality 0..=51: roughly CRF for x264, CQ/QP for hardware encoders
    /// (0 = lossless). Out of range is refused before the take folder exists.
    #[arg(long, default_value_t = takekit_core::encoder::DEFAULT_QUALITY)]
    quality: u8,

    /// Mouse effects (the panel's "鼠标效果"): draw click ripples and drag
    /// trails into the recording (MouseShow)
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    marks: bool,
}

#[derive(clap::Args)]
struct MediaArgs {
    /// List the screens, windows, cameras and microphones the APIs can see
    #[arg(long)]
    list: bool,

    /// Record a screen: the one-based monitor index
    #[arg(long, value_name = "INDEX")]
    screen: Option<usize>,

    /// Record a window by handle. Unlike the ffmpeg backend this one keeps
    /// recording while the window sits behind others
    #[arg(long, value_name = "HWND", value_parser = parse_hwnd)]
    window: Option<isize>,

    /// Record a camera: its name, or its symbolic link
    #[arg(long, value_name = "CAMERA")]
    camera: Option<String>,

    /// Record a microphone: its name, or its endpoint id
    #[arg(long, value_name = "MIC")]
    mic: Option<String>,

    /// Record what the speakers play (system sound)
    #[arg(long)]
    system_audio: bool,

    /// Record only one window's sound: its handle, e.g. --window-audio 0x1234
    #[arg(long, value_name = "HWND", value_parser = parse_hwnd)]
    window_audio: Option<isize>,

    /// Stop after this many seconds
    #[arg(short, long, default_value_t = 5.0)]
    duration: f64,

    /// Where the files go
    #[arg(short, long, default_value = "takes-media")]
    out: PathBuf,

    /// Capture rate in frames per second
    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Video bitrate in kbit/s
    #[arg(long, default_value_t = 12_000)]
    bitrate: u32,

    /// Sample the picture at --fps instead of writing only when it changed.
    /// The file is a fixed-frame-rate either way — the encoder repeats frames
    /// to fill a gap — so this only decides whether a slot shows the picture
    /// as it was at the slot or the last one that arrived
    #[arg(long)]
    cfr: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum EncoderArg {
    Auto,
    X264,
    Nvenc,
    Qsv,
    Amf,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CropArg(pub(crate) Crop);

impl std::str::FromStr for CropArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<u32> = s
            .split(',')
            .map(|p| p.trim().parse::<u32>().map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        match parts.as_slice() {
            [t, b, l, r] => Ok(CropArg(Crop {
                top: *t,
                bottom: *b,
                left: *l,
                right: *r,
            })),
            _ => Err("expected top,bottom,left,right".into()),
        }
    }
}

// ── stopping from the console ───────────────────────────────────────────────
//
// The default console handler kills the process outright. On the
// Windows-API backend that is data loss, not an interruption: the MP4 is
// finalised by `stop`, so a killed process leaves an unplayable file with
// no `take.json` and no history row. Ctrl-C therefore flips this flag and
// the wait loops stop the take properly.

#[cfg(windows)]
static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
unsafe extern "system" fn on_console_signal(ctrltype: u32) -> windows::core::BOOL {
    // CTRL_C_EVENT = 0, CTRL_BREAK_EVENT = 1: claim these so the wait loops
    // can stop the take properly instead of the process dying mid-mux.
    // Everything else (close / logoff / shutdown) returns FALSE, i.e. "not
    // handled", so the default handler still runs — the process is on its
    // way out either way, and the ~5s close grace period is not enough to
    // finalise an MP4. Claiming those would suppress the default handler
    // while still being killed, which is strictly worse.
    match ctrltype {
        0 | 1 => {
            STOP_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
            windows::core::BOOL(1)
        }
        _ => windows::core::BOOL(0),
    }
}

/// Ask the console to report Ctrl-C instead of killing the process. Once
/// per recording command; harmless to call when it is already armed.
#[cfg(windows)]
fn arm_console_stop() {
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    // SAFETY: the handler only touches an atomic bool and returns "handled".
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(on_console_signal), true);
    }
}

#[cfg(not(windows))]
fn arm_console_stop() {}

fn stop_requested() -> bool {
    #[cfg(windows)]
    {
        STOP_REQUESTED.load(std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Wait out `secs`, returning early when the console asks to stop. Sliced
/// into 100ms steps so a Ctrl-C during a long take is noticed at once.
fn wait_or_stop(secs: f64) {
    let mut left = std::time::Duration::from_secs_f64(secs.max(0.0));
    while left > std::time::Duration::ZERO && !stop_requested() {
        let slice = left.min(std::time::Duration::from_millis(100));
        std::thread::sleep(slice);
        left = left.saturating_sub(slice);
    }
}

fn main() {
    let cli = Cli::parse();
    // Global override, honoured by every `ffmpeg::locate` below (the panel has
    // its own setting for this; the CLI takes the flag).
    if let Some(path) = cli.ffmpeg {
        ffmpeg::set_path(Some(path));
    }
    let result = match cli.command {
        Command::List { filter } => list(&filter),
        Command::Record(args) => record(args),
        Command::Media(args) => media(args),
        Command::Ctl(cmd) => ctl::run(cmd),
        Command::Mcp => mcp::run(),
    };
    if let Err(e) = result {
        // A `diagnose` code becomes its English sentence here; anything else
        // is already the sentence.
        eprintln!("takekit: {}", takekit_core::diagnose::explain(&e));
        std::process::exit(1);
    }
}

fn list(filter: &str) -> Result<(), String> {
    let ff = ffmpeg::locate().ok_or_else(|| takekit_core::session::Error::NoFfmpeg.to_string())?;

    println!("windows:");
    for w in takekit_core::source::list_windows(filter, None) {
        println!(
            "  [{}] {}   [{}x{} at {},{}]",
            w.hwnd, w.title, w.rect.w, w.rect.h, w.rect.x, w.rect.y
        );
    }

    println!("\ncapture devices:");
    // The merged enumeration (dshow ∪ MF), the same list the panel's picker
    // and the agent's control surface show. Tags: which backends can open it,
    // and `(no signal)` when DirectShow reports a registered filter with no
    // output pin — a virtual camera whose host app is off.
    let devs = takekit_core::devices::cameras(Some(&ff));
    if devs.is_empty() {
        println!("  (none)");
    }
    for d in devs {
        let status = if d.live == Some(false) {
            "  (no signal)"
        } else {
            ""
        };
        let backends = match (d.dshow, d.mf) {
            (true, true) => "dshow+mf",
            (true, false) => "dshow",
            (false, true) => "mf",
            (false, false) => "?",
        };
        println!("  {}{}  [{}]", d.name, status, backends);
    }
    Ok(())
}

/// What `--window` names: a title substring, or an explicit handle.
#[derive(Debug, PartialEq, Eq)]
enum WindowTarget {
    Handle(isize),
    Title(String),
}

fn parse_window_target(spec: &str) -> Result<WindowTarget, String> {
    match spec.strip_prefix('#') {
        Some(digits) => digits
            .trim()
            .parse::<isize>()
            .map(WindowTarget::Handle)
            .map_err(|_| format!("\"{spec}\" is not a window handle")),
        None => Ok(WindowTarget::Title(spec.to_string())),
    }
}

fn window_target(spec: &str) -> Result<WindowItem, String> {
    match parse_window_target(spec)? {
        WindowTarget::Handle(hwnd) => takekit_core::source::window_by_hwnd(hwnd),
        WindowTarget::Title(needle) => takekit_core::source::find_window(&needle),
    }
}

/// What `--desktop` records: the whole virtual screen, or the region `--crop`
/// carves out of it. Without this the CLI accepted `--crop` and silently
/// recorded everything.
fn desktop_capture(crop: Crop) -> Capture {
    // The region is the crop: `rect` is where the desktop grab starts and how
    // much of it to take, so storing the edges as well would trim twice.
    if crop.is_zero() {
        Capture::desktop(None)
    } else {
        Capture::desktop(Some(
            crop.cropped(takekit_core::source::virtual_screen_rect()),
        ))
    }
}

fn record(args: RecordArgs) -> Result<(), String> {
    arm_console_stop();
    // `Duration::from_secs_f64` panics on NaN, infinity and values past
    // `u64::MAX`; a day is far past any real take, so it doubles as the cap.
    if !args.duration.is_finite() || !(0.0..=86_400.0).contains(&args.duration) {
        return Err(format!(
            "--duration must be between 0 and 86400 seconds (got {})",
            args.duration
        ));
    }
    let mut sources = Vec::new();
    if args.window_sound && args.window.is_none() {
        return Err("--window-sound records a window's sound, so it needs --window".into());
    }
    if args.desktop_sound && args.window.is_some() {
        return Err(
            "--desktop-sound records the desktop's sound; drop --window, or use --window-sound"
                .into(),
        );
    }
    if let Some(target) = &args.window {
        let win = window_target(target)?;
        // Called after its app, like the panel does (2026-10-27): the file in
        // the take folder should say which app was recorded. No collision check
        // is needed because `--window` takes one window.
        let mut source = Source::new(
            win.app.clone(),
            Capture::Window {
                hwnd: win.hwnd,
                // The handle alone can be recycled onto another app's window;
                // the process makes "this window" mean something.
                pid: win.pid,
                title: win.title,
                crop: args.crop.0,
            },
            args.fps,
        );
        // The sound of this window goes into this source's own file, not into
        // one of its own: what a user picked is a window, sound included.
        source.audio = args.window_sound;
        sources.push(source);
    } else {
        let mut source = Source::new("screen", desktop_capture(args.crop.0), args.fps);
        // Windows has no per-monitor sound: what the desktop source carries is
        // everything the speakers play, the same loopback the `--system-audio`
        // source records — but in this source's own file, behind its picture.
        source.audio = args.desktop_sound;
        sources.push(source);
    }
    if let Some(device) = &args.cam {
        sources.push(Source::new(
            "cam",
            Capture::Camera {
                device: device.clone(),
                crop: args.crop.0,
            },
            args.fps,
        ));
    }
    // Sound sources are in-process on either backend (ffmpeg has no WASAPI
    // loopback), so the same flags mean the same thing with and without
    // `--windows`. Added here rather than in the media harness so a long
    // multi-source take lands a take.json like any other record run.
    if let Some(device) = &args.mic {
        sources.push(Source::new(
            "mic",
            Capture::Mic {
                device: device.clone(),
            },
            args.fps,
        ));
    }
    if args.system_audio {
        sources.push(Source::new("sys", Capture::SystemAudio {}, args.fps));
    }

    let settings = Settings {
        sources,
        out_root: args.out.clone(),
        quality: args.quality,
        encoder: match args.encoder {
            EncoderArg::Auto => None,
            EncoderArg::X264 => Some(Encoder::X264),
            EncoderArg::Nvenc => Some(Encoder::Nvenc),
            EncoderArg::Qsv => Some(Encoder::Qsv),
            EncoderArg::Amf => Some(Encoder::Amf),
        },
        marks: args.marks,
        backend: if args.windows {
            takekit_core::source::Backend::Windows
        } else {
            takekit_core::source::Backend::Ffmpeg
        },
    };

    let ids: Vec<String> = settings.sources.iter().map(|s| s.id.clone()).collect();

    let prepared = Session::prepare(settings).map_err(|e| e.to_string())?;
    if prepared.backend() == Backend::Windows {
        // The engine and the encoder pick are both ffmpeg concepts; on this
        // path neither was consulted and printing them would be a lie.
        println!("backend: Windows APIs (WGC + Media Foundation)");
    } else {
        let engine = prepared.engine();
        let encoder = prepared.encoder();
        if engine == Engine::Gdi {
            println!("note: capturing with gdigrab, not Desktop Duplication");
        }
        println!(
            "encoder: {}{}",
            encoder.name(),
            if encoder.is_hardware() {
                " (hardware)"
            } else {
                ""
            }
        );
    }

    let rec = prepared.start().map_err(|e| e.to_string())?;
    println!("take   : {}", rec.dir().display());
    for id in &ids {
        println!("  {id} -> {}", rec.path_for(id).display());
    }

    // Without a duration the take waits for Enter. A pipe or an agent's
    // stdin hits EOF immediately, which used to record a 0-second take and
    // report it as a success.
    if args.duration <= 0.0 && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let _ = rec.stop();
        return Err("no --duration and stdin is not a terminal: pass --duration".to_string());
    }

    if args.duration > 0.0 {
        wait_or_stop(args.duration);
    } else {
        println!("press Enter to stop …");
        // Enter is read on its own thread so the wait stays free to notice a
        // Ctrl-C: a blocking read_line would sit through it, and the take
        // would be lost instead of stopped.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            let _ = tx.send(());
        });
        while rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err()
        {
            if stop_requested() {
                break;
            }
        }
    }
    if stop_requested() {
        println!("stopping on Ctrl-C …");
    }

    let finished = rec.stop().map_err(|e| e.to_string())?;
    for outcome in &finished.sources {
        match (&outcome.error, outcome.seconds) {
            (Some(err), _) => println!(
                "  {}: FAILED — {}",
                outcome.id,
                takekit_core::diagnose::explain(err)
            ),
            (None, Some(d)) => {
                println!("  {}: {d:.3}s  ({:.1} MB)", outcome.id, outcome.megabytes())
            }
            (None, None) => println!("  {}: (duration unknown)", outcome.id),
        }
    }
    if finished.sources.len() > 1 {
        if let Some(d) = finished.delta() {
            println!("duration delta: {d:+.3}s");
        }
    }
    Ok(())
}

/// Record through the Windows capture APIs: Windows Graphics Capture for a
/// screen, Media Foundation for a camera and a microphone, Media Foundation
/// again to encode. No ffmpeg involved.
/// One audio writer started on its own thread: the capture's end of the
/// frame channel, the end its format arrives on, and the thread itself.
type AudioWriterStart = (
    std::sync::mpsc::SyncSender<MediaFrame>,
    std::sync::mpsc::Sender<(u32, u32)>,
    std::thread::JoinHandle<Result<(String, u64), String>>,
);

/// One audio writer on its own thread — Media Foundation objects are not
/// `Send`, and a capture only learns its format once it runs, so the rate
/// and channel count travel to the writer over a channel before the file
/// can be opened. The same shape the core's `write_audio` uses: a device
/// harness must not grow a second opinion about how an AAC file is written.
///
/// Returns the capture's end of the frame channel, the format's end, and
/// the writer thread; the caller starts the capture and hands the format
/// over the moment it reports one.
fn open_audio(name: &str, path: PathBuf, bitrate: u32) -> AudioWriterStart {
    use std::sync::mpsc;
    use std::thread;

    // Same bounded queue the core's `start_audio_media` uses; a full one drops
    // the chunk (see `takekit_media::offer`).
    let (tx, rx) = mpsc::sync_channel::<MediaFrame>(takekit_media::AUDIO_QUEUE);
    let (format_tx, format_rx) = mpsc::channel();
    let name = name.to_string();
    let handle = thread::spawn(move || -> Result<(String, u64), String> {
        let (rate, channels) = format_rx
            .recv()
            .map_err(|_| format!("the {name} capture never reported a format"))?;
        let mut writer = AudioWriter::create(&path, rate, channels, bitrate).map_err(string_of)?;
        let mut delivered = 0u64;
        while let Ok(frame) = rx.recv() {
            if let MediaFrame::Audio(chunk) = frame {
                delivered += 1;
                writer.write(&chunk).map_err(string_of)?;
            }
        }
        writer.finish().map_err(string_of)?;
        Ok((name, delivered))
    });
    (tx, format_tx, handle)
}

fn media(args: MediaArgs) -> Result<(), String> {
    arm_console_stop();
    use std::sync::mpsc;
    use std::thread;

    if args.list {
        return media_list();
    }
    let wanted = args.screen.is_some()
        || args.window.is_some()
        || args.camera.is_some()
        || args.mic.is_some()
        || args.system_audio
        || args.window_audio.is_some();
    if !wanted {
        return Err("nothing to record: pass --screen, --window, --camera, --mic, --system-audio or --window-audio".into());
    }
    if !args.duration.is_finite() || !(0.1..=86_400.0).contains(&args.duration) {
        return Err(format!(
            "--duration must be between 0.1 and 86400 seconds (got {})",
            args.duration
        ));
    }

    std::fs::create_dir_all(&args.out).map_err(|e| format!("{}: {e}", args.out.display()))?;
    // The same local-time stamp the panel and `record` name their takes with
    // (shared from the core): this harness once computed the date from the
    // UNIX epoch by hand, which is UTC — on a UTC+8 machine its folders read
    // eight hours off the panel's history.
    let stamp = takekit_core::take::timestamp();
    let dir = args.out.join(stamp);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    let devices = takekit_media::devices::list();
    // One origin for every source, so several files can be cut against each
    // other: a source whose first frame arrives late keeps that gap visible
    // instead of silently starting its own timeline at zero.
    let clock = TakeClock::start();
    // Latched when the take stops so every file ends on the same instant of the
    // take's timeline — otherwise the channels differ by however long each
    // writer's drain took.
    let end_hint = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let mut writers: Vec<thread::JoinHandle<Result<(String, u64), String>>> = Vec::new();
    let mut senders: Vec<mpsc::SyncSender<MediaFrame>> = Vec::new();

    // Each source gets its own channel and its own writer thread: a capture
    // must never block on an encoder, and the files are separate anyway.
    //
    // The output is a fixed-frame-rate MP4 either way: the encoder expands a
    // held sample into repeated frames. Without `--cfr` a frame is written
    // when it arrives with the gap it really had; with `--cfr` the picture is
    // sampled at --fps. Same frames out, different sampling in.
    type SourceWriter = thread::JoinHandle<Result<(String, u64), String>>;
    let open = |name: &str| -> (mpsc::SyncSender<MediaFrame>, SourceWriter) {
        let (tx, rx) = mpsc::sync_channel::<MediaFrame>(takekit_media::VIDEO_QUEUE);
        let path = dir.join(format!("{name}.mp4"));
        let name = name.to_string();
        let fps = args.fps.max(1);
        let bitrate = args.bitrate;
        let cfr = args.cfr;
        let writer_end = end_hint.clone();
        // A copy per source: the closure runs once for every device the harness
        // was given, and each writer thread needs its own handle on the one
        // shared (pausable) clock.
        let writer_clock = clock.clone();
        let handle = thread::spawn(move || -> Result<(String, u64), String> {
            // The one writer loop again (`write_stream`): --cfr picks the
            // keep-latest sampler, everything else writes what arrives.
            let sampling = if cfr {
                writer::Sampling::KeepLatest
            } else {
                writer::Sampling::EveryFrame
            };
            let report = writer::write_stream(
                rx,
                &path,
                writer::Encode {
                    fps,
                    bitrate,
                    // The device harness records the device, not a crop of it.
                    crop: writer::CropBox::default(),
                    sampling,
                    // One device, one take: zero is the take's origin.
                    origin: writer::Origin::Take,
                    first: None,
                },
                writer_clock,
                writer_end,
            )
            .map_err(string_of)?;
            Ok((name, report.frames))
        });
        (tx, handle)
    };

    let screen = match args.screen {
        Some(index) => {
            let (tx, handle) = open("screen");
            senders.push(tx.clone());
            writers.push(handle);
            Some(
                ScreenCapture::start(ScreenTarget::Monitor(index), args.fps, tx, clock.clone())
                    .map_err(string_of)?,
            )
        }
        None => None,
    };

    let window = match args.window {
        Some(hwnd) => {
            let (tx, handle) = open("window");
            senders.push(tx.clone());
            writers.push(handle);
            Some(
                ScreenCapture::start(ScreenTarget::Window(hwnd), args.fps, tx, clock.clone())
                    .map_err(string_of)?,
            )
        }
        None => None,
    };

    let camera = match &args.camera {
        Some(name) => {
            let link = resolve_camera(&devices, name)?;
            let (tx, handle) = open("camera");
            senders.push(tx.clone());
            writers.push(handle);
            Some(CameraCapture::start(&link, args.fps, tx, clock.clone()).map_err(string_of)?)
        }
        None => None,
    };

    let mic = match &args.mic {
        Some(name) => {
            let endpoint = resolve_mic(&devices, name)?;
            let (tx, format_tx, handle) =
                open_audio("mic", dir.join("mic.mp4"), args.bitrate.min(256));
            let capture =
                MicCapture::start(&endpoint, tx.clone(), clock.clone()).map_err(string_of)?;
            let format = capture.format();
            format_tx
                .send((format.sample_rate, format.channels))
                .map_err(|_| "the microphone writer is gone".to_string())?;
            senders.push(tx);
            writers.push(handle);
            Some(capture)
        }
        None => None,
    };

    // Loopback: what the speakers play, either all of it or just one window's
    // process tree. Same shape as the microphone — the writer needs the
    // format the capture settles on, so it travels over a channel.
    let mut loopback = None;
    if args.system_audio || args.window_audio.is_some() {
        let target = match args.window_audio {
            Some(hwnd) => {
                let pid = takekit_media::loopback::window_pid(hwnd)
                    .ok_or_else(|| format!("no process owns window {hwnd:#x}"))?;
                LoopbackTarget::ProcessTree(pid)
            }
            None => LoopbackTarget::All,
        };
        let (tx, format_tx, handle) = open_audio(
            "loopback",
            dir.join(if target == LoopbackTarget::All {
                "system-audio.mp4"
            } else {
                "window-audio.mp4"
            }),
            args.bitrate.min(256),
        );
        let capture =
            LoopbackCapture::start(target, tx.clone(), clock.clone()).map_err(string_of)?;
        let format = capture.format();
        format_tx
            .send((format.sample_rate, format.channels))
            .map_err(|_| "the loopback writer is gone".to_string())?;
        senders.push(tx);
        writers.push(handle);
        loopback = Some(capture);
    }

    println!("recording for {:.1}s into {}", args.duration, dir.display());
    wait_or_stop(args.duration);
    if stop_requested() {
        println!("stopping on Ctrl-C …");
    }

    // The take ends now, for every channel alike.
    end_hint.store(clock.now_100ns(), std::sync::atomic::Ordering::SeqCst);

    // Stop the captures before the writers: dropping a sender is what tells its
    // writer thread there are no more frames, and a capture stopped afterwards
    // would push into a closed channel.
    if let Some(c) = screen {
        c.stop().map_err(string_of)?;
    }
    if let Some(c) = window {
        c.stop().map_err(string_of)?;
    }
    if let Some(c) = camera {
        c.stop().map_err(string_of)?;
    }
    if let Some(c) = mic {
        c.stop().map_err(string_of)?;
    }
    if let Some(c) = loopback {
        c.stop().map_err(string_of)?;
    }
    drop(senders);

    let mut failed = 0;
    for w in writers {
        match w.join() {
            Ok(Ok((name, frames))) => println!("  {name}: {frames} frames delivered"),
            Ok(Err(e)) => {
                failed += 1;
                eprintln!("takekit: {e}");
            }
            Err(_) => failed += 1,
        }
    }
    if failed > 0 {
        return Err(format!("{failed} source(s) failed"));
    }
    println!("saved {}", dir.display());
    Ok(())
}

fn parse_hwnd(s: &str) -> Result<isize, String> {
    let (digits, radix) = match s
        .trim()
        .strip_prefix("0x")
        .or_else(|| s.trim().strip_prefix("0X"))
    {
        Some(rest) => (rest, 16),
        None => (s.trim(), 10),
    };
    isize::from_str_radix(digits, radix).map_err(|_| format!("not a window handle: {s}"))
}

fn media_list() -> Result<(), String> {
    let devices = takekit_media::devices::list();
    println!("screens:");
    for s in &devices.screens {
        println!(
            "  [{}] {} {}x{} @{}Hz",
            s.index, s.name, s.width, s.height, s.refresh
        );
    }
    println!("windows:");
    // The same list the panel offers, since a window is addressed by the same
    // handle either way — and unlike the ffmpeg backend this one keeps
    // recording while the window is behind another.
    let windows = takekit_core::source::list_windows("", None);
    if windows.is_empty() {
        println!("  (none)");
    }
    for w in &windows {
        let title: String = w.title.chars().take(56).collect();
        println!("  [{:#x}] {} {}x{}", w.hwnd, title, w.rect.w, w.rect.h);
    }
    println!("cameras:");
    // The merged enumeration (dshow ∪ MF), tagged by which backend can open
    // each one — same list as `takekit list`, the panel's picker and the
    // agent's control surface. `media` itself records on the windows backend
    // and needs an `mf` camera.
    let cams = takekit_core::devices::cameras(takekit_core::ffmpeg::locate().as_deref());
    if cams.is_empty() {
        println!("  (none)");
    }
    for c in &cams {
        let status = if c.live == Some(false) {
            "  (no signal)"
        } else {
            ""
        };
        let backends = match (c.dshow, c.mf) {
            (true, true) => "dshow+mf",
            (true, false) => "dshow",
            (false, true) => "mf",
            (false, false) => "?",
        };
        println!("  {}{}  [{}]", c.name, status, backends);
    }
    println!("microphones:");
    if devices.mics.is_empty() {
        println!("  (none)");
    }
    for m in &devices.mics {
        println!("  {}", m.name);
    }
    Ok(())
}

fn resolve_camera(devices: &DeviceList, name: &str) -> Result<String, String> {
    if let Some(c) = devices
        .cameras
        .iter()
        .find(|c| c.name == name || c.link == name)
    {
        return Ok(c.link.clone());
    }
    // Not MF-visible. If the merged list knows it as a DirectShow-only
    // camera, say that instead of "no such device" — same guidance the
    // other doors give (the `err.cam-not-mf` sentence).
    let known = takekit_core::devices::cameras(takekit_core::ffmpeg::locate().as_deref())
        .into_iter()
        .find(|c| c.name == name);
    match known {
        Some(c) if !c.mf => Err(takekit_core::diagnose::explain(&format!(
            "{}|{name}",
            takekit_core::diagnose::CAM_NOT_MF
        ))),
        _ => Err(format!("no camera \"{name}\"; see `takekit media --list`")),
    }
}

fn resolve_mic(devices: &DeviceList, name: &str) -> Result<String, String> {
    devices
        .mics
        .iter()
        .find(|m| m.name == name || m.endpoint == name)
        .map(|m| m.endpoint.clone())
        .ok_or_else(|| format!("no microphone \"{name}\"; see `takekit media --list`"))
}

fn string_of<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_prefix_names_a_handle() {
        assert_eq!(
            parse_window_target("#12345"),
            Ok(WindowTarget::Handle(12345))
        );
    }

    #[test]
    fn anything_else_is_a_title_substring() {
        assert_eq!(
            parse_window_target("Editor"),
            Ok(WindowTarget::Title("Editor".to_string()))
        );
    }

    #[test]
    fn a_bad_handle_is_rejected_before_looking_anything_up() {
        let err = parse_window_target("#not-a-number").unwrap_err();
        assert!(err.contains("is not a window handle"), "{err}");
    }

    #[test]
    fn a_crop_takes_four_trimmed_numbers() {
        let crop: CropArg = " 10, 20 ,30,40 ".parse().unwrap();
        assert_eq!(crop.0.top, 10);
        assert_eq!(crop.0.bottom, 20);
        assert_eq!(crop.0.left, 30);
        assert_eq!(crop.0.right, 40);
    }

    #[test]
    fn a_crop_needs_exactly_four_numbers() {
        let err = "1,2,3".parse::<CropArg>().unwrap_err();
        assert!(err.contains("top,bottom,left,right"), "{err}");
        let err = "1,2,3,4,5".parse::<CropArg>().unwrap_err();
        assert!(err.contains("top,bottom,left,right"), "{err}");
    }

    #[test]
    fn a_crop_rejects_non_numbers() {
        assert!("1,x,3,4".parse::<CropArg>().is_err());
    }

    #[test]
    fn a_desktop_crop_turns_into_a_region() {
        let crop = Crop {
            top: 10,
            bottom: 0,
            left: 0,
            right: 0,
        };
        match desktop_capture(crop) {
            Capture::Desktop { rect: Some(r), .. } => {
                assert_eq!(r.y, takekit_core::source::virtual_screen_rect().y + 10);
            }
            other => panic!("expected a desktop region, got {other:?}"),
        }
        assert!(matches!(
            desktop_capture(Crop::default()),
            Capture::Desktop { rect: None, .. }
        ));
    }
}
