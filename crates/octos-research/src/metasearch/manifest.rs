//! Engine manifests: what an engine is, which hosts it may reach, how fast,
//! and which documentation it was written from.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::item::ItemKind;

/// Categories an engine can serve. A search asks for one category; every
/// enabled engine that lists it runs.
pub const CATEGORIES: &[&str] = &["general", "news", "science", "it", "social"];

/// `manifest.json` of one engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineManifest {
    /// Stable id (`[a-z0-9_]+`), also the directory name.
    pub id: String,
    pub name: String,
    pub categories: Vec<String>,
    /// BCP-47 primary subtags the engine can search in; `["*"]` for any.
    #[serde(default = "any_language")]
    pub languages: Vec<String>,
    /// Whether one request can cover several languages (e.g. GDELT's
    /// `sourcelang:` OR-group). Otherwise the core calls once per language.
    #[serde(default)]
    pub multi_language: bool,
    /// Hosts the engine may reach (exact names, no wildcards). A setting with
    /// `host: true` adds its configured value to this list at run time.
    pub hosts: Vec<String>,
    /// Engine needs a host-held API key; it is skipped when none is set.
    #[serde(default)]
    pub needs_key: bool,
    /// How the host attaches a key. The script never sees it. With
    /// `needs_key: false` the key is optional (e.g. a GitHub token raises
    /// the rate limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<KeyAuth>,
    /// Environment variable a host may read the key from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    pub rate_limit: RateLimit,
    /// Provider documentation the engine was written from.
    pub docs_url: Vec<String>,
    /// Terms, attribution or licence notes for the data.
    pub license_note: String,
    /// What the engine's results are: `article` (default) or `post`
    /// (social posts). An item may override it with its own `kind`.
    #[serde(default, skip_serializing_if = "ItemKind::is_article")]
    pub kind: ItemKind,
    /// Relative weight in ranking (default 1.0).
    #[serde(default = "one")]
    pub weight: f64,
    /// Seconds a successful response may be reused without asking again.
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_secs: u64,
    /// Requests one `build_request` may return (e.g. one per feed); 1..=16.
    #[serde(default = "one_request")]
    pub max_requests: usize,
    /// Per-request timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// The host publishes robots.txt rules for this path. They are checked
    /// only when the operator turns robots checks on
    /// ([`crate::RESPECT_ROBOTS_ENV`]); off by default.
    #[serde(default)]
    pub robots: bool,
    /// Whether `http://` URLs are allowed (default: https only).
    #[serde(default)]
    pub allow_http: bool,
    /// Host-configurable settings passed to the script as `opts.settings`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, Setting>,
    /// Off unless the host enables it.
    #[serde(default)]
    pub disabled_by_default: bool,
    /// The engine lists entries it did not search for (feeds): the core
    /// keeps only hits whose title or snippet match the query (the phrase,
    /// or every significant term; see [`super::topic`]) and reports the rest
    /// as skipped (`query_mismatch`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub query_match: bool,
}

/// Where the host puts a key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum KeyAuth {
    /// Request header, e.g. `X-Subscription-Token`.
    Header { name: String },
    /// Header with a prefix, e.g. `Authorization: Bearer <key>`.
    Bearer,
    /// Query parameter, e.g. `key`.
    Query { name: String },
}

/// Published rate limit, enforced per host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Minimum spacing between requests to one host.
    pub min_interval_ms: u64,
    /// Where the provider states the limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One host-configurable setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// The value is a host name the engine may reach.
    #[serde(default)]
    pub host: bool,
}

fn any_language() -> Vec<String> {
    vec!["*".to_string()]
}
fn one() -> f64 {
    1.0
}
fn default_cache_ttl() -> u64 {
    300
}
fn one_request() -> usize {
    1
}
fn default_timeout() -> u64 {
    10
}

