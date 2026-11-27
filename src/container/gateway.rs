//! OAuth-only credential injection. Agents see placeholders, never real tokens.
//! Static upstreams, TLS verification, no redirects, no secret argv/environment.
use crate::{network_policy, proxy, PRIVATE};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
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
    thread,
    time::Duration,
};

/// Loopback ports of the gateways inside the container.
pub const CLAUDE_PORT: u16 = 3130;
pub const CODEX_PORT: u16 = 3131;
/// ANTHROPIC_BASE_URL for Claude Code.
pub fn claude_url() -> String {
    format!("http://127.0.0.1:{CLAUDE_PORT}")
}
/// chatgpt_base_url for Codex; its model provider appends /codex.
pub fn codex_url() -> String {
    format!("http://127.0.0.1:{CODEX_PORT}/backend-api")
}
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
/// Serializes credential writers: explicit imports, /login and host sync.
fn lock(tool: &str) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(PathBuf::from(PRIVATE).join(format!("{tool}.lock")))?;
    FileExt::lock_exclusive(&file)?;
    Ok(file)
}
pub fn import(tool: &str, value: &Value) -> Result<()> {
    let _lock = lock(tool)?;
    store(tool, value)
}
/// Host sync never replaces a later credential with an older copy, such as one
/// another session read before a refresh. Returns whether the gateway changed.
pub fn sync(tool: &str, value: &Value) -> Result<bool> {
    token(tool, value)?;
    let _lock = lock(tool)?;
    let current: Option<Value> = match fs::read(credential_path(tool)?) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if !newer(tool, current.as_ref(), value) {
        return Ok(false);
    }
    store(tool, value)?;
    Ok(true)
}
fn newer(tool: &str, current: Option<&Value>, incoming: &Value) -> bool {
    let Some(current) = current else {
        return true;
    };
    if token(tool, current).ok() == token(tool, incoming).ok() {
        return false;
    }
    match (expiry(tool, current), expiry(tool, incoming)) {
        (Some(current), Some(incoming)) => incoming > current,
        (None, incoming) => incoming.is_some(),
        (Some(_), None) => false,
    }
}
/// Access-token expiry in Unix milliseconds. Codex states it only in the
/// public `exp` claim of its JWT; the signature is not needed for ordering.
fn expiry(tool: &str, value: &Value) -> Option<u64> {
    if tool == "claude" {
        return value["claudeAiOauth"]["expiresAt"].as_u64();
    }
    let payload = value["tokens"]["access_token"]
        .as_str()?
        .split('.')
        .nth(1)?;
    let claims: Value = serde_json::from_slice(&decode(payload)?).ok()?;
    claims["exp"].as_u64()?.checked_mul(1000)
}
fn store(tool: &str, value: &Value) -> Result<()> {
    token(tool, value)?;
    let path = credential_path(tool)?;
    let tmp = path.with_extension(format!(
        "{}-{}.new",
        std::process::id(),
        REQUEST.fetch_add(1, Ordering::Relaxed)
    ));
    let cleanup = BodyFile(tmp.clone());
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    f.write_all(&serde_json::to_vec(value)?)?;
    f.sync_all()?;
    fs::rename(tmp, path)?;
    drop(cleanup);
    Ok(())
}
pub(crate) fn base64(bytes: &[u8]) -> String {
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
pub(crate) fn decode(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = vec![];
    for c in text.trim_end_matches('=').bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        bits = ((bits << 6) | value as u32) & 0xffff;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Some(out)
}
pub fn placeholder(tool: &str, real: &Value) -> Value {
    if tool == "claude" {
        // No refreshToken: Claude Code also sends this token straight to
        // api.anthropic.com (profile, bootstrap) and gets 401. With a refresh
        // token it would refresh, fail, and clear this file ("Login expired").
        let mut oauth = json!({"accessToken":"jail-proxy-placeholder", "expiresAt":4102444800000u64,
            "scopes":["user:inference","user:profile"]});
        // Real scopes keep the plan shown and plan-dependent features enabled.
        for key in ["subscriptionType", "rateLimitTier", "scopes"] {
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
pub(crate) fn upstream(tool: &str, method: &str, path: &str) -> Result<(&'static str, String)> {
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
/// A request head, read through its blank line.
pub(crate) struct Head<'a> {
    pub(crate) method: &'a str,
    pub(crate) path: &'a str,
    /// The header lines.
    pub(crate) fields: &'a str,
}
pub(crate) fn parse_head(head: &[u8]) -> Result<Head<'_>> {
    let head = std::str::from_utf8(head)?;
    let (line, fields) = head.split_once("\r\n").context("invalid request")?;
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() != 3 || !["HTTP/1.1", "HTTP/1.0"].contains(&parts[2]) {
        bail!("invalid request");
    }
    Ok(Head {
        method: parts[0],
        path: parts[1],
        fields,
    })
}
/// The body length and the headers to forward upstream.
pub(crate) fn forwarded(fields: &str) -> Result<(u64, Vec<String>)> {
    let mut length = None;
    let mut headers = vec![];
    for line in fields.split("\r\n").filter(|l| !l.is_empty()) {
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
                length = Some(value.parse::<u64>()?);
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
    Ok((length, headers))
}
fn handle(client: &mut TcpStream, tool: &str) -> Result<()> {
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
    let head = parse_head(&header)?;
    let (host, url) = upstream(tool, head.method, head.path)?;
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
    let (length, mut headers) = forwarded(head.fields)?;
    let id = REQUEST.fetch_add(1, Ordering::Relaxed);
    let body = BodyFile(PathBuf::from(PRIVATE).join(format!("body-{}-{id}", std::process::id())));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&body.0)?;
    let copied = std::io::copy(&mut reader.take(length), &mut file)?;
    if copied != length {
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
        quoted(head.method),
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
    if head.method == "POST" {
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
        let Some(slot) = proxy::Slot::take(&active, 64) else {
            continue;
        };
        thread::spawn(move || {
            let _slot = slot;
            if handle(&mut client, tool).is_err() {
                let _ = client.write_all(
                    b"HTTP/1.1 502 Gateway Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
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
    fn claude_placeholder_keeps_real_scopes() {
        let scopes = json!([
            "user:inference",
            "user:profile",
            "user:sessions:claude_code"
        ]);
        let real = json!({"claudeAiOauth":{"accessToken":"secret-access","scopes":scopes}});
        assert_eq!(
            placeholder("claude", &real)["claudeAiOauth"]["scopes"],
            scopes
        );
        let real = json!({"claudeAiOauth":{"accessToken":"secret-access","refreshToken":"secret-refresh"}});
        assert!(placeholder("claude", &real)["claudeAiOauth"]
            .get("refreshToken")
            .is_none());
        assert_eq!(
            placeholder("claude", &Value::Null)["claudeAiOauth"]["scopes"],
            json!(["user:inference", "user:profile"])
        );
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
    #[test]
    fn request_heads_are_parsed_strictly() {
        let head = parse_head(b"POST /v1/messages HTTP/1.1\r\nContent-Length: 2\r\n\r\n").unwrap();
        assert_eq!((head.method, head.path), ("POST", "/v1/messages"));
        assert_eq!(forwarded(head.fields).unwrap().0, 2);
        for head in [
            b"POST /v1/messages\r\n\r\n".as_slice(),
            b"POST /v1/messages HTTP/2\r\n\r\n",
            b"POST /v1/messages HTTP/1.1 extra\r\n\r\n",
            b"\xff /v1/messages HTTP/1.1\r\n\r\n",
        ] {
            assert!(parse_head(head).is_err());
        }
        let (length, headers) = forwarded(
            "Authorization: Bearer agent\r\nAccept: text/event-stream\r\nX-Forwarded-Host: evil.test\r\n\r\n",
        )
        .unwrap();
        assert_eq!(length, 0);
        assert_eq!(headers, ["accept: text/event-stream"]);
        for fields in [
            "Content-Length: 1\r\nContent-Length: 1\r\n",
            "Transfer-Encoding: chunked\r\n",
            "Content-Length: 67108865\r\n",
            "Content-Length: -1\r\n",
            "Accept text/plain\r\n",
            "Bad Key: x\r\n",
            "Accept: a\x7fb\r\n",
        ] {
            assert!(forwarded(fields).is_err(), "{fields:?}");
        }
    }
    #[test]
    fn sync_keeps_the_later_login() {
        for text in ["", "h", "he", "hel", "hello", "{\"exp\":1}"] {
            assert_eq!(decode(&base64(text.as_bytes())).unwrap(), text.as_bytes());
        }
        assert!(decode("a+b").is_none());
        let claude = |token: &str, at: Option<u64>| {
            let mut oauth = json!({"accessToken": token});
            if let Some(at) = at {
                oauth["expiresAt"] = json!(at);
            }
            json!({"claudeAiOauth": oauth})
        };
        let old = claude("old", Some(1_000));
        assert!(newer("claude", None, &old));
        assert!(!newer("claude", Some(&old), &claude("old", Some(2_000))));
        assert!(newer("claude", Some(&old), &claude("new", Some(2_000))));
        assert!(!newer("claude", Some(&old), &claude("new", Some(500))));
        assert!(!newer("claude", Some(&old), &claude("new", None)));
        assert!(newer(
            "claude",
            Some(&claude("old", None)),
            &claude("new", Some(1_000))
        ));
        let codex = |token: &str, exp: u64| {
            let jwt = format!(
                "e30.{}.sig",
                base64(format!("{{\"exp\":{exp}}}").as_bytes())
            );
            json!({"tokens": {"access_token": format!("{jwt}{token}")}})
        };
        assert_eq!(expiry("codex", &codex("", 7)), Some(7_000));
        assert!(newer("codex", Some(&codex("a", 7)), &codex("b", 8)));
        assert!(!newer("codex", Some(&codex("a", 8)), &codex("b", 7)));
        assert_eq!(
            expiry("codex", &json!({"tokens": {"access_token": "opaque"}})),
            None
        );
    }
}
