use crate::{
    backend::Backend,
    project::{self, Project, Record},
};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use fs2::FileExt;
use jail::{
    config::{Config, NetworkMode},
    network_policy::Denial,
};
use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, path::Path, process::Stdio};

#[derive(Subcommand)]
pub enum Action {
    /// Show the configured network policy (does not launch a container).
    Show,
    /// Inspect rejected proxy destinations; no headers, URLs or request bodies.
    Log {
        #[arg(long)]
        json: bool,
    },
    /// Add a hostname to the trusted config and reload a running container.
    Allow {
        host: String,
        #[arg(long, default_value_t = 443)]
        port: u16,
    },
    /// Apply allowlist changes without restarting; mode/forwarding changes need recreate.
    Reload,
}
pub fn sync(backend: &Backend, record: &Record, cfg: &Config) -> Result<()> {
    backend.require_owned(record)?;
    let mut child = backend
        .command(&[
            "exec",
            "--interactive",
            "--user",
            "root",
            &record.id,
            "/usr/local/bin/jail-guest",
            "policy-set",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;
    let sent = child
        .stdin
        .take()
        .context("missing policy pipe")?
        .write_all(&serde_json::to_vec(&cfg.network)?);
    let status = child.wait()?;
    sent?;
    if !status.success() {
        bail!("could not reload network policy; recreate is required for mode/forwarding changes");
    }
    Ok(())
}
pub fn run(
    action: Action,
    path: &Path,
    cfg: &Config,
    backend: &Backend,
    data: &Path,
    project: &Project,
) -> Result<i32> {
    if matches!(action, Action::Show) {
        print!("{}", toml::to_string_pretty(&cfg.network)?);
        return Ok(0);
    }
    let lock = project::lock(data, &project.id, "lifecycle")?;
    FileExt::lock_exclusive(&lock)?;
    let mut cfg = cfg.clone();
    if let Action::Allow { host, port } = &action {
        if cfg.network.mode != NetworkMode::Allowlist {
            bail!("policy allow requires network.mode = \"allowlist\"; change mode in trusted config and recreate first");
        }
        if !cfg.network.allowed_hosts.contains(host) {
            cfg.network.allowed_hosts.push(host.to_ascii_lowercase());
        }
        if !cfg.network.allowed_ports.contains(port) {
            cfg.network.allowed_ports.push(*port);
        }
        cfg.validate()?;
        // Separate config lock serializes updates across projects sharing config.
        let config_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.with_extension("toml.lock"))?;
        FileExt::lock_exclusive(&config_lock)?;
        let mut current = Config::load(path)?;
        if current.network.mode != NetworkMode::Allowlist {
            bail!("network mode changed while updating config");
        }
        if !current
            .network
            .allowed_hosts
            .contains(&host.to_ascii_lowercase())
        {
            current
                .network
                .allowed_hosts
                .push(host.to_ascii_lowercase());
        }
        if !current.network.allowed_ports.contains(port) {
            current.network.allowed_ports.push(*port);
        }
        current.validate()?;
        if fs::symlink_metadata(path)?.file_type().is_symlink() {
            bail!("policy allow refuses symlinked configs; supply the real config path");
        }
        let mut doc = fs::read_to_string(path)?.parse::<toml_edit::DocumentMut>()?;
        let hosts: toml_edit::Array = current
            .network
            .allowed_hosts
            .iter()
            .map(String::as_str)
            .collect();
        let ports: toml_edit::Array = current
            .network
            .allowed_ports
            .iter()
            .map(|p| i64::from(*p))
            .collect();
        doc["network"]["allowed_hosts"] = toml_edit::value(hosts);
        doc["network"]["allowed_ports"] = toml_edit::value(ports);
        let temp = path.with_extension("toml.new");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)
            .context("config temporary file exists; inspect it before retrying")?;
        file.write_all(doc.to_string().as_bytes())?;
        file.sync_all()?;
        fs::rename(temp, path)?;
        cfg = current;
        println!("allowed {host}:{port} in {}", path.display());
    }
    let record = project.record(data)?;
    match action {
        Action::Log { json } => {
            let record = record.context("no container for this project")?;
            backend.require_owned(&record)?;
            let out = backend
                .command(&[
                    "exec",
                    "--user",
                    "root",
                    &record.id,
                    "/usr/local/bin/jail-guest",
                    "policy-log",
                ])
                .output()?;
            if !out.status.success() {
                bail!("could not read policy log; container must be running");
            }
            let entries: Vec<Denial> = serde_json::from_slice(&out.stdout)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                println!("No proxy denials recorded (journal resets when the container stops).");
            } else {
                println!("TIME (UNIX)  HOST:PORT  REASON");
                for e in entries {
                    println!("{}  {}:{}  {}", e.time, e.host, e.port, e.reason);
                }
            }
        }
        Action::Allow { .. } | Action::Reload => {
            if let Some(record) = record {
                if backend.owned_state(&record)?.as_deref() == Some("running") {
                    sync(backend, &record, &cfg)?;
                    println!("reloaded {}", record.id);
                } else {
                    println!("Container is stopped; policy will apply on next launch.");
                }
            } else {
                println!("No container yet; policy will apply on first launch.");
            }
        }
        Action::Show => unreachable!(),
    }
    Ok(0)
}
