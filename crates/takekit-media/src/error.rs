//! Errors from the Windows capture APIs.
//!
//! Three families reach this point: `windows::core::Error` from the OS
//! bindings, the error type of the screen-capture crate, and plain messages for
//! the things we decide ourselves (no device matches, a size we cannot encode).
//! Callers only ever need to display them, so they collapse into one type.

use std::fmt;

#[derive(Debug)]
pub enum MediaError {
    Windows(windows::core::Error),
    Capture(String),
    Message(String),
}

impl MediaError {
    pub fn message(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }

    /// Prefix the error with where it happened, so a bare HRESULT can be
    /// traced back to the call that produced it.
    pub fn context(self, where_: &str) -> Self {
        Self::Message(format!("{where_}: {self}"))
    }
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Windows(e) => write!(f, "{e}"),
            Self::Capture(e) => write!(f, "{e}"),
            Self::Message(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MediaError {}

impl From<windows::core::Error> for MediaError {
    fn from(e: windows::core::Error) -> Self {
        Self::Windows(e)
    }
}

impl From<String> for MediaError {
    fn from(e: String) -> Self {
        Self::Message(e)
    }
}

pub type Result<T> = std::result::Result<T, MediaError>;
