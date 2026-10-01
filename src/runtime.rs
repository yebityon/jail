use crate::{
    backend::Backend,
    project::{self, Port, Project, Record},
};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use jail::{
    config::{Config, NetworkMode},
    digest,
};
use std::{
    fs,
    net::{Ipv4Addr, TcpListener},
    path::Path,
    time::{Duration, Instant},
};

const FILES: &[(&str, &str)] = &[
    ("Cargo.toml", include_str!("../Cargo.toml")),
    ("Cargo.lock", include_str!("../Cargo.lock")),
    ("src/main.rs", "fn main() {}\n"),
    ("src/lib.rs", include_str!("lib.rs")),
    ("src/config.rs", include_str!("config.rs")),
    ("src/policy.rs", include_str!("policy.rs")),
    ("src/proxy.rs", include_str!("proxy.rs")),
    ("src/network_policy.rs", include_str!("network_policy.rs")),
    ("src/gateway.rs", include_str!("gateway.rs")),
    ("src/bin/jail-guest.rs", include_str!("bin/jail-guest.rs")),
    ("Dockerfile", include_str!("../container/Dockerfile")),
];
pub fn revision() -> String {
    digest(include_bytes!("runtime.rs"))
}
pub fn image_tag(cfg: &Config) -> String {
    let mut material = format!(
        "{}:{}:{}:{}",
        cfg.image.claude_version,
        cfg.image.codex_version,
        unsafe { libc::getuid() },
        unsafe { libc::getgid() }
    );
    for (path, content) in FILES {
        material.push_str(path);
        material.push_str(content);
    }
    format!("jail-runtime:{}", &digest(material)[..20])
}
pub fn build(backend: &Backend, data: &Path, cfg: &Config, force: bool) -> Result<String> {
    let image = image_tag(cfg);
    let lock = project::lock(data, "image", "build")?;
    FileExt::lock_exclusive(&lock)?;
    if !force
        && backend
            .command(&["image", "inspect", &image])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        return Ok(image);
    }
    let context = data.join("build").join(image.replace(':', "-"));
    project::private_dir(&context)?;
    for (path, content) in FILES {
        let destination = context.join(path);
        fs::create_dir_all(destination.parent().unwrap())?;
        fs::write(destination, content)?;
    }
    fs::write(context.join("Cargo.lock"), include_str!("../Cargo.lock"))?;
    // This build context contains source and version pins only, never a workspace or credentials.
    eprintln!("jail: building CLI image (first launch can take several minutes)");
    let mut args = vec![
        "build".into(),
        "--tag".into(),
        image.clone(),
        "--build-arg".into(),
        format!("CLAUDE_VERSION={}", cfg.image.claude_version),
        "--build-arg".into(),
        format!("CODEX_VERSION={}", cfg.image.codex_version),
        "--build-arg".into(),
        format!("JAIL_UID={}", unsafe { libc::getuid() }),
        "--build-arg".into(),
        format!("JAIL_GID={}", unsafe { libc::getgid() }),
    ];
    if force {
        args.extend(["--no-cache".into(), "--pull".into()]);
    }
    args.push(context.to_string_lossy().into_owned());
    if backend.interactive_owned(&args)? != 0 {
        bail!("image build failed");
    }
    Ok(image)
}
pub fn ensure(
    backend: &Backend,
    data: &Path,
    project: &Project,
    cfg: &Config,
    spec: &str,
) -> Result<Record> {
    let volume = format!("{}-home", project.id);
    let image = image_tag(cfg);
    let existing = project.record(data)?;
    if let Some(record) = &existing {
        if record.root != project.root || record.volume != volume {
            bail!("project state does not match this workspace");
        }
        if record.spec != spec {
            bail!("configuration or image changed; close sessions, then run jail recreate (home/history persist, host login is re-imported; installed OS packages will be removed)");
        }
        if let Some(state) = backend.owned_state(record)? {
            if state != "running" {
                let allocation = project::lock(data, "port-forwarding", "allocation")?;
                FileExt::lock_exclusive(&allocation)?;
                check_restart_ports(&record.ports)?;
                backend.checked(&["start", &record.id]).context("could not restart container; if a saved host port is occupied, free it or run jail recreate to allocate new ports")?;
            }
            wait_ready(backend, record)?;
            return Ok(record.clone());
        }
    }
    build(backend, data, cfg, false)?;
    // Serialize jail's port selection and creation across different projects.
    // Other host apps can still race the final bind; the backend must fail safely.
    let allocation = project::lock(data, "port-forwarding", "allocation")?;
    FileExt::lock_exclusive(&allocation)?;
    let id = if let Some(record) = &existing {
        record.id.clone() // Retry a failed creation using the saved name.
    } else {
        project.new_container_name()?
    };
    // Do not claim an existing container name whose state was lost.
    if backend.list()?.iter().any(|v| {
        v.get("id")
            .or_else(|| v.pointer("/configuration/id"))
            .and_then(serde_json::Value::as_str)
            == Some(&id)
    }) {
        bail!(
            "container name {} is already in use without matching jail state",
            id
        );
    }
    if !backend
        .command(&["volume", "inspect", &volume])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        backend.checked(&["volume", "create", &volume])?;
    }
    let requested = if cfg.network.mode == NetworkMode::None {
        &[][..]
    } else {
        cfg.network.published_ports.as_slice()
    };
    let (ports, reservations) = allocate_ports(requested, cfg.network.auto_port)?;
    let guard = crate::write_guard::prepare(&project.root, cfg)?;
    let mut args = vec![
        "run".into(),
        "--detach".into(),
        "--name".into(),
        id.clone(),
        "--label".into(),
        format!("io.jail.project={}", project.root.display()),
        "--label".into(),
        "io.jail.managed=true".into(),
        "--cpus".into(),
        cfg.resources.cpus.to_string(),
        "--memory".into(),
        cfg.resources.memory.clone(),
        "--cap-drop".into(),
        "ALL".into(),
        "--tmpfs".into(),
        "/run".into(),
    ];
    // The trusted supervisor alone sets firewall and user identity. Agents drop
    // every capability and set no_new_privs before executing a CLI.
    for cap in [
        "CHOWN",
        "DAC_OVERRIDE",
        "FOWNER",
        "SETUID",
        "SETGID",
        "SETPCAP",
        "NET_ADMIN",
        "SYS_ADMIN",
        "KILL",
    ] {
        args.extend(["--cap-add".into(), cap.into()]);
    }
    let root = project.root.to_string_lossy();
    let read_only = cfg.workspace.read_only || !cfg.tools.edit;
    args.extend([
        "--mount".into(),
        format!(
            "type=bind,source={root},target={root}{}",
            if read_only { ",readonly" } else { "" }
        ),
        "--mount".into(),
        format!("type=volume,source={volume},target=/home/jail"),
        "--env".into(),
        format!("JAIL_POLICY={}", serde_json::to_string(cfg)?),
        "--env".into(),
        format!("JAIL_WRITE_GUARD={}", serde_json::to_string(&guard)?),
    ]);
    for mount in &cfg.workspace.mounts {
        args.extend([
            "--mount".into(),
            format!(
                "type=bind,source={},target={}{}",
                mount.source.display(),
                mount.target.display(),
                if mount.read_only || read_only {
                    ",readonly"
                } else {
                    ""
                }
            ),
        ]);
    }
    for hidden in &cfg.workspace.hidden {
        args.extend([
            "--masked-path".into(),
            project.root.join(hidden).to_string_lossy().into_owned(),
        ]);
    }
    for port in &ports {
        args.extend(["--publish".into(), port.specification()]);
    }
    // No host home, SSH agent or runtime socket is shared. Ports bind loopback only.
    args.push(image.clone());
    let record = Record {
        id,
        root: project.root.clone(),
        spec: spec.into(),
        image,
        volume,
        ports,
    };
    // Save ownership metadata before creation so a failed startup is diagnosable.
    project.save(data, &record)?;
    drop(reservations); // Apple container now owns the binds, not jail.
    backend
        .checked(&args)
        .context("container creation failed; check host port conflicts and container logs")?;
    wait_ready(backend, &record)?;
    if cfg.network.mode == NetworkMode::None {
        eprintln!("jail: network is disabled; online model requests cannot run");
    }
    Ok(record)
}
fn allocate_ports(requested: &[u16], auto_port: bool) -> Result<(Vec<Port>, Vec<TcpListener>)> {
    let mut ports = Vec::new();
    let mut reservations = Vec::new();
    for &container in requested {
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, container)) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && auto_port => {
                // Avoid stealing another explicitly requested port, even when the
                // OS's ephemeral port range happens to overlap it.
                let mut candidates = Vec::new();
                loop {
                    let candidate = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
                    if !requested.contains(&candidate.local_addr()?.port()) {
                        break candidate;
                    }
                    candidates.push(candidate);
                    if candidates.len() >= requested.len() {
                        bail!("could not allocate an unused host port");
                    }
                }
            }
            Err(error) => return Err(error).with_context(|| format!("host port 127.0.0.1:{container} is unavailable; set network.auto_port=true or change network.published_ports")),
        };
        ports.push(Port {
            host: listener.local_addr()?.port(),
            container,
        });
        reservations.push(listener);
    }
    Ok((ports, reservations))
}
fn check_restart_ports(ports: &[Port]) -> Result<()> {
    let mut reservations = Vec::new();
    for port in ports {
        reservations.push(TcpListener::bind((Ipv4Addr::LOCALHOST, port.host))
            .with_context(|| format!("saved host port 127.0.0.1:{} is occupied; free it or run jail recreate to reassign ports", port.host))?);
    }
    Ok(())
}
fn wait_ready(backend: &Backend, record: &Record) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        let status = backend
            .command(&[
                "exec",
                "--user",
                "root",
                &record.id,
                "test",
                "-f",
                "/run/jail-ready",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("container did not become ready; policy setup may have failed. Inspect with: container logs {}", record.id)
}
