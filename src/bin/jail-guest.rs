//! Trusted Linux-side supervisor. The user-facing CLI is the `jail` binary.
use anyhow::{bail, Context, Result};
use jail::{
    config::{Config, Network, NetworkMode},
    gateway, network_policy, policy, proxy,
};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("jail-guest: {e:#}");
            std::process::exit(1);
        }
    }
}
fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("init") => init(),
        Some("has-auth") => {
            let path = gateway::credential_path(&args.next().context("missing tool")?)?;
            if !path.is_file() {
                std::process::exit(3);
            }
            Ok(())
        }
        Some("import-auth") => import_auth(&args.next().context("missing tool")?),
        Some("prepare-auth") => prepare_auth(),
        Some("policy-log") => {
            println!("{}", serde_json::to_string(&network_policy::journal()?)?);
            Ok(())
        }
        Some("policy-set") => {
            let mut bytes = vec![];
            std::io::stdin().take(1024 * 1024).read_to_end(&mut bytes)?;
            network_policy::reload(&serde_json::from_slice(&bytes)?, &config()?.network)
        }
        Some("launch") => launch(&args.next().context("missing tool")?, args.collect()),
        _ => bail!("unknown supervisor command"),
    }
}
fn identity() -> Result<(u32, u32)> {
    let uid = fs::read_to_string("/etc/jail-uid")?.trim().parse()?;
    let gid = fs::read_to_string("/etc/jail-gid")?.trim().parse()?;
    if uid == 0 {
        bail!("agent must not run as root");
    }
    Ok((uid, gid))
}
fn config() -> Result<Config> {
    Ok(serde_json::from_slice(&fs::read("/etc/jail/policy.json")?)?)
}
fn secure_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
fn chown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    if unsafe { libc::chown(path.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn init() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("supervisor must start as root");
    }
    let c: Config = serde_json::from_str(&std::env::var("JAIL_POLICY")?)?;
    c.validate()?;
    protect_paths()?;
    gateway::prepare()?;
    network_policy::store(&c.network)?;
    let (uid, gid) = identity()?;
    fs::create_dir_all("/etc/jail")?;
    secure_write(
        Path::new("/etc/jail/policy.json"),
        &serde_json::to_vec(&c)?,
        0o600,
    )?;
    for dir in ["/home/jail", "/home/jail/.claude", "/home/jail/.codex"] {
        if let Ok(metadata) = fs::symlink_metadata(dir) {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("refusing symlink or non-directory in persistent home: {dir}");
            }
        }
        fs::create_dir_all(dir)?;
        chown(Path::new(dir), uid, gid)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let claude = serde_json::to_vec(&policy::claude_settings(&c))?;
    secure_write(Path::new("/etc/jail/claude-settings.json"), &claude, 0o644)?;
    secure_write(
        Path::new("/etc/claude-code/managed-settings.json"),
        &claude,
        0o644,
    )?;
    secure_write(
        Path::new("/etc/codex/requirements.toml"),
        policy::codex_requirements(&c).as_bytes(),
        0o644,
    )?;
    // The CLI handles onboarding without copying ~/.claude.json host settings.
    let onboarding = Path::new("/home/jail/.claude.json");
    if !onboarding.exists() {
        secure_write(onboarding, br#"{"hasCompletedOnboarding":true}"#, 0o600)?;
        chown(onboarding, uid, gid)?;
    }
    // Migrate known legacy credential files as the agent, never root-write into
    // agent-owned home paths. Real credentials live only in root-owned tmpfs.
    let status = Command::new("/usr/local/bin/jail-guest")
        .arg("prepare-auth")
        .status()?;
    if !status.success() {
        bail!("could not prepare credential placeholders");
    }
    let claude_listener = TcpListener::bind("127.0.0.1:3130")?;
    let codex_listener = TcpListener::bind("127.0.0.1:3131")?;
    let listener = if c.network.mode == NetworkMode::Allowlist {
        Some(TcpListener::bind("127.0.0.1:3128")?)
    } else {
        None
    };
    if c.network.mode != NetworkMode::Open {
        firewall(uid, &c.network)?;
    }
    std::thread::spawn(move || {
        if gateway::serve(claude_listener, "claude").is_err() {
            std::process::exit(1);
        }
    });
    std::thread::spawn(move || {
        if gateway::serve(codex_listener, "codex").is_err() {
            std::process::exit(1);
        }
    });
    // Readiness is emitted only after both IPv4 and IPv6 policy succeeds.
    secure_write(Path::new("/run/jail-ready"), b"ready\n", 0o644)?;
    if let Some(listener) = listener {
        return proxy::serve(listener, c.network);
    }
    loop {
        std::thread::park();
    }
}
#[cfg(target_os = "linux")]
fn protect_paths() -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Guard {
        anchors: Vec<PathBuf>,
        readonly: Vec<PathBuf>,
    }
    let guard: Guard = serde_json::from_str(&std::env::var("JAIL_WRITE_GUARD")?)?;
    for (path, readonly) in guard
        .anchors
        .iter()
        .map(|p| (p, false))
        .chain(guard.readonly.iter().map(|p| (p, true)))
    {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        if unsafe {
            libc::mount(
                path.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error())
                .context("cannot establish protected mount boundary");
        }
        if readonly
            && unsafe {
                libc::mount(
                    std::ptr::null(),
                    path.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND
                        | libc::MS_REMOUNT
                        | libc::MS_RDONLY
                        | libc::MS_NOSUID
                        | libc::MS_NODEV,
                    std::ptr::null(),
                )
            } != 0
        {
            return Err(std::io::Error::last_os_error())
                .context("cannot enforce read-only protected path");
        }
    }
    // Root gateway does not need mount privileges after initialization.
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let mut header = Header {
        version: 0x20080522,
        pid: 0,
    };
    let mut data = [Data {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    if unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    data[0].effective &= !(1 << 21);
    data[0].permitted &= !(1 << 21);
    data[0].inheritable &= !(1 << 21);
    if unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } != 0
        || unsafe { libc::prctl(libc::PR_CAPBSET_DROP, 21, 0, 0, 0) } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn protect_paths() -> Result<()> {
    bail!("write protection requires Linux guest")
}
fn firewall(uid: u32, network: &Network) -> Result<()> {
    for binary in ["iptables", "ip6tables"] {
        checked(binary, &["-w", "-N", "JAIL_OUTPUT"])?;
        // Attach the chain before adding any allow rule: failure leaves deny in
        // place, and readiness is never signaled. Only agent UID is restricted.
        checked(binary, &["-w", "-A", "JAIL_OUTPUT", "-j", "REJECT"])?;
        checked(
            binary,
            &[
                "-w",
                "-I",
                "OUTPUT",
                "1",
                "-m",
                "owner",
                "--uid-owner",
                &uid.to_string(),
                "-j",
                "JAIL_OUTPUT",
            ],
        )?;
        if binary == "iptables" && network.mode == NetworkMode::Allowlist {
            for port in ["3128", "3130", "3131"] {
                checked(
                    binary,
                    &[
                        "-w",
                        "-I",
                        "JAIL_OUTPUT",
                        "1",
                        "-p",
                        "tcp",
                        "-d",
                        "127.0.0.1",
                        "--dport",
                        port,
                        "-j",
                        "ACCEPT",
                    ],
                )?;
            }
        }
        if binary == "iptables" && network.mode == NetworkMode::Allowlist {
            // Only replies to inbound service connections; never permit the
            // ORIGINAL direction of outbound connections around the proxy.
            for port in &network.published_ports {
                checked(binary, &reply_rule(*port))?;
            }
        }
    }
    Ok(())
}
fn reply_rule(port: u16) -> Vec<String> {
    ["-w", "-I", "JAIL_OUTPUT", "1", "-p", "tcp", "--sport"]
        .into_iter()
        .map(str::to_owned)
        .chain(std::iter::once(port.to_string()))
        .chain(
            [
                "-m",
                "conntrack",
                "--ctstate",
                "ESTABLISHED",
                "--ctdir",
                "REPLY",
                "-j",
                "ACCEPT",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .collect()
}
fn checked<S: AsRef<std::ffi::OsStr>>(binary: &str, args: &[S]) -> Result<()> {
    let status = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        bail!("cannot enforce network policy: {binary} failed");
    }
    Ok(())
}
fn auth_path(tool: &str) -> Result<PathBuf> {
    match tool {
        "claude" => Ok("/home/jail/.claude/.credentials.json".into()),
        "codex" => Ok("/home/jail/.codex/auth.json".into()),
        _ => bail!("unsupported auth tool"),
    }
}
fn import_auth(tool: &str) -> Result<()> {
    let path = auth_path(tool)?;
    let (uid, gid) = identity()?;
    let mut bytes = vec![];
    std::io::stdin().take(1024 * 1024).read_to_end(&mut bytes)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if !value.is_object() {
        bail!("authentication must be an object");
    }
    gateway::import(tool, &value)?;
    drop_privileges(uid, gid)?;
    secure_write(
        &path,
        &serde_json::to_vec(&gateway::placeholder(tool, &value))?,
        0o600,
    )
}
fn prepare_auth() -> Result<()> {
    let (uid, gid) = identity()?;
    drop_privileges(uid, gid)?;
    for tool in ["claude", "codex"] {
        let path = auth_path(tool)?;
        if let Ok(meta) = fs::symlink_metadata(&path) {
            use std::os::unix::fs::MetadataExt;
            if !meta.is_file() || meta.nlink() != 1 {
                bail!("credential file has unsafe aliases");
            }
        }
        secure_write(
            &path,
            &serde_json::to_vec(&gateway::placeholder(tool, &serde_json::Value::Null))?,
            0o600,
        )?;
    }
    Ok(())
}
fn launch(tool: &str, args: Vec<String>) -> Result<()> {
    if !Path::new("/run/jail-ready").is_file() {
        bail!("container policy is not ready");
    }
    let c = config()?;
    let (uid, gid) = identity()?;
    let executable = match tool {
        "claude" => "/usr/local/bin/claude",
        "codex" => "/usr/local/bin/codex",
        "shell" if c.tools.shell => "/bin/bash",
        _ => bail!("tool is disabled or unsupported"),
    };
    let mut command = Command::new(executable);
    command.env_clear();
    command
        .env("HOME", "/home/jail")
        .env("USER", "jail")
        .env("LOGNAME", "jail")
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env(
            "TERM",
            std::env::var("TERM").unwrap_or("xterm-256color".into()),
        )
        .env("LANG", "C.UTF-8")
        .env("CODEX_HOME", "/home/jail/.codex")
        // Keep Claude's default layout: setting CLAUDE_CONFIG_DIR also moves
        // its global .claude.json inside that directory, breaking onboarding.
        .env("DISABLE_AUTOUPDATER", "1");
    command.env("ANTHROPIC_BASE_URL", "http://127.0.0.1:3130");
    for (key, value) in &c.environment.set {
        command.env(key, value);
    }
    for key in &c.environment.pass {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    if c.network.mode == NetworkMode::Allowlist {
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            command.env(key, "http://127.0.0.1:3128");
        }
        command
            .env("NO_PROXY", "127.0.0.1")
            .env("no_proxy", "127.0.0.1");
    }
    if tool == "shell" {
        command.arg("-l");
    } else {
        command.args(args);
    }
    drop_privileges(uid, gid)?;
    Err(command.exec().into())
}
#[cfg(target_os = "linux")]
fn drop_privileges(uid: u32, gid: u32) -> Result<()> {
    unsafe {
        for capability in 0..=40 {
            if libc::prctl(libc::PR_CAPBSET_DROP, capability, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        if libc::setgroups(0, std::ptr::null()) != 0
            || libc::setgid(gid) != 0
            || libc::setuid(uid) != 0
            || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn drop_privileges(_: u32, _: u32) -> Result<()> {
    bail!("jail-guest runs only on Linux")
}
