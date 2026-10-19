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
    pub duration: Option<f64>,
    pub source: Source,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub encoder: String,
    pub hardware: bool,
    pub started_at: String,
    pub duration: Option<f64>,
    pub sources: Vec<SourceRecord>,
}

pub struct Take {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Take {
    /// Create `root/<timestamp>/` for a new take.
    pub fn create(root: &Path, encoder: Encoder, sources: &[Source]) -> std::io::Result<Self> {
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
            sources: sources
                .iter()
                .map(|s| SourceRecord {
                    id: s.id.clone(),
                    file: format!("{}.mkv", s.id),
                    duration: None,
                    source: s.clone(),
                })
                .collect(),
        };

        Ok(Self { dir, manifest })
    }

    pub fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.mkv"))
    }

    pub fn save_manifest(&self) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(&self.manifest)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // Written beside the target and renamed over it: the manifest is
        // rewritten when the take ends, and a crash mid-write must not leave a
        // truncated take.json where a valid one already was.
        crate::fsx::write_atomic(&self.dir.join("take.json"), &text)
    }

    pub fn set_duration(&mut self, id: &str, seconds: Option<f64>) {
        if let Some(s) = self.manifest.sources.iter_mut().find(|s| s.id == id) {
            s.duration = seconds;
        }
    }
}

fn timestamp() -> String {
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
            capture: crate::source::Capture::Desktop { rect: None },
            fps: 30,
            quality: None,
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
        let mut take = Take::create(&root, Encoder::X264, &[source("screen")]).unwrap();
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

    #[test]
    fn a_second_take_in_the_same_second_gets_its_own_directory() {
        let root = scratch("collision");
        let first = Take::create(&root, Encoder::X264, &[source("screen")]).unwrap();
        let second = Take::create(&root, Encoder::X264, &[source("screen")]).unwrap();
        assert_ne!(first.dir, second.dir);
        assert!(second.dir.to_string_lossy().ends_with("-2"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn set_duration_leaves_unknown_ids_alone() {
        let root = scratch("duration");
        let mut take = Take::create(&root, Encoder::X264, &[source("screen")]).unwrap();
        take.set_duration("missing", Some(1.0));
        assert!(take.manifest.sources[0].duration.is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
