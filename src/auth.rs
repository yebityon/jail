use crate::{
    backend::Backend,
    project::{self, Record},
    user_home,
};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use jail::{digest, GUEST};
use serde_json::Value;
use std::{
    fs,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

struct Source {
    file: PathBuf,
    keychain: Option<(&'static str, Option<String>)>,
}
fn source(tool: &str) -> Result<Source> {
    let home = user_home()?;
    let (file, service, account) = match tool {
        "claude" => {
            let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or(home.join(".claude"));
            // Custom Keychain namespaces are not guessed; file credentials still work.
            (
                dir.join(".credentials.json"),
                "Claude Code-credentials",
                None,
            )
        }
        "codex" => {
            let dir = std::env::var_os("CODEX_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or(home.join(".codex"));
            let canonical = dir.canonicalize().unwrap_or(dir.clone());
            let account = format!(
                "cli|{}",
                &digest(canonical.to_string_lossy().as_bytes())[..16]
            );
            (dir.join("auth.json"), "Codex Auth", Some(account))
        }
        _ => bail!("auth tool must be claude or codex"),
    };
    let keychain = std::env::consts::OS == "macos"
        && !(tool == "claude" && std::env::var_os("CLAUDE_CONFIG_DIR").is_some());
    Ok(Source {
        file,
        keychain: keychain.then_some((service, account)),
    })
}
fn host_credentials(tool: &str) -> Result<Option<Vec<u8>>> {
    let source = source(tool)?;
    let bytes = if source.file.is_file() {
        Some(fs::read(&source.file).context("could not read host authentication file")?)
    } else if let Some((service, account)) = source.keychain {
        let mut command = Command::new("/usr/bin/security");
        command.args(["find-generic-password", "-s", service, "-w"]);
        if let Some(account) = account {
            command.args(["-a", &account]);
        }
        let output = command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("could not access host Keychain")?;
        if output.status.success() {
            Some(output.stdout)
        } else {
            None
        }
    } else {
        None
    };
    bytes.map(|bytes| sanitize(tool, &bytes)).transpose()
}
/// Change marker for the host login, never the secret itself: file metadata,
/// or Keychain attributes including `mdat`. Reading attributes does not ask
/// for Keychain access, so polling cannot trigger repeated prompts.
fn fingerprint(tool: &str) -> Result<String> {
    let source = source(tool)?;
    if source.file.is_file() {
        use std::os::unix::fs::MetadataExt;
        let m = fs::metadata(&source.file)?;
        return Ok(format!(
            "file:{}:{}:{}:{}",
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec()
        ));
    }
    if let Some((service, account)) = source.keychain {
        let mut command = Command::new("/usr/bin/security");
        command.args(["find-generic-password", "-s", service]);
        if let Some(account) = account {
            command.args(["-a", &account]);
        }
        let output = command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("could not access host Keychain")?;
        if output.status.success() {
            return Ok(format!("keychain:{}", digest(&output.stdout)));
        }
    }
    Ok("none".into())
}
fn fingerprint_path(data: &Path, record: &Record, tool: &str) -> PathBuf {
    // The file keeps its earlier name, so upgrading does not resend the login.
    project::dir(data, &project::key(&record.root)).join(format!("auth-{tool}.version"))
}
fn save_fingerprint(data: &Path, record: &Record, tool: &str, fingerprint: &str) -> Result<()> {
    let path = fingerprint_path(data, record, tool);
    project::private_dir(path.parent().context("fingerprint has no parent")?)?;
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?
        .write_all(fingerprint.as_bytes())?;
    Ok(())
}
fn sanitize(tool: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    let input: Value =
        serde_json::from_slice(bytes).context("host authentication is not valid JSON")?;
    let keys: &[&str] = match tool {
        "claude" => &["claudeAiOauth"],
        "codex" => &["auth_mode", "tokens", "last_refresh"],
        _ => bail!("unsupported authentication tool"),
    };
    let mut out = serde_json::Map::new();
    for key in keys {
        if let Some(value) = input.get(key) {
            out.insert((*key).to_owned(), value.clone());
        }
    }
    let has_credentials = if tool == "claude" {
        out.get("claudeAiOauth").is_some_and(|v| v.is_object())
    } else {
        out.get("tokens").is_some_and(|v| v.is_object())
    };
    if !has_credentials {
        bail!("host subscription login is required; API keys are not supported");
    }
    jail::gateway::token(tool, &Value::Object(out.clone()))?;
    Ok(serde_json::to_vec(&out)?)
}
/// Loads the host login into the container's gateway. `explicit` is set by
/// `jail auth`: it replaces a login the container already has and fails when
/// the host has none. A launch keeps an existing login, and on a terminal it
/// only warns about a missing one.
pub fn import(
    backend: &Backend,
    record: &Record,
    data: &Path,
    tool: &str,
    explicit: bool,
) -> Result<()> {
    if !["claude", "codex"].contains(&tool) {
        bail!("auth tool must be claude or codex");
    }
    if !explicit {
        let output = backend
            .command(&[
                "exec",
                "--user",
                "root",
                &record.container,
                GUEST,
                "has-auth",
                tool,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if output.success() {
            return Ok(());
        }
        if output.code() != Some(3) {
            bail!("could not check container authentication; run jail doctor");
        }
    }
    // Capture the marker first; a host refresh during the read is synced later.
    let fingerprint = fingerprint(tool)?;
    let credentials = host_credentials(tool)?;
    let Some(bytes) = credentials else {
        if !explicit && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            eprintln!(
                "jail: no host login found; run `{}` on the Mac and this session receives it within about 15 seconds",
                host_login_command(tool)
            );
            return Ok(());
        }
        bail!(
            "host login was not found or Keychain access was denied; run `{}` on the Mac, then run jail auth {tool}",
            host_login_command(tool)
        );
    };
    send(backend, record, tool, &bytes, "import-auth")?;
    save_fingerprint(data, record, tool, &fingerprint)
}

const SYNC_INTERVAL: Duration = Duration::from_secs(15);

/// Follows refreshes made by the official CLI on the Mac while a session runs.
/// jail never refreshes OAuth or writes host credentials itself, so it cannot
/// consume a rotating refresh token that the host CLI still needs.
pub struct Watch {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<Option<anyhow::Error>>>,
}
impl Watch {
    /// Stops watching and returns the last sync failure. Printing during the
    /// session would corrupt the agent's TUI, so it is reported afterwards.
    pub fn finish(mut self) -> Option<anyhow::Error> {
        drop(self.stop.take());
        match self.thread.take()?.join() {
            Ok(failure) => failure,
            Err(_) => Some(anyhow::anyhow!("host login watcher panicked")),
        }
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
pub fn watch(record: &Record, data: &Path, tool: &str) -> Watch {
    let (stop, wake) = mpsc::channel::<()>();
    let record = record.clone();
    let data = data.to_owned();
    let tool = tool.to_owned();
    let thread = std::thread::spawn(move || {
        let backend = Backend::new();
        let key = project::key(&record.root);
        let mut owner = None;
        let mut failure = None;
        loop {
            // One watcher per project and tool reads the host login; other
            // sessions take over when it exits.
            if owner.is_none() {
                match project::lock(&data, &key, &format!("auth-{tool}")) {
                    Ok(file) if FileExt::try_lock_exclusive(&file).is_ok() => owner = Some(file),
                    Ok(_) => {}
                    Err(e) => failure = Some(e),
                }
            }
            if owner.is_some() {
                // Failures are retried when the host login changes again.
                match follow(&backend, &record, &data, &tool) {
                    Ok(true) => failure = None,
                    Ok(false) => {}
                    Err(e) => failure = Some(e),
                }
            }
            if wake.recv_timeout(SYNC_INTERVAL) != Err(mpsc::RecvTimeoutError::Timeout) {
                break;
            }
        }
        failure
    });
    Watch {
        stop: Some(stop),
        thread: Some(thread),
    }
}
/// Returns whether the host login changed and a sync was attempted.
fn follow(backend: &Backend, record: &Record, data: &Path, tool: &str) -> Result<bool> {
    let fingerprint = fingerprint(tool)?;
    if fs::read_to_string(fingerprint_path(data, record, tool)).ok() == Some(fingerprint.clone()) {
        return Ok(false);
    }
    let credentials = host_credentials(tool);
    // Record the attempt even on failure: a denied Keychain read must not
    // prompt again every interval. A partly written file changes the marker.
    save_fingerprint(data, record, tool, &fingerprint)?;
    if let Some(bytes) = credentials? {
        pipe(backend, record, tool, &bytes, "sync-auth")?;
    }
    Ok(true)
}

fn send(
    backend: &Backend,
    record: &Record,
    tool: &str,
    bytes: &[u8],
    operation: &str,
) -> Result<()> {
    pipe(backend, record, tool, bytes, operation)?;
    eprintln!(
        "jail: {tool} subscription login loaded into root-only proxy; agent receives placeholders"
    );
    Ok(())
}

fn pipe(
    backend: &Backend,
    record: &Record,
    tool: &str,
    bytes: &[u8],
    operation: &str,
) -> Result<()> {
    // Pipe secrets through stdin, never argv, environment, host state, or logs.
    let mut child = backend
        .command(&[
            "exec",
            "--interactive",
            "--user",
            "root",
            &record.container,
            GUEST,
            operation,
            tool,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let write_result = child
        .stdin
        .take()
        .context("missing credential pipe")?
        .write_all(bytes);
    let status = child.wait()?;
    write_result.context("could not send host login")?;
    if !status.success() {
        bail!("could not import host login; credential details are suppressed");
    }
    Ok(())
}

/// The official command that signs in on the Mac. jail itself never signs in.
fn host_login_command(tool: &str) -> &'static str {
    if tool == "claude" {
        "claude auth login"
    } else {
        "codex login"
    }
}

/// Answers a /login typed in a jail session. Sign-in happens only on the Mac;
/// the agent CLI never starts its own login inside the container.
pub fn refuse_login(tool: &str, sync: bool) -> Result<()> {
    let command = host_login_command(tool);
    if sync {
        eprintln!("jail: login from jail is disabled; run `{command}` on the Mac. This session receives the new login within about 15 seconds; then retry.");
    } else {
        eprintln!("jail: login from jail is disabled; run `{command}` on the Mac, close this session, then run jail auth {tool}.");
    }
    eprint!("jail: press Enter to return to the session ");
    std::io::stdin().read_line(&mut String::new())?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copies_only_credentials() {
        let out = sanitize("claude", br#"{"claudeAiOauth":{"accessToken":"secret"},"mcpServers":{"bad":{}},"settings":{"hook":"bad"}}"#).unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert!(value.get("claudeAiOauth").is_some());
        assert!(value.get("mcpServers").is_none());
        assert!(value.get("settings").is_none());
    }
    #[test]
    fn api_key_only_authentication_and_header_injection_are_rejected() {
        assert!(sanitize(
            "codex",
            br#"{"auth_mode":"api_key","OPENAI_API_KEY":"sk-secret"}"#
        )
        .is_err());
        assert!(sanitize(
            "claude",
            br#"{"claudeAiOauth":{"accessToken":"secret\r\ninjected: header"}}"#
        )
        .is_err());
        let out=sanitize("codex",br#"{"auth_mode":"chatgpt","tokens":{"access_token":"oauth-secret"},"OPENAI_API_KEY":"sk-secret"}"#).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("sk-secret"));
    }
}
