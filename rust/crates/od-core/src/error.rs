use std::io;
use std::path::PathBuf;

/// Shared error type for the OpenDesign Rust workspace.
///
/// Message wording that is user-visible in the CLI or daemon logs should stay
/// stable: the TypeScript daemon's error strings are the parity reference.
#[derive(Debug, thiserror::Error)]
pub enum OdError {
    /// The daemon data root must be provided explicitly. Per
    /// `AGENTS.md` → "Daemon data directory contract", `OD_DATA_DIR` is the
    /// single truth source; there is no cwd-relative fallback in Rust.
    #[error("OD_DATA_DIR is not set: the daemon data root must be provided explicitly")]
    DataDirUnset,

    #[error("OD_DATA_DIR \"{path}\" could not be created: {source}")]
    DataDirUnusable {
        path: PathBuf,
        source: io::Error,
    },

    #[error("OD_DATA_DIR \"{path}\" is not writable: {source}")]
    DataDirNotWritable {
        path: PathBuf,
        source: io::Error,
    },

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: io::Error,
    },

    #[error(transparent)]
    IoSimple(#[from] io::Error),
}

impl OdError {
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        OdError::Io {
            path: path.into(),
            source,
        }
    }
}
