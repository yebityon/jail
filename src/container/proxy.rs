//! CONNECT proxy for allowlist mode. DNS resolution occurs in the trusted process;
//! connections use the resolved address directly to avoid a second DNS lookup.
use crate::config::Network;
use anyhow::{bail, Context, Result};
use std::{
    io::{Read, Write},
    net::{IpAddr, Shutdown, TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

/// Loopback port of the proxy inside the container.
pub const PORT: u16 = 3128;
/// Proxy URL for the agent's HTTP(S)_PROXY variables.
pub fn url() -> String {
    format!("http://127.0.0.1:{PORT}")
}

pub fn host_allowed(host: &str, rules: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    if host.is_empty()
        || host.ends_with('.')
        || !host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
    {
        return false;
    }
    rules.iter().any(|rule| {
        let rule = rule.to_ascii_lowercase();
        if let Some(suffix) = rule.strip_prefix("*.") {
            host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}"))
        } else {
            host == rule
        }
    })
}

pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_multicast()
                && !ip.is_unspecified()
                && !ip.is_broadcast()
                && !ip.is_documentation()
                && a != 0
                && a < 224
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 198 && (b == 18 || b == 19))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            let s = ip.segments();
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || s[0] & 0xfe00 == 0xfc00
                || s[0] & 0xffc0 == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0xdb8))
                && s[0] & 0xe000 == 0x2000
        }
    }
}

/// One of a bounded number of concurrent connections. Dropping it frees the
/// slot, also when a connection's thread panics.
pub(crate) struct Slot(Arc<AtomicUsize>);
impl Slot {
    pub(crate) fn take(active: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        if active.fetch_add(1, Ordering::SeqCst) >= limit {
            active.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(active.clone()))
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a request head asks the proxy for.
#[derive(Debug, PartialEq)]
pub(crate) enum Request<'a> {
    Connect {
        host: &'a str,
        port: u16,
    },
    /// A response status for anything else.
    Reject(u16),
}
pub(crate) fn parse_request(head: &[u8]) -> Result<Request<'_>> {
    let request = std::str::from_utf8(head).context("invalid request")?;
    let mut parts = request.lines().next().unwrap_or("").split_whitespace();
    if parts.next() != Some("CONNECT") {
        return Ok(Request::Reject(405));
    }
    let authority = parts.next().unwrap_or("");
    let Some((host, port)) = authority.rsplit_once(':') else {
        return Ok(Request::Reject(400));
    };
    // An invalid port becomes 0, which the policy denies and records.
    let port = port.parse::<u16>().unwrap_or(0);
    Ok(Request::Connect { host, port })
}
fn handle(mut client: TcpStream, _original: &Network) -> Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(15)))?;
    client.set_write_timeout(Some(Duration::from_secs(15)))?;
    let mut header = Vec::new();
    // Read exactly through CRLFCRLF; never consume TLS bytes following CONNECT.
    while !header.ends_with(b"\r\n\r\n") && header.len() < 16_384 {
        let mut byte = [0];
        if client.read(&mut byte)? == 0 {
            bail!("incomplete request");
        }
        header.push(byte[0]);
    }
    let (host, port) = match parse_request(&header)? {
        Request::Connect { host, port } => (host, port),
        Request::Reject(code) => {
            reject(&mut client, code)?;
            return Ok(());
        }
    };
    let policy = crate::network_policy::load()?;
    if !crate::network_policy::permits(&policy, host, port) {
        crate::network_policy::deny(host, port, "allowlist")?;
        reject(&mut client, 403)?;
        return Ok(());
    }
    let addresses: Vec<_> = (host, port).to_socket_addrs()?.collect();
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|a| !policy.allow_private_ips && !public_ip(a.ip()))
    {
        crate::network_policy::deny(host, port, "private_ip")?;
        reject(&mut client, 403)?;
        return Ok(());
    }
    let mut upstream = None;
    for address in addresses {
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_secs(10)) {
            upstream = Some(stream);
            break;
        }
    }
    let mut upstream = upstream.context("upstream connection failed")?;
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    client.set_read_timeout(None)?;
    client.set_write_timeout(None)?;
    let mut upload = client.try_clone()?;
    let mut to_upstream = upstream.try_clone()?;
    let task = thread::spawn(move || {
        let _ = std::io::copy(&mut upload, &mut to_upstream);
        let _ = to_upstream.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut upstream, &mut client);
    let _ = client.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    let _ = task.join();
    Ok(())
}
fn reject(client: &mut TcpStream, code: u16) -> Result<()> {
    write!(
        client,
        "HTTP/1.1 {code} Denied\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    Ok(())
}
pub fn serve(listener: TcpListener, policy: Network) -> Result<()> {
    let policy = Arc::new(policy);
    let active = Arc::new(AtomicUsize::new(0));
    for client in listener.incoming() {
        let client = client?;
        let Some(slot) = Slot::take(&active, 128) else {
            drop(client);
            continue;
        };
        let policy = policy.clone();
        thread::spawn(move || {
            let _slot = slot;
            // Never log headers, credentials, URLs, or payloads.
            let _ = handle(client, &policy);
        });
    }
    bail!("proxy listener stopped")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hostname_allowlist_cannot_be_suffix_spoofed() {
        let rules = vec!["*.anthropic.com".into(), "chatgpt.com".into()];
        assert!(host_allowed("api.anthropic.com", &rules));
        for h in [
            "evil-anthropic.com",
            "anthropic.com.evil.test",
            "chatgpt.com.evil.test",
            "chatgpt.com.",
            "user@chatgpt.com",
            "anthropic.com",
        ] {
            assert!(!host_allowed(h, &rules), "{h}");
        }
    }
    #[test]
    fn only_connect_with_a_port_is_forwarded() {
        fn parse(head: &[u8]) -> Request<'_> {
            parse_request(head).unwrap()
        }
        assert_eq!(
            parse(b"CONNECT api.anthropic.com:443 HTTP/1.1\r\n\r\n"),
            Request::Connect {
                host: "api.anthropic.com",
                port: 443
            }
        );
        assert_eq!(
            parse(b"GET http://evil.test/ HTTP/1.1\r\n\r\n"),
            Request::Reject(405)
        );
        assert_eq!(
            parse(b"CONNECT evil.test HTTP/1.1\r\n\r\n"),
            Request::Reject(400)
        );
        assert_eq!(
            parse(b"CONNECT evil.test:99999 HTTP/1.1\r\n\r\n"),
            Request::Connect {
                host: "evil.test",
                port: 0
            }
        );
        assert!(parse_request(b"CONNECT \xff:443 HTTP/1.1\r\n\r\n").is_err());
    }
    #[test]
    fn a_panicking_connection_frees_its_slot() {
        let active = Arc::new(AtomicUsize::new(0));
        let first = Slot::take(&active, 2).unwrap();
        let second = Slot::take(&active, 2).unwrap();
        assert!(Slot::take(&active, 2).is_none());
        drop(first);
        let panicked = thread::spawn(move || {
            let _slot = second;
            panic!("connection failed");
        })
        .join();
        assert!(panicked.is_err());
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn internal_destinations_are_denied() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.64.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.1.2.3",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()));
    }
}
