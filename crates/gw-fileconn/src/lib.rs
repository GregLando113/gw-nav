//! FileConn: download files from the Guild Wars fileserver.
//!
//! [`FileClient`] speaks the fileserver protocol (see
//! `docs/fileserver-protocol.md`), [`decompress`] unpacks the downloaded
//! files, [`AssetManifest`] maps base file ids to their current revisions, and
//! [`ConnectionPool`] shares connections between threads.

mod client;
pub mod decompress;
pub mod manifest;
pub mod pool;

pub use client::{FILESERVER_FALLBACK_PORT, FILESERVER_PORT, Fetch, FileClient, RawFile};
pub use decompress::{DecompressError, decompress};
pub use manifest::{AssetManifest, ManifestEntry, ManifestError, MapFileCandidate};
pub use pool::{ConnectionPool, Pool, Pooled};

#[derive(thiserror::Error, Debug)]
pub enum FileConnError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not connect to any fileserver ({0})")]
    NoServer(String),
    #[error("file {0} not found on the fileserver")]
    NotFound(u32),
    #[error("the fileserver cancelled the request for file {0}")]
    Cancelled(u32),
    #[error("unexpected packet seq={seq:#04x} action={action} (expected {expected})")]
    UnexpectedPacket {
        seq: u8,
        action: u8,
        expected: &'static str,
    },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("decompression failed: {0}")]
    Decompress(#[from] DecompressError),
    #[error("bad asset manifest: {0}")]
    Manifest(#[from] ManifestError),
}
