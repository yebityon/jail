mod auth;
mod backend;
mod monitor;
mod policy_cli;
mod project;
mod runtime;
mod write_guard;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use jail::{config::Config, digest, policy};
use std::{
    ffi::OsString,
    fs,
    io::{IsTerminal, Write},
    os::unix::{io::AsRawFd, process::CommandExt},
    path::PathBuf,
    process::{Command, ExitStatus},
};

#[derive(Parser)]
#[command(
    version,
    about = "Run Claude Code / Codex CLI inside Apple containers",
    after_help = "Examples:\n  jail claude --resume\n  jail codex exec 'run tests'\n  jail --auto-stop claude\n  jail --tail\n  jail --tail --all\n\nPut jail options before claude/codex. Everything after the tool name is forwarded."
)]
struct Cli {
    #[arg(long, help = "Trusted TOML config (never auto-loads project configs)")]
    config: Option<PathBuf>,
    #[arg(long, help = "Stop after exit unless another jail session is active")]
    auto_stop: bool,
    #[arg(
        long,
        help = "Show launch plan without starting containers or reading credentials"
    )]
    dry_run: bool,
    #[arg(long, help = "Live CPU/memory/process dashboard (same as jail tail)")]
    tail: bool,
    #[arg(
        long,
        requires = "tail",
        help = "Monitor all jail containers with --tail"
    )]
    all: bool,
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(Subcommand)]
enum Action {
    #[command(about = "Live dashboard; Ctrl+C exits monitoring without stopping containers")]
    Tail {
        #[arg(long)]
        all: bool,
    },
    Status {
        #[arg(long)]
        watch: bool,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    Ps,
    #[command(about = "Inspect denied destinations and adjust the trusted network allowlist")]
    Policy {
        #[command(subcommand)]
        action: policy_cli::Action,
    },
    Shell,
    Stop {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        force: bool,
    },
    #[command(
        about = "Recreate this project's container; preserves its home volume but removes installed OS packages"
    )]
    Recreate,
    Doctor,
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    #[command(about = "Explicitly re-import this tool's host login (claude or codex)")]
    Auth {
        tool: String,
    },
    #[command(about = "Rebuild the configured image; use recreate to switch existing containers")]
    Update,
    #[command(external_subcommand)]
    Tool(Vec<OsString>),
}
#[derive(Subcommand)]
enum ConfigAction {
    Init,
    Show,
}

