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
    pub fps: u32,
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
    pub fn create(root: &Path, encoder: Encoder, fps: u32, sources: &[Source]) -> std::io::Result<Self> {
        let stamp = timestamp();
        let dir = root.join(&stamp);
        fs::create_dir_all(&dir)?;

        let manifest = Manifest {
            encoder: encoder.name().to_string(),
            hardware: encoder.is_hardware(),
            fps,
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
        fs::write(self.dir.join("take.json"), text)
    }

    pub fn set_duration(&mut self, id: &str, seconds: Option<f64>) {
        if let Some(s) = self.manifest.sources.iter_mut().find(|s| s.id == id) {
            s.duration = seconds;
        }
    }
}

fn timestamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02}-{:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}
