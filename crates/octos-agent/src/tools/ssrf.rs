//! Shared SSRF (Server-Side Request Forgery) protection.
//!
//! Provides hostname and IP validation to block requests to private/internal
//! network addresses. Used by both `web_fetch` and `browser` tools.

use std::net::{IpAddr, SocketAddr};

/// Result of a successful SSRF check: the URL is safe, and we optionally have
/// the resolved addresses for DNS pinning (prevents DNS rebinding / TOCTOU).
#[derive(Debug)]
pub(crate) struct SsrfCheckResult {
    /// Resolved socket addresses (empty if host was a literal IP).
    pub resolved_addrs: Vec<SocketAddr>,
}

/// Validate a URL against SSRF protections: checks scheme, hostname, and DNS resolution.
/// Returns `Ok(SsrfCheckResult)` if the URL is safe, `Err(error_message)` if blocked.
///
/// Fails closed: DNS lookup failures are treated as blocked (prevents bypass
/// by causing DNS resolution to fail at check time but succeed at fetch time).
pub(crate) async fn check_ssrf_with_addrs(url: &str) -> Result<SsrfCheckResult, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "Invalid URL".to_string())?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    if is_private_host(host) {
        return Err("Requests to private/internal hosts are not allowed".to_string());
    }

    // Literal IPs were already checked by is_private_host — no DNS needed.
    if host.parse::<IpAddr>().is_ok() {
        return Ok(SsrfCheckResult {
            resolved_addrs: vec![],
        });
    }

    let port = parsed.port_or_known_default().unwrap_or(443);
    match tokio::net::lookup_host(format!("{host}:{port}")).await {
        Ok(addrs) => {
            let mut safe_addrs = Vec::new();
            for addr in addrs {
                if is_private_ip(&addr.ip()) {
                    return Err(
                        "Requests to private/internal hosts are not allowed (DNS resolved to private IP)"
                            .to_string(),
                    );
                }
                safe_addrs.push(addr);
            }
            Ok(SsrfCheckResult {
                resolved_addrs: safe_addrs,
            })
        }
        Err(e) => {
            // Fail closed: if DNS fails, block the request. An attacker could
            // trigger DNS failure at check time, then succeed at fetch time
            // (DNS rebinding variant).
            Err(format!(
                "DNS resolution failed for host '{host}' — blocking request (fail closed): {e}"
            ))
        }
    }
}

/// Validate a URL against SSRF protections: checks scheme, hostname, and DNS resolution.
/// Returns `Some(error_message)` if the URL should be blocked, `None` if it's safe.
///
/// This is the simple API for callers that don't need resolved addresses (e.g.
/// browser/crawl tools where a separate process handles the actual connection).
pub(crate) async fn check_ssrf(url: &str) -> Option<String> {
    check_ssrf_with_addrs(url).await.err()
}

/// Maximum redirects [`ssrf_safe_send`] follows before failing.
pub(crate) const SSRF_MAX_REDIRECTS: usize = 10;