fn user_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
fn config_path(cli: &Cli, action: &Action) -> Result<PathBuf> {
    let initializing = matches!(
        action,
        Action::Config {
            action: ConfigAction::Init
        }
    );
    if let Some(path) = &cli.config {
        if !path.is_file() && !initializing {
            bail!("config does not exist: {}", path.display());
        }
        return Ok(path.clone());
    }
    if let Some(path) = std::env::var_os("JAIL_CONFIG") {
        let path = PathBuf::from(path);
        if !path.is_file() && !initializing {
            bail!("JAIL_CONFIG does not exist: {}", path.display());
        }
        return Ok(path);
    }
    Ok(user_home()?.join(".config/jail/config.toml"))
}
fn data_path() -> Result<PathBuf> {
    Ok(std::env::var_os("JAIL_HOME")
        .map(PathBuf::from)
        .unwrap_or(user_home()?.join(".local/share/jail")))
}
fn main() {
    let code = match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("jail: {e:#}");
            1
        }
    };
    std::process::exit(code);
}
fn run(mut cli: Cli) -> Result<i32> {
    let action = match (cli.tail, cli.command.take()) {
        (true, None) => Action::Tail { all: cli.all },
        (true, Some(_)) => bail!(
            "--tail cannot be combined with a subcommand; use jail --tail in another terminal"
        ),
        (false, Some(action)) => action,
        (false, None) => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            return Ok(0);
        }
    };
    if matches!(&action, Action::Tail { .. }) && (cli.auto_stop || cli.dry_run) {
        bail!("tail is read-only monitoring; --auto-stop and --dry-run are not applicable");
    }
    let path = config_path(&cli, &action)?;
    if let Action::Config {
        action: ConfigAction::Init,
    } = action
    {
        if path.exists() {
            bail!("config already exists: {}", path.display());
        }
        fs::create_dir_all(path.parent().context("config has no parent")?)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(include_bytes!("../config.example.toml"))?;
        println!("{}", path.display());
        return Ok(0);
    }
    let cfg = Config::load(&path)?;
    if matches!(
        action,
        Action::Config {
            action: ConfigAction::Show
        }
    ) {
        print!("{}", toml::to_string_pretty(&cfg)?);
        return Ok(0);
    }
    let backend = backend::Backend::new();
    if matches!(action, Action::Doctor) {
        return backend.doctor();
    }
    let data = data_path()?;
    let project = project::Project::discover()?;
    if let Action::Policy { action } = action {
        return policy_cli::run(action, &path, &cfg, &backend, &data, &project);
    }
    let mut launch = None;
    let mut import_login = true;
    match &action {
        Action::Tool(raw) => {
            let tool = raw
                .first()
                .and_then(|s| s.to_str())
                .context("missing tool")?;
            let args = raw[1..]
                .iter()
                .map(|a| {
                    a.to_str()
                        .map(str::to_owned)
                        .context("arguments must be UTF-8")
                })
                .collect::<Result<Vec<_>>>()?;
            import_login = !args
                .iter()
                .any(|a| ["--help", "-h", "--version", "-V"].contains(&a.as_str()));
            launch = Some((tool.to_owned(), policy::agent_args(tool, &args, &cfg)?));
        }
        Action::Shell => {
            if !cfg.tools.shell {
                bail!("shell is disabled by tools.shell");
            }
            launch = Some(("shell".into(), vec![]));
        }
        _ => {}
    }
    if launch.is_some() || matches!(&action, Action::Auth { .. }) {
        let home = user_home()?.canonicalize().context("cannot resolve HOME")?;
        if home.starts_with(&project.root) {
            bail!(
                "refusing to share your home directory or its ancestor; run jail inside a project directory"
            );
        }
    }
    let image = runtime::image_tag(&cfg);
    let mut creation_cfg = cfg.clone();
    creation_cfg.auth = Default::default();
    creation_cfg.lifecycle = Default::default();
    // These fields are root-owned live policy, not container topology.
    creation_cfg.network.allowed_hosts.clear();
    creation_cfg.network.allowed_ports = vec![443];
    creation_cfg.network.allow_private_ips = false;
    let spec = digest(serde_json::to_vec(&(
        &creation_cfg,
        runtime::revision(),
        &project.root,
        &image,
        unsafe { libc::getuid() },
        unsafe { libc::getgid() },
    ))?);
    if cli.dry_run {
        let (tool, args) = launch.context("--dry-run is supported for claude, codex, and shell")?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "container": project.record(&data)?.map(|r| r.id).unwrap_or(project.new_container_name()?),
                "project_key": project.id, "home_volume": format!("{}-home", project.id),
                "container_name_note": "new names include local creation time; dry-run name is a preview",
                "image": image, "workspace": project.root,
                "cwd": project.cwd, "read_only": cfg.workspace.read_only || !cfg.tools.edit,
                "network": cfg.network, "resources": cfg.resources, "tool": tool, "arguments": args,
                "auto_stop": cli.auto_stop || cfg.lifecycle.auto_stop,
                "auth": if cfg.auth.import_host { "subscription login in root-only proxy; agent gets placeholders" } else { "explicit jail auth required; no API keys" },
                "protect_sensitive": cfg.workspace.protect_sensitive, "deny_write": cfg.workspace.deny_write,
                "environment_names": cfg.environment.pass.iter().chain(cfg.environment.set.keys()).collect::<Vec<_>>()
            }))?
        );
        return Ok(0);
    }
    match action {
        Action::Tail { all } => return monitor::run(&backend, &data, &project, all),
        Action::Status { watch, all, json } => {
            return status(&backend, &data, &project, watch, all, json)
        }
        Action::Ps => {
            let record = project
                .record(&data)?
                .context("no container for this project")?;
            backend.require_owned(&record)?;
            return backend.interactive(&[
                "exec",
                "--user",
                "root",
                &record.id,
                "ps",
                "-eo",
                "pid,ppid,user,pcpu,pmem,etime,args",
            ]);
        }
        Action::Stop { all, force } => {
            let records = if all {
                project::records(&data)?
            } else {
                project.record(&data)?.into_iter().collect()
            };
            for record in records {
                let key = project::project_id(&record.root);
                let mutation = project::lock(&data, &key, "lifecycle")?;
                FileExt::lock_exclusive(&mutation)?;
                let sessions = project::lock(&data, &key, "sessions")?;
                if !force && FileExt::try_lock_exclusive(&sessions).is_err() {
                    bail!(
                        "{} has active sessions; use --force to stop them",
                        record.id
                    );
                }
                backend.require_owned(&record)?;
                backend.checked(&["stop", &record.id])?;
                println!("stopped {}", record.id);
            }
            return Ok(0);
        }
        Action::Recreate => {
            let mutation = project::lock(&data, &project.id, "lifecycle")?;
            FileExt::lock_exclusive(&mutation)?;
            let sessions = project::lock(&data, &project.id, "sessions")?;
            FileExt::try_lock_exclusive(&sessions)
                .context("active sessions; close them before recreating")?;
            if let Some(record) = project.record(&data)? {
                backend.require_owned(&record)?;
                backend.checked(&["stop", &record.id])?;
                backend.checked(&["delete", &record.id])?;
                fs::remove_file(project.record_path(&data))?;
                println!(
                    "removed {} root filesystem; home volume {} is preserved",
                    record.id, record.volume
                );
            }
            return Ok(0);
        }
        Action::Update => {
            backend.ensure_service()?;
            runtime::build(&backend, &data, &cfg, true)?;
            return Ok(0);
        }
        _ => {}
    }
    backend.ensure_service()?;
    let mutation = project::lock(&data, &project.id, "lifecycle")?;
    FileExt::lock_exclusive(&mutation)?;
    let sessions = project::lock(&data, &project.id, "sessions")?;
    FileExt::lock_shared(&sessions)?;
    let record = runtime::ensure(&backend, &data, &project, &cfg, &spec)?;
    policy_cli::sync(&backend, &record, &cfg)?;
    if !record.ports.is_empty() {
        eprintln!("jail: {}", record.id);
        for port in &record.ports {
            eprintln!("jail: {}", port.description());
        }
    }
    if let Action::Auth { tool } = &action {
        FileExt::try_lock_exclusive(&sessions)
            .context("close active sessions before re-importing authentication")?;
        auth::import(&backend, &record, tool, true)?;
        return Ok(0);
    }
    let (tool, agent_args) = launch.context("unsupported command")?;
    if tool != "shell" && cfg.auth.import_host && import_login {
        auth::import(&backend, &record, &tool, false)?;
    }
    drop(mutation);
    let mut args = vec!["exec".to_owned(), "--interactive".into()];
    for key in &cfg.environment.pass {
        args.extend(["--env".into(), key.clone()]);
    }
    if let Ok(term) = std::env::var("TERM") {
        args.extend(["--env".into(), format!("TERM={term}")]);
    }
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        args.push("--tty".into());
    }
    args.extend([
        "--user".into(),
        "root".into(),
        "--workdir".into(),
        project.cwd.to_string_lossy().into_owned(),
        record.id.clone(),
        "/usr/local/bin/jail-guest".into(),
        "launch".into(),
        tool,
    ]);
    args.extend(agent_args);
    let mut command = backend.command(&args);
    let auto_stop = cli.auto_stop || cfg.lifecycle.auto_stop;
    if !auto_stop {
        // The container client owns the terminal. Retain the shared session lock
        // across exec so stop/recreate cannot race an active client.
        let fd = sessions.as_raw_fd();
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        return Err(command.exec().into());
    }
    let result = wait_with_signals(&mut command);
    drop(sessions);
    let mutation = project::lock(&data, &project.id, "lifecycle")?;
    FileExt::lock_exclusive(&mutation)?;
    let sessions = project::lock(&data, &project.id, "sessions")?;
    if FileExt::try_lock_exclusive(&sessions).is_ok() {
        if let Err(e) = backend.checked(&["stop", &record.id]) {
            eprintln!("jail: auto-stop failed: {e:#}");
        }
    } else {
        eprintln!("jail: keeping container running for another active session");
    }
    result
}

