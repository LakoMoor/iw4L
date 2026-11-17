//! One filesystem API for the whole engine.
//!
//! Native targets re-export `std::fs`. The browser has no disk the engine can
//! read synchronously, so there the tree lives in memory: IW4PK shards are
//! mounted under the games root and files the engine writes (settings, caches)
//! stay in the page's memory.

#[cfg(not(target_arch = "wasm32"))]
pub use std::fs::*;

#[cfg(not(target_arch = "wasm32"))]
mod native_paths {
    use std::path::Path;
    pub fn is_dir(p: impl AsRef<Path>) -> bool {
        p.as_ref().is_dir()
    }
    pub fn is_file(p: impl AsRef<Path>) -> bool {
        p.as_ref().is_file()
    }
    pub fn exists(p: impl AsRef<Path>) -> bool {
        p.as_ref().exists()
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub use native_paths::{exists, is_dir, is_file};

/// Entries of a packed archive (an `.iwd` mounted from a shard).
#[cfg(not(target_arch = "wasm32"))]
pub fn archive_entries(_archive: &std::path::Path) -> Option<Vec<ArchiveEntry>> {
    None
}
#[cfg(not(target_arch = "wasm32"))]
pub fn archive_read(
    _archive: &std::path::Path,
    _entry: &str,
    _limit: Option<usize>,
) -> Option<std::io::Result<Vec<u8>>> {
    None
}

#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    pub name: String,
    pub size: u64,
}

#[cfg(target_arch = "wasm32")]
mod mem;
#[cfg(target_arch = "wasm32")]
pub use mem::*;
