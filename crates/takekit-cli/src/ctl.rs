//! `takekit ctl` — the shell face of the panel's control surface
//! (`docs/agent-control.md`). Every subcommand is a thin map onto one HTTP
//! route; the answer is JSON on stdout, the failures are one line on stderr.
//!
//! JSON in and out on purpose: the callers are agents and scripts, and a
//! tabled human view is the panel's job.

use clap::Subcommand;
use serde_json::{Value, json};

use crate::client::Client;

#[derive(Subcommand)]
pub enum Ctl {
    /// Show settings, sources, live status and history as one JSON snapshot
    State,
    /// List windows, monitors, cameras, microphones or sound kinds that can be added
    Devices {
        /// window | desktop | camera | mic | system-audio | window-audio (all groups when omitted)
        #[arg(long)]
        kind: Option<String>,
    },
    /// Add one source to the list
    Add {
        /// window | desktop | camera | mic | system-audio | window-audio
        #[arg(long, value_name = "KIND")]
        kind: String,
        /// What to capture: a window title substring, a camera name or a mic name
        #[arg(long)]
        target: Option<String>,
        /// Monitor number for --kind desktop (from `devices`); whole desktop when omitted
        #[arg(long)]
        monitor: Option<usize>,
        /// Trim the capture: "top,bottom,left,right" in pixels
        #[arg(long)]
        crop: Option<String>,
        /// Frames per second (default 30)
        #[arg(long)]
        fps: Option<u32>,
        /// Per-source quality 0..=51 (default: the session quality)
        #[arg(long)]
        quality: Option<u8>,
        /// Record this source's sound into its own file (window or desktop
        /// kinds; the same switch the HTTP spec and MCP expose)
        #[arg(long)]
        sound: Option<String>,
    },
    /// Pause the take: the clock stops and every source closes its current file
    Pause,
    /// Resume a paused take; each source opens its next file
    Resume,
    /// Add one source to the take that is recording right now
    AddLive {
        /// window | desktop | camera | mic | system-audio | window-audio
        #[arg(long, value_name = "KIND")]
        kind: String,
        /// What to capture: a window title substring, a camera name or a mic name
        #[arg(long)]
        target: Option<String>,
        /// Monitor number for --kind desktop (from `devices`); whole desktop when omitted
        #[arg(long)]
        monitor: Option<usize>,
        /// Trim the capture: "top,bottom,left,right" in pixels
        #[arg(long)]
        crop: Option<String>,
        /// Frames per second (default 30)
        #[arg(long)]
        fps: Option<u32>,
        /// Per-source quality (default: the session quality)
        #[arg(long)]
        quality: Option<u8>,
        /// Record this source's sound into its own file (window or desktop
        /// kinds) — the same switch `add` and MCP take
        #[arg(long)]
        sound: Option<String>,
    },
    /// Remove the source at an index from /state
    Remove {
        /// Position in the /state sources array (0-based)
        #[arg(long)]
        index: usize,
    },
    /// Replace the whole source list; the JSON array comes from stdin
    SetSources,
    /// Change settings; only the flags given are touched
    Config {
        /// Global stop/start hotkey, e.g. "F9" or "Ctrl+Alt+R"
        #[arg(long)]
        hotkey: Option<String>,
        /// Seconds of countdown before a take starts (0 = immediately)
        #[arg(long)]
        countdown: Option<u8>,
        /// Where takes land; a timestamped folder is created inside
        #[arg(long)]
        out_root: Option<String>,
        /// nvenc | qsv | amf | x264 | auto
        #[arg(long)]
        encoder: Option<String>,
        /// windows | ffmpeg
        #[arg(long)]
        backend: Option<String>,
        /// on | off
        #[arg(long)]
        marks: Option<String>,
        /// on | off — raise a window to the front before recording it
        #[arg(long)]
        raise: Option<String>,
        /// on | off — show the per-source checklist when a take finishes
        #[arg(long)]
        finalize_panel: Option<String>,
        /// zh | en
        #[arg(long)]
        language: Option<String>,
        /// auto | light | dark
        #[arg(long)]
        theme: Option<String>,
        /// Path to a specific ffmpeg (empty string clears the override)
        #[arg(long)]
        ffmpeg_path: Option<String>,
    },
    /// Start a take on the current sources
    Start {
        /// Session quality 0..=51 for this take (default 20)
        #[arg(long)]
        quality: Option<u8>,
    },
    /// Stop the take and print where the files landed
    Stop,
    /// Show the history (takes and log)
    History,
    /// Show the warning/error register alone; the first stop when a take went wrong
    Log {
        /// warn | error (all entries when omitted)
        #[arg(long)]
        level: Option<String>,
    },
    /// Wipe the warning/error register (destructive: --confirm)
    ClearLog {
        /// Say it out loud: the entries go away
        #[arg(long)]
        confirm: bool,
    },
    /// Delete one log entry by its `at` stamp (destructive: --confirm)
    DeleteLog {
        /// The entry's `at` stamp, from `takekit ctl log`
        #[arg(long)]
        at: u64,
        /// Say it out loud: the row goes away
        #[arg(long)]
        confirm: bool,
    },
    /// Delete one take from history and from disk (destructive: --confirm)
    Delete {
        /// The take folder name from /history
        #[arg(long)]
        id: String,
        /// Say it out loud: this removes the folder
        #[arg(long)]
        confirm: bool,
    },
}

