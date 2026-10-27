//! Panel settings that outlive a run: the global hotkey, the two
//! recording-feedback switches, and the capture setup from last time.
//!
//! Stored as JSON under `%APPDATA%\TakeKit\config.json`. A missing or corrupt
//! file is not an error: the panel comes up on defaults rather than refusing to
//! start.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use takekit_core::encoder::Encoder;
use takekit_core::source::{Backend, Capture, Source};

/// Every writer takes this, so a drag and a source edit cannot lose each
/// other's field to a read-modify-write race on the shared file.
static LOCK: Mutex<()> = Mutex::new(());

/// Where the panel was last time, in physical pixels — the units
/// `outer_position` reports and the monitor list is measured in.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowPos {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Global shortcut that starts/stops a take, e.g. `"F9"`.
    pub hotkey: String,
    /// Global shortcut that pauses/resumes the running take, e.g. `"F10"`.
    /// Empty switches it off; the panel's button and the tray popover still
    /// work, the key is only a shortcut.
    pub pause_hotkey: String,
    /// Full-screen countdown before the take starts; `0` turns it off.
    pub countdown_secs: u8,
    /// Show the full-screen summary while a take is being finalised.
    pub finalize_panel: bool,
    /// Capture setup from the last run, restored at startup so a repeated take
    /// needs no re-picking.
    pub sources: Vec<Source>,
    pub out_root: Option<String>,
    /// Where ffmpeg lives, when it is not on PATH. `None` keeps the PATH
    /// lookup. A path that stops naming a file degrades to the PATH lookup
    /// too (see `ffmpeg::locate`) — the setting survives, the lookup just
    /// stops trusting it until the file is back.
    pub ffmpeg_path: Option<String>,
    /// `None` keeps the automatic pick.
    pub encoder: Option<Encoder>,
    /// ffmpeg children, or the OS capture APIs in-process (the only one that
    /// records a window sitting behind others).
    pub backend: Backend,
    pub marks: bool,
    /// Bring window sources to the front before recording. Off by default;
    /// the ffmpeg backend needs it or the take shows whatever covered the
    /// window, the Windows backend needs it never.
    pub raise_sources: bool,
    /// UI language: `"zh"` or `"en"`. Anything else reads as Chinese.
    pub language: String,
    /// Palette: `"auto"` (follow the system), `"light"` or `"dark"`.
    pub theme: String,
    /// The loopback control surface (`docs/agent-control.md`): on by default,
    /// so an agent can drive the panel without the user having found a switch
    /// first. Off means the server does not start and no `control.json` is
    /// written — nothing else changes.
    pub control: bool,
    /// Tab the user last had open (`"sources"` / `"history"` / `"log"` /
    /// `"settings"`); a restart lands there. Anything else reads as the first.
    pub active_tab: String,
    /// `None` lets the window manager place the panel.
    pub window_pos: Option<WindowPos>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "F9".to_string(),
            pause_hotkey: "F10".to_string(),
            countdown_secs: 3,
            finalize_panel: true,
            sources: Vec::new(),
            out_root: None,
            ffmpeg_path: None,
            encoder: None,
            backend: Backend::Windows,
            marks: true,
            raise_sources: false,
            language: "zh".to_string(),
            theme: "auto".to_string(),
            control: true,
            active_tab: "sources".to_string(),
            window_pos: None,
        }
    }
}

impl Config {
    /// `%APPDATA%\TakeKit\config.json`, falling back to the temp dir when
    /// `APPDATA` is unset (a stripped environment still gets persistence).
    /// The directory itself is `fsx::app_dir()` — the same one the control
    /// surface's `control.json` and the CLI client use.
    pub fn path() -> PathBuf {
        takekit_core::fsx::app_dir().join("config.json")
    }

    pub fn load() -> Config {
        let path = Self::path();
        let (cfg, renamed) = Self::read(&path);
        // The rename has to survive a restart even when nothing else is saved:
        // a source that goes back to being called `win` on the next launch is
        // the same bug with a longer delay.
        if renamed {
            let _ = cfg.save_to(&path);
        }
        cfg
    }

    pub fn load_from(path: &Path) -> Config {
        Self::read(path).0
    }

