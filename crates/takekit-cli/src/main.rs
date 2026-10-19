//! `takekit` — command-line front end.
//!
//! The GUI will drive `takekit-core` directly; this binary exists so the core
//! can be exercised without any UI.

use std::io::BufRead;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use takekit_core::encoder::Encoder;
use takekit_core::ffmpeg;
use takekit_core::session::{Session, Settings};
use takekit_core::source::{Capture, Crop, Engine, Source, WindowItem};
use takekit_media::{
    AudioWriter, CameraCapture, DeviceList, MediaFrame, MicCapture, ScreenCapture, ScreenTarget,
    VideoFrame, VideoWriter, writer,
};

#[derive(Parser)]
#[command(
    name = "takekit",
    version,
    about = "Record several sources at once into one take"
)]
struct Cli {
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
}

#[derive(clap::Args)]
struct RecordArgs {
    /// Record a window, matched by a substring of its title
    #[arg(long, value_name = "TITLE")]
    window: Option<String>,

    /// Record the whole desktop instead of a window
    #[arg(long, conflicts_with = "window")]
    desktop: bool,

    /// Crop the captured area: "top,bottom,left,right" in pixels
    #[arg(long, value_name = "T,B,L,R", default_value = "0,0,0,0")]
    crop: CropArg,

    /// Also record a video device (see `takekit list`)
    #[arg(long, value_name = "DEVICE")]
    cam: Option<String>,

    /// Stop after this many seconds (otherwise press Enter)
    #[arg(short, long, default_value_t = 0.0)]
    duration: f64,

    /// Output root; a timestamped directory is created inside
    #[arg(short, long, default_value = "takes")]
    out: PathBuf,

    #[arg(long, default_value_t = 30)]
    fps: u32,

    #[arg(long, value_enum, default_value_t = EncoderArg::Auto)]
    encoder: EncoderArg,

    /// Quality: roughly CRF for x264, CQ for hardware encoders
    #[arg(long, default_value_t = 20)]
    quality: u8,

    /// Draw click ripples and drag trails into the recording (MouseShow)
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    marks: bool,
}

#[derive(clap::Args)]
struct MediaArgs {
    /// List the screens, cameras and microphones the APIs can see
    #[arg(long)]
    list: bool,

    /// Record a screen: the one-based monitor index
    #[arg(long, value_name = "INDEX")]
    screen: Option<usize>,

    /// Record a camera: its name, or its symbolic link
    #[arg(long, value_name = "CAMERA")]
    camera: Option<String>,

    /// Record a microphone: its name, or its endpoint id
    #[arg(long, value_name = "MIC")]
    mic: Option<String>,

    /// Stop after this many seconds
    #[arg(short, long, default_value_t = 5.0)]
    duration: f64,

