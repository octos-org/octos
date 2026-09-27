//! Result types: provider hits and the structured research items the tools
//! write into the research folder (`items.json`) and return in `items` mode.

use serde::{Deserialize, Serialize};

/// Schema tag for [`ItemsDocument`].
pub const ITEMS_SCHEMA: &str = "octos.research.items.v1";

/// One search result as a provider returned it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snippet: String,
    /// Publisher / site name, when the provider names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Publisher home page, when the provider gives it (Google News).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// BCP-47 language, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// ISO 8601 publication time/date, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    /// Provider id (`gdelt`, `google_news_rss`, `searxng`, `brave`, ...).
    pub provider: String,
}

impl SearchHit {
    /// URL whose domain represents the publisher. Aggregator links (Google
    /// News `/rss/articles/…` redirects) use the `<source url>` publisher
    /// home page instead, so domain lists and per-domain caps apply to the
    /// publisher rather than to the aggregator.
    pub fn domain_url(&self) -> &str {
        let aggregator = crate::urls::domain_of(&self.url).is_some_and(|d| d == "news.google.com");
        match (&self.source_url, aggregator) {
            (Some(src), true) if !src.is_empty() => src,
            _ => &self.url,
        }
    }
}

/// How an item's `summary` was produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryKind {
    /// Sentences copied verbatim from the page's main text.
    Extractive,
    /// Written by a language model from the page text.
    Model,
    /// Only the provider's snippet/headline was available (page not read).
    Snippet,
    #[default]
    None,
}

/// A structured research item (ADR 0002: title, URL, source, language,
/// date, summary, citations).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResearchItem {
    /// Canonical URL (page `<link rel=canonical>` when present, else the
    /// fetched URL with tracking parameters and fragments removed).
    pub url: String,
    pub title: String,
    /// Site / publisher name.
    pub source: String,
    /// Host without `www.`.
    pub domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// ISO 8601 publication date/time, from feed or page metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    pub summary: String,
    pub summary_kind: SummaryKind,
    /// Provider snippet or the page's own description.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub snippet: String,
    /// When the page was read (RFC 3339); absent if it was not read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
    /// Which provider found it (`gdelt`, `google_news_rss`, `searxng`,
    /// a keyed API, or `reference` / `site_crawl` for chased links).
    pub provider: String,
    /// Whether the page main text was read (plain HTTP or browser).
    pub read: bool,
    /// Whether a real browser rendered the page (JS-heavy pages).
    pub rendered: bool,
    /// `[N]` label of this source in the report, if it is a report source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citation: Option<usize>,
    /// Whether the report's synthesis actually cites `[N]`.
    pub cited: bool,
    /// Saved main-text file, relative to the research folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// A URL that was deliberately not read, and why (`robots`, `domain_deny`,
/// `domain_allow`, `per_domain_cap`, `lang`, `older_than_since`,
/// `fetch_error: ...`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedUrl {
    pub url: String,
    pub reason: String,
}

/// The `items` output document (also written as `items.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ItemsDocument {
    pub schema: String,
    pub query: String,
    pub generated_at: String,
    /// Controls as applied (echoed so a consumer can tell what was filtered).
    pub controls: serde_json::Value,
    pub items: Vec<ResearchItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedUrl>,
    /// Providers that returned results, in order of use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
    /// Markdown report path, when one was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items_file: Option<String>,
}

impl ItemsDocument {
    pub fn new(query: &str, controls: serde_json::Value) -> Self {
        Self {
            schema: ITEMS_SCHEMA.to_string(),
            query: query.to_string(),
            generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            controls,
            ..Default::default()
        }
    }
}

/// Extractive summary: the first sentences of `text` up to ~`max_chars`,
/// skipping short lines (menus, bylines). Sentence ends include CJK
/// punctuation. Returns an empty string if nothing substantial is found.
pub fn extractive_summary(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.chars().count() < 40 {
            continue;
        }
        let mut sentence = String::new();
        for ch in line.chars() {
            sentence.push(ch);
            if matches!(ch, '.' | '!' | '?' | '。' | '！' | '？') {
                if !push_sentence(&mut out, &sentence, max_chars) {
                    return out;
                }
                sentence.clear();
            }
        }
        if !sentence.trim().is_empty() && !push_sentence(&mut out, &sentence, max_chars) {
            return out;
        }
        if out.chars().count() >= max_chars / 2 {
            break;
        }
    }
    out
}

fn push_sentence(out: &mut String, sentence: &str, max_chars: usize) -> bool {
    let s = sentence.trim();
    if s.is_empty() {
        return true;
    }
    let need = s.chars().count() + usize::from(!out.is_empty());
    if out.chars().count() + need > max_chars {
        if out.is_empty() {
            // First sentence is longer than the budget: cut on a char boundary.
            out.extend(s.chars().take(max_chars.saturating_sub(1)));
            out.push('…');
        }
        return false;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(s);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_serialize_items_with_stable_field_names() {
        let item = ResearchItem {
            url: "https://example.com/a".into(),
            title: "A".into(),
            source: "Example".into(),
            domain: "example.com".into(),
            lang: Some("en".into()),
            published: Some("2026-09-26".into()),
            summary: "S.".into(),
            summary_kind: SummaryKind::Extractive,
            snippet: String::new(),
            fetched_at: Some("2026-09-27T00:00:00Z".into()),
            provider: "gdelt".into(),
            read: true,
            rendered: false,
            citation: Some(1),
            cited: true,
            file: Some("01_example-com.md".into()),
        };
        let v = serde_json::to_value(&item).unwrap();
        for key in [
            "url",
            "title",
            "source",
            "domain",
            "lang",
            "published",
            "summary",
            "summary_kind",
            "fetched_at",
            "provider",
            "read",
            "rendered",
            "citation",
            "cited",
            "file",
        ] {
            assert!(v.get(key).is_some(), "missing {key}: {v}");
        }
        assert_eq!(v["summary_kind"], "extractive");
        assert!(v.get("snippet").is_none(), "empty snippet is omitted");
    }

    #[test]
    fn should_build_extractive_summary_from_substantial_lines() {
        let text = "Home\nMenu\nThe summit opened on Monday with delegates from 190 countries. \
                    Negotiators expect a draft by Friday. A third sentence follows here.\n";
        let s = extractive_summary(text, 120);
        assert!(s.starts_with("The summit opened"), "{s}");
        assert!(s.contains("Negotiators expect"), "{s}");
        assert!(!s.contains("Menu"));
        assert!(s.chars().count() <= 120);

        let cjk =
            "峰会周一开幕，来自190个国家的代表出席了会议并发表讲话。谈判代表预计周五前拿出草案。";
        assert!(extractive_summary(cjk, 200).contains("峰会周一开幕"));
        assert_eq!(extractive_summary("short\nlines", 100), "");
    }
}
