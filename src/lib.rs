pub mod agent_policy;
pub mod config;
pub mod gateway;
pub mod network_policy;
pub mod proxy;

use sha2::{Digest, Sha256};

/// Supervisor inside the container; container/Dockerfile installs it here.
pub const GUEST: &str = "/usr/local/bin/jail-guest";
/// Written by the supervisor once network policy is in place.
pub const READY: &str = "/run/jail-ready";
/// Root-only tmpfs for real credentials and the live network policy.
pub const PRIVATE: &str = "/run/jail-private";

pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
/// Whether `s` is a `digest` prefix of exactly `len` characters.
pub fn is_digest(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