    /// Where the files go
    #[arg(short, long, default_value = "takes-media")]
    out: PathBuf,

    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Video bitrate in kbit/s
    #[arg(long, default_value_t = 12_000)]
    bitrate: u32,
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
struct CropArg(Crop);

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

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::List { filter } => list(&filter),
        Command::Record(args) => record(args),
        Command::Media(args) => media(args),
    };
    if let Err(e) = result {
        eprintln!("takekit: {e}");
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
    let devs = ffmpeg::list_capture_devices(&ff);
    if devs.is_empty() {
        println!("  (none)");
    }
    for d in devs {
        println!("  {d}");
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
    if crop.is_zero() {
        Capture::Desktop { rect: None }
    } else {
        Capture::Desktop {
            rect: Some(crop.cropped(takekit_core::source::virtual_screen_rect())),
        }
    }
}

fn record(args: RecordArgs) -> Result<(), String> {
    // `Duration::from_secs_f64` panics on NaN, infinity and values past
    // `u64::MAX`; a day is far past any real take, so it doubles as the cap.
    if !args.duration.is_finite() || !(0.0..=86_400.0).contains(&args.duration) {
        return Err(format!(
            "--duration must be between 0 and 86400 seconds (got {})",
            args.duration
        ));
    }
    let mut sources = Vec::new();
    if let Some(target) = &args.window {
        let win = window_target(target)?;
        sources.push(Source::new(
            "win",
            Capture::Window {
                hwnd: win.hwnd,
                title: win.title,
                crop: args.crop.0,
            },
            args.fps,
        ));
    } else {
        sources.push(Source::new(
            "screen",
            desktop_capture(args.crop.0),
            args.fps,
        ));
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
    };

    let ids: Vec<String> = settings.sources.iter().map(|s| s.id.clone()).collect();

    let prepared = Session::prepare(settings).map_err(|e| e.to_string())?;
    let engine = prepared.engine();
    let encoder = prepared.encoder();
    if engine == Engine::Gdi {
        println!("note: capturing with gdigrab, not Desktop Duplication");
    }

    let rec = prepared.start().map_err(|e| e.to_string())?;
    println!("take   : {}", rec.dir().display());
    println!(
        "encoder: {}{}",
        encoder.name(),
        if encoder.is_hardware() {
            " (hardware)"
        } else {
            ""
        }
    );
    for id in &ids {
        println!("  {id} -> {}", rec.path_for(id).display());
    }

    if args.duration > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(args.duration));
    } else {
        println!("press Enter to stop …");
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
    }

    let finished = rec.stop().map_err(|e| e.to_string())?;
    for outcome in &finished.sources {
        match (&outcome.error, outcome.seconds) {
            (Some(err), _) => println!("  {}: FAILED — {}", outcome.id, err),
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
fn media(args: MediaArgs) -> Result<(), String> {
    use std::sync::mpsc;
    use std::thread;

    if args.list {
        return media_list();
    }
    let wanted = args.screen.is_some() || args.camera.is_some() || args.mic.is_some();
    if !wanted {
        return Err("nothing to record: pass --screen, --camera or --mic".into());
    }
    if !args.duration.is_finite() || !(0.1..=86_400.0).contains(&args.duration) {
        return Err(format!(
            "--duration must be between 0.1 and 86400 seconds (got {})",
            args.duration
        ));
    }

    std::fs::create_dir_all(&args.out).map_err(|e| format!("{}: {e}", args.out.display()))?;
    let stamp = chrono_stamp();
    let dir = args.out.join(stamp);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    let devices = takekit_media::devices::list();
    let mut writers: Vec<thread::JoinHandle<Result<(), String>>> = Vec::new();
    let mut senders: Vec<mpsc::Sender<MediaFrame>> = Vec::new();

    // Each source gets its own channel and its own writer thread: a capture
    // must never block on an encoder, and the files are separate anyway.
    // A source is written at a steady rate even when it delivers nothing:
    // Windows Graphics Capture only produces a frame when the picture changes,
    // so a still desktop is a pause, and a take of it has to hold the last
    // picture for as long as the pause really was. Every writer thread starts
    // its clock when the take does, which is also what keeps several sources in
    // step with each other.
    let open = |name: &str| -> (
        mpsc::Sender<MediaFrame>,
        thread::JoinHandle<Result<(), String>>,
    ) {
        let (tx, rx) = mpsc::channel::<MediaFrame>();
        let path = dir.join(format!("{name}.mp4"));
        let fps = args.fps.max(1);
        let bitrate = args.bitrate;
        let handle = thread::spawn(move || -> Result<(), String> {
            let interval = Duration::from_secs_f64(1.0 / fps as f64);
            let started = std::time::Instant::now();
            let mut video: Option<VideoWriter> = None;
            let mut latest: Option<VideoFrame> = None;
            let mut due_slot = 0u64;

            loop {
                match rx.recv_timeout(interval) {
                    Ok(MediaFrame::Video(frame)) => latest = Some(writer::even_frame(&frame)),
                    Ok(MediaFrame::Audio(_)) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                let elapsed = started.elapsed().as_secs_f64() * fps as f64;
                let end_slot = elapsed as u64;
                while due_slot < end_slot {
                    if let Some(frame) = latest.as_ref() {
                        if video.is_none() {
                            video = Some(open_video(&path, frame, fps, bitrate)?);
                        }
                        let mut out = frame.clone();
                        out.time = (due_slot as i64 * 10_000_000) / i64::from(fps);
                        video
                            .as_mut()
                            .expect("just opened")
                            .write(&out)
                            .map_err(string_of)?;
                    }
                    due_slot += 1;
                }
            }
            if let Some(w) = video.take() {
                w.finish().map_err(string_of)?;
            }
            Ok(())
        });
        (tx, handle)
    };

    let screen = match args.screen {
        Some(index) => {
            let (tx, handle) = open("screen");
            senders.push(tx.clone());
            writers.push(handle);
            Some(
                ScreenCapture::start(ScreenTarget::Monitor(index), args.fps, tx)
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
            Some(CameraCapture::start(&link, args.fps, tx).map_err(string_of)?)
        }
        None => None,
    };

    let mic = match &args.mic {
        Some(name) => {
            let endpoint = resolve_mic(&devices, name)?;
            let (tx, rx) = mpsc::channel::<MediaFrame>();
            // The writer has to live on its own thread — Media Foundation
            // objects are not `Send` — so the rate and channel count the
            // microphone reported travel over to it.
            let (format_tx, format_rx) = mpsc::channel();
            let path = dir.join("mic.mp4");
            let bitrate = args.bitrate;
            let handle = thread::spawn(move || -> Result<(), String> {
                let (rate, channels) = format_rx
                    .recv()
                    .map_err(|_| "the microphone never reported a format".to_string())?;
                let mut writer = AudioWriter::create(&path, rate, channels, bitrate.min(256))
                    .map_err(string_of)?;
                while let Ok(frame) = rx.recv() {
                    if let MediaFrame::Audio(chunk) = frame {
                        writer.write(&chunk).map_err(string_of)?;
                    }
                }
                writer.finish().map_err(string_of)
            });
            let capture = MicCapture::start(&endpoint, tx.clone()).map_err(string_of)?;
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

    println!("recording for {:.1}s into {}", args.duration, dir.display());
    thread::sleep(Duration::from_secs_f64(args.duration));

    // Stop the captures before the writers: dropping a sender is what tells its
    // writer thread there are no more frames, and a capture stopped afterwards
    // would push into a closed channel.
    if let Some(c) = screen {
        c.stop().map_err(string_of)?;
    }
    if let Some(c) = camera {
        c.stop().map_err(string_of)?;
    }
    if let Some(c) = mic {
        c.stop().map_err(string_of)?;
    }
    drop(senders);

    let mut failed = 0;
    for w in writers {
        match w.join() {
            Ok(Ok(())) => {}
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

fn open_video(
    path: &std::path::Path,
    frame: &VideoFrame,
    fps: u32,
    bitrate: u32,
) -> Result<VideoWriter, String> {
    let out = if matches!(frame.format, takekit_media::PixelFormat::Nv12) {
        VideoWriter::create_nv12(path, frame.width, frame.height, fps, bitrate)
    } else {
        VideoWriter::create(path, frame.width, frame.height, fps, bitrate)
    };
    out.map_err(string_of)
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
    println!("cameras:");
    if devices.cameras.is_empty() {
        println!("  (none)");
    }
    for c in &devices.cameras {
        println!("  {}", c.name);
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
    devices
        .cameras
        .iter()
        .find(|c| c.name == name || c.link == name)
        .map(|c| c.link.clone())
        .ok_or_else(|| format!("no camera \"{name}\"; see `takekit media --list`"))
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

/// A directory name from the local clock, the same shape the ffmpeg backend
/// uses for a take.
fn chrono_stamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = seconds / 86_400;
    let time_of_day = seconds % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{:04}-{:02}-{:02}-{:02}-{:02}-{:02}",
        y,
        m,
        d,
        time_of_day / 3600,
        time_of_day % 3600 / 60,
        time_of_day % 60
    )
}

/// Days since 1970-01-01 as a calendar date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
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
            Capture::Desktop { rect: Some(r) } => {
                assert_eq!(r.y, takekit_core::source::virtual_screen_rect().y + 10);
            }
            other => panic!("expected a desktop region, got {other:?}"),
        }
        assert!(matches!(
            desktop_capture(Crop::default()),
            Capture::Desktop { rect: None }
        ));
    }
}
