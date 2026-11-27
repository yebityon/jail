//! Write-protection mounts that the host prepares and the guest applies.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Environment variable that carries a `Guard` as JSON into the container.
pub const ENV: &str = "JAIL_WRITE_GUARD";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Guard {
    /// Parents of protected paths, each bound onto itself.
    pub anchors: Vec<PathBuf>,
    pub readonly: Vec<PathBuf>,
    /// Set when writes are limited to an allowlist.
    pub limit: Option<Limit>,
}
/// The workspace `root` is read-only except for `writable`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Limit {
    pub root: PathBuf,
    pub writable: Vec<PathBuf>,
}
