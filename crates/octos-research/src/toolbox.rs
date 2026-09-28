//! The octos side of the OctoSense system toolbox (ADR 0002 §6).
//!
//! App agents do not search, read or crawl themselves; they are granted
//! toolbox tools that the host runs with the app's **scope** (languages,
//! regions, domains, recency, categories, sizes) and writes as structured
//! items into the app's folder. This module is that host-side engine:
//!
//! - [`Scope`]: an app's grant, parsed from its manifest request.
//! - [`Scope::search_args`] / [`Scope::crawl_args`]: a tool call narrowed to
//!   the grant, or refused with a reason. `deep_research` runs the
//!   `deep-search` skill with the narrowed search arguments; `deep_crawl`
//!   runs the `deep-crawl` skill with the narrowed crawl arguments.
//! - [`Toolbox::search`] and [`Toolbox::web_read`]: run in-library and
//!   write an [`ItemsDocument`] into the app's folder, returning a short
//!   summary and item references for the agent.
//! - [`tool_specs`]: the tools' JSON schemas for the kernel to register for
//!   an app's peer.
//!
//! The same policy as every research path applies: identifiable User-Agent,
//! per-host rates with backoff, SSRF-safe fetching, no search-results
//! scraping, robots.txt only when the operator turns it on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::access::{ReadError, ReadFailure};
use crate::date::Since;
use crate::filter::Filters;
use crate::item::{ItemsDocument, ResearchItem, SummaryKind};
use crate::metasearch::{Metasearch, SearchRequest, manifest::CATEGORIES};
use crate::reader::Reader;
use crate::{lang, urls};

/// An app's research/crawl grant (the `research` and `crawl` capabilities
/// with their scope). Empty lists mean "no restriction".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    /// BCP-47 languages the app may search in.
    #[serde(default)]
    pub langs: Vec<String>,
    /// ISO 3166-1 alpha-2 regions.
    #[serde(default)]
    pub regions: Vec<String>,
    #[serde(default)]
    pub domains_allow: Vec<String>,
    #[serde(default)]
    pub domains_deny: Vec<String>,
    /// Oldest material the app may ask for, in days back from now.
    #[serde(default)]
    pub max_age_days: Option<u32>,
    /// Metasearch categories (`news`, `general`, `science`, `it`, `social`).
    #[serde(default)]
    pub categories: Vec<String>,
    /// Most results per `search` call.
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    /// `deep_crawl` limits (the `crawl` capability); 0 = crawling not granted.
    #[serde(default)]
    pub max_depth: u32,
    #[serde(default)]
    pub max_pages: u32,
}

fn default_max_results() -> usize {
    20
}

/// A tool call narrowed to the app's scope.
#[derive(Debug, Clone)]
pub struct ScopedSearch {
    pub query: String,
    pub query_by_lang: BTreeMap<String, String>,
    pub langs: Vec<String>,
    pub region: Option<String>,
    pub since: Option<Since>,
    pub category: String,
    pub count: usize,
    pub filters: Filters,
    /// What was narrowed, for the agent (e.g. "since clamped to 30 days").
    pub notes: Vec<String>,
}

impl Scope {
    /// Parse and validate a grant.
    pub fn from_grant(grant: &Value) -> Result<Self, String> {
        let mut s: Scope =
            serde_json::from_value(grant.clone()).map_err(|e| format!("scope: {e}"))?;
        let mut langs = Vec::new();
        for l in &s.langs {
            langs.push(lang::normalize(l).ok_or_else(|| format!("scope: bad language {l:?}"))?);
        }
        s.langs = langs;
        s.regions = s
            .regions
            .iter()
            .map(|r| r.trim().to_ascii_uppercase())
            .collect();
        if let Some(r) = s.regions.iter().find(|r| r.len() != 2) {
            return Err(format!("scope: bad region {r:?}"));
        }
        if let Some(c) = s
            .categories
            .iter()
            .find(|c| !CATEGORIES.contains(&c.as_str()))
        {
            return Err(format!("scope: unknown category {c:?}"));
        }
        if s.max_results == 0 {
            return Err("scope: max_results must be > 0".into());
        }
        Ok(s)
    }

    fn lang_granted(&self, tag: &str) -> bool {
        self.langs.is_empty() || lang::matches_any(tag, &self.langs)
    }

