//! A take: one directory holding every output of one recording run, plus a
//! manifest describing how it was made.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::encoder::Encoder;
use crate::source::Source;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceRecord {
    pub id: String,
    pub file: String,
    /// Where this file belongs on the take's timeline, in seconds from the
    /// take's origin — and how many seconds of media it holds. The pair is the
    /// contract every reader of a take works from: a clip sits at `start` and
    /// is `duration` long. `None` until the file has a first sample.
    ///
    /// A source that was there from the start says `0` even when its device
    /// woke up late: its file pays for the wait with black frames, so the file
    /// really does begin at the take's origin (see `takekit_media::writer`).
    /// A source added mid-take begins at its own first sample, and `start` is
    /// that instant — nothing was recorded before it, and nothing in the file
    /// pretends otherwise.
    #[serde(default)]
    pub start: Option<f64>,
    pub duration: Option<f64>,
    /// Frames the capture delivered (Windows-API sources). The file itself has
    /// `duration x fps` slots — a pause is held frames — so this number says
    /// how much of the take was a fresh picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
    /// Take-time at which this source was asked for mid-take. `None` for a
    /// source that was in the list before the take began.
    ///
    /// Kept separate from `start` on purpose: the difference between the two is
    /// the time the device took to wake up, which is the only evidence of how
    /// honest `start` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<f64>,
    /// What ffprobe measured in the finished file, when it could be run.
    ///
    /// Carried beside `duration` rather than folded into it: a disagreement is
    /// a fact about the take, and rounding it away is how a manifest stops
    /// being worth reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed: Option<f64>,
    pub source: Source,
}

impl SourceRecord {
    /// How far `probed` sits from `duration`. `None` when nothing measured it.
    pub fn length_error(&self) -> Option<f64> {
        match (self.duration, self.probed) {
            (Some(d), Some(p)) => Some(p - d),
            _ => None,
        }
    }

    /// Whether the container disagrees with the manifest about how long this
    /// file is, by enough to matter.
    pub fn length_mismatch(&self) -> bool {
        length_mismatch(self.duration, self.probed, self.source.fps)
    }
}

/// Whether a container's reading of a file's length disagrees with what the
/// take wrote down, by enough to matter.
///
/// A frame and a half is the tolerance: MP4 rounds a track's duration to its
/// own timescale, and a clip placed by these numbers is only wrong once the
/// error reaches a visible fraction of a second. Under that, the two readings
/// are the same number said twice.
pub fn length_mismatch(duration: Option<f64>, probed: Option<f64>, fps: u32) -> bool {
    let (Some(duration), Some(probed)) = (duration, probed) else {
        return false;
    };
    let frame = 1.0 / fps.max(1) as f64;
    (probed - duration).abs() > (1.5 * frame).max(0.15)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub encoder: String,
    pub hardware: bool,
    pub started_at: String,
    pub duration: Option<f64>,
    /// What every `start` was measured against.
    ///
    /// `"frames"`: the file's own first sample, read off the take's shared
    /// clock — exact, and what the Windows backend produces. `"spawn"`: when
    /// the encoder was launched, which is all the ffmpeg backend can see (each
    /// child files its own clock) — its offsets carry that backend's startup
    /// latency, up to a couple of hundred milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_clock: Option<String>,
    pub sources: Vec<SourceRecord>,
}

pub struct Take {
    pub dir: PathBuf,
    /// The output extension: `.mkv` for the ffmpeg backend, `.mp4` for the
    /// Windows one — Media Foundation's sink writer muxes MP4.
    pub ext: &'static str,
    pub manifest: Manifest,
}