pub fn run(cmd: Ctl) -> Result<(), String> {
    let client = Client::discover()?;
    let out = match cmd {
        Ctl::State => client.get("/state")?,
        Ctl::Devices { kind } => {
            let path = match kind {
                Some(kind) => format!("/devices?kind={}", crate::client::url_escape(&kind)),
                None => "/devices".to_string(),
            };
            client.get(&path)?
        }
        Ctl::Add {
            kind,
            target,
            monitor,
            crop,
            fps,
            quality,
            sound,
        } => {
            let mut body = json!({ "kind": kind });
            if let Some(target) = target {
                body["target"] = json!(target);
            }
            if let Some(monitor) = monitor {
                body["monitor"] = json!(monitor);
            }
            if let Some(crop) = crop {
                body["crop"] = crop_json(&crop)?;
            }
            if let Some(fps) = fps {
                body["fps"] = json!(fps);
            }
            if let Some(quality) = quality {
                body["quality"] = json!(quality);
            }
            if let Some(sound) = sound {
                // The third door for the source's own sound switch: the HTTP
                // spec and MCP both take it, and `check-three-surface` exists
                // because "one surface forgot a field" is the drift that
                // hurts. Only picture kinds can carry sound; the server
                // answers 400 for the rest, with the reason.
                body["sound"] = on_off("sound", &sound)?;
            }
            client.post("/sources", body)?
        }
        Ctl::AddLive {
            kind,
            target,
            monitor,
            crop,
            fps,
            quality,
            sound,
        } => {
            let mut body = json!({ "kind": kind });
            if let Some(target) = target {
                body["target"] = json!(target);
            }
            if let Some(monitor) = monitor {
                body["monitor"] = json!(monitor);
            }
            if let Some(crop) = crop {
                body["crop"] = crop_json(&crop)?;
            }
            if let Some(fps) = fps {
                body["fps"] = json!(fps);
            }
            if let Some(quality) = quality {
                body["quality"] = json!(quality);
            }
            // `/record/sources` parses the same spec as `/sources`, so a source
            // added mid-take must be able to carry its sound too — otherwise
            // "add a window with its sound" works before a take and not during
            // one, on the one surface that is a shell script.
            if let Some(sound) = sound {
                body["sound"] = on_off("sound", &sound)?;
            }
            // The answer carries `start` — where the new file begins on the
            // take's timeline, in seconds — which is the number a caller needs
            // to place it later.
            client.post("/record/sources", body)?
        }
        Ctl::Pause => client.post("/record/pause", Value::Null)?,
        Ctl::Resume => client.post("/record/resume", Value::Null)?,
        Ctl::Remove { index } => client.delete(&format!("/sources/{index}"), Value::Null)?,
        Ctl::SetSources => {
            // stdin, not an argument: a full source list is long, and shells
            // quote it differently enough to be a trap.
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .map_err(|e| e.to_string())?;
            let sources: Value =
                serde_json::from_str(&text).map_err(|e| format!("stdin is not JSON: {e}"))?;
            client.put("/sources", sources)?
        }
        Ctl::Config {
            hotkey,
            countdown,
            out_root,
            encoder,
            backend,
            marks,
            raise,
            finalize_panel,
            language,
            theme,
            ffmpeg_path,
        } => {
            let mut body = json!({});
            if let Some(v) = hotkey {
                body["hotkey"] = json!(v);
            }
            if let Some(v) = countdown {
                body["countdown_secs"] = json!(v);
            }
            if let Some(v) = out_root {
                body["out_root"] = json!(v);
            }
            if let Some(v) = encoder {
                body["encoder"] = if v == "auto" { Value::Null } else { json!(v) };
            }
            if let Some(v) = backend {
                body["backend"] = json!(v);
            }
            if let Some(v) = marks {
                body["marks"] = on_off("marks", &v)?;
            }
            if let Some(v) = raise {
                body["raise_sources"] = on_off("raise", &v)?;
            }
            if let Some(v) = finalize_panel {
                body["finalize_panel"] = on_off("finalize-panel", &v)?;
            }
            if let Some(v) = language {
                body["language"] = json!(v);
            }
            if let Some(v) = theme {
                body["theme"] = json!(v);
            }
            if let Some(v) = ffmpeg_path {
                // Empty string clears the override — the same shape the
                // panel's settings row writes.
                body["ffmpeg_path"] = if v.is_empty() { Value::Null } else { json!(v) };
            }
            if body.as_object().is_none_or(|o| o.is_empty()) {
                return Err("nothing to set: pass at least one flag".to_string());
            }
            client.patch("/config", body)?
        }
        Ctl::Start { quality } => {
            let body = match quality {
                Some(quality) => json!({ "quality": quality }),
                None => json!({}),
            };
            client.post("/record/start", body)?
        }
        Ctl::Stop => client.post("/record/stop", json!({}))?,
        Ctl::History => client.get("/history")?,
        Ctl::Log { level } => {
            let path = match level {
                Some(level) => format!("/log?level={}", crate::client::url_escape(&level)),
                None => "/log".to_string(),
            };
            client.get(&path)?
        }
        Ctl::ClearLog { confirm } => {
            if !confirm {
                return Err(
                    "clearing the log wipes the entries; pass --confirm to mean it".to_string(),
                );
            }
            client.delete("/log", json!({ "confirm": true }))?
        }
        Ctl::DeleteLog { at, confirm } => {
            if !confirm {
                return Err(
                    "deleting a log entry removes the row; pass --confirm to mean it".to_string(),
                );
            }
            client.delete(&format!("/log/{at}"), json!({ "confirm": true }))?
        }
        Ctl::Delete { id, confirm } => {
            if !confirm {
                return Err(
                    "deleting a take removes its folder; pass --confirm to mean it".to_string(),
                );
            }
            // Escaped like a query value: a folder name is machine-made and
            // URL-safe, but the id arrives from whoever typed the command.
            let path = format!("/history/{}", crate::client::url_escape(&id));
            client.delete(&path, json!({ "confirm": true }))?
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
    );
    Ok(())
}

/// `--crop` in the same "top,bottom,left,right" shape the `record` subcommand
/// takes, so the two cannot drift.
fn crop_json(text: &str) -> Result<Value, String> {
    let crop: crate::CropArg = text.parse()?;
    Ok(json!({
        "top": crop.0.top,
        "bottom": crop.0.bottom,
        "left": crop.0.left,
        "right": crop.0.right,
    }))
}

fn on_off(flag: &str, value: &str) -> Result<Value, String> {
    match value {
        "on" | "true" | "1" => Ok(Value::Bool(true)),
        "off" | "false" | "0" => Ok(Value::Bool(false)),
        other => Err(format!("--{flag} wants on or off, got {other:?}")),
    }
}
