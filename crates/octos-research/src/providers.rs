//! Free providers: request builders and response parsers for the GDELT DOC
//! 2.0 API, Google News search RSS (and any RSS/Atom feed), and a
//! self-hosted SearXNG JSON endpoint. No I/O here; callers fetch.

use std::time::Duration;

use chrono::{DateTime, Utc};
use quick_xml::events::Event;
use url::Url;

use crate::date::{self, Since};
use crate::item::SearchHit;
use crate::lang;

pub const GDELT_DOC_API: &str = "https://api.gdeltproject.org/api/v2/doc/doc";
/// GDELT asks for at most one request every 5 seconds.
pub const GDELT_MIN_INTERVAL: Duration = Duration::from_secs(5);
pub const GOOGLE_NEWS_RSS_SEARCH: &str = "https://news.google.com/rss/search";

// ---------------------------------------------------------------------------
// GDELT DOC 2.0
// ---------------------------------------------------------------------------

/// GDELT DOC 2.0 `ArtList` JSON request. `lang` becomes a `sourcelang:`
/// operator; `since` becomes `timespan` (relative) or `startdatetime`
/// (absolute). GDELT searches at most the last 3 months.
pub fn gdelt_request_url(
    query: &str,
    lang_tag: Option<&str>,
    since: Option<&Since>,
    max_records: usize,
    now: DateTime<Utc>,
) -> String {
    let mut q = query.trim().to_string();
    if let Some(name) = lang_tag.and_then(lang::gdelt_sourcelang) {
        q.push_str(&format!(" sourcelang:{name}"));
    }
    let mut u = Url::parse(GDELT_DOC_API).expect("static GDELT URL");
    {
        let mut qp = u.query_pairs_mut();
        qp.append_pair("query", &q)
            .append_pair("mode", "ArtList")
            .append_pair("format", "json")
            .append_pair("maxrecords", &max_records.clamp(1, 250).to_string())
            .append_pair("sort", "DateDesc");
        if let Some(s) = since {
            match s.span {
                Some(span) => {
                    let hours = span.num_hours().max(1);
                    let value = if hours < 72 {
                        format!("{hours}h")
                    } else {
                        format!("{}d", (hours / 24).min(92))
                    };
                    qp.append_pair("timespan", &value);
                }
                None => {
                    let floor = now - chrono::Duration::days(92);
                    let start = if s.cutoff < floor { floor } else { s.cutoff };
                    qp.append_pair("startdatetime", &start.format("%Y%m%d%H%M%S").to_string());
                }
            }
        }
    }
    u.to_string()
}

