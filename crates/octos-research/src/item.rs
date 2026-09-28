//! Result types: provider hits and the structured research items the tools
//! write into the research folder (`items.json`) and return in `items` mode.

use serde::{Deserialize, Serialize};

/// Schema tag for [`ItemsDocument`].
pub const ITEMS_SCHEMA: &str = "octos.research.items.v1";

/// What a result is: an article (a news story, page, paper or repository
/// that can be read and cited as a source) or a post (a social post, or a
/// discussion thread without a linked article). Posts are signal about what
/// people are saying, not evidence for a news claim: in category `news` the
/// metasearch ranks them after every article, and callers can choose not to
/// read them. Serialized only when it is `post`; absent means `article`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    #[default]
    Article,
    Post,
}

impl ItemKind {
    pub fn is_article(&self) -> bool {
        *self == Self::Article
    }

    pub fn is_post(&self) -> bool {
        *self == Self::Post
    }

    /// Parse `article` / `post`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "article" => Some(Self::Article),
            "post" => Some(Self::Post),
            _ => None,
        }
    }
}

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
    /// Metasearch engines that returned this result, best first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engines: Vec<String>,
    /// Metasearch rank score (higher is better), when ranked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Article or post ([`ItemKind`]); absent means article.
    #[serde(default, skip_serializing_if = "ItemKind::is_article")]
    pub kind: ItemKind,
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
    /// Metasearch engines that found it, best first (provider `metasearch`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engines: Vec<String>,
    /// Metasearch rank score, when the item came from the metasearch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Article or post ([`ItemKind`]); absent means article.
    #[serde(default, skip_serializing_if = "ItemKind::is_article")]
    pub kind: ItemKind,
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

/// A URL that was deliberately not read, or could not be, and why:
/// filters (`robots`, `domain_deny`, `domain_allow`, `per_domain_cap`,
/// `lang`, `older_than_since`) or a failed read as `<code>: <detail>` (see
/// [`crate::ReadFailure`]: `bot_challenge`, `paywall`, `http_403`, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedUrl {
    pub url: String,
    pub reason: String,
    /// Where a failed read ended (after redirects or in the browser), when
    /// known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
}

impl SkippedUrl {
    /// A URL skipped for `reason` (a filter name).
    pub fn new(url: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            reason: reason.into(),
            final_url: None,
        }
    }

    /// A URL whose read failed: `reason` is `<code>: <detail>`, and the
    /// final URL is kept.
    pub fn read_failed(url: impl Into<String>, err: &crate::ReadError) -> Self {
        let reason = if err.detail.is_empty() {
            err.code()
        } else {
            format!("{}: {}", err.code(), err.detail)
        };
        Self {
            url: url.into(),
            reason,
            final_url: err.final_url.clone(),
        }
    }
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
    /// Human-readable note, e.g. why there are no items and how to fix it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Problems with this result a consumer should know about, e.g. a
    /// report whose synthesis was cut off or has uncited sentences.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
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
            engines: Vec::new(),
            score: None,
            kind: ItemKind::Article,
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

    #[test]
    fn should_mark_posts_and_omit_kind_when_item_is_an_article() {
        let post = SearchHit {
            url: "https://mastodon.social/@a/1".into(),
            kind: ItemKind::Post,
            ..Default::default()
        };
        assert_eq!(serde_json::to_value(&post).unwrap()["kind"], "post");
        let article = SearchHit::default();
        assert!(
            serde_json::to_value(&article)
                .unwrap()
                .get("kind")
                .is_none()
        );
        let back: SearchHit = serde_json::from_value(serde_json::json!({
            "url": "u", "title": "t", "provider": "p"
        }))
        .unwrap();
        assert_eq!(back.kind, ItemKind::Article, "absent kind means article");
        assert_eq!(ItemKind::parse("post"), Some(ItemKind::Post));
        assert_eq!(ItemKind::parse("thread"), None);
    }

    #[test]
    fn should_keep_reason_code_and_final_url_when_read_failed() {
        let err = crate::ReadError::new(crate::ReadFailure::Paywall, "subscriber-only")
            .at("https://publisher.example/story");
        let s = SkippedUrl::read_failed("https://news.google.com/rss/articles/x", &err);
        assert_eq!(s.reason, "paywall: subscriber-only");
        assert_eq!(
            s.final_url.as_deref(),
            Some("https://publisher.example/story")
        );
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["final_url"], "https://publisher.example/story");
        assert!(
            serde_json::to_value(SkippedUrl::new("u", "lang"))
                .unwrap()
                .get("final_url")
                .is_none()
        );
    }
}
