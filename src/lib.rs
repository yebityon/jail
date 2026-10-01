pub mod config;
pub mod gateway;
pub mod network_policy;
pub mod policy;
pub mod proxy;

use sha2::{Digest, Sha256};

pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