    /// Narrow a `search` / `deep_research` call to this scope. A request for
    /// something outside the grant is refused (so the agent learns why);
    /// a too-old `since` or too-large `count` is clamped, with a note.
    pub fn search_args(&self, args: &Value, now: DateTime<Utc>) -> Result<ScopedSearch, String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or("query is required")?
            .to_string();
        let mut notes = Vec::new();

        // Languages: requested ones must be granted; none requested = the
        // granted ones.
        let requested: Vec<String> = match args.get("lang") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) => s.split(',').map(|x| x.trim().to_string()).collect(),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str().map(|x| x.trim().to_string()))
                .collect(),
            Some(_) => return Err("lang must be a string or a list".into()),
        };
        let raw_by_lang: BTreeMap<String, String> = match args.get("query_by_lang") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|_| "query_by_lang must map language tags to strings")?,
        };
        let query_by_lang = lang::parse_query_by_lang(&raw_by_lang)?;
        let mut langs = Vec::new();
        for l in requested
            .iter()
            .filter(|l| !l.is_empty())
            .chain(query_by_lang.keys())
        {
            let tag = lang::normalize(l).ok_or_else(|| format!("invalid language tag {l:?}"))?;
            if !self.lang_granted(&tag) {
                return Err(format!(
                    "language {tag} is not in this app's research grant"
                ));
            }
            if !langs.contains(&tag) {
                langs.push(tag);
            }
        }
        if langs.is_empty() {
            langs = self.langs.clone();
        }

        let region = match args.get("region").and_then(Value::as_str).map(str::trim) {
            None | Some("") => None,
            Some(r) => {
                let r = r.to_ascii_uppercase();
                if !self.regions.is_empty() && !self.regions.contains(&r) {
                    return Err(format!("region {r} is not in this app's research grant"));
                }
                Some(r)
            }
        };

        // Recency: clamp to the grant.
        let mut since = match args.get("since").and_then(Value::as_str).map(str::trim) {
            None | Some("") => None,
            Some(s) => Some(Since::parse(s, now)?),
        };
        if let Some(days) = self.max_age_days {
            let floor = now - chrono::Duration::days(days as i64);
            let too_old = since.as_ref().is_none_or(|s| s.cutoff < floor);
            if too_old {
                if since.is_some() {
                    notes.push(format!("since clamped to this app's {days}-day limit"));
                }
                since = Some(Since {
                    cutoff: floor,
                    span: Some(chrono::Duration::days(days as i64)),
                });
            }
        }

        let category = match args.get("category").and_then(Value::as_str).map(str::trim) {
            None | Some("") | Some("auto") => {
                let news = crate::plan::looks_newsish(&query, since.as_ref(), now);
                let c = if news { "news" } else { "general" };
                if self.categories.is_empty() || self.categories.iter().any(|x| x == c) {
                    c.to_string()
                } else {
                    self.categories[0].clone()
                }
            }
            Some(c) => {
                let c = c.to_ascii_lowercase();
                if !CATEGORIES.contains(&c.as_str()) {
                    return Err(format!("unknown category {c:?}"));
                }
                if !self.categories.is_empty() && !self.categories.contains(&c) {
                    return Err(format!("category {c} is not in this app's research grant"));
                }
                c
            }
        };

        let asked = args
            .get("count")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(10)
            .max(1);
        let count = asked.min(self.max_results);
        if count < asked {
            notes.push(format!("count clamped to this app's limit of {count}"));
        }

        // Domains: the app's deny list always applies; requested allow-lists
        // must stay inside the app's allow-list.
        let str_list = |k: &str| -> Vec<String> {
            args.get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        let asked_allow = str_list("domains_allow");
        if !self.domains_allow.is_empty() {
            if let Some(d) = asked_allow.iter().find(|d| {
                !self
                    .domains_allow
                    .iter()
                    .any(|p| urls::domain_matches(d.trim_start_matches("www."), p))
            }) {
                return Err(format!("domain {d} is outside this app's research grant"));
            }
        }
        let allow = if asked_allow.is_empty() {
            self.domains_allow.clone()
        } else {
            asked_allow
        };
        let mut deny = self.domains_deny.clone();
        deny.extend(str_list("domains_deny"));
        let max_per_domain = args
            .get("max_per_domain")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let filters = Filters::new(langs.clone(), since.clone(), allow, deny, max_per_domain)?;

        Ok(ScopedSearch {
            query,
            query_by_lang,
            langs,
            region,
            since,
            category,
            count,
            filters,
            notes,
        })
    }

    /// The narrowed call as `deep-search` skill arguments (`deep_research`).
    pub fn research_args(&self, args: &Value, now: DateTime<Utc>) -> Result<Value, String> {
        let s = self.search_args(args, now)?;
        let mut out = json!({
            "query": s.query,
            "category": s.category,
            "max_results": s.count.min(20),
            "output": "items",
        });
        if !s.langs.is_empty() {
            out["lang"] = json!(s.langs);
        }
        if !s.query_by_lang.is_empty() {
            out["query_by_lang"] = json!(s.query_by_lang);
        }
        if let Some(r) = s.region {
            out["region"] = json!(r);
        }
        if let Some(since) = &s.since {
            out["since"] = json!(
                since
                    .cutoff
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            );
        }
        if !s.filters.domains_allow.is_empty() {
            out["domains_allow"] = json!(s.filters.domains_allow);
        }
        if !s.filters.domains_deny.is_empty() {
            out["domains_deny"] = json!(s.filters.domains_deny);
        }
        if let Some(depth) = args.get("depth").and_then(Value::as_u64) {
            out["depth"] = json!(depth.clamp(1, 3));
        }
        Ok(out)
    }

    /// Narrow a `deep_crawl` call: the site must be inside the domain grant,
    /// depth and pages within the `crawl` limits.
    pub fn crawl_args(&self, args: &Value) -> Result<Value, String> {
        if self.max_pages == 0 || self.max_depth == 0 {
            return Err("crawling is not in this app's grant".into());
        }
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or("url is required")?;
        self.check_domain(url)?;
        let depth = args
            .get("max_depth")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, self.max_depth as u64);
        let pages = args
            .get("max_pages")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .clamp(1, self.max_pages as u64);
        let mut out = json!({"url": url, "max_depth": depth, "max_pages": pages});
        if let Some(p) = args.get("path_prefix").and_then(Value::as_str) {
            out["path_prefix"] = json!(p);
        }
        Ok(out)
    }

    /// Whether `url` may be read under this scope's domain lists.
    pub fn check_domain(&self, url: &str) -> Result<(), String> {
        let f = Filters::new(
            Vec::new(),
            None,
            self.domains_allow.clone(),
            self.domains_deny.clone(),
            None,
        )?;
        f.check_domain(url)
            .map_err(|why| format!("{url} is outside this app's research grant ({why})"))
    }
}

