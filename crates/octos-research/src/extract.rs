//! Main-text and metadata extraction from HTML, readability-style
//! (`dom_smoothie`, an MIT-licensed port of Mozilla readability.js).

use std::sync::OnceLock;

use dom_smoothie::{Config, Readability, TextMode};
use regex::Regex;
use url::Url;

use crate::{date, lang};

/// Pages whose extracted main text is shorter than this are treated as
/// "no main text" (JS-rendered shells, consent walls) and are candidates
/// for browser rendering.
pub const MIN_MAIN_TEXT_CHARS: usize = 200;

/// Metadata found in a page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageMeta {
    pub title: Option<String>,
    pub site_name: Option<String>,
    /// Normalized BCP-47.
    pub lang: Option<String>,
    /// ISO 8601.
    pub published: Option<String>,
    /// Absolute `<link rel=canonical>` / `og:url`.
    pub canonical: Option<String>,
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extracted {
    /// Main text (paragraphs separated by blank lines).
    pub text: String,
    pub meta: PageMeta,
}

impl Extracted {
    /// Whether the page yielded no usable main text.
    pub fn is_empty_text(&self) -> bool {
        self.text.chars().filter(|c| !c.is_whitespace()).count() < MIN_MAIN_TEXT_CHARS
    }
}