/// Send an HTTP request with SSRF protection re-applied to EVERY hop.
///
/// Default reqwest redirect-following resolves and connects to redirect
/// targets WITHOUT re-running the SSRF check, so an allowed public URL that
/// 30x-redirects to `169.254.169.254` / `10.x` / any private host is
/// followed unchecked. This helper closes that gap: it disables reqwest's
/// automatic redirects and follows them manually, re-validating (and
/// DNS-pinning) each hop.
///
/// For each hop it: validates + DNS-resolves the current URL (fail-closed on
/// DNS error), builds a per-hop client with `redirect(Policy::none())` and
/// `.resolve()` pinned to the validated addresses (defeats the DNS-rebinding
/// TOCTOU between check and connect), then sends. A 3xx response with a
/// `Location` header re-enters the loop against the resolved target; any
/// other status returns the response.
///
/// - `configure` layers caller-specific base client config (timeouts,
///   user-agent) onto the builder; the redirect policy and DNS pins are
///   always applied on top and cannot be overridden.
/// - `build_request` builds the per-hop request (method, body, headers)
///   against the pinned client and current URL. It is invoked once per hop,
///   so a POST body is re-sent on each redirect.
pub(crate) async fn ssrf_safe_send<F, G>(
    initial_url: &str,
    max_redirects: usize,
    configure: F,
    build_request: G,
) -> Result<reqwest::Response, String>
where
    F: Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    G: Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
{
    let mut current_url = initial_url.to_string();

    for _ in 0..max_redirects {
        // Validate + resolve the CURRENT hop (fail-closed on DNS error).
        let check = check_ssrf_with_addrs(&current_url).await?;

        let parsed = reqwest::Url::parse(&current_url).map_err(|_| "Invalid URL".to_string())?;
        let host = parsed
            .host_str()
            .ok_or_else(|| "URL has no host".to_string())?
            .to_string();

        // Per-hop client: caller config, redirects OFF, DNS pinned.
        let mut builder =
            configure(reqwest::Client::builder()).redirect(reqwest::redirect::Policy::none());
        // Pin ALL validated addresses in a SINGLE override. `resolve()` called
        // in a loop REPLACES the entry each time (reqwest keys `dns_overrides`
        // by host), so a loop would pin only the LAST address — and if that one
        // is unreachable (e.g. an IPv6 answer on an IPv4-only host) the fetch
        // fails even though another validated address would have worked.
        // `resolve_to_addrs` installs the whole list at once. (Empty for a
        // literal-IP host — leave reqwest's own resolution in place then.)
        if !check.resolved_addrs.is_empty() {
            builder = builder.resolve_to_addrs(&host, &check.resolved_addrs);
        }
        let client = builder
            .build()
            .map_err(|e| format!("HTTP client error: {e}"))?;

        let response = build_request(&client, &current_url)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        if !response.status().is_redirection() {
            return Ok(response);
        }

        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| "Redirect with no Location header".to_string())?;
        // Resolve relative redirects against the current URL.
        current_url = parsed
            .join(location)
            .map_err(|_| format!("Invalid redirect URL: {location}"))?
            .to_string();
    }

    Err(format!("Too many redirects (max {max_redirects})"))
}