/// What a toolbox call returns to the agent: a short summary with `[n]`
/// references, the items, and where they were written in the app's folder.
#[derive(Debug, Clone, Serialize)]
pub struct ToolboxResult {
    pub summary: String,
    pub items: Vec<ResearchItem>,
    pub items_file: PathBuf,
}

/// The toolbox engine the host runs.
pub struct Toolbox {
    pub metasearch: Metasearch,
    pub reader: Reader,
}

impl Toolbox {
    pub fn new(metasearch: Metasearch, reader: Reader) -> Self {
        Self { metasearch, reader }
    }

    /// `search`: one query through the provider chain's free first tier
    /// (the metasearch), scoped, written to `app_dir`.
    pub async fn search(
        &self,
        scope: &Scope,
        args: &Value,
        app_dir: &Path,
        now: DateTime<Utc>,
    ) -> Result<ToolboxResult, String> {
        let s = scope.search_args(args, now)?;
        let mut req = SearchRequest::new(&s.query, &s.category);
        req.query_by_lang = s.query_by_lang.clone();
        req.langs = s.langs.clone();
        req.region = s.region.clone();
        req.since = s.since.clone();
        req.count = s.count;
        req.limit = s.count;
        req.filters = s.filters.clone();
        req.now = now;
        let resp = self.metasearch.search(&req).await;

        let mut doc = ItemsDocument::new(&s.query, s.filters.to_json());
        doc.providers = resp.used_engines();
        doc.skipped = resp.skipped.clone();
        let mut notes = s.notes.clone();
        notes.extend(resp.note.clone());
        if !notes.is_empty() {
            doc.note = Some(notes.join(" "));
        }
        doc.items = resp
            .items
            .iter()
            .map(|m| {
                let domain =
                    urls::domain_of(m.source_url.as_deref().unwrap_or(&m.url)).unwrap_or_default();
                ResearchItem {
                    url: m.url.clone(),
                    title: m.title.clone(),
                    source: if m.source.is_empty() {
                        domain.clone()
                    } else {
                        m.source.clone()
                    },
                    domain,
                    lang: m.lang.clone(),
                    published: m.published.clone(),
                    summary: m.snippet.clone(),
                    summary_kind: if m.snippet.is_empty() {
                        SummaryKind::None
                    } else {
                        SummaryKind::Snippet
                    },
                    snippet: m.snippet.clone(),
                    provider: crate::metasearch::PROVIDER_ID.to_string(),
                    engines: m.engines.clone(),
                    score: Some(m.score),
                    kind: m.kind,
                    ..Default::default()
                }
            })
            .collect();
        write_items(doc, app_dir, "search", now)
    }

