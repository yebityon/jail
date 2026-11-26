//! Root-owned live policy and bounded, metadata-only denial journal.
use crate::{
    config::{Config, Network, NetworkMode},
    PRIVATE,
};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

fn live() -> PathBuf {
    Path::new(PRIVATE).join("network.json")
}
fn log() -> PathBuf {
    Path::new(PRIVATE).join("denials.jsonl")
}
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
    let live = live();
    let new = live.with_extension("json.new");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&new)?;
    f.write_all(&serde_json::to_vec(network)?)?;
    f.sync_all()?;
    fs::rename(new, live)?;
    Ok(())
}
pub fn load() -> Result<Network> {
    let network: Network = serde_json::from_slice(&fs::read(live())?)?;
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
    let log = log();
    if fs::metadata(&log)
        .map(|m| m.len() >= 1024 * 1024)
        .unwrap_or(false)
    {
        fs::rename(&log, log.with_extension("jsonl.previous"))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&log)?;
    serde_json::to_writer(&mut file, &entry)?;
    file.write_all(b"\n")?;
    Ok(())
}
pub fn journal() -> Result<Vec<Denial>> {
    let _lock = JOURNAL
        .lock()
        .map_err(|_| anyhow::anyhow!("journal lock failed"))?;
    let mut out = vec![];
    let log = log();
    for path in [log.with_extension("jsonl.previous"), log] {
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
