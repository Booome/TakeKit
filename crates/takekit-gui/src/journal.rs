//! The panel's own record of what it did: one entry per finished take, plus
//! the warnings and errors it raised along the way.
//!
//! Stored as JSON under `%APPDATA%\TakeKit\journal.json` — deliberately not
//! `config.json`: losing this file costs you the lists, not the capture setup.
//! A missing or corrupt file reads as an empty journal.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// How many takes the history keeps. Older ones fall off the end: the panel is
/// a workspace, not an archive, and the takes themselves stay on disk.
pub const TAKES_KEPT: usize = 20;

/// How many log entries the log keeps.
pub const LOG_KEPT: usize = 50;

/// Every writer takes this, so a take finishing and a warning raised at the
/// same moment cannot lose each other's entry to a read-modify-write race.
static LOCK: Mutex<()> = Mutex::new(());

/// The two panes that can carry an unread marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Takes,
    Log,
}

impl Pane {
    pub fn parse(name: &str) -> Option<Pane> {
        match name {
            "history" => Some(Pane::Takes),
            "log" => Some(Pane::Log),
            _ => None,
        }
    }
}

/// One finished take.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TakeRecord {
    /// The take folder — what a click opens.
    pub dir: String,
    /// Unix milliseconds when the take stopped.
    pub at: u64,
    pub seconds: Option<f64>,
    pub bytes: u64,
    /// Sources that produced nothing; they turn the row's dot orange.
    pub failed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Warn,
    Error,
}

/// One line in the log. Only things worth knowing about later — a successful
/// save belongs to the history, not here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    pub at: u64,
    pub level: Level,
    /// The one-sentence conclusion, the same text the notice showed.
    pub text: String,
    /// The technical half: ffmpeg's reason, the window that went away, …
    #[serde(default)]
    pub detail: Option<String>,
    /// The take folder whose own log explains this entry, when there is one.
    #[serde(default)]
    pub dir: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Journal {
    /// Newest first.
    pub takes: Vec<TakeRecord>,
    /// Newest first.
    pub log: Vec<LogRecord>,
    /// When each pane was last looked at. Anything newer is unread.
    pub seen_takes: u64,
    pub seen_log: u64,
}