impl Take {
    /// Create `root/<timestamp>/` for a new take.
    pub fn create(
        root: &Path,
        ext: &'static str,
        encoder: Encoder,
        sources: &[Source],
    ) -> std::io::Result<Self> {
        let stamp = timestamp();
        // A stop followed by a start inside the same second would otherwise
        // reuse the directory: the second take's manifest would overwrite the
        // first one's and `-y` would overwrite its files. `create_dir` is what
        // decides the winner — two processes racing here still get separate
        // folders instead of both seeing "does not exist" and sharing one.
        fs::create_dir_all(root)?;
        let mut dir = root.join(&stamp);
        let mut n = 2;
        loop {
            match fs::create_dir(&dir) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if n > 100 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            format!("too many takes in one second under {}", root.display()),
                        ));
                    }
                    dir = root.join(format!("{stamp}-{n}"));
                    n += 1;
                }
                Err(e) => return Err(e),
            }
        }

        let manifest = Manifest {
            encoder: encoder.name().to_string(),
            hardware: encoder.is_hardware(),
            started_at: stamp,
            duration: None,
            start_clock: None,
            sources: sources
                .iter()
                .map(|s| SourceRecord {
                    id: s.id.clone(),
                    // Audio lands in `.mp4` even in a `.mkv` take: the MF sink
                    // writer that renders it names its container after the
                    // extension and does not know Matroska (see
                    // `Capture::output_ext`).
                    file: format!("{}.{}", s.id, s.capture.output_ext(ext)),
                    start: None,
                    duration: None,
                    frames: None,
                    added_at: None,
                    probed: None,
                    source: s.clone(),
                })
                .collect(),
        };

        Ok(Self { dir, ext, manifest })
    }

    /// Record a source that joined the take after it began.
    ///
    /// `added_at` is the take-time of the request. The file's own `start` is
    /// filled in later, from the first sample that actually arrived — the
    /// request is what the user asked for, the sample is what happened, and a
    /// manifest that only kept the first would be a wish rather than a record.
    pub fn add_source(&mut self, src: &Source, added_at: f64) {
        self.manifest.sources.push(SourceRecord {
            id: src.id.clone(),
            file: format!("{}.{}", src.id, src.capture.output_ext(self.ext)),
            start: None,
            duration: None,
            frames: None,
            added_at: Some(added_at),
            probed: None,
            source: src.clone(),
        });
    }

    /// Take a source back out — a mid-take add whose capture refused to open
    /// leaves no file and nothing to report, so it must not leave a row that
    /// claims otherwise.
    pub fn remove_source(&mut self, id: &str) {
        self.manifest.sources.retain(|s| s.id != id);
    }

    /// Where a source's file sits on the take's timeline.
    pub fn set_start(&mut self, id: &str, seconds: Option<f64>) {
        if let Some(s) = self.manifest.sources.iter_mut().find(|s| s.id == id) {
            s.start = seconds;
        }
    }

    /// What ffprobe measured for a finished file.
    pub fn set_probed(&mut self, id: &str, seconds: Option<f64>) {
        if let Some(s) = self.manifest.sources.iter_mut().find(|s| s.id == id) {
            s.probed = seconds;
        }
    }

    /// Where a source's file lives. Reads the manifest rather than recomputing
    /// the name, so an audio source's `.mp4` inside a `.mkv` take (see
    /// `Capture::output_ext`) is found exactly where `create` said it would.
    pub fn path_for(&self, id: &str) -> PathBuf {
        self.manifest
            .sources
            .iter()
            .find(|s| s.id == id)
            .map(|s| self.dir.join(&s.file))
            .unwrap_or_else(|| self.dir.join(format!("{id}.{}", self.ext)))
    }

    pub fn save_manifest(&self) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(&self.manifest)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // Written beside the target and renamed over it: the manifest is
        // rewritten when the take ends, and a crash mid-write must not leave a
        // truncated take.json where a valid one already was.
        crate::fsx::write_atomic(&self.dir.join("take.json"), &text)
    }

    /// Record how many frames a source's capture delivered.
    pub fn set_frames(&mut self, id: &str, frames: Option<u64>) {
        if let Some(s) = self.manifest.sources.iter_mut().find(|s| s.id == id) {
            s.frames = frames;
        }
    }

    pub fn set_duration(&mut self, id: &str, seconds: Option<f64>) {
        if let Some(s) = self.manifest.sources.iter_mut().find(|s| s.id == id) {
            s.duration = seconds;
        }
    }
}

/// Why `delete_dir` refused to touch a folder.
///
/// The variants name the reason, not a message: `Display` composes the
/// Chinese sentence logs and the control plane print, and the panel maps
/// each variant onto a translated `UiError` code — a user-visible sentence
/// composed in Rust would be stuck in whichever language it was written in
/// (see `takekit_gui::proto::UiError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteError {
    /// Empty, or no final component (`C:\`): nothing that could name a take
    /// folder. Refused before the path is touched at all.
    NotADirectory(String),
    /// The path exists but is a file.
    NotATake(String),
    /// A directory without our manifest — the `take.json` guard.
    MissingManifest(String),
    /// `remove_dir_all` itself failed; the folder may be half gone.
    Io(String),
}

impl std::fmt::Display for DeleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotADirectory(dir) => write!(f, "不是可删除的录制目录：{dir}"),
            Self::NotATake(dir) => write!(f, "不是录制目录：{dir}"),
            Self::MissingManifest(dir) => write!(f, "不是录制目录（缺少 take.json）：{dir}"),
            Self::Io(e) => write!(f, "删除失败：{e}"),
        }
    }
}

impl std::error::Error for DeleteError {}

