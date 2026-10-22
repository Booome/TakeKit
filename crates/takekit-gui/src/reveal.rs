//! Opening a take folder in File Explorer.
//!
//! Split from the tauri command so the part that can be wrong — the folder was
//! deleted, or the path was never a folder — is testable without a desktop.
//!
//! Failures arrive as `UiError` codes, not sentences: the panel translates
//! them, and a string composed here would be stuck in whichever language it
//! was written in.

use std::path::{Path, PathBuf};

use crate::proto::UiError;

/// What a history row points at. A folder the user deleted reports itself
/// rather than opening nothing and leaving the click unanswered.
pub fn take_dir(dir: &str) -> Result<PathBuf, UiError> {
    let path = PathBuf::from(dir);
    if path.is_dir() {
        Ok(path)
    } else {
        Err(UiError::with_detail("err.take-gone", dir))
    }
}

/// Hand the folder to Explorer. Explorer is the shell here, not a program we
/// wait on: it returns an exit code that means nothing to us, so the spawn
/// result is all that matters.
pub fn reveal(path: &Path) -> Result<(), UiError> {
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| UiError::with_detail("err.reveal-failed", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("takekit-reveal-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_existing_take_folder_resolves() {
        let dir = scratch("ok");
        let resolved = take_dir(&dir.to_string_lossy()).unwrap();
        assert_eq!(resolved, dir);
    }

    #[test]
    fn a_deleted_folder_is_reported_by_name() {
        let dir = scratch("gone");
        let text = dir.to_string_lossy().into_owned();
        std::fs::remove_dir_all(&dir).unwrap();
        let err = take_dir(&text).unwrap_err();
        assert_eq!(err.code, "err.take-gone");
        assert_eq!(err.detail.as_deref(), Some(text.as_str()));
    }

    #[test]
    fn a_file_is_not_a_take_folder() {
        let dir = scratch("file");
        let file = dir.join("take.json");
        std::fs::write(&file, "{}").unwrap();
        assert!(take_dir(&file.to_string_lossy()).is_err());
    }

    #[test]
    fn an_empty_path_is_reported_not_opened() {
        assert!(take_dir("").is_err());
    }
}
