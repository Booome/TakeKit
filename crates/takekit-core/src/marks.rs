//! Marks (click ripples, drag trails) come from MouseShow, linked as a library
//! rather than launched as a process: no handshake, no orphaned overlay if this
//! process dies.

pub use mouseshow::{Marks, MarksError};

/// Start the overlay with MouseShow's defaults.
pub fn start() -> Result<Marks, MarksError> {
    Marks::start(mouseshow::Config::default())
}