    /// The config on disk, and whether any window source had to be renamed on
    /// the way in (`load` writes the file back when it did).
    fn read(path: &Path) -> (Config, bool) {
        // Read as bytes: a file written as UTF-16 fails `read_to_string`,
        // and returning defaults there would skip the backup below and let
        // the next save erase what was there.
        let Ok(bytes) = std::fs::read(path) else {
            return (Config::default(), false);
        };
        // A hand-edited file can carry a UTF-8 BOM — PS `Set-Content -Encoding
        // UTF8` writes one — and it is not JSON.
        let text = String::from_utf8_lossy(&bytes);
        let body = text.trim_start_matches('\u{feff}');
        match serde_json::from_str::<Config>(body) {
            Ok(mut cfg) => {
                let renamed = cfg.name_window_sources();
                (cfg, renamed)
            }
            Err(_) => {
                // Keep the unparseable file before the next save replaces it
                // with defaults: the alternative is losing the hotkey and the
                // source list because one byte was wrong.
                takekit_core::fsx::backup_corrupt(path);
                (Config::default(), false)
            }
        }
    }

    /// A window source's id is the file it becomes, and configs written before
    /// 2026-10-27 hold `win`, `win-2`: names that say nothing about the window.
    /// Adding a source now derives its name from the app, so the rename reaches
    /// every *new* source and none of the ones a user already has — which is to
    /// say it never reaches the person who has been using the panel. So the
    /// names are re-derived on load.
    ///
    /// Only the legacy shapes are touched (`win`, `win-N`, and the same with
    /// `-audio`). A name that already came from an app is left exactly as it
    /// is: it may be the only record of a window that has since closed.
    ///
    /// Returns whether anything changed, so `load` can write the file back.
    fn name_window_sources(&mut self) -> bool {
        self.name_window_sources_with(takekit_core::source::app_of_pid)
    }

    fn name_window_sources_with(&mut self, name: impl Fn(u32) -> Option<String>) -> bool {
        let mut renamed = false;
        let mut taken: Vec<String> = self
            .sources
            .iter()
            .filter(|s| !is_legacy_window_id(&s.id))
            .map(|s| s.id.clone())
            .collect();
        for s in &mut self.sources {
            if !is_legacy_window_id(&s.id) {
                continue;
            }
            let pid = match s.capture {
                Capture::Window { pid, .. } => pid,
                _ => continue,
            };
            // A closed window's process is gone, and `win` is still a better
            // name than the empty one a failed lookup would give.
            let Some(app) = name(pid) else { continue };
            let base = if s.id.ends_with("-audio") {
                format!("{app}-audio")
            } else {
                app
            };
            let mut id = base.clone();
            for n in 2.. {
                if !taken.contains(&id) {
                    break;
                }
                id = format!("{base}-{n}");
            }
            taken.push(id.clone());
            s.id = id;
            renamed = true;
        }
        renamed
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let body = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        // Written beside the target and renamed over it, like the journal:
        // window drags rewrite this file every 400ms, and a crash mid-write
        // would leave truncated JSON that loads as "all defaults" — the hotkey,
        // the source list and the window position would vanish silently.
        takekit_core::fsx::write_atomic(path, &body).map_err(|e| e.to_string())
    }

    /// Load, change and write back in one step.
    ///
    /// Nothing is written when `f` left the settings alone, which is what makes
    /// it cheap enough to call from a window drag.
    pub fn update_at(path: &Path, f: impl FnOnce(&mut Config)) -> Result<(), String> {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = Self::load_from(path);
        let before = cfg.clone();
        f(&mut cfg);
        if cfg == before {
            return Ok(());
        }
        cfg.save_to(path)
    }

    pub fn update(f: impl FnOnce(&mut Config)) -> Result<(), String> {
        Self::update_at(&Self::path(), f)
    }

    /// The configured hotkey in the form the registrar accepts, falling back to
    /// the built-in default when the file holds something unparseable — it is
    /// plain JSON, editable by hand.
    pub fn effective_hotkey(&self) -> String {
        crate::shortcut::normalize(&self.hotkey).unwrap_or_else(|| Config::default().hotkey)
    }

    /// The pause key in the form the registrar accepts, `None` when it is off.
    ///
    /// No forced fallback here, unlike `hotkey`: a pause key the user did not
    /// ask for takes that shortcut away from whatever else owns it, and "no
    /// pause key" is a legitimate setting (the button and the tray still work).
    pub fn effective_pause_hotkey(&self) -> Option<String> {
        crate::shortcut::normalize(&self.pause_hotkey)
    }

    /// A language this build can actually draw in.
    ///
    /// The file is hand-editable and outlives a version that adds a third
    /// language, so an unknown value must land on something the panel has
    /// strings for rather than render the raw key.
    pub fn effective_language(&self) -> String {
        match self.language.as_str() {
            "en" => "en".to_string(),
            _ => "zh".to_string(),
        }
    }

    /// Same for the palette: only the three named choices exist, and anything
    /// else (a typo, a future value) follows the system.
    pub fn effective_theme(&self) -> String {
        match self.theme.as_str() {
            "light" => "light".to_string(),
            "dark" => "dark".to_string(),
            _ => "auto".to_string(),
        }
    }

