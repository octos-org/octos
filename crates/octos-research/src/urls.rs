//! URL helpers: canonical form for items/dedup and domain matching.

use url::Url;

/// Query parameters that only track the click and never select content.
fn is_tracking_param(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("utm_")
        || matches!(
            n.as_str(),
            "fbclid"
                | "gclid"
                | "dclid"
                | "msclkid"
                | "mc_cid"
                | "mc_eid"
                | "igshid"
                | "ocid"
                | "cmpid"
                | "ito"
                | "ref_src"
                | "_ga"
                | "yclid"
        )
}

/// Canonical form of a URL: lowercase scheme/host (via `url`), no fragment,
/// no tracking parameters, no trailing slash on non-root paths. Returns the
/// input unchanged if it does not parse.
pub fn canonicalize(raw: &str) -> String {
    let Ok(mut u) = Url::parse(raw.trim()) else {
        return raw.trim().to_string();
    };
    u.set_fragment(None);
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !is_tracking_param(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    let path = u.path().to_string();
    if path.len() > 1 && path.ends_with('/') {
        u.set_path(path.trim_end_matches('/'));
    }
    u.to_string()
}

/// Dedup key: canonical form, lowercased, `www.` ignored, scheme-agnostic.
pub fn dedup_key(raw: &str) -> String {
    let c = canonicalize(raw).to_lowercase();
    let rest = c
        .strip_prefix("https://")
        .or_else(|| c.strip_prefix("http://"))
        .unwrap_or(&c);
    rest.strip_prefix("www.").unwrap_or(rest).to_string()
}

/// Host without a leading `www.`, lowercase.
pub fn domain_of(raw: &str) -> Option<String> {
    let u = Url::parse(raw.trim()).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

/// Whether `host` is `pattern` or a subdomain of it. Patterns may be given
/// as bare domains (`example.com`), with `www.`, a leading dot, or a
/// `*.` wildcard, and are compared case-insensitively.
pub fn domain_matches(host: &str, pattern: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let p = pattern.trim().to_ascii_lowercase();
    let p = p
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .trim_end_matches('.');
    let p = p.strip_prefix("www.").unwrap_or(p);
    if p.is_empty() {
        return false;
    }
    host == p || host.ends_with(&format!(".{p}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_canonicalize_urls() {
        assert_eq!(
            canonicalize("https://Example.com/a/b/?utm_source=x&id=3#frag"),
            "https://example.com/a/b?id=3"
        );
        assert_eq!(
            canonicalize("https://example.com/?fbclid=abc"),
            "https://example.com/"
        );
        assert_eq!(canonicalize("not a url"), "not a url");
        assert_eq!(
            dedup_key("http://www.A.com/x/#f"),
            dedup_key("https://a.com/x?utm_medium=y")
        );
    }

    #[test]
    fn should_match_domains_and_subdomains() {
        assert!(domain_matches("www.reuters.com", "reuters.com"));
        assert!(domain_matches("uk.reuters.com", "*.reuters.com"));
        assert!(!domain_matches("notreuters.com", "reuters.com"));
        assert!(!domain_matches("reuters.com", ""));
        assert_eq!(
            domain_of("https://www.BBC.co.uk/news").as_deref(),
            Some("bbc.co.uk")
        );
    }
}
