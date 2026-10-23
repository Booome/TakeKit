//! Turning capture failures into something a person can act on.
//!
//! The OS and ffmpeg say what happened ("Could not find output pin from
//! video capture device"); nobody says what to *do*. A virtual camera like
//! Mikey Cam is registered the moment its app is installed, shows up in every
//! device list, and fails exactly like this whenever its host app is not
//! running. The fix is one sentence away, so the failure is recognised here
//! and comes back as a stable code (`err.cam-no-signal|Mikey Cam`) that the
//! panel translates for people and the CLI prints as an English sentence.
//!
//! Codes are the currency (the same convention as `preview.audio` and
//! `UiError`): a sentence composed in Rust would be stuck in one language,
//! and the same failure has to read correctly in Chinese and English.

/// Known failure signatures, in the order they are matched against a raw
/// error. Each becomes `code|detail` — the detail is whatever the message is
/// about (a device name when the caller knows one).
pub const CAM_NO_SIGNAL: &str = "err.cam-no-signal";
pub const CAM_NOT_MF: &str = "err.cam-not-mf";
pub const DEVICE_GONE: &str = "err.device-gone";
pub const CAM_OPEN: &str = "err.cam-open";
pub const FFMPEG_DIED: &str = "err.ffmpeg-died";

/// Recognise a raw capture failure and give it a code.
///
/// `raw` is an ffmpeg log tail or a Media Foundation message; `device` is
/// the source's own name for the device when the caller knows it. Anything
/// unrecognised comes back unchanged — the raw words are the only truth
/// left, and guessing would be worse than quoting.
pub fn diagnose(raw: &str, device: Option<&str>) -> String {
    match classify(raw, device) {
        Some(code) => code,
        None => raw.to_string(),
    }
}

/// The same recognition, but the caller keeps its own words when nothing
/// matches — a log tail quoted verbatim is better than a wrong guess.
pub fn classify(raw: &str, device: Option<&str>) -> Option<String> {
    let detail = device.unwrap_or_default();
    let code = if raw.contains("Could not find output pin") {
        // A DirectShow virtual camera whose host app is not pushing frames:
        // the filter is registered, but it has nothing to output. Looked for
        // before "Error opening input", which the same failure also logs.
        Some(CAM_NO_SIGNAL)
    } else if raw.contains("visible to Media Foundation") {
        // Raised by session::camera_link itself: the camera exists for
        // DirectShow only, which the Windows backend cannot reach.
        Some(CAM_NOT_MF)
    } else if raw.contains("Could not find video device")
        || raw.contains("Could not find audio device")
        || raw.contains("No such device")
    {
        Some(DEVICE_GONE)
    } else if raw.contains("Error opening input")
        || raw.contains("I/O error")
        || raw.contains("Could not set video options")
    {
        // The dshow open failed without saying why. On this machine that is
        // how a virtual camera with no host app behind it reports (the
        // "no output pin" wording only appears on some builds), so the
        // sentence leads with the fix that covers it.
        Some(CAM_OPEN)
    } else {
        None
    }?;
    Some(if detail.is_empty() {
        code.to_string()
    } else {
        format!("{code}|{detail}")
    })
}

/// The code back into an English sentence — what the CLI prints and agents
/// read. A string without a code is already the sentence (a raw OS line).
pub fn explain(message: &str) -> String {
    let (code, detail) = split(message);
    let what = if detail.is_empty() {
        "the device".to_string()
    } else {
        format!("\"{detail}\"")
    };
    match code {
        CAM_NO_SIGNAL => format!(
            "{what} is registered but sends no video — start its host app (e.g. the Mikey client) and try again"
        ),
        CAM_NOT_MF => format!(
            "{what} is a DirectShow-only camera; Media Foundation cannot open it — record it on the ffmpeg backend"
        ),
        DEVICE_GONE => format!(
            "no capture device named {what} — it may have been unplugged or renamed; pick it again"
        ),
        CAM_OPEN => format!(
            "{what} could not be opened — if it is a virtual camera, start its host app (e.g. the Mikey client); otherwise it may be in use by another app"
        ),
        FFMPEG_DIED => "the source exited without writing a frame".to_string(),
        _ => message.to_string(),
    }
}

/// `code|detail` back into its halves. No code means the whole string is
/// both halves' fallback: the code is empty and the detail is the message.
pub fn split(message: &str) -> (&str, &str) {
    match message.split_once('|') {
        Some((code, detail)) if is_code(code) => (code, detail),
        _ => ("", message),
    }
}

fn is_code(s: &str) -> bool {
    s.starts_with("err.") && s[4..].chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_virtual_camera_without_a_host_gets_the_no_signal_code() {
        let raw = "Could not find output pin from video capture device.";
        assert_eq!(
            diagnose(raw, Some("Mikey Cam")),
            "err.cam-no-signal|Mikey Cam"
        );
    }

    #[test]
    fn a_dshow_open_failure_without_a_reason_still_points_at_the_host_app() {
        // This machine's ffmpeg build reports the same no-host failure as a
        // bare open error; the sentence has to cover it either way.
        let raw = "Could not set video options\nError opening input: I/O error";
        let msg = diagnose(raw, Some("Mikey Cam"));
        assert_eq!(msg, "err.cam-open|Mikey Cam");
        assert!(explain(&msg).contains("host app"));
    }

    #[test]
    fn an_mf_invisible_camera_names_the_ffmpeg_backend() {
        assert_eq!(
            diagnose("no camera \"x\" visible to Media Foundation", Some("x")),
            "err.cam-not-mf|x"
        );
    }

    #[test]
    fn an_unknown_failure_passes_through_untouched() {
        assert_eq!(
            diagnose("something else entirely", None),
            "something else entirely"
        );
    }

    #[test]
    fn explain_turns_a_code_into_a_sentence_and_leaves_others_alone() {
        let s = explain("err.cam-no-signal|Mikey Cam");
        assert!(s.contains("Mikey Cam"));
        assert!(s.contains("host app"));
        assert_eq!(explain("raw words"), "raw words");
    }

    #[test]
    fn split_reads_the_two_halves_back() {
        assert_eq!(split("err.cam-open|USB Cam"), ("err.cam-open", "USB Cam"));
        assert_eq!(split("plain"), ("", "plain"));
    }
}