impl EngineManifest {
    pub fn parse(json: &str) -> Result<Self, String> {
        let m: Self = serde_json::from_str(json).map_err(|e| format!("manifest.json: {e}"))?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<(), String> {
        let id_ok = !self.id.is_empty()
            && self.id.len() <= 48
            && self
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !id_ok {
            return Err(format!("invalid engine id {:?}", self.id));
        }
        if self.categories.is_empty() {
            return Err(format!("{}: no categories", self.id));
        }
        if let Some(c) = self
            .categories
            .iter()
            .find(|c| !CATEGORIES.contains(&c.as_str()))
        {
            return Err(format!("{}: unknown category {c:?}", self.id));
        }
        let host_setting = self.settings.values().any(|s| s.host);
        if self.hosts.is_empty() && !host_setting {
            return Err(format!("{}: no hosts declared", self.id));
        }
        if let Some(h) = self.hosts.iter().find(|h| !is_host_name(h)) {
            return Err(format!("{}: invalid host {h:?}", self.id));
        }
        if self.docs_url.is_empty() || self.docs_url.iter().any(|u| !u.starts_with("https://")) {
            return Err(format!("{}: docs_url must list https URLs", self.id));
        }
        if self.needs_key && self.auth.is_none() {
            return Err(format!("{}: needs_key requires auth", self.id));
        }
        if self.rate_limit.min_interval_ms == 0 {
            return Err(format!(
                "{}: rate_limit.min_interval_ms must be > 0",
                self.id
            ));
        }
        if !(self.weight.is_finite() && self.weight > 0.0 && self.weight <= 10.0) {
            return Err(format!("{}: weight must be in (0, 10]", self.id));
        }
        if self.max_requests == 0 || self.max_requests > super::sandbox::MAX_REQUESTS {
            return Err(format!("{}: max_requests must be 1..=16", self.id));
        }
        if self.timeout_secs == 0 || self.timeout_secs > 60 {
            return Err(format!("{}: timeout_secs must be 1..=60", self.id));
        }
        Ok(())
    }

    pub fn min_interval(&self) -> Duration {
        Duration::from_millis(self.rate_limit.min_interval_ms)
    }

    pub fn serves(&self, category: &str) -> bool {
        self.categories.iter().any(|c| c == category)
    }

    /// Whether the engine can search in `lang` (a BCP-47 tag).
    pub fn supports_lang(&self, lang: &str) -> bool {
        let p = crate::lang::primary(lang);
        self.languages.iter().any(|l| l == "*" || *l == p)
    }
}

/// A public DNS host name an engine may reach: no scheme, port, path or
/// wildcard, not an IP literal, not `localhost`, and not a name that
/// `net::is_private_host` classifies as internal. (Each request is also
/// checked after DNS resolution, so a name that resolves to a private
/// address is refused at fetch time.)
pub fn is_host_name(h: &str) -> bool {
    let shaped = !h.is_empty()
        && h.len() <= 253
        && h.contains('.')
        && !h.starts_with('.')
        && !h.ends_with('.')
        && h.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
    let ip_literal = h.parse::<std::net::IpAddr>().is_ok()
        // Dotted numbers that are not a valid address still look like one.
        || h.split('.').all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()));
    let internal_suffix = [".local", ".internal", ".localdomain", ".home.arpa", ".lan"]
        .iter()
        .any(|s| h.ends_with(s));
    shaped && !ip_literal && !internal_suffix && !crate::net::is_private_host(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> serde_json::Value {
        serde_json::json!({
            "id": "demo",
            "name": "Demo",
            "categories": ["news"],
            "hosts": ["api.example.org"],
            "rate_limit": {"min_interval_ms": 1000},
            "docs_url": ["https://example.org/docs"],
            "license_note": "test"
        })
    }

    #[test]
    fn should_parse_minimal_manifest_with_defaults() {
        let m = EngineManifest::parse(&base().to_string()).unwrap();
        assert_eq!(m.languages, vec!["*"]);
        assert_eq!(m.weight, 1.0);
        assert!(m.supports_lang("zh-CN"));
        assert!(m.serves("news") && !m.serves("general"));
    }

    #[test]
    fn should_reject_bad_manifests() {
        let cases: Vec<(&str, serde_json::Value)> = vec![
            ("id", serde_json::json!("Bad-Id")),
            ("categories", serde_json::json!(["shopping"])),
            ("hosts", serde_json::json!(["*.example.org"])),
            ("hosts", serde_json::json!(["https://example.org"])),
            ("hosts", serde_json::json!(["169.254.169.254"])),
            ("hosts", serde_json::json!(["127.0.0.1"])),
            ("hosts", serde_json::json!(["10.0.0.8"])),
            ("hosts", serde_json::json!(["999.1.1.1"])),
            ("hosts", serde_json::json!(["metadata.google.internal"])),
            ("hosts", serde_json::json!(["printer.local"])),
            ("hosts", serde_json::json!(["dev.localhost"])),
            ("docs_url", serde_json::json!(["http://example.org"])),
            ("needs_key", serde_json::json!(true)),
            ("rate_limit", serde_json::json!({"min_interval_ms": 0})),
            ("unknown_field", serde_json::json!(1)),
        ];
        for (k, v) in cases {
            let mut m = base();
            m[k] = v;
            assert!(
                EngineManifest::parse(&m.to_string()).is_err(),
                "{k} accepted"
            );
        }
    }
}
