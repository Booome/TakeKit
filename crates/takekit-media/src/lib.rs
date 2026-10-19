//! Capture screens, cameras and microphones through Windows APIs.
//!
//! The ffmpeg backend drives a child process per source and lets each one keep
//! its own clock. This crate is the alternative: one process, one clock, and
//! frames arriving straight from the OS — Windows Graphics Capture for a screen
//! or a window (which can capture a window that is behind others), Media
//! Foundation for a camera and a microphone, and Media Foundation again to
//! encode and mux.

pub mod camera;
pub mod devices;
pub mod error;
pub mod frame;
pub mod mediatype;
pub mod mic;
pub mod reader;
pub mod screen;
pub mod writer;

pub use camera::{CameraCapture, CameraFormat};
pub use devices::{CameraInfo, DeviceList, MicInfo, ScreenInfo};
pub use error::{MediaError, Result};
pub use frame::{AudioChunk, MediaFrame, PixelFormat, VideoFrame};
pub use mic::{MicCapture, MicFormat};
pub use screen::{ScreenCapture, ScreenTarget};
pub use writer::{AudioWriter, VideoWriter};
