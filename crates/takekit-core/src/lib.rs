//! TakeKit — record several sources at once into a single take.
//!
//! The idea is the film-slate one: a *take* is one run of the camera(s). Every
//! source starts and stops with the take, so the outputs line up without any
//! post-production alignment.

pub mod encoder;
pub mod ffmpeg;
pub mod fsx;
pub mod marks;
pub mod proc;
pub mod session;
pub mod source;
pub mod take;
