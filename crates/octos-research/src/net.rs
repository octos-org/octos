//! Network safety for the research tools: the one SSRF implementation the
//! research readers share (deep-search skill, built-in `search`, deep-crawl
//! browser interception).
//!
//! - [`is_private_ip`] / [`is_private_host`]: loopback, RFC 1918, link-local
//!   (169.254/16 incl. cloud metadata), CGNAT, benchmarking, multicast,
//!   reserved, ULA, site-local, IPv4-mapped/compatible IPv6.
//! - [`check_url`]: http(s) only, host not private, DNS resolved **fail
//!   closed**, every answer public; returns the validated addresses so the
//!   caller can pin them (no rebinding between check and connect).
//! - [`safe_get`]: GET with that check and DNS pinning on **every** redirect
//!   hop, redirects followed manually, identifiable User-Agent.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use url::{Host, Url};

/// Max redirects [`safe_get`] follows.
pub const MAX_REDIRECTS: usize = 10;

/// SSRF-relevant IPv4 ranges that `Ipv4Addr::is_private()` /
/// `is_link_local()` do not cover (their std predicates are nightly-only).
fn is_special_purpose_v4(v4: &std::net::Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    // 0.0.0.0/8 "this network"
    a == 0
        // CGNAT 100.64.0.0/10 (RFC 6598)
        || (a == 100 && (64..=127).contains(&b))
        // IETF protocol assignments 192.0.0.0/24 (RFC 6890)
        || v4.octets()[..3] == [192, 0, 0]
        // Benchmarking 198.18.0.0/15 (RFC 2544)
        || (a == 198 && (b == 18 || b == 19))
        // Multicast 224/4, reserved 240/4, broadcast
        || a >= 224
}

/// Whether an address is private/internal (not public internet).
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || is_special_purpose_v4(v4)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || matches!(v6.segments()[0], 0xfc00..=0xfdff)
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || (v6.segments()[0] & 0xffc0) == 0xfec0
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_private_ip(&IpAddr::V4(v4)))
                || v6
                    .to_ipv4()
                    .is_some_and(|v4| is_private_ip(&IpAddr::V4(v4)))
        }
    }
}

/// Whether a host string is private without DNS: `localhost` or a private
/// IP literal (bracketed IPv6 accepted).
pub fn is_private_host(host: &str) -> bool {
    let lower = host.trim_end_matches('.').to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return true;
    }
    let bare = lower.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<IpAddr>().is_ok_and(|ip| is_private_ip(&ip))
}

/// Validate a URL for an outbound request: http(s), public host, DNS
/// resolved fail-closed with every answer public. Returns the host (for
/// pinning) and the validated socket addresses.
pub async fn check_url(raw: &str) -> Result<(String, Vec<SocketAddr>), String> {
    let u = Url::parse(raw).map_err(|_| "invalid URL".to_string())?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!("blocked: scheme {} is not http(s)", u.scheme()));
    }
    let port = u.port_or_known_default().unwrap_or(443);
    match u.host() {
        None => Err("URL has no host".to_string()),
        Some(Host::Ipv4(ip)) => check_literal(IpAddr::V4(ip), port),
        Some(Host::Ipv6(ip)) => check_literal(IpAddr::V6(ip), port),
        Some(Host::Domain(d)) => {
            if is_private_host(d) {
                return Err("blocked: private/internal host".to_string());
            }
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((d, port))
                .await
                .map_err(|e| format!("blocked: DNS resolution failed (fail closed): {e}"))?
                .collect();
            if addrs.is_empty() {
                return Err("blocked: DNS returned no addresses (fail closed)".to_string());
            }
            if addrs.iter().any(|a| is_private_ip(&a.ip())) {
                return Err("blocked: host resolves to a private/internal address".to_string());
            }
            Ok((d.to_string(), addrs))
        }
    }
}

fn check_literal(ip: IpAddr, port: u16) -> Result<(String, Vec<SocketAddr>), String> {
    if is_private_ip(&ip) {
        return Err("blocked: private/internal address".to_string());
    }
    Ok((ip.to_string(), vec![SocketAddr::new(ip, port)]))
}

/// GET `url`, re-validating and DNS-pinning every redirect hop.
pub async fn safe_get(url: &str, timeout: Duration) -> Result<reqwest::Response, String> {
    let mut current = url.to_string();
    for _ in 0..MAX_REDIRECTS {
        let (host, addrs) = check_url(&current).await?;
        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(crate::USER_AGENT)
            .redirect(reqwest::redirect::Policy::none());
        // Pin all validated addresses at once (a looped `resolve()` would
        // keep only the last one). IP literals need no pinning.
        if host.parse::<IpAddr>().is_err() {
            builder = builder.resolve_to_addrs(&host, &addrs);
        }
        let client = builder
            .build()
            .map_err(|e| format!("HTTP client error: {e}"))?;
        let response = client
            .get(&current)
            .send()
            .await
            .map_err(|e| format!("fetch failed: {e}"))?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| "redirect with no Location header".to_string())?;
        current = Url::parse(&current)
            .and_then(|b| b.join(location))
            .map_err(|_| format!("invalid redirect URL: {location}"))?
            .to_string();
    }
    Err(format!("too many redirects (max {MAX_REDIRECTS})"))
}

/// Read a response body up to `cap` bytes (lossy UTF-8).
pub async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<String, String> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = cap.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if buf.len() >= cap {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("read body failed: {e}")),
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_classify_private_and_metadata_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private_ip(&ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "93.184.216.34", "2606:4700::1111"] {
            assert!(!is_private_ip(&ip.parse().unwrap()), "{ip}");
        }
        assert!(is_private_host("localhost"));
        assert!(is_private_host("LOCALHOST."));
        assert!(is_private_host("foo.localhost"));
        assert!(is_private_host("[::1]"));
        assert!(!is_private_host("example.com"));
    }

    #[tokio::test]
    async fn should_block_private_urls_without_network() {
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:8080/",
            "http://[::1]/",
            "http://[::ffff:169.254.169.254]/",
            "http://localhost/",
            "http://10.0.0.5/admin",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            let err = check_url(url).await.unwrap_err();
            assert!(
                err.contains("blocked") || err.contains("invalid"),
                "{url}: {err}"
            );
        }
        let (host, addrs) = check_url("http://93.184.216.34/").await.unwrap();
        assert_eq!(host, "93.184.216.34");
        assert_eq!(addrs.len(), 1);
    }
}
