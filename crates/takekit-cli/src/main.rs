//! `takekit` — command-line front end.
//!
//! The GUI will drive `takekit-core` directly; this binary exists so the core
//! can be exercised without any UI.

use std::io::BufRead;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use takekit_core::encoder::{self, Encoder};
use takekit_core::ffmpeg;
use takekit_core::source::{Capture, Crop, Source};
use takekit_core::take::Take;

#[derive(Parser)]
#[command(name = "takekit", version, about = "Record several sources at once into one take")]
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
            [t, b, l, r] => Ok(CropArg(Crop { top: *t, bottom: *b, left: *l, right: *r })),
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
    let ff = ffmpeg::locate().ok_or("ffmpeg not found on PATH")?;

    println!("windows:");
    for (title, r) in takekit_core::source::list_windows(filter) {
        println!("  {title}   [{}x{} at {},{}]", r.w, r.h, r.x, r.y);
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

fn record(args: RecordArgs) -> Result<(), String> {
    let ff = ffmpeg::locate().ok_or("ffmpeg not found on PATH")?;
    let probe = ffmpeg::ffprobe(&ff);

    let enc = match args.encoder {
        EncoderArg::Auto => encoder::pick(&ff),
        EncoderArg::X264 => Encoder::X264,
        EncoderArg::Nvenc => Encoder::Nvenc,
        EncoderArg::Qsv => Encoder::Qsv,
        EncoderArg::Amf => Encoder::Amf,
    };
    if enc.is_hardware() && !encoder::works(&ff, enc) {
        return Err(format!("{} does not work on this machine", enc.name()));
    }

    let mut sources = Vec::new();
    if let Some(title) = &args.window {
        sources.push(Source::new(
            "win",
            Capture::Window { title: title.clone(), crop: args.crop.0 },
        ));
    } else {
        sources.push(Source::new("screen", Capture::Desktop { rect: None }));
    }
    if let Some(dev) = &args.cam {
        sources.push(Source::new(
            "cam",
            Capture::Camera { device: dev.clone(), crop: args.crop.0 },
        ));
    }

    let mut take = Take::create(&args.out, enc, args.fps, &sources).map_err(|e| e.to_string())?;
    println!("take   : {}", take.dir.display());
    println!("encoder: {}{}", enc.name(), if enc.is_hardware() { " (hardware)" } else { "" });

    let mut children = Vec::new();
    for src in &sources {
        let (input, extra_filter) = src.input_args()?;
        let out = take.path_for(&src.id);

        let mut cmd: Vec<String> = vec!["-hide_banner".into(), "-y".into()];
        cmd.extend(input);
        if let Some(f) = extra_filter {
            cmd.extend(["-vf".into(), f]);
        }
        cmd.extend(enc.args(args.quality));
        cmd.push(out.to_string_lossy().into_owned());

        let log = take.dir.join(format!("{}.log", src.id));
        println!("  {} -> {}", src.id, out.display());
        let child = ffmpeg::spawn(&ff, &cmd, &log).map_err(|e| format!("spawn failed: {e}"))?;
        children.push((src.id.clone(), child, log));
    }

    if args.duration > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(args.duration));
    } else {
        println!("press Enter to stop …");
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
    }

    for (id, mut child, log) in children {
        ffmpeg::stop(&mut child);
        let file = take.path_for(&id);
        let bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        if bytes == 0 {
            println!("  {id}: FAILED — {}", ffmpeg::tail(&log, 3));
            continue;
        }
        let d = ffmpeg::duration(&probe, &file);
        take.set_duration(&id, d);
        match d {
            Some(d) => println!("  {id}: {d:.3}s  ({:.1} MB)", bytes as f64 / (1024.0 * 1024.0)),
            None => println!("  {id}: (duration unknown)"),
        }
    }
    take.manifest.duration = take.manifest.sources.iter().filter_map(|s| s.duration).reduce(f64::max);
    take.save_manifest().map_err(|e| e.to_string())?;

    let ds: Vec<Option<f64>> = take.manifest.sources.iter().map(|s| s.duration).collect();
    if let (Some(a), Some(b)) = (
        ds.first().and_then(|x| *x),
        ds.get(1).and_then(|x| *x),
    ) {
        println!("duration delta: {:+.3}s", a - b);
    }
    Ok(())
}