/// Enforce a per-host allowlist (PR A fleet worker grant).
///
/// The allowlist is an `Option`, and the None/Some distinction is
/// SECURITY-LOAD-BEARING (fail closed, not open):
/// - `None` — no per-host restriction (unrestricted; the backward-compatible
///   default for every non-fleet caller, and the `Full` network grant).
/// - `Some(list)` — RESTRICTED to `list`. A NON-EMPTY list admits `host` only
///   when it exactly matches (case-insensitively) a listed host or is a
///   subdomain of one (`docs.example.com` ⊂ `example.com`). An EMPTY (or
///   all-blank) list denies EVERYTHING — "restricted to nothing" reaches
///   nothing, never "unrestricted". So a `Hosts([])` grant that somehow bypassed
///   [`WorkerGrant::validate`] still fails closed here.
///
/// This is layered ON TOP of the private-IP block in [`check_ssrf_with_addrs`],
/// so a fleet worker granted `Hosts([example.com])` can reach only those hosts
/// over HTTP(S) via the web tools and nothing else.
///
/// Subdomain matching uses the label boundary (`.`) so `example.com` does NOT
/// admit `notexample.com` or `example.com.evil.tld`.
///
/// [`WorkerGrant::validate`]: octos_fleet::WorkerGrant::validate
pub(crate) fn check_host_allowlist(host: &str, allowlist: Option<&[String]>) -> Result<(), String> {
    let Some(allowlist) = allowlist else {
        return Ok(()); // unrestricted
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let admitted = allowlist.iter().any(|allowed| {
        let allowed = allowed.trim().trim_end_matches('.').to_ascii_lowercase();
        !allowed.is_empty() && (host == allowed || host.ends_with(&format!(".{allowed}")))
    });
    if admitted {
        Ok(())
    } else {
        Err(format!(
            "host `{host}` is not in the granted network allowlist"
        ))
    }
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
/// SSRF-relevant IPv4 ranges that are NOT routable public internet but which
/// `Ipv4Addr::is_private()`/`is_link_local()` do not cover. The std predicates
/// for these (`is_shared`, `is_benchmarking`, `is_reserved`, …) are all
/// nightly-only, so match the octets explicitly.
fn is_special_purpose_v4(v4: &std::net::Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    // Shared address space / CGNAT 100.64.0.0/10 (RFC 6598) — routes to ISP
    // carrier-grade NAT infrastructure.
    (a == 100 && (64..=127).contains(&b))
        // IETF protocol assignments 192.0.0.0/24 (RFC 6890).
        || v4.octets()[..3] == [192, 0, 0]
        // Benchmarking 198.18.0.0/15 (RFC 2544).
        || (a == 198 && (b == 18 || b == 19))
        // Multicast 224.0.0.0/4 and reserved/future 240.0.0.0/4 (RFC 1112),
        // plus the limited-broadcast 255.255.255.255 that 240/4 subsumes.
        || a >= 224
}

pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()                 // 127.0.0.0/8
                || v4.is_private()           // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()        // 169.254/16 (AWS metadata)
                || v4.is_unspecified()       // 0.0.0.0
                || is_special_purpose_v4(v4) // CGNAT/benchmark/reserved/multicast
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
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private_ip(&IpAddr::V4(v4)))
                // IPv4-compatible ::x.x.x.x (deprecated RFC 4291)
                || v6.to_ipv4().is_some_and(|v4| is_private_ip(&IpAddr::V4(v4)))
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
    fn test_private_host_special_purpose_ranges() {
        // CGNAT / shared address space (RFC 6598) — the carrier-grade-NAT
        // range that routes to ISP infrastructure, previously un-blocked.
        assert!(is_private_host("100.64.0.1"), "CGNAT low edge");
        assert!(is_private_host("100.100.100.100"), "CGNAT middle");
        assert!(is_private_host("100.127.255.255"), "CGNAT high edge");
        // Just OUTSIDE the /10 must stay public (100.64/10 boundaries).
        assert!(!is_private_host("100.63.255.255"), "below CGNAT is public");
        assert!(!is_private_host("100.128.0.0"), "above CGNAT is public");
        // IETF protocol assignments (RFC 6890) 192.0.0.0/24.
        assert!(is_private_host("192.0.0.1"));
        assert!(!is_private_host("192.0.1.1"), "192.0.1/24 is public");
        // Benchmarking (RFC 2544) 198.18.0.0/15.
        assert!(is_private_host("198.18.0.1"));
        assert!(is_private_host("198.19.255.255"));
        assert!(
            !is_private_host("198.20.0.0"),
            "above benchmarking is public"
        );
        // Reserved / future use (RFC 1112) 240.0.0.0/4 + limited broadcast.
        assert!(is_private_host("240.0.0.1"));
        assert!(is_private_host("255.255.255.255"));
        // IPv4 multicast 224.0.0.0/4 as a literal host.
        assert!(is_private_host("224.0.0.1"));
        // Mapped/compat forms of CGNAT must be blocked too (defense in depth).
        assert!(is_private_host("::ffff:100.64.0.1"), "mapped CGNAT");
    }

    // --- check_host_allowlist tests (PR A fleet worker grant) ---

    #[test]
    fn test_host_allowlist_none_admits_everything() {
        // `None` = no restriction — the backward-compatible default for every
        // non-fleet caller (and the `Full` network grant).
        assert!(check_host_allowlist("anything.example.com", None).is_ok());
    }

    #[test]
    fn test_host_allowlist_some_empty_denies_everything() {
        // FAIL CLOSED: `Some([])` (restricted to nothing) reaches NOTHING —
        // never "unrestricted". Defense-in-depth for a `Hosts([])` grant that
        // bypassed validation (e.g. an old serde row).
        assert!(check_host_allowlist("example.com", Some(&[])).is_err());
        let blank = ["   ".to_string()];
        assert!(
            check_host_allowlist("example.com", Some(&blank)).is_err(),
            "an all-blank list is empty → deny all"
        );
    }

    #[test]
    fn test_host_allowlist_exact_and_subdomain() {
        let list = vec!["example.com".to_string()];
        assert!(
            check_host_allowlist("example.com", Some(&list)).is_ok(),
            "exact"
        );
        assert!(
            check_host_allowlist("docs.example.com", Some(&list)).is_ok(),
            "subdomain admitted"
        );
        assert!(
            check_host_allowlist("EXAMPLE.COM", Some(&list)).is_ok(),
            "case-insensitive"
        );
        assert!(
            check_host_allowlist("example.com.", Some(&list)).is_ok(),
            "trailing dot normalized"
        );
    }

    #[test]
    fn test_host_allowlist_refuses_others_and_lookalikes() {
        let list = vec!["example.com".to_string()];
        assert!(check_host_allowlist("other.com", Some(&list)).is_err());
        assert!(
            check_host_allowlist("notexample.com", Some(&list)).is_err(),
            "label boundary: notexample.com is NOT a subdomain of example.com"
        );
        assert!(
            check_host_allowlist("example.com.evil.tld", Some(&list)).is_err(),
            "suffix attack rejected"
        );
        let err = check_host_allowlist("other.com", Some(&list)).unwrap_err();
        assert!(
            err.contains("allowlist"),
            "error names the allowlist: {err}"
        );
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

    // --- check_ssrf_with_addrs tests ---

    #[tokio::test]
    async fn test_with_addrs_blocks_private_host() {
        let result = check_ssrf_with_addrs("http://127.0.0.1/secret").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("private"));
    }

    #[tokio::test]
    async fn test_with_addrs_returns_resolved_for_public_ip() {
        // Literal public IP — no DNS needed, resolved_addrs should be empty
        let result = check_ssrf_with_addrs("https://8.8.8.8/").await;
        assert!(result.is_ok());
        assert!(
            result.unwrap().resolved_addrs.is_empty(),
            "literal IP should not trigger DNS, resolved_addrs empty"
        );
    }

    #[tokio::test]
    async fn test_with_addrs_fails_closed_on_nonexistent_domain() {
        // This domain should fail DNS resolution → must be blocked (fail closed)
        let result =
            check_ssrf_with_addrs("https://this-domain-does-not-exist-ssrf-test.invalid/foo").await;
        assert!(
            result.is_err(),
            "DNS failure should block request (fail closed)"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("DNS resolution failed") || err.contains("fail closed"),
            "error message should indicate DNS failure: {err}"
        );
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_nonexistent_domain() {
        // The simple API should also fail closed
        let result = check_ssrf("https://this-domain-does-not-exist-ssrf-test.invalid/foo").await;
        assert!(
            result.is_some(),
            "DNS failure should block via simple API too"
        );
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_ipv4_mapped_ipv6_url() {
        // IPv4-mapped IPv6 pointing to loopback
        let result = check_ssrf("http://[::ffff:127.0.0.1]/secret").await;
        assert!(
            result.is_some(),
            "IPv4-mapped IPv6 loopback should be blocked"
        );
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_ipv4_mapped_ipv6_private() {
        let result = check_ssrf("http://[::ffff:192.168.1.1]/internal").await;
        assert!(
            result.is_some(),
            "IPv4-mapped IPv6 private should be blocked"
        );
    }

    // --- ssrf_safe_send tests ---

    #[tokio::test]
    async fn ssrf_safe_send_blocks_private_initial_url() {
        // Every hop is SSRF-checked, including hop 0. A private initial URL is
        // rejected BEFORE any socket is opened — the error is the SSRF "private"
        // message, not a connection error (which is how this distinguishes a
        // wired-in check from a check that was accidentally dropped from the
        // loop).
        let result = ssrf_safe_send(
            "http://127.0.0.1:9/",
            SSRF_MAX_REDIRECTS,
            |builder| builder,
            |client, url| client.get(url),
        )
        .await;
        assert!(result.is_err(), "private initial URL must be blocked");
        assert!(
            result.unwrap_err().contains("private"),
            "must be blocked by the SSRF check, not a connection error"
        );
    }
}
