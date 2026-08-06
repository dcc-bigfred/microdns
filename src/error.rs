//! Error types for microdns.

use std::path::{Path, PathBuf};

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("I/O error on {path}: {source}")]
    IoPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config: {0}")]
    Config(String),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("mDNS error: {0}")]
    Mdns(String),

    #[error("nix error: {0}")]
    Nix(#[from] nix::Error),

    #[error("{0}")]
    Other(String),
}

impl Error {
    #[must_use]
    pub fn io_at(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Self::IoPath {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }
}
