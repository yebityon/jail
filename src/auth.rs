use crate::{backend::Backend, project::Record, user_home};
use anyhow::{bail, Context, Result};
use jail::digest;
use serde_json::Value;
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

fn host_credentials(tool: &str) -> Result<Option<Vec<u8>>> {
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
    let bytes = if file.is_file() {
        Some(fs::read(&file).context("could not read host authentication file")?)
    } else if std::env::consts::OS == "macos"
        && !(tool == "claude" && std::env::var_os("CLAUDE_CONFIG_DIR").is_some())
    {
        let mut command = Command::new("/usr/bin/security");
        command.args(["find-generic-password", "-s", service, "-w"]);
        if let Some(account) = account {
            command.args(["-a", &account]);
        }
        let output = command
            .stdin(Stdio::null())
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
pub fn import(backend: &Backend, record: &Record, tool: &str, force: bool) -> Result<()> {
    if !["claude", "codex"].contains(&tool) {
        bail!("auth tool must be claude or codex");
    }
    if !force {
        let output = backend
            .command(&[
                "exec",
                "--user",
                "root",
                &record.id,
                "/usr/local/bin/jail-guest",
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
    let credentials = host_credentials(tool)?;
    let Some(bytes) = credentials else {
        bail!("host login was not found or Keychain access was denied; log in with {tool} on the host, then run jail auth {tool}");
    };
    // Pipe secrets through stdin, never argv, environment, host state, or logs.
    let mut child = backend
        .command(&[
            "exec",
            "--interactive",
            "--user",
            "root",
            &record.id,
            "/usr/local/bin/jail-guest",
            "import-auth",
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
        .write_all(&bytes);
    let status = child.wait()?;
    write_result.context("could not send host login")?;
    if !status.success() {
        bail!("could not import host login; credential details are suppressed");
    }
    eprintln!(
        "jail: {tool} subscription login loaded into root-only proxy; agent receives placeholders"
    );
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
