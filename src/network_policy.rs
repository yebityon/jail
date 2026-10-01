//! Root-owned live policy and bounded, metadata-only denial journal.
use crate::config::{Config, Network, NetworkMode};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

pub const LIVE: &str = "/run/jail-private/network.json";
pub const LOG: &str = "/run/jail-private/denials.jsonl";
static JOURNAL: Mutex<()> = Mutex::new(());
#[derive(Debug, Serialize, Deserialize)]
pub struct Denial {
    pub time: u64,
    pub host: String,
    pub port: u16,
    pub reason: String,
}
pub fn store(network: &Network) -> Result<()> {
    let cfg = Config {
        network: network.clone(),
        ..Config::default()
    };
    cfg.validate()?;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(format!("{LIVE}.new"))?;
    f.write_all(&serde_json::to_vec(network)?)?;
    f.sync_all()?;
    fs::rename(format!("{LIVE}.new"), LIVE)?;
    Ok(())
}
pub fn load() -> Result<Network> {
    let network: Network = serde_json::from_slice(&fs::read(LIVE)?)?;
    let cfg = Config {
        network: network.clone(),
        ..Config::default()
    };
    cfg.validate()?;
    Ok(network)
}
pub fn reload(network: &Network, original: &Network) -> Result<()> {
    if network.mode != original.mode || network.published_ports != original.published_ports {
        bail!("changing network mode or published ports requires jail recreate");
    }
    store(network)
}
pub fn permits(network: &Network, host: &str, port: u16) -> bool {
    network.mode == NetworkMode::Open
        || (network.mode == NetworkMode::Allowlist
            && crate::proxy::host_allowed(host, &network.allowed_hosts)
            && network.allowed_ports.contains(&port))
}
pub fn deny(host: &str, port: u16, reason: &str) -> Result<()> {
    let _lock = JOURNAL
        .lock()
        .map_err(|_| anyhow::anyhow!("journal lock failed"))?;
    let host: String = host
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || ".-:".contains(*c))
        .take(253)
        .collect();
    let entry = Denial {
        time: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        host,
        port,
        reason: reason.into(),
    };
    if fs::metadata(LOG)
        .map(|m| m.len() >= 1024 * 1024)
        .unwrap_or(false)
    {
        fs::rename(LOG, format!("{LOG}.previous"))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(LOG)?;
    serde_json::to_writer(&mut file, &entry)?;
    file.write_all(b"\n")?;
    Ok(())
}
pub fn journal() -> Result<Vec<Denial>> {
    let _lock = JOURNAL
        .lock()
        .map_err(|_| anyhow::anyhow!("journal lock failed"))?;
    let mut out = vec![];
    for path in [format!("{LOG}.previous"), LOG.into()] {
        match fs::read_to_string(path) {
            Ok(s) => {
                for line in s.lines() {
                    if let Ok(entry) = serde_json::from_str(line) {
                        out.push(entry);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(out)
}