    /// Same again for the remembered tab: only the four real panes exist, and
    /// an unknown name lands on the first rather than an empty window.
    pub fn effective_active_tab(&self) -> String {
        valid_tab(&self.active_tab).to_string()
    }
}

/// The one list of panes that exist. Used on the way in (`set_active_tab`) and
/// on the way out (`config_get`); the panel just shows what it is handed. The
/// set once lived in three places at once — an inline match here, one in
/// main.rs, one in the JS — and a dead copy of this function was the tell.
pub fn valid_tab(tab: &str) -> &'static str {
    match tab {
        "history" => "history",
        "log" => "log",
        "settings" => "settings",
        _ => "sources",
    }
}

/// Whether a source id is one of the names window sources had before they were
/// named after their app: `win`, `win-2`, `win-audio`, `win-2-audio`. Everything
/// else is left alone — in particular an app really called `win` stays `win`,
/// because re-deriving it gives the same answer.
fn is_legacy_window_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("win") else {
        return false;
    };
    let rest = rest.strip_suffix("-audio").unwrap_or(rest);
    rest.is_empty()
        || (rest.len() > 1
            && rest.starts_with('-')
            && rest[1..].chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use takekit_core::source::Crop;

    fn window_source(id: &str, pid: u32) -> Source {
        Source::new(
            id,
            Capture::Window {
                hwnd: 1,
                pid,
                title: String::new(),
                crop: Crop::default(),
            },
            30,
        )
    }

    #[test]
    fn a_window_source_drops_its_win_name() {
        // The state a config is actually in on this machine: two window sources
        // called `win` and `win-2`, and a mic that must keep its own name.
        let mut cfg = Config {
            sources: vec![
                Source::new(
                    "mic",
                    Capture::Mic {
                        device: "x".to_string(),
                    },
                    30,
                ),
                window_source("win", 7),
                window_source("win-2", 7),
                window_source("win-audio", 8),
            ],
            ..Config::default()
        };
        assert!(cfg.name_window_sources_with(|pid| match pid {
            7 => Some("chrome".to_string()),
            8 => Some("code".to_string()),
            _ => None,
        }));
        let ids: Vec<&str> = cfg.sources.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["mic", "chrome", "chrome-2", "code-audio"]);
        // Idempotent: a second pass has nothing left to rename, so `load` does
        // not rewrite the file on every launch.
        assert!(!cfg.name_window_sources_with(|_| Some("chrome".to_string())));
    }

    #[test]
    fn a_name_that_already_came_from_the_app_is_left_alone() {
        // The migration must not rename a source it did not create: a window
        // that has since closed keeps the only name it will ever have.
        let mut cfg = Config {
            sources: vec![window_source("chrome", 9)],
            ..Config::default()
        };
        assert!(!cfg.name_window_sources_with(|_| None));
        assert_eq!(cfg.sources[0].id, "chrome");
        assert!(!cfg.name_window_sources_with(|_| Some("firefox".to_string())));
        assert_eq!(cfg.sources[0].id, "chrome");
    }

    #[test]
    fn loading_a_config_renames_the_win_sources_it_holds() {
        // End to end through the file, with a pid that is guaranteed to answer:
        // this test's own process. Whatever the test binary is called, `win` is
        // not it.
        let path = scratch("legacy-on-load");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"{{"sources":[{{"id":"win","capture":{{"Window":{{"hwnd":1,"pid":{},"title":"","crop":{{"top":0,"bottom":0,"left":0,"right":0}}}}}},"fps":30}}]}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        let cfg = Config::load_from(&path);
        assert_ne!(cfg.sources[0].id, "win");
        assert!(!cfg.sources[0].id.is_empty());
    }

    #[test]
    fn only_the_legacy_shapes_count_as_legacy() {
        for id in ["win", "win-2", "win-11", "win-audio", "win-2-audio"] {
            assert!(is_legacy_window_id(id), "{id}");
        }
        for id in ["winamp", "window", "chrome", "win-", "win--2", "win-x"] {
            assert!(!is_legacy_window_id(id), "{id}");
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("takekit-cfg-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("config.json")
    }

    #[test]
    fn round_trip() {
        let path = scratch("rt");
        let cfg = Config {
            hotkey: "Ctrl+Alt+R".to_string(),
            countdown_secs: 5,
            finalize_panel: false,
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), cfg);
    }

    #[test]
    fn a_session_survives_a_restart() {
        let path = scratch("session");
        let cfg = Config {
            sources: vec![
                Source::new("screen", Capture::desktop(None), 60),
                Source::new(
                    "win",
                    Capture::Window {
                        hwnd: 133890,
                        // A config from before the field existed: unknown owner.
                        pid: 0,
                        title: "TakeKit - TRAE CN".to_string(),
                        crop: Crop::default(),
                    },
                    30,
                ),
            ],
            out_root: Some(r"D:\takes".to_string()),
            encoder: Some(Encoder::X264),
            marks: false,
            raise_sources: false,
            window_pos: Some(WindowPos { x: 240.0, y: 260.0 }),
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();

        let back = Config::load_from(&path);
        assert_eq!(back, cfg);
        assert_eq!(back.sources.len(), 2);
        assert_eq!(back.window_pos, Some(WindowPos { x: 240.0, y: 260.0 }));
    }

    #[test]
    fn update_keeps_the_fields_it_does_not_touch() {
        let path = scratch("update");
        let cfg = Config {
            hotkey: "F8".to_string(),
            out_root: Some("D:\\keep".to_string()),
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();

        Config::update_at(&path, |c| c.marks = false).unwrap();

        let back = Config::load_from(&path);
        assert_eq!(back.hotkey, "F8");
        assert_eq!(back.out_root.as_deref(), Some("D:\\keep"));
        assert!(!back.marks);
    }

    #[test]
    fn missing_file_is_default() {
        let path = scratch("none");
        assert_eq!(Config::load_from(&path), Config::default());
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_settings() {
        let path = scratch("bom");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "\u{feff}{\"hotkey\":\"F7\"}").unwrap();
        assert_eq!(Config::load_from(&path).hotkey, "F7");
    }

    #[test]
    fn corrupt_file_is_default() {
        let path = scratch("bad");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Config::load_from(&path), Config::default());
    }

    #[test]
    fn partial_file_fills_in_defaults() {
        let path = scratch("part");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"hotkey":"F8"}"#).unwrap();
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.hotkey, "F8");
        assert_eq!(cfg.countdown_secs, 3);
        assert!(cfg.finalize_panel);
    }

    #[test]
    fn bad_hotkey_falls_back_to_default() {
        let cfg = Config {
            hotkey: "Ctrl+NotAKey".to_string(),
            ..Config::default()
        };
        assert_eq!(cfg.effective_hotkey(), Config::default().hotkey);
    }

    #[test]
    fn good_hotkey_comes_back_normalised() {
        let cfg = Config {
            hotkey: "ctrl+alt+r".to_string(),
            ..Config::default()
        };
        assert_eq!(
            cfg.effective_hotkey(),
            crate::shortcut::normalize("ctrl+alt+r").unwrap()
        );
    }

    #[test]
    fn unknown_ui_values_fall_back_instead_of_rendering_raw() {
        // The file is hand-editable: a value this build does not know must not
        // reach the panel as a key with no strings behind it.
        let cfg = Config {
            language: "fr".to_string(),
            theme: "neon".to_string(),
            active_tab: "nowhere".to_string(),
            ..Config::default()
        };
        assert_eq!(cfg.effective_language(), "zh");
        assert_eq!(cfg.effective_theme(), "auto");
        assert_eq!(cfg.effective_active_tab(), "sources");
    }

    #[test]
    fn known_ui_values_survive_a_round_trip() {
        let path = scratch("ui");
        let cfg = Config {
            language: "en".to_string(),
            theme: "dark".to_string(),
            active_tab: "settings".to_string(),
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();
        let back = Config::load_from(&path);
        assert_eq!(back.effective_language(), "en");
        assert_eq!(back.effective_theme(), "dark");
        assert_eq!(back.effective_active_tab(), "settings");
    }

    #[test]
    fn raising_sources_defaults_to_off_and_round_trips() {
        // The switch is opt-in: a config from before it existed must read as
        // "off", not "on", and the value the panel sets must survive a save.
        let path = scratch("raise");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"hotkey":"F7","marks":false,"backend":"windows"}"#,
        )
        .unwrap();
        let cfg = Config::load_from(&path);
        assert!(!cfg.raise_sources);
        assert_eq!(cfg.hotkey, "F7");

        let mut cfg = cfg;
        cfg.raise_sources = true;
        cfg.save_to(&path).unwrap();
        assert!(Config::load_from(&path).raise_sources);
    }

    #[test]
    fn the_control_surface_is_on_by_default_and_round_trips() {
        // The opposite case from `raise_sources`: an agent talking to a panel
        // that was never configured for it is the point of the feature, so a
        // config from before the field existed must come up with the door open.
        let path = scratch("control");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"hotkey":"F7"}"#).unwrap();
        assert!(Config::load_from(&path).control);

        let mut cfg = Config::load_from(&path);
        cfg.control = false;
        cfg.save_to(&path).unwrap();
        assert!(!Config::load_from(&path).control);
    }
}
