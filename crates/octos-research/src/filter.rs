//! Result controls: language, `since`, domain allow/deny lists and the
//! per-domain cap, applied to provider hits and again to read pages.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::date::Since;
use crate::item::{SearchHit, SkippedUrl};
use crate::{lang, urls};

/// A value given either as one string or as a list (`"en"` or
/// `["en", "zh"]`), for tool inputs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    /// Non-empty, trimmed values; a single string may also be
    /// comma-separated (`"en,zh"`).
    pub fn into_vec(self) -> Vec<String> {
        let raw = match self {
            OneOrMany::None => Vec::new(),
            OneOrMany::One(s) => s.split(',').map(str::to_string).collect(),
            OneOrMany::Many(v) => v,
        };
        raw.into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// Parsed controls shared by the research tools.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    /// Normalized BCP-47 tags; empty = any language.
    pub langs: Vec<String>,
    pub since: Option<Since>,
    pub domains_allow: Vec<String>,
    pub domains_deny: Vec<String>,
    pub max_per_domain: Option<usize>,
}

impl Filters {
    /// Build from raw inputs. Invalid language tags are an error so the
    /// caller learns about a typo instead of silently getting everything.
    pub fn new(
        langs: Vec<String>,
        since: Option<Since>,
        domains_allow: Vec<String>,
        domains_deny: Vec<String>,
        max_per_domain: Option<usize>,
    ) -> Result<Self, String> {
        let mut norm = Vec::new();
        for l in langs {
            let n = lang::normalize(&l).ok_or_else(|| {
                format!("invalid language tag {l:?} (use BCP-47, e.g. en, zh-CN)")
            })?;
            if !norm.contains(&n) {
                norm.push(n);
            }
        }
        Ok(Self {
            langs: norm,
            since,
            domains_allow,
            domains_deny,
            max_per_domain: max_per_domain.filter(|n| *n > 0),
        })
    }

    /// Allow/deny check on a URL's host. `Err(reason)` when excluded.
    pub fn check_domain(&self, url: &str) -> Result<(), &'static str> {
        let Some(host) = urls::domain_of(url) else {
            return Err("invalid_url");
        };
        if self
            .domains_deny
            .iter()
            .any(|p| urls::domain_matches(&host, p))
        {
            return Err("domain_deny");
        }
        if !self.domains_allow.is_empty()
            && !self
                .domains_allow
                .iter()
                .any(|p| urls::domain_matches(&host, p))
        {
            return Err("domain_allow");
        }
        Ok(())
    }

    /// Language check; unknown language passes.
    pub fn admits_lang(&self, item_lang: Option<&str>) -> bool {
        match item_lang {
            Some(l) if !l.trim().is_empty() => lang::matches_any(l, &self.langs),
            _ => true,
        }
    }

    /// `since` check; unknown date passes.
    pub fn admits_published(&self, published: Option<&str>) -> bool {
        self.since.as_ref().is_none_or(|s| s.admits(published))
    }

    /// Everything except the per-domain cap, for one hit or page.
    pub fn check(
        &self,
        url: &str,
        item_lang: Option<&str>,
        published: Option<&str>,
    ) -> Result<(), &'static str> {
        self.check_domain(url)?;
        if !self.admits_lang(item_lang) {
            return Err("lang");
        }
        if !self.admits_published(published) {
            return Err("older_than_since");
        }
        Ok(())
    }

    /// Dedupe (canonical URL), filter and cap provider hits, preserving
    /// order. Returns the kept hits and the skipped URLs with reasons.
    pub fn apply(&self, hits: Vec<SearchHit>) -> (Vec<SearchHit>, Vec<SkippedUrl>) {
        let mut seen = HashSet::new();
        let mut cap = DomainCap::new(self.max_per_domain);
        let mut kept = Vec::new();
        let mut skipped = Vec::new();
        for hit in hits {
            if !seen.insert(urls::dedup_key(&hit.url)) {
                continue;
            }
            if let Err(reason) = self.check(
                hit.domain_url(),
                hit.lang.as_deref(),
                hit.published.as_deref(),
            ) {
                skipped.push(SkippedUrl::new(hit.url, reason.to_string()));
                continue;
            }
            if !cap.admit(hit.domain_url()) {
                skipped.push(SkippedUrl::new(hit.url, "per_domain_cap".to_string()));
                continue;
            }
            kept.push(hit);
        }
        (kept, skipped)
    }

    /// Controls as JSON, echoed in the items document.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "lang": self.langs,
            "since": self.since.as_ref().map(|s| s.cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            "domains_allow": self.domains_allow,
            "domains_deny": self.domains_deny,
            "max_per_domain": self.max_per_domain,
        })
    }
}

/// Per-domain counter enforcing `max_per_domain`.
#[derive(Debug, Default)]
pub struct DomainCap {
    max: Option<usize>,
    counts: HashMap<String, usize>,
}

impl DomainCap {
    pub fn new(max: Option<usize>) -> Self {
        Self {
            max,
            counts: HashMap::new(),
        }
    }

    /// Count `url` against its domain; `false` once the domain is full.
    pub fn admit(&mut self, url: &str) -> bool {
        let Some(max) = self.max else {
            return true;
        };
        let domain = urls::domain_of(url).unwrap_or_default();
        let n = self.counts.entry(domain).or_insert(0);
        if *n >= max {
            return false;
        }
        *n += 1;
        true
    }
}

/// Interleave hits round-robin across a key (e.g. language) so a page
/// budget cut does not drop a whole language.
pub fn interleave_by<K: Eq + std::hash::Hash + Clone>(
    hits: Vec<SearchHit>,
    key: impl Fn(&SearchHit) -> K,
) -> Vec<SearchHit> {
    let mut order: Vec<K> = Vec::new();
    let mut buckets: HashMap<K, std::collections::VecDeque<SearchHit>> = HashMap::new();
    for h in hits {
        let k = key(&h);
        if !buckets.contains_key(&k) {
            order.push(k.clone());
        }
        buckets.entry(k).or_default().push_back(h);
    }
    let mut out = Vec::new();
    loop {
        let mut progressed = false;
        for k in &order {
            if let Some(h) = buckets.get_mut(k).and_then(|b| b.pop_front()) {
                out.push(h);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    out
}