    /// `web_read`: read one page (browser-rendered when plain HTTP has no
    /// main text and a renderer is configured) and return its main text as
    /// one item, written to `app_dir`. A failure is the read's reason as
    /// `<code>: <detail> (final URL: …)` (see [`ReadError`]); pages outside
    /// the grant are `blocked`.
    pub async fn web_read(
        &self,
        scope: &Scope,
        args: &Value,
        app_dir: &Path,
        now: DateTime<Utc>,
    ) -> Result<ToolboxResult, String> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or("url is required")?;
        let blocked = |e: String| ReadError::new(ReadFailure::Blocked, e);
        scope.check_domain(url).map_err(blocked)?;
        let page = self.reader.read(url).await?;
        let canonical = page.canonical_url();
        check_read_location(scope, &page.final_url, &canonical)
            .map_err(|e| blocked(e).at(&page.final_url))?;
        let domain = urls::domain_of(&canonical).unwrap_or_default();
        let summary = crate::item::extractive_summary(&page.text, 600);
        let item = ResearchItem {
            url: canonical,
            title: page.meta.title.clone().unwrap_or_else(|| url.to_string()),
            source: page
                .meta
                .site_name
                .clone()
                .unwrap_or_else(|| domain.clone()),
            domain,
            lang: page.meta.lang.clone(),
            published: page.meta.published.clone(),
            summary_kind: if summary.is_empty() {
                SummaryKind::None
            } else {
                SummaryKind::Extractive
            },
            summary,
            snippet: page.meta.excerpt.clone().unwrap_or_default(),
            fetched_at: Some(page.fetched_at.clone()),
            provider: "web_read".to_string(),
            read: true,
            rendered: page.rendered,
            ..Default::default()
        };
        let mut doc = ItemsDocument::new(url, json!({"url": url}));
        doc.items = vec![item];
        let dir = app_dir.join("research");
        let _ = std::fs::create_dir_all(&dir);
        let text_file = unique_path(
            &dir,
            &format!("{}-{}", slug(url), now.format("%Y%m%dT%H%M%S")),
            "md",
        );
        std::fs::write(&text_file, &page.text).map_err(|e| format!("write page text: {e}"))?;
        doc.items[0].file = text_file
            .strip_prefix(app_dir)
            .ok()
            .map(|p| p.display().to_string());
        write_items(doc, app_dir, "read", now)
    }
}

/// A page that was read must be inside the grant both where it was actually
/// fetched from (after redirects) and under the URL it is filed as. The
/// canonical alone is not enough: a page may name its parent domain, so an
/// allowed URL redirecting into a denied subdomain would otherwise pass.
fn check_read_location(scope: &Scope, final_url: &str, canonical: &str) -> Result<(), String> {
    scope.check_domain(final_url)?;
    scope.check_domain(canonical)
}

