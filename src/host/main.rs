mod apple_container;
mod auth;
mod backend;
mod cleanup;
mod policy_cli;
mod project;
mod runtime;
mod terminal;
mod write_guard;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use jail::{agent_policy, config::Config, digest, GUEST};
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
    after_help = "Examples:\n  jail claude --resume\n  jail codex exec 'run tests'\n  jail --auto-stop claude\n\nPut jail options before claude/codex. Everything after the tool name is forwarded."
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
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    #[command(about = "Inspect denied destinations and adjust the trusted network allowlist")]
    Policy {
        #[command(subcommand)]
        action: policy_cli::Action,
    },
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
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    #[command(about = "Explicitly re-import this tool's host login (claude or codex)")]
    Auth { tool: String },
    #[command(
        about = "List jail home volumes and images; delete named ones if no container uses them, or all with --all-delete",
        args_conflicts_with_subcommands = true
    )]
    Cleanup {
        #[arg(
            long,
            help = "Delete every jail container, home volume and image, without confirmation"
        )]
        all_delete: bool,
        #[command(subcommand)]
        target: Option<cleanup::Target>,
    },
    #[command(external_subcommand)]
    Tool(Vec<OsString>),
}
#[derive(Subcommand)]
enum ConfigAction {
    Init,
    Show,
}
/// How the host process runs a launched agent.
enum Session {
    /// Replace jail with the container client.
    Exec,
    /// Relay the terminal through a PTY so jail answers a typed /login.
    Pty,
    /// Wait and forward signals, for host login sync and auto-stop.
    Wait,
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
    let Some(action) = cli.command.take() else {
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        return Ok(0);
    };
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
        file.write_all(include_bytes!("../../config.example.toml"))?;
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
    let client = backend::client();
    let data = data_path()?;
    let project = project::Project::discover()?;
    if let Action::Policy { action } = action {
        return policy_cli::run(action, &path, &cfg, &client, &data, &project);
    }
    let mut launch = None;
    // --help and --version run without a login.
    let mut needs_login = true;
    if let Action::Tool(raw) = &action {
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
        needs_login = !args
            .iter()
            .any(|a| ["--help", "-h", "--version", "-V"].contains(&a.as_str()));
        launch = Some((
            tool.to_owned(),
            agent_policy::agent_args(tool, &args, &cfg)?,
        ));
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
        let (tool, args) = launch.context("--dry-run is supported for claude and codex")?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "container": project.record(&data)?.map(|r| r.container).unwrap_or(project.new_container_name()?),
                "project_key": project.key, "home_volume": project::home_volume(&project.key),
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
        Action::Stop { all, force } => {
            let records = if all {
                project::records(&data)?
            } else {
                project.record(&data)?.into_iter().collect()
            };
            for record in records {
                let key = project::key(&record.root);
                let lifecycle = project::lock(&data, &key, "lifecycle")?;
                FileExt::lock_exclusive(&lifecycle)?;
                let sessions = project::lock(&data, &key, "sessions")?;
                if !force && FileExt::try_lock_exclusive(&sessions).is_err() {
                    bail!(
                        "{} has active sessions; use --force to stop them",
                        record.container
                    );
                }
                backend::require_owned(&client, &record)?;
                client.stop(&record.container)?;
                println!("stopped {}", record.container);
            }
            return Ok(0);
        }
        Action::Recreate => {
            let lifecycle = project::lock(&data, &project.key, "lifecycle")?;
            FileExt::lock_exclusive(&lifecycle)?;
            let sessions = project::lock(&data, &project.key, "sessions")?;
            FileExt::try_lock_exclusive(&sessions)
                .context("active sessions; close them before recreating")?;
            if let Some(record) = project.record(&data)? {
                backend::require_owned(&client, &record)?;
                client.stop(&record.container)?;
                client.delete(&record.container)?;
                fs::remove_file(project.record_path(&data))?;
                println!(
                    "removed {} root filesystem; home volume {} is preserved",
                    record.container, record.volume
                );
            }
            return Ok(0);
        }
        Action::Cleanup { all_delete, target } => {
            return cleanup::run(target, all_delete, &client, &data, &project, &image);
        }
        _ => {}
    }
    backend::ensure_service(&client)?;
    let lifecycle = project::lock(&data, &project.key, "lifecycle")?;
    FileExt::lock_exclusive(&lifecycle)?;
    let sessions = project::lock(&data, &project.key, "sessions")?;
    FileExt::lock_shared(&sessions)?;
    let record = runtime::ensure(&client, &data, &project, &cfg, &spec)?;
    policy_cli::sync(&client, &record, &cfg)?;
    if !record.ports.is_empty() {
        eprintln!("jail: {}", record.container);
        for port in &record.ports {
            eprintln!("jail: {}", port.description());
        }
    }
    if let Action::Auth { tool } = &action {
        FileExt::try_lock_exclusive(&sessions)
            .context("close active sessions before re-importing authentication")?;
        auth::import(&client, &record, &data, tool, true)?;
        return Ok(0);
    }
    let (tool, agent_args) = launch.context("unsupported command")?;
    let host_login = cfg.auth.import_host && needs_login;
    if host_login {
        auth::import(&client, &record, &data, &tool, false)?;
    }
    drop(lifecycle);
    let mut exec = client.exec(&record.container);
    exec.interactive();
    for key in &cfg.environment.pass {
        exec.pass_env(key);
    }
    if let Ok(term) = std::env::var("TERM") {
        exec.env("TERM", &term);
    }
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        exec.tty();
    }
    exec.user("root").workdir(&project.cwd);
    let mut args = vec![GUEST.to_owned(), "launch".into(), tool.clone()];
    args.extend(agent_args);
    let mut command = exec.command(&args);
    let auto_stop = cli.auto_stop || cfg.lifecycle.auto_stop;
    let session = if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Session::Pty
    } else if auto_stop || host_login {
        Session::Wait
    } else {
        Session::Exec
    };
    if let Session::Exec = session {
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
    // The host process stays alive for the session so host refreshes reach it.
    let watch = host_login.then(|| auth::watch(&record, &data, &tool));
    let result = if let Session::Pty = session {
        terminal::run(&mut command, || auth::refuse_login(&tool, host_login))
    } else {
        wait_with_signals(&mut command)
    };
    if let Some(e) = watch.and_then(auth::Watch::finish) {
        eprintln!("jail: could not sync refreshed host login: {e:#}");
    }
    if !auto_stop {
        return result;
    }
    drop(sessions);
    let lifecycle = project::lock(&data, &project.key, "lifecycle")?;
    FileExt::lock_exclusive(&lifecycle)?;
    let sessions = project::lock(&data, &project.key, "sessions")?;
    if FileExt::try_lock_exclusive(&sessions).is_ok() {
        if let Err(e) = client.stop(&record.container) {
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
