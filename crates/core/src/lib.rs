//! riplika - turn a disc into a tagged, subtitled library.
//!
//! The pipeline is four stages: **rip** the disc, **identify** what is on it,
//! **transcode** the titles, and recognise their **subtitles**. Each stage is a
//! module here; `job` runs them in order and reports progress.
//!
//! Two rules shape the whole crate, and both come from bugs that shipped in the
//! shell scripts this replaces:
//!
//! 1. **Deciding is separate from doing.** Nothing that talks to ffmpeg or
//!    MakeMKV also decides what to ask them. A planner turns state into an
//!    argv vector, and a runner executes it. Every planner is a pure function,
//!    so a test can assert on the exact arguments with no disc in the drive -
//!    which is how the missing `-map 0:s` and the off-by-one `-disposition`
//!    indices would have been caught.
//!
//! 2. **The outside world is behind a trait.** `Prober`, `Runner`, `Ripper` and
//!    `Catalogue` all have fake implementations used by the tests, so the whole
//!    pipeline runs end to end in milliseconds without hardware or network.

pub mod audio;
pub mod cdtext;
pub mod disc;
pub mod format;
pub mod hash;
pub mod host;
pub mod identify;
pub mod job;
pub mod joblog;
pub mod lang;
pub mod media;
pub mod mkvtags;
pub mod model;
pub mod musicjob;
pub mod naming;
pub mod prefs;
pub mod redump;
pub mod rescue;
pub mod rip;
pub mod secret;
pub mod subs;
pub mod transcode;

pub use model::*;

/// Anything that can go wrong in the pipeline.
///
/// One flat error type: the callers - a CLI printing a line and a GUI showing a
/// banner - both only ever want the message, and a deep error hierarchy would
/// be ceremony neither uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error(s.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Build an [`Error`] from a format string.
#[macro_export]
macro_rules! err {
    ($($t:tt)*) => { $crate::Error(format!($($t)*)) };
}