/// `dir/<stem>.<ext>`, or `dir/<stem>-2.<ext>`, … if that name is taken, so
/// two calls in the same second never overwrite each other.
fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem}-{n}.{ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

fn slug(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').chars().take(48).collect()
}

/// Write the items document into `app_dir/research/` and build the agent's
/// summary: one line per item with its `[n]` reference.
fn write_items(
    mut doc: ItemsDocument,
    app_dir: &Path,
    kind: &str,
    now: DateTime<Utc>,
) -> Result<ToolboxResult, String> {
    let dir = app_dir.join("research");
    std::fs::create_dir_all(&dir).map_err(|e| format!("app folder: {e}"))?;
    let file = unique_path(
        &dir,
        &format!(
            "{kind}-{}-{}",
            slug(&doc.query),
            now.format("%Y%m%dT%H%M%S")
        ),
        "items.json",
    );
    for (i, item) in doc.items.iter_mut().enumerate() {
        item.citation = Some(i + 1);
    }
    doc.items_file = file
        .strip_prefix(app_dir)
        .ok()
        .map(|p| p.display().to_string());
    let body = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    std::fs::write(&file, body).map_err(|e| format!("write items: {e}"))?;

    let mut summary = format!("{} item(s) for {:?}", doc.items.len(), doc.query);
    if let Some(f) = &doc.items_file {
        summary.push_str(&format!(" (saved to {f})"));
    }
    summary.push_str(":\n");
    for item in &doc.items {
        let meta: Vec<&str> = [
            Some(item.source.as_str()),
            item.published.as_deref(),
            item.lang.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
        summary.push_str(&format!(
            "[{}] {} — {} — {}\n",
            item.citation.unwrap_or(0),
            item.title,
            meta.join(" · "),
            item.url
        ));
    }
    if let Some(note) = &doc.note {
        summary.push_str(&format!("Note: {note}\n"));
    }
    Ok(ToolboxResult {
        summary,
        items: doc.items,
        items_file: file,
    })
}

/// The toolbox tools' definitions for the kernel to register for an app's
/// peer: `{name, description, capability, input_schema}`. Only the tools the
/// app was granted should be offered.
pub fn tool_specs() -> Vec<Value> {
    let lang = json!({
        "description": "BCP-47 language(s) to search in (must be granted to the app)",
        "anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]
    });
    let query_by_lang = json!({
        "type": "object",
        "additionalProperties": {"type": "string"},
        "description": "The query in each language's own words, e.g. {\"zh\": \"人工智能 监管\"}"
    });
    let search_props = json!({
        "query": {"type": "string"},
        "lang": lang,
        "query_by_lang": query_by_lang,
        "region": {"type": "string", "description": "ISO 3166-1 alpha-2"},
        "since": {"type": "string", "description": "ISO date or 24h / 7d / 2w / 3m / 1y"},
        "category": {"type": "string", "enum": ["auto", "news", "general", "science", "it", "social"]},
        "count": {"type": "integer", "minimum": 1},
        "domains_allow": {"type": "array", "items": {"type": "string"}},
        "domains_deny": {"type": "array", "items": {"type": "string"}},
        "max_per_domain": {"type": "integer", "minimum": 1}
    });
    let mut research_props = search_props.clone();
    research_props["depth"] = json!({"type": "integer", "minimum": 1, "maximum": 3});
    vec![
        json!({
            "name": "search",
            "capability": "research",
            "description": "Search free sources (news, encyclopedias, papers, code, social) in the granted languages; returns cited, dated items saved in the app's folder.",
            "input_schema": {"type": "object", "properties": search_props, "required": ["query"]}
        }),
        json!({
            "name": "deep_research",
            "capability": "research",
            "description": "Investigate a topic across sources and languages: sub-queries, reading pages, a cited report and items saved in the app's folder.",
            "input_schema": {"type": "object", "properties": research_props, "required": ["query"]}
        }),
        json!({
            "name": "web_read",
            "capability": "research",
            "description": "Read one page (rendered by a real browser when needed) and return its main text as a cited item.",
            "input_schema": {"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"]}
        }),
        json!({
            "name": "deep_crawl",
            "capability": "crawl",
            "description": "Crawl one site within the app's limits (same site, depth, page count, path prefix).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "max_depth": {"type": "integer", "minimum": 1},
                    "max_pages": {"type": "integer", "minimum": 1},
                    "path_prefix": {"type": "string"}
                },
                "required": ["url"]
            }
        }),
    ]
}

#[cfg(test)]
mod tests;