fn wait_with_signals(command: &mut Command) -> Result<i32> {
    use signal_hook::{
        consts::signal::{SIGHUP, SIGINT, SIGTERM, SIGWINCH},
        iterator::{exfiltrator::WithOrigin, SignalsInfo},
    };
    let mut signals = SignalsInfo::<WithOrigin>::new([SIGHUP, SIGINT, SIGTERM, SIGWINCH])?;
    let handle = signals.handle();
    let mut child = command.spawn()?;
    let pid = child.id() as i32;
    let thread = std::thread::spawn(move || {
        for origin in signals.forever() {
            // The terminal already sends keyboard/resize signals to both
            // processes in the foreground group. Forward only process-origin
            // signals, so a single Ctrl+C is never delivered twice.
            if origin.process.is_some_and(|process| process.pid > 0) {
                unsafe {
                    libc::kill(pid, origin.signal);
                }
            }
        }
    });
    let result = child.wait();
    handle.close();
    let _ = thread.join();
    Ok(exit_code(result?))
}
fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or(128 + status.signal().unwrap_or(1))
}
fn status(
    b: &backend::Backend,
    data: &std::path::Path,
    project: &project::Project,
    watch: bool,
    all: bool,
    json: bool,
) -> Result<i32> {
    if watch && json {
        bail!("--watch and --json cannot be combined");
    }
    let records = if all {
        project::records(data)?
    } else {
        project.record(data)?.into_iter().collect()
    };
    if records.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("no jail containers; run jail claude or jail codex");
        }
        return Ok(0);
    }
    let mut rows = vec![];
    let mut running = vec![];
    for record in records {
        let state = b.owned_state(&record)?;
        if state.as_deref() == Some("running") {
            running.push(record.id.clone());
        }
        rows.push(serde_json::json!({ "id": record.id, "project": record.root, "state": state.unwrap_or("missing".into()), "ports": record.ports }));
    }
    if json {
        let stats = if running.is_empty() {
            serde_json::json!([])
        } else {
            let mut args = vec![
                "stats".into(),
                "--no-stream".into(),
                "--format".into(),
                "json".into(),
            ];
            args.extend(running.clone());
            serde_json::from_slice::<serde_json::Value>(&b.capture(&args)?)?
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"containers": rows, "stats": stats}))?
        );
    } else {
        for row in rows {
            println!(
                "{}  {}  {}",
                row["id"].as_str().unwrap_or(""),
                row["state"].as_str().unwrap_or(""),
                row["project"].as_str().unwrap_or("")
            );
            if let Some(ports) = row["ports"].as_array() {
                for port in ports {
                    println!(
                        "  http://127.0.0.1:{} -> container:{} (TCP)",
                        port["host"], port["container"]
                    );
                }
            }
        }
        if !running.is_empty() {
            let mut args = vec!["stats".into()];
            if !watch {
                args.push("--no-stream".into());
            }
            args.extend(running);
            return b.interactive_owned(&args);
        }
    }
    // watch delegates to Apple's live CPU/memory display without polling a service.
    Ok(0)
}
