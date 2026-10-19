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