/// Delete one take folder — the disk half of removing a history row.
///
/// The `take.json` check is what makes this safe to call with a string that
/// came off a journal entry: a folder this app did not write a manifest into
/// is never deleted, so a hand-edited journal cannot aim the command at an
/// arbitrary directory. Same for a path with no final component (`C:\`).
///
/// A path that is already gone returns `Ok`: there is nothing left to
/// protect, and the caller drops the record instead of keeping a tombstone
/// row that can never be removed. `Err` always means "did not delete, and
/// the caller should not drop the record either".
pub fn delete_dir(dir: &str) -> Result<(), DeleteError> {
    let path = Path::new(dir);
    if dir.is_empty() || path.file_name().is_none() {
        return Err(DeleteError::NotADirectory(dir.to_string()));
    }
    if !path.exists() {
        return Ok(());
    }
    if !path.is_dir() {
        return Err(DeleteError::NotATake(dir.to_string()));
    }
    if !path.join("take.json").is_file() {
        return Err(DeleteError::MissingManifest(dir.to_string()));
    }
    fs::remove_dir_all(path).map_err(|e| DeleteError::Io(e.to_string()))
}

/// The take-folder stamp: **local** time as `YYYY-MM-DD-HH-MM-SS`.
///
/// Shared by every front end (panel, CLI `record`, CLI `media`) so a folder
/// name means the same instant everywhere. Local, not UTC: the stamp is read
/// by people and lined up against the panel's history, and a UTC name on a
/// UTC+8 machine reads eight hours off (the CLI's media harness once computed
/// the date from the UNIX epoch by hand and had exactly that skew).
pub fn timestamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: GetLocalTime fills a SYSTEMTIME by value and has no pointer or
    // lifetime preconditions.
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02}-{:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::Encoder;

    fn source(id: &str) -> Source {
        Source {
            id: id.to_string(),
            capture: crate::source::Capture::desktop(None),
            fps: 30,
            quality: None,
            audio: false,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("takekit-core-take-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_manifest_round_trips_through_disk() {
        let root = scratch("roundtrip");
        let mut take = Take::create(&root, "mkv", Encoder::X264, &[source("screen")]).unwrap();
        take.set_duration("screen", Some(2.5));
        take.save_manifest().unwrap();

        let body = fs::read_to_string(take.dir.join("take.json")).unwrap();
        let back: Manifest = serde_json::from_str(&body).unwrap();
        assert_eq!(back.sources.len(), 1);
        assert_eq!(back.sources[0].id, "screen");
        assert_eq!(back.sources[0].file, "screen.mkv");
        assert_eq!(back.sources[0].duration, Some(2.5));
        assert!(back.duration.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// A source added mid-take records the request and the arrival separately:
    /// the gap between them is the device's wake-up, and it is the only
    /// evidence of how honest `start` is.
    #[test]
    fn a_source_added_mid_take_keeps_request_and_arrival_apart() {
        let root = scratch("added");
        let mut take = Take::create(&root, "mp4", Encoder::X264, &[source("screen")]).unwrap();
        take.add_source(&source("cam"), 12.5);
        // Nothing has arrived yet, so the file has no place on the timeline.
        assert_eq!(take.manifest.sources[1].start, None);
        assert_eq!(take.manifest.sources[1].added_at, Some(12.5));
        assert_eq!(take.path_for("cam"), take.dir.join("cam.mp4"));

        take.set_start("cam", Some(12.83));
        take.set_duration("cam", Some(47.17));
        take.set_probed("cam", Some(47.2));
        let rec = &take.manifest.sources[1];
        assert_eq!(rec.start, Some(12.83));
        // The two lengths disagree by 30 ms — on record, not rounded away.
        let err = rec.length_error().unwrap();
        assert!((err - 0.03).abs() < 1e-9, "got {err}");

        // The take's own length is where the longest source reaches.
        take.manifest.duration = Some(60.0);
        take.save_manifest().unwrap();
        let body = fs::read_to_string(take.dir.join("take.json")).unwrap();
        let back: Manifest = serde_json::from_str(&body).unwrap();
        assert_eq!(back.sources[1].start, Some(12.83));
        assert_eq!(back.sources[1].added_at, Some(12.5));
        assert_eq!(back.sources[1].probed, Some(47.2));
        let _ = fs::remove_dir_all(&root);
    }

    /// A failed mid-take add must not leave a row claiming a file that was
    /// never written.
    #[test]
    fn removing_a_source_takes_its_row_with_it() {
        let root = scratch("removed");
        let mut take = Take::create(&root, "mp4", Encoder::X264, &[source("screen")]).unwrap();
        take.add_source(&source("cam"), 1.0);
        assert_eq!(take.manifest.sources.len(), 2);
        take.remove_source("cam");
        assert_eq!(take.manifest.sources.len(), 1);
        assert_eq!(take.manifest.sources[0].id, "screen");
        let _ = fs::remove_dir_all(&root);
    }

    /// A container reading and the take's own number only disagree once the
    /// gap is worth seeing: MP4 rounds a track duration to its timescale, so
    /// a few tens of milliseconds is the same number said twice.
    #[test]
    fn a_length_only_mismatches_once_it_is_visible() {
        // 30 fps: a frame is 33 ms, so the floor of 150 ms is what decides.
        assert!(!length_mismatch(Some(10.0), Some(10.12), 30));
        assert!(length_mismatch(Some(10.0), Some(10.16), 30));
        // 5 fps: a frame and a half is 300 ms, and that wins over the floor.
        assert!(!length_mismatch(Some(10.0), Some(10.2), 5));
        assert!(length_mismatch(Some(10.0), Some(10.4), 5));
        // Nothing to compare is not a mismatch: silence is not a verdict.
        assert!(!length_mismatch(Some(10.0), None, 30));
        assert!(!length_mismatch(None, Some(10.0), 30));
    }

    #[test]
    fn a_second_take_in_the_same_second_gets_its_own_directory() {
        let root = scratch("collision");
        let first = Take::create(&root, "mkv", Encoder::X264, &[source("screen")]).unwrap();
        let second = Take::create(&root, "mkv", Encoder::X264, &[source("screen")]).unwrap();
        assert_ne!(first.dir, second.dir);
        assert!(second.dir.to_string_lossy().ends_with("-2"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn set_duration_leaves_unknown_ids_alone() {
        let root = scratch("duration");
        let mut take = Take::create(&root, "mkv", Encoder::X264, &[source("screen")]).unwrap();
        take.set_duration("missing", Some(1.0));
        assert!(take.manifest.sources[0].duration.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_dir_removes_a_take_folder_whole() {
        let root = scratch("delete");
        let take = Take::create(&root, "mkv", Encoder::X264, &[source("screen")]).unwrap();
        take.save_manifest().unwrap();
        fs::write(take.dir.join("screen.mkv"), b"x").unwrap();

        delete_dir(&take.dir.to_string_lossy()).unwrap();
        assert!(!take.dir.exists());
        // The output root it lived in is not collateral damage.
        assert!(root.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_dir_refuses_a_folder_without_a_manifest() {
        let root = scratch("no-manifest");
        let plain = root.join("not-a-take");
        fs::create_dir_all(&plain).unwrap();
        fs::write(plain.join("notes.txt"), b"x").unwrap();

        let err = delete_dir(&plain.to_string_lossy()).unwrap_err();
        assert_eq!(
            err,
            DeleteError::MissingManifest(plain.to_string_lossy().into_owned())
        );
        assert!(err.to_string().contains("take.json"), "{err}");
        assert!(plain.exists(), "the folder must survive a refused delete");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_dir_refuses_a_file() {
        let root = scratch("a-file");
        let file = root.join("take.json");
        fs::write(&file, b"x").unwrap();

        let err = delete_dir(&file.to_string_lossy()).unwrap_err();
        assert_eq!(
            err,
            DeleteError::NotATake(file.to_string_lossy().into_owned())
        );
        assert!(err.to_string().contains("不是录制目录"), "{err}");
        assert!(file.exists(), "the file must survive a refused delete");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_dir_refuses_an_empty_path() {
        assert!(delete_dir("").is_err());
    }

    #[test]
    fn delete_dir_refuses_a_drive_root() {
        let err = delete_dir(r"C:\").unwrap_err();
        assert_eq!(err, DeleteError::NotADirectory(r"C:\".to_string()));
        assert!(err.to_string().contains("不是可删除的录制目录"), "{err}");
    }

    #[test]
    fn delete_dir_treats_a_folder_already_gone_as_done() {
        // Already gone = nothing left to protect. The row is the only thing
        // left to remove, and an error here would trap it in the list forever.
        let root = scratch("gone");
        let dir = root.join("vanished");
        assert!(delete_dir(&dir.to_string_lossy()).is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    /// The stamp every front end names its folders with: six zero-padded
    /// numbers, plausible as a date. Guards the CLI's hand-rolled UTC
    /// version from coming back — it produced names eight hours off the
    /// panel's history on this machine's timezone.
    #[test]
    fn the_stamp_is_a_plausible_local_date_time() {
        let stamp = timestamp();
        let digits: Vec<u32> = stamp.split('-').map(|p| p.parse().unwrap()).collect();
        assert_eq!(digits.len(), 6, "{stamp}");
        let [y, m, d, h, min, s] = digits.as_slice() else {
            unreachable!()
        };
        assert!((1970..=9999).contains(y), "{stamp}");
        assert!((1..=12).contains(m), "{stamp}");
        assert!((1..=31).contains(d), "{stamp}");
        assert!(h <= &23 && min <= &59 && s <= &59, "{stamp}");
    }
}
