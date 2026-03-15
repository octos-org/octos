//! Shared SSRF (Server-Side Request Forgery) protection.
//!
//! Provides hostname and IP validation to block requests to private/internal
//! network addresses. Used by both `web_fetch` and `browser` tools.

use std::net::IpAddr;

/// Validate a URL against SSRF protections: checks scheme, hostname, and DNS resolution.
/// Returns `Some(error_message)` if the URL should be blocked, `None` if it's safe.
pub(crate) async fn check_ssrf(url: &str) -> Option<String> {
    let parsed = match reqwest::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return Some("Invalid URL".to_string()),
    };
    let host = match parsed.host_str() {
        Some(h) => h,
        None => return Some("URL has no host".to_string()),
    };
    if is_private_host(host) {
        return Some("Requests to private/internal hosts are not allowed".to_string());
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    if let Ok(addrs) = tokio::net::lookup_host(format!("{host}:{port}")).await {
        for addr in addrs {
            if is_private_ip(&addr.ip()) {
                return Some(
                    "Requests to private/internal hosts are not allowed (DNS resolved to private IP)"
                        .to_string(),
                );
            }
        }
    }
    None
}

/// Check if a hostname is private/internal (string check + IP parse).
pub fn is_private_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower == "localhost." {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_private_ip(&ip);
    }
    false
}

/// Check if an IP address is in a private/internal range.
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()           // 127.0.0.0/8
                || v4.is_private()     // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()  // 169.254/16 (AWS metadata)
                || v4.is_unspecified() // 0.0.0.0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()           // ::1
                || v6.is_unspecified() // ::
                || v6.is_multicast()   // ff00::/8
                // ULA fc00::/7
                || matches!(v6.segments()[0], 0xfc00..=0xfdff)
                // Link-local fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // Site-local fec0::/10 (deprecated RFC 3879, still routable)
                || (v6.segments()[0] & 0xffc0) == 0xfec0
                // IPv4-mapped ::ffff:x.x.x.x
                || v6.to_ipv4_mapped().is_some_and(|v4| {
                    v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
                })
                // IPv4-compatible ::x.x.x.x (deprecated RFC 4291)
                || v6.to_ipv4().is_some_and(|v4| {
                    v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Async check_ssrf() tests ---

    #[tokio::test]
    async fn test_check_ssrf_blocks_localhost() {
        let result = check_ssrf("http://localhost/secret").await;
        assert!(result.is_some(), "localhost should be blocked");
        assert!(result.unwrap().contains("private"));
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_loopback_ip() {
        let result = check_ssrf("http://127.0.0.1:8080/admin").await;
        assert!(result.is_some(), "127.0.0.1 should be blocked");
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_metadata_endpoint() {
        // AWS metadata endpoint
        let result = check_ssrf("http://169.254.169.254/latest/meta-data/").await;
        assert!(result.is_some(), "AWS metadata IP should be blocked");
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_private_network() {
        let result = check_ssrf("http://10.0.0.1/internal").await;
        assert!(result.is_some(), "10.x.x.x should be blocked");

        let result = check_ssrf("http://192.168.1.1/router").await;
        assert!(result.is_some(), "192.168.x.x should be blocked");
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_invalid_url() {
        let result = check_ssrf("not-a-url").await;
        assert!(result.is_some(), "invalid URL should be blocked");
        assert!(result.unwrap().contains("Invalid URL"));
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_no_host() {
        let result = check_ssrf("file:///etc/passwd").await;
        assert!(result.is_some(), "file:// URL should be blocked (no host)");
    }

    #[tokio::test]
    async fn test_check_ssrf_allows_public_ip() {
        // 8.8.8.8 is Google's public DNS — always resolves to itself
        let result = check_ssrf("https://8.8.8.8/").await;
        assert!(result.is_none(), "public IP 8.8.8.8 should be allowed");
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_ipv6_loopback() {
        let result = check_ssrf("http://[::1]/secret").await;
        assert!(result.is_some(), "IPv6 loopback should be blocked");
    }

    // --- Sync helper tests ---

    #[test]
    fn test_private_host_localhost() {
        assert!(is_private_host("localhost"));
        assert!(is_private_host("LOCALHOST"));
        assert!(is_private_host("localhost."));
    }

    #[test]
    fn test_private_host_ipv4() {
        assert!(is_private_host("127.0.0.1"));
        assert!(is_private_host("10.0.0.1"));
        assert!(is_private_host("172.16.0.1"));
        assert!(is_private_host("192.168.1.1"));
        assert!(is_private_host("169.254.169.254"));
        assert!(is_private_host("0.0.0.0"));
    }

    #[test]
    fn test_private_host_ipv6() {
        assert!(is_private_host("::1"));
        assert!(is_private_host("::"));
        assert!(is_private_host("fc00::1"));
        assert!(is_private_host("fd12:3456::1"));
        assert!(is_private_host("fe80::1"));
        assert!(is_private_host("::ffff:127.0.0.1"));
        assert!(is_private_host("::ffff:192.168.1.1"));
        assert!(is_private_host("ff02::1"));
        assert!(is_private_host("fec0::1"));
        assert!(is_private_host("::192.168.1.1"));
    }

    #[test]
    fn test_public_host_allowed() {
        assert!(!is_private_host("8.8.8.8"));
        assert!(!is_private_host("1.1.1.1"));
        assert!(!is_private_host("example.com"));
        assert!(!is_private_host("2001:4860:4860::8888"));
    }

    #[test]
    fn test_private_ip_check() {
        assert!(is_private_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"192.168.1.1".parse().unwrap()));
        assert!(is_private_ip(&"::1".parse().unwrap()));
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip(&"1.1.1.1".parse().unwrap()));
    }
}
