//! Global-hotkey strings: validation and normalisation for display.
//!
//! The plugin owns the parsing and the registration; this module only decides
//! whether a string is one the plugin can actually accept, and returns the form
//! the panel should show back to the user.

use tauri_plugin_global_shortcut::Shortcut;

/// The normalised display form of `input`, or `None` when the plugin cannot
/// parse it (an empty string, a typo'd key name, a bare modifier).
pub fn normalize(input: &str) -> Option<String> {
    input.trim().parse::<Shortcut>().ok().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_plain_key() {
        assert!(normalize("f9").is_some());
        assert!(normalize("F9").is_some());
    }

    #[test]
    fn accepts_a_combo() {
        assert!(normalize("Ctrl+Shift+R").is_some());
        assert!(normalize("ctrl+alt+r").is_some());
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(normalize("nonsense"), None);
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("Ctrl+NotAKey"), None);
    }

    #[test]
    fn is_idempotent() {
        let once = normalize("ctrl+alt+r").unwrap();
        assert_eq!(normalize(&once), Some(once));
    }

    #[test]
    fn the_form_the_panel_generates_parses() {
        // main.js builds "<mods>+<KeyboardEvent.code>": Ctrl/Alt/Shift then a
        // `Code` name such as KeyR, F9, ArrowLeft. If this stops parsing, the
        // hotkey chip silently rejects every combination.
        assert_eq!(
            normalize("Ctrl+Alt+KeyR").as_deref(),
            Some("control+alt+KeyR")
        );
        assert!(normalize("Ctrl+Shift+F9").is_some());
        assert!(normalize("Alt+ArrowLeft").is_some());
        // The registrar's own spelling must survive the round trip too.
        assert_eq!(
            normalize("control+alt+KeyR").as_deref(),
            Some("control+alt+KeyR")
        );
    }
}
