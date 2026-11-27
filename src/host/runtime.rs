use crate::{
    apple_container::{Client, Run},
    backend::{self, PROJECT_LABEL},
    project::{self, Port, Project, Record},
    sys,
};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use jail::{
    config::{Config, NetworkMode},
    digest, is_digest, READY,
};
use std::{
    fs,
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const FILES: &[(&str, &str)] = &[
    ("Cargo.toml", include_str!("../../Cargo.toml")),
    ("Cargo.lock", include_str!("../../Cargo.lock")),
    ("src/host/main.rs", "fn main() {}\n"),
    ("src/shared/lib.rs", include_str!("../shared/lib.rs")),
    ("src/shared/config.rs", include_str!("../shared/config.rs")),
    ("src/shared/guard.rs", include_str!("../shared/guard.rs")),
    (
        "src/shared/agent_policy.rs",
        include_str!("../shared/agent_policy.rs"),
    ),
    (
        "src/container/proxy.rs",
        include_str!("../container/proxy.rs"),
    ),
    (
        "src/container/network_policy.rs",
        include_str!("../container/network_policy.rs"),
    ),
    (
        "src/container/gateway.rs",
        include_str!("../container/gateway.rs"),
    ),
    (
        "src/container/main.rs",
        include_str!("../container/main.rs"),
    ),
    ("src/container/sys.rs", include_str!("../container/sys.rs")),
    ("Dockerfile", include_str!("../../container/Dockerfile")),
];
/// Label marking a container that jail deletes when its session ends.
const EPHEMERAL_LABEL: &str = "io.jail.ephemeral";
const IMAGE_PREFIX: &str = "jail-runtime:";
/// Hex digits of an image tag.
const TAG_DIGITS: usize = 20;
pub fn revision() -> String {
    digest(include_bytes!("runtime.rs"))
}
pub fn image_tag(cfg: &Config) -> String {
    let mut material = format!(
        "{}:{}:{}:{}",
        cfg.image.claude_version,
        cfg.image.codex_version,
        sys::uid(),
        sys::gid()
    );
    for (path, content) in FILES {
        material.push_str(path);
        material.push_str(content);
    }
    format!("{IMAGE_PREFIX}{}", &digest(material)[..TAG_DIGITS])
}
/// The tag without a registry prefix, if `name` is an image in jail's format.
pub fn image_name(name: &str) -> Option<&str> {
    let name = name.strip_prefix("docker.io/library/").unwrap_or(name);
    name.strip_prefix(IMAGE_PREFIX)
        .is_some_and(|h| is_digest(h, TAG_DIGITS))
        .then_some(name)
}
/// Source and version pins for building `image`, never a workspace or credentials.
pub fn build_context(data: &Path, image: &str) -> PathBuf {
    data.join("build").join(image.replace(':', "-"))
}
pub fn build(client: &Client, data: &Path, cfg: &Config) -> Result<String> {
    let image = image_tag(cfg);
    let image_build = project::global_lock(data, "image-build")?;
    FileExt::lock_exclusive(&image_build)?;
    if client.image_exists(&image) {
        return Ok(image);
    }
    let context = build_context(data, &image);
    project::private_dir(&context)?;
    for (path, content) in FILES {
        let destination = context.join(path);
        fs::create_dir_all(destination.parent().unwrap())?;
        fs::write(destination, content)?;
    }
    fs::write(context.join("Cargo.lock"), include_str!("../../Cargo.lock"))?;
    // This build context contains source and version pins only, never a workspace or credentials.
    eprintln!("jail: building CLI image (first launch can take several minutes)");
    client.build(
        &image,
        &[
            ("CLAUDE_VERSION", cfg.image.claude_version.to_string()),
            ("CODEX_VERSION", cfg.image.codex_version.to_string()),
            ("JAIL_UID", sys::uid().to_string()),
            ("JAIL_GID", sys::gid().to_string()),
        ],
        &context,
    )?;
    Ok(image)
}
pub fn ensure(
    client: &Client,
    data: &Path,
    project: &Project,
    cfg: &Config,
    spec: &str,
) -> Result<Record> {
    // An ephemeral container keeps its home in its own root filesystem.
    let volume = (!project.ephemeral).then(|| project::home_volume(&project.key));
    let image = image_tag(cfg);
    let existing = project.record(data)?;
    if let Some(record) = &existing {
        if record.root != project.root || record.volume != volume {
            bail!("jail record does not match this workspace");
        }
        if record.spec != spec {
            bail!("configuration or image changed; close sessions, then run jail recreate (home/history persist, host login is re-imported; installed OS packages will be removed)");
        }
        if let Some(state) = backend::owned_state(client, record)? {
            if state != "running" {
                let port_allocation = project::global_lock(data, "port-allocation")?;
                FileExt::lock_exclusive(&port_allocation)?;
                check_restart_ports(&record.ports)?;
                client.start(&record.container).context("could not restart container; if a saved host port is occupied, free it or run jail recreate to allocate new ports")?;
            }
            wait_ready(client, record)?;
            return Ok(record.clone());
        }
    }
    build(client, data, cfg)?;
    // Serialize jail's port selection and creation across different projects.
    // Other host apps can still race the final bind; the backend must fail safely.
    let port_allocation = project::global_lock(data, "port-allocation")?;
    FileExt::lock_exclusive(&port_allocation)?;
    // jail cleanup may have removed the image after the build. Never let the
    // backend resolve a missing local tag from a registry instead.
    if !client.image_exists(&image) {
        bail!("image {image} was removed during launch; retry");
    }
    let container = if let Some(record) = &existing {
        record.container.clone() // Retry a failed creation using the saved name.
    } else {
        project.new_container_name()?
    };
    // Do not claim an existing container name whose state was lost.
    if client.list()?.iter().any(|c| c.id() == Some(&container)) {
        bail!(
            "container name {} is already in use without a matching jail record",
            container
        );
    }
    if let Some(volume) = volume.as_deref().filter(|v| !client.volume_exists(v)) {
        // The label lets jail cleanup name the project after its state is gone.
        client.create_volume(
            volume,
            &[(PROJECT_LABEL, project.root.display().to_string().as_str())],
        )?;
    }
    let requested = if cfg.network.mode == NetworkMode::None {
        &[][..]
    } else {
        cfg.network.published_ports.as_slice()
    };
    let (ports, reservations) = allocate_ports(requested, cfg.network.auto_port)?;
    let guard = if project.ephemeral {
        Default::default()
    } else {
        crate::write_guard::prepare(&project.root, cfg)?
    };
    let mut run = Run::new(&container, &image);
    run.detach()
        .label(PROJECT_LABEL, &project.root.display().to_string())
        .label("io.jail.managed", "true")
        .cpus(cfg.resources.cpus)
        .memory(&cfg.resources.memory)
        .cap_drop("ALL")
        .tmpfs("/run");
    if project.ephemeral {
        run.label(EPHEMERAL_LABEL, "true");
    }
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
        run.cap_add(cap);
    }
    let read_only = cfg.workspace.read_only || !cfg.tools.edit;
    // An ephemeral container mounts nothing from the host.
    if let Some(volume) = &volume {
        run.bind(&project.root, &project.root, read_only)
            .volume(volume, "/home/jail");
    }
    run.env("JAIL_CONFIG_JSON", &serde_json::to_string(cfg)?)
        .env(jail::guard::ENV, &serde_json::to_string(&guard)?);
    if !project.ephemeral {
        for mount in &cfg.workspace.mounts {
            run.bind(&mount.source, &mount.target, mount.read_only || read_only);
        }
        for hidden in &cfg.workspace.hidden {
            run.masked_path(&project.root.join(hidden));
        }
    }
    for port in &ports {
        run.publish(&port.specification());
    }
    // No host home, SSH agent or runtime socket is shared. Ports bind loopback only.
    let record = Record {
        container,
        root: project.root.clone(),
        spec: spec.into(),
        image,
        volume,
        ports,
    };
    // Save the record before creation so a failed startup is diagnosable.
    project.save(data, &record)?;
    drop(reservations); // Apple container now owns the binds, not jail.
    client
        .run(&run)
        .context("container creation failed; check host port conflicts and container logs")?;
    wait_ready(client, &record)?;
    if cfg.network.mode == NetworkMode::None {
        eprintln!("jail: network is disabled; online model requests cannot run");
    }
    Ok(record)
}
/// Deletes an ephemeral project's container, if it was created, and its jail
/// state. The caller has released its own locks of that project.
pub fn discard(client: &Client, data: &Path, project: &Project) -> Result<()> {
    let lifecycle = project::lock(data, &project.key, "lifecycle")?;
    FileExt::lock_exclusive(&lifecycle)?;
    if let Some(record) = project.record(data)? {
        if backend::owned_state(client, &record)?.is_some() {
            client.stop(&record.container)?;
            client.delete(&record.container)?;
        }
    }
    fs::remove_dir_all(project::dir(data, &project.key))?;
    Ok(())
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
fn wait_ready(client: &Client, record: &Record) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        let status = client
            .exec(&record.container)
            .user("root")
            .command(&["test", "-f", READY])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("container did not become ready; policy setup may have failed. Inspect with: container logs {}", record.container)
}