/// Parse a GDELT `ArtList` JSON body. GDELT answers errors and rate limits
/// with plain text, which becomes `Err` (containing "rate limit" when
/// throttled so callers can rotate).
pub fn parse_gdelt(body: &str) -> Result<Vec<SearchHit>, String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let v: serde_json::Value = serde_json::from_str(trimmed).map_err(|_| {
        let head: String = trimmed.chars().take(160).collect();
        if head.to_ascii_lowercase().contains("limit requests") {
            format!("GDELT rate limit: {head}")
        } else {
            format!("GDELT error: {head}")
        }
    })?;
    let Some(articles) = v.get("articles").and_then(|a| a.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(articles
        .iter()
        .filter_map(|a| {
            let url = a.get("url")?.as_str()?.trim().to_string();
            if !url.starts_with("http") {
                return None;
            }
            let title = a
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            Some(SearchHit {
                url,
                title,
                snippet: String::new(),
                source: a
                    .get("domain")
                    .and_then(|d| d.as_str())
                    .filter(|d| !d.is_empty())
                    .map(String::from),
                source_url: None,
                lang: a
                    .get("language")
                    .and_then(|l| l.as_str())
                    .and_then(lang::from_gdelt_name)
                    .map(String::from),
                published: a
                    .get("seendate")
                    .and_then(|d| d.as_str())
                    .and_then(date::to_iso),
                provider: "gdelt".to_string(),
                ..Default::default()
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Google News RSS / generic feeds
// ---------------------------------------------------------------------------

/// Google News search RSS URL with `hl`/`gl`/`ceid` for the language and
/// region, and `when:` (relative) or `after:` (absolute) for `since`.
pub fn google_news_rss_url(
    query: &str,
    lang_tag: Option<&str>,
    region: Option<&str>,
    since: Option<&Since>,
) -> String {
    let (hl, gl, ceid) = lang::google_news_locale(lang_tag, region);
    let mut q = query.trim().to_string();
    if let Some(s) = since {
        match s.span {
            Some(span) => {
                let hours = span.num_hours().max(1);
                if hours <= 48 {
                    q.push_str(&format!(" when:{hours}h"));
                } else {
                    q.push_str(&format!(" when:{}d", hours / 24));
                }
            }
            None => q.push_str(&format!(" after:{}", s.cutoff.format("%Y-%m-%d"))),
        }
    }
    let mut u = Url::parse(GOOGLE_NEWS_RSS_SEARCH).expect("static Google News URL");
    u.query_pairs_mut()
        .append_pair("q", &q)
        .append_pair("hl", &hl)
        .append_pair("gl", &gl)
        .append_pair("ceid", &ceid);
    u.to_string()
}

/// Language implied by a Google News `ceid` (`TW:zh-Hant` → `zh-TW`).
pub fn google_news_lang(lang_tag: Option<&str>, region: Option<&str>) -> String {
    let (hl, _, _) = lang::google_news_locale(lang_tag, region);
    lang::normalize(&hl).unwrap_or(hl)
}

#[derive(Default)]
struct FeedEntry {
    title: String,
    link: String,
    published: String,
    description: String,
    source: String,
    source_url: String,
    authors: Vec<String>,
}

/// Parse an RSS 2.0 or Atom feed into hits. `default_lang` is used when the
/// feed has no `<language>`. For Google News, the ` - Publisher` suffix on
/// titles is removed when it matches the `<source>` element.
pub fn parse_feed(
    xml: &str,
    provider: &str,
    default_lang: Option<&str>,
) -> Result<Vec<SearchHit>, String> {
    Ok(parse_feed_items(xml, provider, default_lang)?
        .into_iter()
        .map(|(hit, _)| hit)
        .collect())
}

/// Feed entries as JSON records for metasearch engine scripts:
/// `{url, title, snippet, source, source_url, lang, published, authors}`.
pub fn parse_feed_entries(
    xml: &str,
    default_lang: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    Ok(parse_feed_items(xml, "", default_lang)?
        .into_iter()
        .map(|(h, authors)| {
            serde_json::json!({
                "url": h.url,
                "title": h.title,
                "snippet": h.snippet,
                "source": h.source,
                "source_url": h.source_url,
                "lang": h.lang,
                "published": h.published,
                "authors": authors,
            })
        })
        .collect())
}

type FeedItem = (SearchHit, Vec<String>);

fn parse_feed_items(
    xml: &str,
    provider: &str,
    default_lang: Option<&str>,
) -> Result<Vec<FeedItem>, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut hits = Vec::new();
    let mut entry: Option<FeedEntry> = None;
    let mut path: Vec<String> = Vec::new();
    let mut channel_lang: Option<String> = None;
    let mut saw_root = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase();
                if matches!(name.as_str(), "rss" | "feed" | "rdf") {
                    saw_root = true;
                }
                if name == "item" || name == "entry" {
                    entry = Some(FeedEntry::default());
                }
                if let Some(en) = entry.as_mut() {
                    if name == "source" {
                        en.source_url = attr(&e, b"url").unwrap_or_default();
                    }
                    if name == "link" {
                        take_atom_link(&e, en);
                    }
                }
                path.push(name);
            }
            Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase();
                if let Some(en) = entry.as_mut() {
                    if name == "link" {
                        take_atom_link(&e, en);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase();
                if (name == "item" || name == "entry") && entry.is_some() {
                    if let Some(hit) = entry.take().and_then(|en| {
                        finish_entry(en, provider, channel_lang.as_deref().or(default_lang))
                    }) {
                        hits.push(hit);
                    }
                }
                path.pop();
            }
            Ok(Event::Text(t)) => {
                let text = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                on_text(&path, &text, &mut entry, &mut channel_lang);
            }
            Ok(Event::CData(c)) => {
                let text = String::from_utf8_lossy(&c.into_inner()).into_owned();
                on_text(&path, &text, &mut entry, &mut channel_lang);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("feed parse error: {e}")),
            _ => {}
        }
    }
    if !saw_root {
        return Err("not an RSS/Atom feed".to_string());
    }
    Ok(hits)
}

fn attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

fn take_atom_link(e: &quick_xml::events::BytesStart<'_>, en: &mut FeedEntry) {
    if let Some(href) = attr(e, b"href") {
        let rel = attr(e, b"rel").unwrap_or_else(|| "alternate".to_string());
        if rel == "alternate" && en.link.is_empty() {
            en.link = href;
        }
    }
}

fn on_text(
    path: &[String],
    text: &str,
    entry: &mut Option<FeedEntry>,
    channel_lang: &mut Option<String>,
) {
    let Some(leaf) = path.last() else { return };
    // Atom `<author><name>` (arXiv lists every author this way).
    if leaf == "name"
        && path.len() >= 3
        && path[path.len() - 2] == "author"
        && matches!(path[path.len() - 3].as_str(), "item" | "entry")
    {
        if let Some(en) = entry.as_mut() {
            let name = collapse_ws(text);
            if !name.is_empty() {
                en.authors.push(name);
            }
        }
        return;
    }
    // Only direct children of <item>/<entry> (ignores e.g. media:title).
    let parent_is_entry =
        path.len() >= 2 && matches!(path[path.len() - 2].as_str(), "item" | "entry");
    match entry.as_mut() {
        Some(_) if !parent_is_entry => {}
        Some(en) => match leaf.as_str() {
            "title" => en.title.push_str(text),
            "link" => en.link.push_str(text.trim()),
            "pubdate" | "published" | "updated" | "date" => {
                if en.published.is_empty() || leaf == "published" || leaf == "pubdate" {
                    en.published = text.trim().to_string();
                }
            }
            "description" | "summary" | "content" | "encoded" => {
                if en.description.is_empty() {
                    en.description = text.to_string();
                }
            }
            "source" => en.source.push_str(text),
            _ => {}
        },
        None => {
            if leaf == "language" && channel_lang.is_none() {
                *channel_lang = lang::normalize(text);
            }
        }
    }
}

fn finish_entry(en: FeedEntry, provider: &str, lang_tag: Option<&str>) -> Option<FeedItem> {
    let url = en.link.trim().to_string();
    if !url.starts_with("http") {
        return None;
    }
    let source = en.source.trim().to_string();
    let mut title = collapse_ws(&en.title);
    if !source.is_empty() {
        if let Some(stripped) = title.strip_suffix(&format!(" - {source}")) {
            title = stripped.trim().to_string();
        }
    }
    let mut snippet = collapse_ws(&strip_tags(&en.description));
    if snippet == title || (!source.is_empty() && snippet == format!("{title} {source}")) {
        snippet.clear();
    }
    if snippet.chars().count() > 400 {
        snippet = snippet.chars().take(400).collect::<String>() + "…";
    }
    let hit = SearchHit {
        url,
        title,
        snippet,
        source: (!source.is_empty()).then_some(source),
        source_url: (!en.source_url.is_empty()).then_some(en.source_url),
        lang: lang_tag.map(String::from),
        published: date::to_iso(&en.published),
        provider: provider.to_string(),
        ..Default::default()
    };
    Some((hit, en.authors))
}

// ---------------------------------------------------------------------------
// SearXNG
// ---------------------------------------------------------------------------

/// SearXNG JSON search URL. `base` is the operator-configured instance
/// (e.g. `http://127.0.0.1:8888` or `https://host/searx`). The instance must
/// have the `json` format enabled in its settings.
pub fn searxng_request_url(
    base: &str,
    query: &str,
    lang_tag: Option<&str>,
    since: Option<&Since>,
    news: bool,
    now: DateTime<Utc>,
) -> Result<String, String> {
    let base = base.trim().trim_end_matches('/');
    let mut u = Url::parse(&format!("{base}/search"))
        .map_err(|e| format!("invalid SearXNG base URL {base:?}: {e}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!("SearXNG base URL must be http(s): {base}"));
    }
    {
        let mut qp = u.query_pairs_mut();
        qp.append_pair("q", query.trim())
            .append_pair("format", "json")
            .append_pair("categories", if news { "news" } else { "general" });
        if let Some(l) = lang_tag.and_then(lang::normalize) {
            qp.append_pair("language", &l);
        }
        if let Some(s) = since {
            qp.append_pair("time_range", s.bucket(now).as_word());
        }
    }
    Ok(u.to_string())
}

/// Parse a SearXNG JSON response.
pub fn parse_searxng(body: &str) -> Result<Vec<SearchHit>, String> {
    let v: serde_json::Value = serde_json::from_str(body.trim()).map_err(|e| {
        let head: String = body.trim().chars().take(160).collect();
        format!("SearXNG returned non-JSON ({e}); is `json` enabled in search.formats? {head}")
    })?;
    let Some(results) = v.get("results").and_then(|r| r.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(results
        .iter()
        .filter_map(|r| {
            let url = r.get("url")?.as_str()?.trim().to_string();
            if !url.starts_with("http") {
                return None;
            }
            let s = |k: &str| {
                r.get(k)
                    .and_then(|x| x.as_str())
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
            };
            Some(SearchHit {
                title: s("title").unwrap_or_default(),
                snippet: s("content").unwrap_or_default(),
                source: None,
                source_url: None,
                lang: None,
                published: s("publishedDate")
                    .or_else(|| s("pubdate"))
                    .and_then(|d| date::to_iso(&d)),
                provider: "searxng".to_string(),
                url,
                ..Default::default()
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Plain-text rendering of hits for tool output and search logs:
/// `N. title` / URL on its own line / `source · date · lang` / snippet.
pub fn format_hits(query: &str, hits: &[SearchHit]) -> String {
    let mut out = format!("Results for: {query}\n\n");
    for (i, h) in hits.iter().enumerate() {
        let title = if h.title.is_empty() { &h.url } else { &h.title };
        out.push_str(&format!("{}. {title}\n   {}\n", i + 1, h.url));
        let meta: Vec<&str> = [
            h.source.as_deref(),
            h.published.as_deref(),
            h.lang.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut meta_line = meta.join(" · ");
        let via = if h.engines.is_empty() {
            h.provider.clone()
        } else {
            format!("{}: {}", h.provider, h.engines.join(", "))
        };
        if !meta_line.is_empty() {
            meta_line = format!("{meta_line} · via {via}");
            out.push_str(&format!("   {meta_line}\n"));
        } else if !h.engines.is_empty() {
            out.push_str(&format!("   via {via}\n"));
        }
        if !h.snippet.is_empty() {
            out.push_str(&format!("   {}\n", h.snippet));
        }
        out.push('\n');
    }
    out
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip HTML tags and decode the few entities feeds commonly double-escape.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Plain text of an HTML fragment: tags removed, named and numeric
/// character references decoded, whitespace collapsed.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let Some(end) = rest[i..].find('>') else {
            rest = &rest[i..];
            break;
        };
        let tag = rest[i + 1..i + end]
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        // Block-level tags separate words; inline ones (a, span, b, em...)
        // do not.
        if matches!(
            tag.as_str(),
            "br" | "p"
                | "div"
                | "li"
                | "ul"
                | "ol"
                | "tr"
                | "td"
                | "th"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "blockquote"
                | "pre"
                | "hr"
        ) {
            out.push(' ');
        }
        rest = &rest[i + end + 1..];
    }
    out.push_str(rest);
    collapse_ws(&decode_entities(&out))
}

/// Decode numeric (`&#39;`, `&#x27;`) and common named character references.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail[1..].find(';').map(|e| e + 1).filter(|e| *e <= 10);
        let decoded = end.and_then(|e| {
            let name = &tail[1..e];
            let ch = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = name.strip_prefix('#') {
                dec.parse::<u32>().ok().and_then(char::from_u32)
            } else {
                match name {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    "hellip" => Some('…'),
                    "mdash" => Some('—'),
                    "ndash" => Some('–'),
                    "rsquo" => Some('’'),
                    "lsquo" => Some('‘'),
                    "rdquo" => Some('”'),
                    "ldquo" => Some('“'),
                    _ => None,
                }
            };
            ch.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn should_turn_html_fragments_into_text() {
        assert_eq!(
            html_to_text(
                "<p>Rust&#39;s <b>async</b> &amp; &#x201C;await&#x201D;</p>\n<p>ok&nbsp;now</p>"
            ),
            "Rust's async & “await” ok now"
        );
        assert_eq!(
            html_to_text(
                "<strong>octos</strong>: see <a href=\"x\"><span>https://</span><span>a.org/x</span></a><br/>next"
            ),
            "octos: see https://a.org/x next"
        );
        assert_eq!(
            decode_entities("a & b &unknown; &#xZZ;"),
            "a & b &unknown; &#xZZ;"
        );
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap()
    }

    #[test]
    fn should_build_gdelt_url_with_sourcelang_and_timespan() {
        let since = Since::parse("24h", now()).unwrap();
        let u = gdelt_request_url("climate summit", Some("es"), Some(&since), 20, now());
        let parsed = Url::parse(&u).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["query"], "climate summit sourcelang:spanish");
        assert_eq!(pairs["mode"], "ArtList");
        assert_eq!(pairs["format"], "json");
        assert_eq!(pairs["timespan"], "24h");
        assert_eq!(pairs["maxrecords"], "20");

        let week = Since::parse("7d", now()).unwrap();
        let u = gdelt_request_url("x y z", None, Some(&week), 5, now());
        assert!(u.contains("timespan=7d"), "{u}");

        let abs = Since::parse("2026-09-20", now()).unwrap();
        let u = gdelt_request_url("x y z", None, Some(&abs), 5, now());
        assert!(u.contains("startdatetime=20260920000000"), "{u}");
    }

    #[test]
    fn should_build_google_news_url_with_locale_and_when() {
        let since = Since::parse("7d", now()).unwrap();
        let u = google_news_rss_url("台风", Some("zh-TW"), None, Some(&since));
        let parsed = Url::parse(&u).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["q"], "台风 when:7d");
        assert_eq!(pairs["hl"], "zh-TW");
        assert_eq!(pairs["gl"], "TW");
        assert_eq!(pairs["ceid"], "TW:zh-Hant");
        let abs = Since::parse("2026-09-01", now()).unwrap();
        assert!(google_news_rss_url("x", None, None, Some(&abs)).contains("after%3A2026-09-01"));
    }

    #[test]
    fn should_build_searxng_url_and_reject_bad_base() {
        let since = Since::parse("24h", now()).unwrap();
        let u = searxng_request_url(
            "http://127.0.0.1:8888/",
            "rust",
            Some("de"),
            Some(&since),
            true,
            now(),
        )
        .unwrap();
        assert!(u.starts_with("http://127.0.0.1:8888/search?"), "{u}");
        assert!(u.contains("format=json"));
        assert!(u.contains("categories=news"));
        assert!(u.contains("language=de"));
        assert!(u.contains("time_range=day"));
        assert!(searxng_request_url("file:///etc", "q", None, None, false, now()).is_err());
        assert!(searxng_request_url("not a url", "q", None, None, false, now()).is_err());
    }

    #[test]
    fn should_report_gdelt_rate_limit_text_as_error() {
        let err =
            parse_gdelt("Please limit requests to one every 5 seconds or contact ...").unwrap_err();
        assert!(err.contains("rate limit"), "{err}");
        assert!(parse_gdelt("{}").unwrap().is_empty());
    }

    #[test]
    fn should_format_hits_with_url_on_its_own_line() {
        let hits = vec![SearchHit {
            url: "https://a.com/x".into(),
            title: "Title".into(),
            snippet: "Snip".into(),
            source: Some("A".into()),
            published: Some("2026-09-26".into()),
            lang: Some("en".into()),
            provider: "gdelt".into(),
            ..Default::default()
        }];
        let out = format_hits("q", &hits);
        assert!(out.contains("\n   https://a.com/x\n"), "{out}");
        assert!(out.contains("A · 2026-09-26 · en · via gdelt"), "{out}");
    }
}