/// Extract main text and metadata. Never fails: on a readability error the
/// text is empty but metadata found by the fallback scanners is kept.
pub fn extract(html: &str, page_url: &str) -> Extracted {
    let cfg = Config {
        text_mode: TextMode::Formatted,
        max_elements_to_parse: 20_000,
        ..Default::default()
    };
    let mut out = Extracted::default();
    if let Ok(mut r) = Readability::new(html, Some(page_url), Some(cfg)) {
        match r.parse() {
            Ok(article) => {
                out.text = tidy_text(&article.text_content);
                out.meta.title = non_empty(article.title);
                out.meta.site_name = article.site_name.and_then(non_empty);
                out.meta.lang = article.lang.as_deref().and_then(lang::normalize);
                out.meta.published = article.published_time.as_deref().and_then(date::to_iso);
                out.meta.excerpt = article.excerpt.and_then(non_empty);
            }
            Err(_) => {
                let meta = r.get_article_metadata(r.parse_json_ld());
                out.meta.title = non_empty(meta.title);
                out.meta.site_name = meta.site_name.and_then(non_empty);
                out.meta.lang = meta.lang.as_deref().and_then(lang::normalize);
                out.meta.published = meta.published_time.as_deref().and_then(date::to_iso);
                out.meta.excerpt = meta.excerpt.and_then(non_empty);
            }
        }
    }
    if out.meta.lang.is_none() {
        out.meta.lang = html_lang(html);
    }
    if out.meta.published.is_none() {
        out.meta.published = meta_published(html);
    }
    if out.meta.title.is_none() {
        out.meta.title = title_tag(html);
    }
    out.meta.canonical = canonical_link(html, page_url);
    out
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn tidy_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut blank = 0;
    for line in raw.lines() {
        let l = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if l.is_empty() {
            blank += 1;
            if blank == 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(&l);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

fn attr_value(tag: &str, name: &str) -> Option<String> {
    static ATTR: OnceLock<Regex> = OnceLock::new();
    let r = re(
        &ATTR,
        r#"(?i)([a-z:_-]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
    );
    r.captures_iter(tag).find_map(|c| {
        if c.get(1)?.as_str().eq_ignore_ascii_case(name) {
            c.get(2)
                .or_else(|| c.get(3))
                .or_else(|| c.get(4))
                .map(|m| m.as_str().trim().to_string())
        } else {
            None
        }
    })
}

/// `<html lang="…">`.
pub fn html_lang(html: &str) -> Option<String> {
    static HTML_TAG: OnceLock<Regex> = OnceLock::new();
    let tag = re(&HTML_TAG, r"(?is)<html\b[^>]*>").find(html)?;
    attr_value(tag.as_str(), "lang")
        .or_else(|| attr_value(tag.as_str(), "xml:lang"))
        .and_then(|l| lang::normalize(&l))
}

/// `<link rel="canonical" href>` or `og:url`, resolved against the page URL.
/// Only http(s) canonicals on the page's own host family (same host, or one
/// a subdomain of the other) are trusted, so a page cannot relabel itself as
/// another site; anything else is ignored.
pub fn canonical_link(html: &str, page_url: &str) -> Option<String> {
    static LINK: OnceLock<Regex> = OnceLock::new();
    static META: OnceLock<Regex> = OnceLock::new();
    let base = Url::parse(page_url).ok()?;
    let from_link = re(&LINK, r"(?is)<link\b[^>]*>")
        .find_iter(html)
        .map(|m| m.as_str())
        .find(|t| {
            attr_value(t, "rel").is_some_and(|r| {
                r.split_whitespace()
                    .any(|x| x.eq_ignore_ascii_case("canonical"))
            })
        })
        .and_then(|t| attr_value(t, "href"));
    let from_og = || {
        re(&META, r"(?is)<meta\b[^>]*>")
            .find_iter(html)
            .map(|m| m.as_str())
            .find(|t| attr_value(t, "property").is_some_and(|p| p.eq_ignore_ascii_case("og:url")))
            .and_then(|t| attr_value(t, "content"))
    };
    let raw = from_link.or_else(from_og)?;
    let resolved = base.join(raw.trim()).ok()?;
    if !matches!(resolved.scheme(), "http" | "https") {
        return None;
    }
    let page_host = crate::urls::domain_of(page_url)?;
    let canon_host = crate::urls::domain_of(resolved.as_str())?;
    if !crate::urls::domain_matches(&page_host, &canon_host)
        && !crate::urls::domain_matches(&canon_host, &page_host)
    {
        return None;
    }
    Some(resolved.to_string())
}

/// Publication date from common `<meta>` names and `<time datetime>`.
pub fn meta_published(html: &str) -> Option<String> {
    static META: OnceLock<Regex> = OnceLock::new();
    static TIME: OnceLock<Regex> = OnceLock::new();
    const KEYS: &[&str] = &[
        "article:published_time",
        "og:published_time",
        "datepublished",
        "pubdate",
        "publishdate",
        "publish-date",
        "parsely-pub-date",
        "sailthru.date",
        "dc.date.issued",
        "dc.date",
        "date",
    ];
    let metas: Vec<&str> = re(&META, r"(?is)<meta\b[^>]*>")
        .find_iter(html)
        .map(|m| m.as_str())
        .collect();
    for key in KEYS {
        for t in &metas {
            let name = attr_value(t, "property")
                .or_else(|| attr_value(t, "name"))
                .or_else(|| attr_value(t, "itemprop"));
            if name.is_some_and(|n| n.eq_ignore_ascii_case(key)) {
                if let Some(iso) = attr_value(t, "content").and_then(|c| date::to_iso(&c)) {
                    return Some(iso);
                }
            }
        }
    }
    re(&TIME, r"(?is)<time\b[^>]*>")
        .find_iter(html)
        .find_map(|m| attr_value(m.as_str(), "datetime").and_then(|d| date::to_iso(&d)))
}

fn title_tag(html: &str) -> Option<String> {
    static TITLE: OnceLock<Regex> = OnceLock::new();
    let c = re(&TITLE, r"(?is)<title[^>]*>(.*?)</title>").captures(html)?;
    non_empty(
        c.get(1)?
            .as_str()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE: &str = r#"<!doctype html>
<html lang="es-mx"><head>
<title>Cumbre climática: acuerdo preliminar | El Diario</title>
<meta property="og:site_name" content="El Diario">
<meta property="article:published_time" content="2026-09-25T08:30:00+00:00">
<link rel="canonical" href="/2026/09/25/cumbre?utm_source=rss">
</head><body>
<nav><a href="/">Inicio</a> <a href="/mundo">Mundo</a></nav>
<article>
<h1>Cumbre climática: acuerdo preliminar</h1>
<p>Los delegados de 190 países alcanzaron un acuerdo preliminar el jueves por la noche,
después de dos semanas de negociaciones intensas en la sede de la conferencia.</p>
<p>El texto prevé nuevas metas de reducción de emisiones para 2035 y un fondo de adaptación
para los países más vulnerables, según fuentes de la presidencia de la cumbre.</p>
<p>Las organizaciones ambientalistas calificaron el resultado de insuficiente, aunque
reconocieron avances en la financiación y en los mecanismos de verificación.</p>
</article>
<footer>© El Diario</footer>
</body></html>"#;

    #[test]
    fn should_extract_main_text_and_metadata() {
        let e = extract(ARTICLE, "https://eldiario.example/2026/09/25/cumbre");
        assert!(
            e.text.contains("acuerdo preliminar el jueves"),
            "{}",
            e.text
        );
        assert!(!e.text.contains("Inicio"), "nav removed: {}", e.text);
        assert!(!e.is_empty_text());
        assert_eq!(e.meta.lang.as_deref(), Some("es-MX"));
        assert_eq!(e.meta.site_name.as_deref(), Some("El Diario"));
        assert_eq!(e.meta.published.as_deref(), Some("2026-09-25T08:30:00Z"));
        assert_eq!(
            e.meta.canonical.as_deref(),
            Some("https://eldiario.example/2026/09/25/cumbre?utm_source=rss")
        );
        assert!(e.meta.title.as_deref().unwrap_or("").contains("Cumbre"));
    }

    #[test]
    fn should_flag_js_shells_as_empty() {
        let shell = r#"<html lang="en"><head><title>App</title></head><body><div id="root"></div><script>boot()</script></body></html>"#;
        let e = extract(shell, "https://spa.example/");
        assert!(e.is_empty_text());
        assert_eq!(e.meta.lang.as_deref(), Some("en"));
        assert_eq!(e.meta.title.as_deref(), Some("App"));
    }

    #[test]
    fn should_find_published_in_time_element() {
        let html = r#"<html><body><time datetime="2026-09-20">Sept 20</time></body></html>"#;
        assert_eq!(meta_published(html).as_deref(), Some("2026-09-20"));
    }

    #[test]
    fn should_ignore_non_http_canonicals() {
        let html = r#"<link rel="canonical" href="javascript:alert(1)">"#;
        assert_eq!(canonical_link(html, "https://a.example/x"), None);
        let other = r#"<link rel="canonical" href="https://other.example/x">"#;
        assert_eq!(canonical_link(other, "https://a.example/x"), None);
        let amp = r#"<link rel="canonical" href="https://www.a.example/x">"#;
        assert_eq!(
            canonical_link(amp, "https://amp.a.example/x").as_deref(),
            Some("https://www.a.example/x")
        );
    }
}
