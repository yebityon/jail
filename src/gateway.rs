//! OAuth-only credential injection. Agents see placeholders, never real tokens.
//! Static upstreams, TLS verification, no redirects, no secret argv/environment.
use crate::{network_policy, proxy};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

pub const PRIVATE: &str = "/run/jail-private";
pub fn credential_path(tool: &str) -> Result<PathBuf> {
    if !["claude", "codex"].contains(&tool) {
        bail!("unsupported auth tool");
    }
    Ok(PathBuf::from(PRIVATE).join(format!("{tool}.json")))
}
pub fn token(tool: &str, value: &Value) -> Result<String> {
    let value = if tool == "claude" {
        &value["claudeAiOauth"]["accessToken"]
    } else {
        &value["tokens"]["access_token"]
    };
    let token = value
        .as_str()
        .context("host subscription login is required; API keys are not supported")?;
    if token.is_empty() || token.len() > 32_768 || token.bytes().any(|b| b <= 32 || b >= 127) {
        bail!("invalid subscription credential");
    }
    Ok(token.into())
}
pub fn import(tool: &str, value: &Value) -> Result<()> {
    token(tool, value)?;
    let path = credential_path(tool)?;
    let tmp = path.with_extension("new");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    f.write_all(&serde_json::to_vec(value)?)?;
    f.sync_all()?;
    fs::rename(tmp, path)?;
    Ok(())
}
fn base64(bytes: &[u8]) -> String {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for part in bytes.chunks(3) {
        let n = ((part[0] as u32) << 16)
            | ((part.get(1).copied().unwrap_or(0) as u32) << 8)
            | part.get(2).copied().unwrap_or(0) as u32;
        out.push(alphabet[((n >> 18) & 63) as usize] as char);
        out.push(alphabet[((n >> 12) & 63) as usize] as char);
        if part.len() > 1 {
            out.push(alphabet[((n >> 6) & 63) as usize] as char);
        }
        if part.len() > 2 {
            out.push(alphabet[(n & 63) as usize] as char);
        }
    }
    out
}
pub fn placeholder(tool: &str, real: &Value) -> Value {
    if tool == "claude" {
        let mut oauth = json!({"accessToken":"jail-proxy-placeholder", "refreshToken":"jail-proxy-placeholder", "expiresAt":4102444800000u64,
            "scopes":["user:inference","user:profile"]});
        for key in ["subscriptionType", "rateLimitTier"] {
            if let Some(v) = real["claudeAiOauth"].get(key) {
                oauth[key] = v.clone();
            }
        }
        json!({"claudeAiOauth":oauth})
    } else {
        // Structurally valid, unsigned synthetic JWT; never reuse the real id_token.
        let claims = json!({"sub":"jail", "email":"jail@localhost", "exp":4102444800u64,
            "https://api.openai.com/auth":{"chatgpt_account_id":"jail-proxy", "chatgpt_plan_type":"plus"}});
        let jwt = format!(
            "{}.{}.jail",
            base64(br#"{"alg":"none","typ":"JWT"}"#),
            base64(serde_json::to_string(&claims).unwrap().as_bytes())
        );
        json!({"auth_mode":"chatgpt", "OPENAI_API_KEY":null,
            "tokens":{"id_token":jwt,"access_token":"jail-proxy-placeholder","refresh_token":"jail-proxy-placeholder", "account_id":"jail-proxy"},
            "last_refresh":"2099-01-01T00:00:00Z"})
    }
}
fn upstream(tool: &str, method: &str, path: &str) -> Result<(&'static str, String)> {
    if !["GET", "POST"].contains(&method)
        || !path.starts_with('/')
        || path.contains(['\r', '\n', '\\', '#'])
        || path.contains("..")
        || path.contains('%')
        || path.len() > 8192
    {
        bail!("unsupported gateway request");
    }
    let route = path.split('?').next().unwrap_or("");
    let allowed = if tool == "claude" {
        route == "/v1/messages" || route == "/v1/messages/count_tokens" || route == "/v1/models"
    } else {
        matches!(
            route,
            "/backend-api/codex/responses"
                | "/backend-api/codex/responses/compact"
                | "/backend-api/codex/models"
                | "/backend-api/codex/usage"
        )
    };
    if !allowed {
        bail!("unsupported gateway route");
    }
    let host = if tool == "claude" {
        "api.anthropic.com"
    } else {
        "chatgpt.com"
    };
    Ok((host, format!("https://{host}{path}")))
}
struct BodyFile(PathBuf);
impl Drop for BodyFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
struct Curl(std::process::Child);
impl Drop for Curl {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
fn handle(mut client: TcpStream, tool: &str) -> Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    client.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let mut header = vec![];
    // Strict bounded parser: no ambiguous CL/TE, no pipelining or upgrades.
    while !header.ends_with(b"\r\n\r\n") && header.len() < 32_768 {
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        header.push(byte[0]);
    }
    if !header.ends_with(b"\r\n\r\n") {
        bail!("header too large");
    }
    let header = String::from_utf8(header)?;
    let mut lines = header.split("\r\n");
    let parts: Vec<_> = lines.next().unwrap_or("").split_whitespace().collect();
    if parts.len() != 3 || !["HTTP/1.1", "HTTP/1.0"].contains(&parts[2]) {
        bail!("invalid request");
    }
    let (host, url) = upstream(tool, parts[0], parts[1])?;
    let policy = network_policy::load()?;
    if !network_policy::permits(&policy, host, 443) {
        network_policy::deny(host, 443, "auth_gateway_allowlist")?;
        client.write_all(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        return Ok(());
    }
    let credentials: Value = match fs::read(credential_path(tool)?) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(_) => {
            client.write_all(
                b"HTTP/1.1 503 Login Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )?;
            return Ok(());
        }
    };
    let secret = token(tool, &credentials)?;
    let mut length = None;
    let mut headers = vec![];
    for line in lines.filter(|l| !l.is_empty()) {
        let (key, value) = line.split_once(':').context("invalid header")?;
        let key = key.to_ascii_lowercase();
        let value = value.trim();
        if !key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'-' || b == b'_')
            || value.bytes().any(|b| b < 32 || b == 127)
        {
            bail!("invalid header");
        }
        match key.as_str() {
            "content-length" => {
                if length.is_some() {
                    bail!("duplicate content length");
                }
                length = Some(value.parse::<usize>()?);
            }
            "transfer-encoding" | "upgrade" | "expect" => {
                bail!("unsupported transfer encoding or upgrade")
            }
            "content-type"
            | "content-encoding"
            | "accept"
            | "anthropic-version"
            | "anthropic-beta"
            | "user-agent"
            | "originator"
            | "session_id"
            | "x-codex-turn-metadata"
            | "x-codex-beta-features" => headers.push(format!("{key}: {value}")),
            _ => {} // Never forward caller Authorization, account IDs or routing headers.
        }
    }
    let length = length.unwrap_or(0);
    if length > 64 * 1024 * 1024 {
        bail!("request too large");
    }
    let id = REQUEST.fetch_add(1, Ordering::Relaxed);
    let body = BodyFile(PathBuf::from(PRIVATE).join(format!("body-{}-{id}", std::process::id())));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&body.0)?;
    let copied = std::io::copy(&mut reader.take(length as u64), &mut file)?;
    if copied != length as u64 {
        bail!("incomplete body");
    }
    drop(file);
    // Resolve once and pin. Never follow a redirect or consult agent proxy env.
    let addresses: Vec<_> = (host, 443).to_socket_addrs()?.collect();
    if addresses.is_empty() || addresses.iter().any(|a| !proxy::public_ip(a.ip())) {
        network_policy::deny(host, 443, "auth_gateway_private_ip")?;
        bail!("unsafe upstream address");
    }
    let ip = addresses[0].ip();
    let resolve = format!(
        "{host}:443:{}",
        if ip.is_ipv6() {
            format!("[{ip}]")
        } else {
            ip.to_string()
        }
    );
    let mut cfg = format!(
        "url = {}\nrequest = {}\nresolve = {}\nheader = {}\n",
        quoted(&url),
        quoted(parts[0]),
        quoted(&resolve),
        quoted(&format!("Authorization: Bearer {secret}"))
    );
    if tool == "codex" {
        if let Some(account) = credentials["tokens"]["account_id"].as_str() {
            if !account
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            {
                bail!("invalid account ID");
            }
            headers.push(format!("ChatGPT-Account-Id: {account}"));
        }
    }
    for h in headers {
        cfg.push_str(&format!("header = {}\n", quoted(&h)));
    }
    if parts[0] == "POST" {
        cfg.push_str(&format!(
            "data-binary = {}\n",
            quoted(&format!("@{}", body.0.display()))
        ));
    }
    let mut curl = Curl(
        Command::new("/usr/bin/curl")
            .env_clear()
            .args([
                "--disable",
                "--config",
                "-",
                "--silent",
                "--include",
                "--no-buffer",
                "--http1.1",
                "--proxy",
                "",
                "--connect-timeout",
                "15",
                "--max-time",
                "1800",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    );
    curl.0
        .stdin
        .take()
        .context("curl stdin")?
        .write_all(cfg.as_bytes())?;
    let mut response = BufReader::new(curl.0.stdout.take().context("curl stdout")?);
    let (status, content_type) = loop {
        let mut line = String::new();
        response.by_ref().take(8192).read_line(&mut line)?;
        let status = line
            .split_whitespace()
            .nth(1)
            .context("invalid upstream response")?
            .parse::<u16>()?;
        let mut content_type = String::from("application/json");
        let mut count = line.len();
        loop {
            line.clear();
            response.by_ref().take(8192).read_line(&mut line)?;
            count += line.len();
            if count > 32_768 || line.is_empty() {
                bail!("invalid upstream headers");
            }
            if line == "\r\n" {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                if k.eq_ignore_ascii_case("content-type") {
                    content_type = v.trim().into();
                }
            }
        }
        if status >= 200 {
            break (status, content_type);
        }
    };
    write!(
        client,
        "HTTP/1.1 {status} Upstream\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n"
    )?;
    let mut buffer = [0u8; 16_384];
    loop {
        let n = response.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        client.write_all(&buffer[..n])?;
        client.flush()?;
    }
    Ok(())
}
static REQUEST: AtomicUsize = AtomicUsize::new(0);
pub fn serve(listener: TcpListener, tool: &'static str) -> Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    for client in listener.incoming() {
        let mut client = client?;
        if active.fetch_add(1, Ordering::SeqCst) >= 64 {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let active = active.clone();
        std::thread::spawn(move || {
            if handle(client.try_clone().unwrap(), tool).is_err() {
                let _ = client.write_all(
                    b"HTTP/1.1 502 Gateway Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    bail!("authentication gateway stopped")
}
pub fn prepare() -> Result<()> {
    fs::create_dir_all(PRIVATE)?;
    fs::set_permissions(PRIVATE, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn placeholders_never_contain_credentials() {
        let real = json!({"claudeAiOauth":{"accessToken":"secret-access","refreshToken":"secret-refresh"},"tokens":{"access_token":"secret-access","id_token":"secret-id","refresh_token":"secret-refresh"}});
        for tool in ["claude", "codex"] {
            let dummy = placeholder(tool, &real).to_string();
            assert!(!dummy.contains("secret-"));
        }
    }
    #[test]
    fn route_and_method_are_fixed() {
        assert!(upstream("claude", "POST", "/v1/messages?beta=true").is_ok());
        assert!(upstream("codex", "POST", "/backend-api/codex/responses").is_ok());
        for path in [
            "http://evil.test/v1/messages",
            "//evil.test/v1/messages",
            "/oauth/token",
            "/v1/messages/../oauth/token",
            "/v1/%6dessages",
        ] {
            assert!(upstream("claude", "POST", path).is_err());
        }
        assert!(upstream("claude", "CONNECT", "/v1/messages").is_err());
        assert_eq!(base64(b"hello"), "aGVsbG8");
    }
}
