//! What can be captured: monitors, cameras and microphones.
//!
//! Cameras and microphones are both enumerated through Media Foundation, which
//! is the only API that hands out a stable identifier for both: a symbolic link
//! for a camera, an endpoint id for a microphone. WASAPI can list endpoints too,
//! but pairing a name with an id there means reading a property store per
//! device; Media Foundation does it in one pass and lets the capture side pick
//! up exactly the device the user was shown.

use serde::{Deserialize, Serialize};

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaSource, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_ENDPOINT_ID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_GUID, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_VERSION, MFCreateAttributes,
    MFEnumDeviceSources, MFSTARTUP_NOSOCKET, MFStartup,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::{GUID, PWSTR};

use crate::error::{MediaError, Result};

/// A display to capture. `index` is one-based, the way the screen-capture crate
/// numbers monitors, and is what a source stores.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}

/// A camera. `link` is the symbolic link Media Foundation addresses it by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraInfo {
    pub name: String,
    pub link: String,
}

/// A microphone. `endpoint` is the WASAPI endpoint id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MicInfo {
    pub name: String,
    pub endpoint: String,
}

/// Everything capturable, in the order the panel shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceList {
    pub screens: Vec<ScreenInfo>,
    pub cameras: Vec<CameraInfo>,
    pub mics: Vec<MicInfo>,
}

impl DeviceList {
    pub fn is_empty(&self) -> bool {
        self.screens.is_empty() && self.cameras.is_empty() && self.mics.is_empty()
    }
}

/// Enumerate everything. A source that cannot be enumerated — a locked-down
/// camera, an unplugged microphone — yields an empty list rather than an error,
/// because the panel still has to open and still has to show the screen.
pub fn list() -> DeviceList {
    DeviceList {
        screens: screens().unwrap_or_default(),
        cameras: cameras().unwrap_or_default(),
        mics: microphones().unwrap_or_default(),
    }
}

pub fn screens() -> Result<Vec<ScreenInfo>> {
    use windows_capture::monitor::Monitor;

    let monitors = Monitor::enumerate().map_err(win_capture_error)?;
    let mut out = Vec::with_capacity(monitors.len());
    for m in &monitors {
        out.push(ScreenInfo {
            index: m.index().map_err(win_capture_error)?,
            name: m.name().map_err(win_capture_error)?,
            width: m.width().map_err(win_capture_error)?,
            height: m.height().map_err(win_capture_error)?,
            refresh: m.refresh_rate().map_err(win_capture_error)?,
        });
    }
    Ok(out)
}

pub fn cameras() -> Result<Vec<CameraInfo>> {
    let devices = video_sources()?;
    let mut out = Vec::with_capacity(devices.len());
    for d in &devices {
        out.push(CameraInfo {
            name: string_of(d, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)?,
            link: string_of(d, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)?,
        });
    }
    Ok(out)
}

pub fn microphones() -> Result<Vec<MicInfo>> {
    let devices = audio_sources()?;
    let mut out = Vec::with_capacity(devices.len());
    for d in &devices {
        out.push(MicInfo {
            name: string_of(d, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)?,
            endpoint: string_of(d, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_ENDPOINT_ID)?,
        });
    }
    Ok(out)
}

/// The error type of the screen-capture crate carries no `std::error::Error`
/// bound, so it is flattened into a message here rather than at every call.
pub(crate) fn win_capture_error<E: std::fmt::Display>(e: E) -> MediaError {
    MediaError::Capture(e.to_string())
}

