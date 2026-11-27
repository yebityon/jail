pub mod agent_policy;
pub mod config;
#[path = "../container/gateway.rs"]
pub mod gateway;
pub mod guard;
#[path = "../container/network_policy.rs"]
pub mod network_policy;
#[path = "../container/proxy.rs"]
pub mod proxy;

use sha2::{Digest, Sha256};

/// Supervisor inside the container; container/Dockerfile installs it here.
pub const GUEST: &str = "/usr/local/bin/jail-guest";
/// Written by the supervisor once network policy is in place.
pub const READY: &str = "/run/jail-ready";
/// Root-only tmpfs for real credentials and the live network policy.
pub const PRIVATE: &str = "/run/jail-private";

/// Entry points for the targets in fuzz/; not part of jail's interface.
#[cfg(feature = "fuzzing")]
pub mod fuzzing {
    /// Parses a gateway request head as an agent could send it.
    pub fn gateway_request(data: &[u8]) {
        let Ok(head) = crate::gateway::parse_head(data) else {
            return;
        };
        for tool in ["claude", "codex"] {
            let _ = crate::gateway::upstream(tool, head.method, head.path);
        }
        let _ = crate::gateway::forwarded(head.fields);
    }
    /// Parses a proxy request head as an agent could send it.
    pub fn proxy_request(data: &[u8]) {
        let _ = crate::proxy::parse_request(data);
    }
    /// Decodes JWT payload text, and checks that encoding round-trips.
    pub fn base64(data: &[u8]) {
        if let Ok(text) = std::str::from_utf8(data) {
            let _ = crate::gateway::decode(text);
        }
        let encoded = crate::gateway::base64(data);
        assert_eq!(crate::gateway::decode(&encoded).as_deref(), Some(data));
    }
}

pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
/// Whether `s` is a `digest` prefix of exactly `len` characters.
pub fn is_digest(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
