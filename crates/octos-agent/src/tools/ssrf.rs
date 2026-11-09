//! Shared SSRF (Server-Side Request Forgery) protection.
//!
//! A thin agent-facing adapter over `octos_research::net` — the one SSRF
//! implementation in the workspace (host/IP classification, fail-closed DNS
//! validation, per-hop pinned fetching). Used by the `browser` and
//! `site_crawl` tools and the MCP remote dispatcher; `web_fetch` takes its
//! fleet allowlist gate ([`check_host_allowlist`]) from here and its URL
//! safety directly from `octos_research::net`.

use std::net::{IpAddr, SocketAddr};

/// Result of a successful SSRF check: the URL is safe, and we optionally have
/// the resolved addresses for DNS pinning (prevents DNS rebinding / TOCTOU).
#[derive(Debug)]
pub(crate) struct SsrfCheckResult {
    /// Resolved socket addresses — empty ONLY when the host was a literal
    /// IP (already validated, nothing to pin). A DNS-resolved host always
    /// carries at least one pinned address: an empty DNS answer fails
    /// closed in `octos_research::net` (`validate_answer_set`) instead of
    /// skipping the pin.
    pub resolved_addrs: Vec<SocketAddr>,
}

/// Validate a URL against SSRF protections: checks scheme, hostname, and DNS resolution.
/// Returns `Ok(SsrfCheckResult)` if the URL is safe, `Err(error_message)` if blocked.
///
/// A thin adapter over [`octos_research::net::check_url`], keeping this
/// module's agent-facing contract: the legacy error messages the tools
/// surface (and their tests pin), and an empty pin set for literal-IP
/// hosts. Fails closed: DNS lookup failures are treated as blocked (prevents
/// bypass by causing DNS resolution to fail at check time but succeed at
/// fetch time).
pub(crate) async fn check_ssrf_with_addrs(url: &str) -> Result<SsrfCheckResult, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "Invalid URL".to_string())?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    let (checked_host, resolved_addrs) = octos_research::net::check_url(url)
        .await
        .map_err(|e| map_check_url_error(&e, host))?;

    // Literal IPs connect directly — nothing to pin. A DNS-resolved host
    // always pins: `check_url` fails closed on an empty answer set.
    let resolved_addrs = if checked_host.parse::<IpAddr>().is_ok() {
        Vec::new()
    } else {
        resolved_addrs
    };
    Ok(SsrfCheckResult { resolved_addrs })
}

/// Map [`octos_research::net::check_url`]'s errors onto this module's
/// agent-facing messages — the strings the tools surface to the model and
/// their tests pin. `check_url` has a closed error vocabulary, so the
/// mapping is exact-match with a passthrough for anything unrecognized
/// (e.g. its non-http(s) scheme refusal, which this module never had).
fn map_check_url_error(err: &str, host: &str) -> String {
    const PRIVATE: &str = "Requests to private/internal hosts are not allowed";
    if let Some(e) = err.strip_prefix("blocked: DNS resolution failed (fail closed): ") {
        // Fail closed: if DNS fails, block the request. An attacker could
        // trigger DNS failure at check time, then succeed at fetch time
        // (DNS rebinding variant).
        return format!(
            "DNS resolution failed for host '{host}' — blocking request (fail closed): {e}"
        );
    }
    if err == "blocked: DNS returned no addresses (fail closed)" {
        return format!(
            "DNS resolution returned no addresses for host '{host}' — blocking request (fail closed)"
        );
    }
    if err == "blocked: private/internal host" || err == "blocked: private/internal address" {
        return PRIVATE.to_string();
    }
    if err == "blocked: host resolves to a private/internal address" {
        return format!("{PRIVATE} (DNS resolved to private IP)");
    }
    err.to_string()
}

/// Validate a URL against SSRF protections: checks scheme, hostname, and DNS resolution.
/// Returns `Some(error_message)` if the URL should be blocked, `None` if it's safe.
///
/// This is the simple API for callers that don't need resolved addresses (e.g.
/// browser/crawl tools where a separate process handles the actual connection).
pub(crate) async fn check_ssrf(url: &str) -> Option<String> {
    check_ssrf_with_addrs(url).await.err()
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

// IP/host classification is shared with the research tools (deep-search,
// deep-crawl): one implementation in `octos_research::net`.
pub use octos_research::net::{is_private_host, is_private_ip};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_check_ssrf_blocks_ipv4_mapped_ipv6_private() {
        let result = check_ssrf("http://[::ffff:192.168.1.1]/internal").await;
        assert!(
            result.is_some(),
            "IPv4-mapped IPv6 private should be blocked"
        );
    }
}