impl Journal {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("TakeKit").join("journal.json")
    }

    pub fn load() -> Journal {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Journal {
        // Bytes, not a string: a UTF-16 file fails `read_to_string` and would
        // skip the backup below, letting the next save erase the history.
        let Ok(bytes) = std::fs::read(path) else {
            return Journal::default();
        };
        // Hand-edited files can carry a UTF-8 BOM and are then not JSON.
        let text = String::from_utf8_lossy(&bytes);
        let body = text.trim_start_matches('\u{feff}');
        match serde_json::from_str(body) {
            Ok(journal) => journal,
            Err(_) => {
                // Same rule as the config: never let a fallback-to-defaults
                // save be the thing that erases the history.
                takekit_core::fsx::backup_corrupt(path);
                Journal::default()
            }
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let body = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        // Written beside the target and renamed over it: this file is rewritten
        // on every take, so a crash mid-write must not be able to truncate the
        // list it was updating.
        takekit_core::fsx::write_atomic(path, &body).map_err(|e| e.to_string())
    }

    pub fn update(f: impl FnOnce(&mut Journal)) -> Result<(), String> {
        Self::update_at(&Self::path(), f)
    }

    pub fn update_at(path: &Path, f: impl FnOnce(&mut Journal)) -> Result<(), String> {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut journal = Self::load_from(path);
        let before = journal.clone();
        f(&mut journal);
        // Nothing written when nothing changed: opening a pane that was already
        // read costs no disk write.
        if journal == before {
            return Ok(());
        }
        journal.save_to(path)
    }

    pub fn push_take(&mut self, take: TakeRecord) {
        self.takes.insert(0, take);
        self.takes.truncate(TAKES_KEPT);
    }

    pub fn push_log(&mut self, entry: LogRecord) {
        self.log.insert(0, entry);
        self.log.truncate(LOG_KEPT);
    }

    pub fn mark_seen(&mut self, pane: Pane) {
        // The newest record, not the wall clock: a second visit then changes
        // nothing (so it costs no write), and a record that landed in between
        // is genuinely newer than this stamp.
        match pane {
            Pane::Takes => self.seen_takes = self.takes.first().map_or(0, |t| t.at),
            Pane::Log => self.seen_log = self.log.first().map_or(0, |e| e.at),
        }
    }

    pub fn unread(&self, pane: Pane) -> usize {
        match pane {
            Pane::Takes => self.takes.iter().filter(|t| t.at > self.seen_takes).count(),
            Pane::Log => self.log.iter().filter(|e| e.at > self.seen_log).count(),
        }
    }

    pub fn clear(&mut self, pane: Pane) {
        match pane {
            Pane::Takes => self.takes.clear(),
            Pane::Log => self.log.clear(),
        }
    }

    /// One log line out of the register, by the `at` stamp it carries — the
    /// only identity the record has, and what the row's delete button sends.
    /// First match wins: two lines written in the same millisecond are
    /// indistinguishable by design, and a second click takes the other one.
    /// Returns whether anything was there to remove.
    pub fn remove_log(&mut self, at: u64) -> bool {
        match self.log.iter().position(|e| e.at == at) {
            Some(pos) => {
                self.log.remove(pos);
                true
            }
            None => false,
        }
    }
}

/// Milliseconds since the Unix epoch, the stamp every record carries.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("takekit-journal-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("journal.json")
    }

    fn take(at: u64) -> TakeRecord {
        TakeRecord {
            dir: format!(r"C:\takes\{at}"),
            at,
            seconds: Some(5.0),
            bytes: 1024,
            failed: 0,
        }
    }

    fn entry(at: u64, text: &str) -> LogRecord {
        LogRecord {
            at,
            level: Level::Warn,
            text: text.to_string(),
            detail: None,
            dir: None,
        }
    }

    #[test]
    fn takes_are_newest_first() {
        let mut j = Journal::default();
        j.push_take(take(100));
        j.push_take(take(200));
        assert_eq!(j.takes.len(), 2);
        assert_eq!(j.takes[0].at, 200);
        assert_eq!(j.takes[1].at, 100);
    }

    #[test]
    fn takes_beyond_the_cap_fall_off_the_old_end() {
        let mut j = Journal::default();
        for i in 0..(TAKES_KEPT as u64 + 3) {
            j.push_take(take(i));
        }
        assert_eq!(j.takes.len(), TAKES_KEPT);
        assert_eq!(j.takes[0].at, TAKES_KEPT as u64 + 2);
        assert!(j.takes.iter().all(|t| t.at >= 3));
    }

    #[test]
    fn log_entries_fall_off_the_old_end() {
        let mut j = Journal::default();
        for i in 0..(LOG_KEPT as u64 + 3) {
            j.push_log(entry(i, "x"));
        }
        assert_eq!(j.log.len(), LOG_KEPT);
        assert_eq!(j.log[0].at, LOG_KEPT as u64 + 2);
    }

    #[test]
    fn one_log_line_leaves_the_others_alone() {
        let mut j = Journal::default();
        j.push_log(entry(100, "a"));
        j.push_log(entry(200, "b"));
        j.push_log(entry(300, "c"));

        assert!(j.remove_log(200));
        assert_eq!(j.log.len(), 2);
        assert_eq!(j.log[0].at, 300);
        assert_eq!(j.log[1].at, 100);

        // A stamp nothing carries is a no-op said out loud, not a silent pass.
        assert!(!j.remove_log(200));
        assert!(!j.remove_log(999));
        assert_eq!(j.log.len(), 2);

        // A duplicated stamp takes the first (newest) of the pair — the one
        // `push_log` put at the front.
        j.push_log(entry(300, "c2"));
        assert!(j.remove_log(300));
        assert_eq!(j.log[0].text, "c");
        assert_eq!(j.log[1].at, 100);
    }

    #[test]
    fn round_trip_keeps_both_lists() {
        let path = scratch("round-trip");
        let mut j = Journal::default();
        j.push_take(TakeRecord {
            dir: r"D:\takes\a".to_string(),
            at: 1_700_000_000_000,
            seconds: Some(12.5),
            bytes: 15_000_000,
            failed: 1,
        });
        j.push_log(LogRecord {
            at: 1_700_000_000_001,
            level: Level::Error,
            text: "录制失败".to_string(),
            detail: Some("601x561".to_string()),
            dir: Some(r"D:\takes\a".to_string()),
        });
        j.save_to(&path).unwrap();

        let back = Journal::load_from(&path);
        assert_eq!(back.takes, j.takes);
        assert_eq!(back.log, j.log);
        assert_eq!(back.takes[0].failed, 1);
        assert_eq!(back.log[0].level, Level::Error);
    }

    #[test]
    fn missing_file_is_an_empty_journal() {
        let path = scratch("missing");
        assert_eq!(Journal::load_from(&path), Journal::default());
    }

    #[test]
    fn corrupt_file_is_an_empty_journal_not_an_error() {
        let path = scratch("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Journal::load_from(&path), Journal::default());
    }

    #[test]
    fn a_partial_file_fills_in_the_missing_lists() {
        let path = scratch("partial");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"takes":[]}"#).unwrap();
        let j = Journal::load_from(&path);
        assert!(j.takes.is_empty());
        assert!(j.log.is_empty());
        assert_eq!(j.seen_takes, 0);
    }

    #[test]
    fn unread_counts_records_newer_than_the_last_look() {
        let mut j = Journal::default();
        j.push_take(take(100));
        j.push_log(entry(100, "a"));
        j.seen_takes = 100;
        j.seen_log = 0;
        assert_eq!(j.unread(Pane::Takes), 0);
        assert_eq!(j.unread(Pane::Log), 1);

        j.push_take(take(101));
        assert_eq!(j.unread(Pane::Takes), 1);
    }

    #[test]
    fn looking_at_a_pane_clears_its_unread_count() {
        let mut j = Journal::default();
        j.push_take(take(100));
        j.push_log(entry(100, "a"));
        j.mark_seen(Pane::Takes);
        assert_eq!(j.unread(Pane::Takes), 0);
        assert_eq!(j.unread(Pane::Log), 1);
    }

    #[test]
    fn seen_is_the_newest_record_not_the_clock() {
        let mut j = Journal::default();
        j.push_take(take(100));
        j.push_log(entry(200, "a"));
        j.mark_seen(Pane::Takes);
        j.mark_seen(Pane::Log);
        // A wall-clock stamp would rewrite the file on every visit and could
        // mark a record that landed in the same millisecond as read.
        assert_eq!(j.seen_takes, 100);
        assert_eq!(j.seen_log, 200);
    }

    #[test]
    fn looking_at_a_pane_twice_changes_nothing_the_second_time() {
        let mut j = Journal::default();
        j.push_take(take(100));
        j.mark_seen(Pane::Takes);
        let once = j.clone();
        j.mark_seen(Pane::Takes);
        assert_eq!(j, once);
    }

    #[test]
    fn clearing_one_list_leaves_the_other() {
        let mut j = Journal::default();
        j.push_take(take(1));
        j.push_log(entry(1, "a"));
        j.clear(Pane::Takes);
        assert!(j.takes.is_empty());
        assert_eq!(j.log.len(), 1);
    }

    #[test]
    fn update_writes_back_and_keeps_untouched_fields() {
        let path = scratch("update");
        let mut j = Journal::default();
        j.push_take(take(1));
        j.save_to(&path).unwrap();

        Journal::update_at(&path, |j| j.push_log(entry(2, "b"))).unwrap();

        let back = Journal::load_from(&path);
        assert_eq!(back.takes.len(), 1);
        assert_eq!(back.log.len(), 1);
    }

    #[test]
    fn saving_leaves_no_staging_file_behind() {
        let path = scratch("staging");
        Journal::default().save_to(&path).unwrap();
        let siblings: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(siblings, vec!["journal.json".to_string()]);
    }
}
