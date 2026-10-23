//! Types exchanged with the webview.
//!
//! One place for the IPC shapes, so the commands, the tray panel and the
//! overlay all name them the same way. Field names are `snake_case` on both
//! sides — Tauri does not rename them.

use serde::{Deserialize, Serialize};

use takekit_core::encoder;
use takekit_core::source::{Backend, Source};

/// `Clone` because `begin` keeps a copy of the request next to the take it
/// produced, so a failed start can say how many sources it was for.
#[derive(Clone, Deserialize)]
pub struct StartRequest {
    pub sources: Vec<Source>,
    pub out_root: String,
    pub quality: u8,
    pub marks: bool,
    /// `None` keeps the automatic encoder pick; a value pins one, and core
    /// rejects it up front when the binary cannot actually encode with it.
    #[serde(default)]
    pub encoder: Option<encoder::Encoder>,
    /// ffmpeg children or the in-process OS capture APIs.
    #[serde(default)]
    pub backend: Backend,
    /// Bring window sources to the front before recording. Off by default:
    /// the Windows backend records the window itself and needs no raise, and
    /// yanking a covered window forward is not something to do unasked.
    #[serde(default)]
    pub raise_sources: bool,
}

#[derive(Serialize)]
pub struct StartResult {
    pub dir: String,
    pub engine: String,
    pub encoder: String,
}

/// The capture setup the panel remembers between runs. Deliberately not the
/// whole `Config`: the hotkey and the feedback switches are owned by Rust, and
/// a panel that echoed a stale copy of them back would clobber them.
#[derive(Deserialize)]
pub struct SessionConfig {
    pub sources: Vec<Source>,
    pub out_root: Option<String>,
    pub encoder: Option<encoder::Encoder>,
    #[serde(default)]
    pub backend: Backend,
    pub marks: bool,
    #[serde(default)]
    pub raise_sources: bool,
}

#[derive(Serialize)]
pub struct LiveSourceInfo {
    pub id: String,
    pub alive: bool,
    /// `false` once the window behind the source is gone, hidden or cloaked —
    /// the panel cuts that one source off rather than let it run on.
    pub usable: bool,
    pub bytes: u64,
}

#[derive(Serialize)]
pub struct StatusInfo {
    pub recording: bool,
    pub elapsed: f64,
    pub sources: Vec<LiveSourceInfo>,
    /// Why the last begin/spawn failed. Cleared when a new take is asked for,
    /// and what the panel's failed state reads (B5).
    pub error: Option<String>,
}

/// Payload of the `overlay:countdown` event: how many seconds the veil counts
/// down from before the encoders start.
#[derive(Serialize)]
pub struct CountdownInfo {
    pub secs: u8,
}

/// Payload of the `take:beep` event: one synthesised tone. The countdown-off
/// path hides the panel before the take starts, so the confirmation beep is
/// asked for here — the tray dot is the other half of that signal.
#[derive(Serialize)]
pub struct BeepInfo {
    pub hz: u16,
    pub ms: u16,
}

/// A failure the user is shown.
///
/// `code` names the message (`err.prepare-failed`); `detail` is the technical
/// half — an OS error, an ffmpeg line. The panel translates the code, because
/// a sentence composed in Rust would be stuck in whichever language it was
/// written in: the same failure has to read correctly in Chinese and English.
#[derive(Clone, Debug, Serialize)]
pub struct UiError {
    pub code: String,
    pub detail: Option<String>,
}

impl UiError {
    pub fn new(code: &str) -> Self {
        Self {
            code: code.to_string(),
            detail: None,
        }
    }

    /// The code plus the raw failure, for the case where the detail is the
    /// only thing that differs between two occurrences.
    pub fn with_detail(code: &str, detail: impl ToString) -> Self {
        Self {
            code: code.to_string(),
            detail: Some(detail.to_string()),
        }
    }

    /// A `diagnose` code string (`err.cam-no-signal|Mikey Cam`) unpacked into
    /// the same shape a hand-written error gets — so the panel translates the
    /// code and the machine face explains it, no matter which layer raised it.
    /// `None` when the string carries no code.
    pub fn from_code_string(text: &str) -> Option<Self> {
        let (code, detail) = takekit_core::diagnose::split(text);
        (!code.is_empty()).then(|| Self {
            code: code.to_string(),
            detail: Some(detail.to_string()),
        })
    }
}

impl std::fmt::Display for UiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{}: {detail}", self.code),
            None => f.write_str(&self.code),
        }
    }
}

/// The language the UI draws in, and the palette it uses. Both are settings
/// the panel owns; Rust only validates them and remembers the choice.
#[derive(Serialize)]
pub struct SettingsInfo {
    pub language: String,
    pub theme: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_info_serialises_the_error_field() {
        let json = serde_json::to_value(&StatusInfo {
            recording: false,
            elapsed: 0.0,
            sources: Vec::new(),
            error: Some("boom".to_string()),
        })
        .unwrap();
        assert_eq!(json["error"], "boom");
        assert_eq!(json["recording"], false);
    }

    #[test]
    fn countdown_info_carries_the_seconds() {
        let json = serde_json::to_value(&CountdownInfo { secs: 3 }).unwrap();
        assert_eq!(json["secs"], 3);
    }
}

#[derive(Clone, Serialize)]
pub struct OutcomeInfo {
    pub id: String,
    pub seconds: Option<f64>,
    pub bytes: u64,
    pub error: Option<String>,
    /// The size the file was written at. `0 x 0` when the backend cannot say
    /// (the ffmpeg path) — the stop checklist shows the resolution when it
    /// has one and leaves the cell blank when it does not.
    pub width: u32,
    pub height: u32,
}

impl From<&takekit_core::session::Outcome> for OutcomeInfo {
    fn from(o: &takekit_core::session::Outcome) -> Self {
        let (width, height) = o.size.unwrap_or((0, 0));
        Self {
            id: o.id.clone(),
            seconds: o.seconds,
            bytes: o.bytes,
            error: o.error.clone(),
            width,
            height,
        }
    }
}

/// Payload of `take:finalize`: the stop screen's first frame. `elapsed` is
/// the take's length at the moment stop was pressed — it freezes here, so
/// nothing downstream has to keep a clock running while the files drain.
/// `total` is the denominator the "writing… n/m" state counts against.
#[derive(Clone, Serialize)]
pub struct FinalizeInfo {
    pub dir: String,
    pub elapsed: f64,
    pub total: usize,
}

#[derive(Clone, Serialize)]
pub struct StopResult {
    pub dir: String,
    pub duration: Option<f64>,
    pub sources: Vec<OutcomeInfo>,
    pub failed: usize,
}
