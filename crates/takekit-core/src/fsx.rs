//! Small filesystem helpers shared by the core and the GUI.

use std::path::{Path, PathBuf};

/// `%APPDATA%\TakeKit` — the one place the app keeps its files, falling back
/// to the temp dir when `APPDATA` is unset (a stripped environment still gets
/// persistence).
///
/// Single definition on purpose: the panel's `config.json`, the control
/// surface's `control.json` and the CLI client that reads it all resolve
/// through here, so the three can never disagree about where the door is.
pub fn app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("TakeKit")
}

/// Write `body` to `path` through a sibling staging file and a rename.
///
/// The files this is used for (take.json, config.json, journal.json) are
/// rewritten in place, so a crash mid-write must not truncate the previous
/// copy: partial JSON loads as "everything is default", which is
/// indistinguishable from data loss.
pub fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    let staging = path.with_extension("tmp");
    std::fs::write(&staging, body)?;
    std::fs::rename(&staging, path)
}

/// Keep a file that failed to parse beside itself, once.
///
/// Config and journal both fall back to defaults on unreadable JSON and then
/// write those defaults back. Without this copy the old content is gone after
/// the next save — a single bad hand edit (or a truncated write from an older
/// build) would silently drop the hotkey, the source list or the history.
pub fn backup_corrupt(path: &Path) -> Option<std::path::PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.push(".corrupt");
    let backup = path.with_file_name(name);
    if !backup.exists() {
        let _ = std::fs::copy(path, &backup);
    }
    Some(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_dir_ends_with_the_one_folder_every_front_end_agrees_on() {
        // The panel, the control server and the CLI client all join their
        // file names onto this; a rename here moves all three together.
        assert_eq!(app_dir().file_name().unwrap(), "TakeKit");
    }

    #[test]
    fn a_write_replaces_the_previous_content() {
        let dir = std::env::temp_dir().join("takekit-fsx-atomic");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        write_atomic(&path, "{\"a\":1}").unwrap();
        write_atomic(&path, "{\"a\":2}").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");
        // The staging file is renamed away, never left behind.
        assert!(!dir.join("config.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
