//! Miner-side runtime management: the pinned llama.cpp runtime, verified
//! weight downloads, managed `llama-server` children, the Clef token counter,
//! and the loopback HTTP client the miner uses to run jobs.

pub mod cache;
pub mod client;
pub mod download;
pub mod helper;
pub mod install;
pub mod manifest;
pub mod process;
pub mod sse;

pub use cache::Cache;
pub use client::{LocalClient, RuntimeError};
pub use helper::ClefCounter;
pub use install::InstalledRuntime;
pub use process::{LaunchSpec, LlamaServer};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Error::Message(message.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Reports `(done, total)` bytes for a long-running step.
pub type Progress<'a> = &'a (dyn Fn(Stage, u64, u64) + Send + Sync);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Downloading,
    Verifying,
    Extracting,
}
