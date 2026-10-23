//! One camera list for every door — panel picker, control surface, CLI.
//!
//! DirectShow and Media Foundation report different sets. A virtual camera
//! (Mikey Cam) usually registers only a DirectShow filter, so the Media
//! Foundation walk never sees it; each backend can open only what its own
//! enumeration reports. Listing one side alone made the panel and the agent
//! disagree about what exists. This merges both and tags every entry with
//! the backends that can open it, plus the DirectShow liveness read.

use std::path::Path;

use serde::Serialize;
use takekit_media::devices::CameraInfo;

use crate::ffmpeg::{self, DshowCamera};

/// One camera, merged across both enumerations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CameraEntry {
    pub name: String,
    /// The DirectShow listing's tag: `Some(false)` = registered but no output
    /// pin, i.e. a virtual camera whose host app is off. `None` when unknown
    /// (no ffmpeg, or a build that prints no tag).
    pub live: Option<bool>,
    /// The ffmpeg backend (DirectShow) can open it.
    pub dshow: bool,
    /// The windows backend (Media Foundation) can open it.
    pub mf: bool,
    /// Media Foundation's symbolic link for the device, for the windows
    /// backend's selection. `None` when `mf` is false.
    pub link: Option<String>,
}

/// Every camera either enumeration reports, in DirectShow order first.
///
/// `ffmpeg` is the binary for the DirectShow probe; `None` skips that side
/// (then every entry is MF-only and `live` is unknown).
pub fn cameras(ffmpeg: Option<&Path>) -> Vec<CameraEntry> {
    let dshow = ffmpeg.map(ffmpeg::list_capture_devices).unwrap_or_default();
    let mf = takekit_media::devices::cameras().unwrap_or_default();
    merge(&dshow, mf)
}

/// The merge itself, split out so tests can feed the two lists directly.
pub fn merge(dshow: &[DshowCamera], mf: Vec<CameraInfo>) -> Vec<CameraEntry> {
    let mut out: Vec<CameraEntry> = Vec::new();
    for d in dshow {
        // One name both sides report is one camera: the dshow row keeps its
        // liveness tag and picks up the MF link.
        let m = mf.iter().find(|m| m.name == d.name);
        out.push(CameraEntry {
            name: d.name.clone(),
            live: d.live,
            dshow: true,
            mf: m.is_some(),
            link: m.map(|m| m.link.clone()),
        });
    }
    // What only MF knows about (rare) trails the dshow order.
    for m in mf {
        if !out.iter().any(|e| e.name == m.name) {
            out.push(CameraEntry {
                name: m.name,
                live: None,
                dshow: false,
                mf: true,
                link: Some(m.link),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(name: &str, live: Option<bool>) -> DshowCamera {
        DshowCamera {
            name: name.into(),
            live,
        }
    }

    fn mf(name: &str, link: &str) -> CameraInfo {
        CameraInfo {
            name: name.into(),
            link: link.into(),
        }
    }

    #[test]
    fn a_virtual_camera_only_directshow_knows_stays_dshow_only() {
        let out = merge(&[ds("Mikey Cam", Some(false))], vec![]);
        assert_eq!(out.len(), 1);
        assert!(out[0].dshow && !out[0].mf);
        assert_eq!(out[0].live, Some(false));
        assert_eq!(out[0].link, None);
    }

    #[test]
    fn a_camera_both_sides_know_is_one_entry_carrying_the_mf_link() {
        let out = merge(
            &[ds("Integrated Camera", Some(true))],
            vec![mf("Integrated Camera", "\\\\?\\usb#vid")],
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].dshow && out[0].mf);
        assert_eq!(out[0].link.as_deref(), Some("\\\\?\\usb#vid"));
    }

    #[test]
    fn mf_only_cameras_trail_the_dshow_order() {
        let out = merge(&[ds("A", None)], vec![mf("B", "b"), mf("A", "a")]);
        let names: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["A", "B"]);
        assert!(!out[1].dshow && out[1].mf);
        assert_eq!(out[1].live, None);
    }
}