/// COM has to be initialised on the thread that makes the calls, and it is
/// refcounted per thread. Idempotent, so every entry point can call it: the
/// one definition four capture threads used to each carry a copy of.
pub(crate) fn ensure_com() {
    // SAFETY: process-global and idempotent per thread; a thread that
    // already initialised COM answers with a benign error, which is
    // ignored: what matters is that it is initialised.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// Media Foundation has to be running before any of its calls, and it is
/// refcounted per process. One start for the lifetime of the process is
/// deliberate: shutting it down while another thread is mid-enumeration is
/// worse than the handful of bytes it keeps alive.
pub(crate) fn ensure_media_foundation() -> Result<()> {
    use std::sync::{Once, OnceLock};

    static START: Once = Once::new();
    static FAILED: OnceLock<String> = OnceLock::new();

    START.call_once(|| {
        // SAFETY: COM and Media Foundation are both process-wide singletons and
        // the `Once` makes their initialisation happen exactly once. A thread
        // that already initialised COM answers with a benign error, which is
        // ignored: what matters is that it is initialised.
        let result = unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
        };
        if let Err(e) = result {
            let _ = FAILED.set(e.to_string());
        }
    });
    match FAILED.get() {
        Some(e) => Err(MediaError::Capture(e.clone())),
        None => Ok(()),
    }
}

/// The activated device sources of one kind: video capture devices, or audio
/// capture devices.
fn device_sources(kind: GUID) -> Result<Vec<IMFActivate>> {
    ensure_media_foundation()?;
    // SAFETY: every call below is a COM call on objects this function owns, and
    // the raw array is freed before returning.
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.ok_or_else(|| MediaError::message("no attribute store"))?;
        attributes.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &kind)?;

        let mut devices: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        let enumerated = MFEnumDeviceSources(&attributes, &mut devices, &mut count);
        if let Err(e) = enumerated {
            return Err(e.into());
        }
        // Ownership moves into the vector; the array itself still has to go
        // back to the COM allocator.
        let found = if devices.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(devices, count as usize)
                .iter()
                .flatten()
                .cloned()
                .collect()
        };
        if !devices.is_null() {
            CoTaskMemFree(Some(devices as *const core::ffi::c_void));
        }
        Ok(found)
    }
}

fn video_sources() -> Result<Vec<IMFActivate>> {
    device_sources(MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)
}

fn audio_sources() -> Result<Vec<IMFActivate>> {
    device_sources(MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_GUID)
}

/// Open one device by its identifier.
///
/// The list is enumerated again rather than handed over: the caller holds a
/// name and an id, both of which survive a restart and a re-plug, while an
/// activated object does not. Matching on the id is what makes a remembered
/// source resolve to the same physical camera every time.
pub(crate) fn activate(kind: GUID, id_key: &GUID, wanted: &str) -> Result<IMFMediaSource> {
    let sources = device_sources(kind)?;
    let device = sources
        .iter()
        .find(|d| string_of(d, id_key).is_ok_and(|id| id == wanted))
        .ok_or_else(|| MediaError::message(format!("no device \"{wanted}\"")))?;
    // SAFETY: `ActivateObject` hands back a reference-counted COM object whose
    // lifetime is managed by the `IMFMediaSource` wrapper it produces.
    unsafe { device.ActivateObject::<IMFMediaSource>() }.map_err(Into::into)
}

/// One string attribute off an activated device, freed as soon as it is read.
fn string_of(device: &IMFActivate, key: &GUID) -> Result<String> {
    // SAFETY: `value` is filled in by the call and freed on this frame; the
    // string is copied out before that. The copy happens before the free on
    // both paths — a failed conversion used to return early and leak.
    unsafe {
        let mut value = PWSTR::null();
        device.GetAllocatedString(key, &mut value, std::ptr::null_mut())?;
        let text = value.to_string();
        CoTaskMemFree(Some(value.as_ptr().cast::<core::ffi::c_void>()));
        text.map_err(|e| MediaError::message(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever this machine has, the list has to agree with itself: a screen is
    /// always capturable, and an entry with an empty id could never be opened.
    #[test]
    fn every_listed_device_carries_an_id() {
        let devices = list();
        for camera in &devices.cameras {
            assert!(!camera.link.is_empty());
            assert!(!camera.name.is_empty());
        }
        for mic in &devices.mics {
            assert!(!mic.endpoint.is_empty());
            assert!(!mic.name.is_empty());
        }
        for screen in &devices.screens {
            assert!(screen.index > 0, "monitor indices are one-based");
            assert!(screen.width > 0 && screen.height > 0, "{screen:?}");
        }
    }

    #[test]
    fn a_screen_is_always_there_to_capture() {
        let screens = screens().expect("a monitor");
        assert!(!screens.is_empty());
    }
}
