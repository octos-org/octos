//! Standalone news fetcher binary.
//!
//! Reads JSON from stdin: `{"categories": ["tech", "world"], "language": "zh"}`
//! Fetches headlines from Google News RSS, Hacker News API, Yahoo News scraping,
//! Substack/Medium RSS, then deep-fetches top article content.
//! Writes JSON to stdout: `{"output": "...", "success": true}`
//!
//! No LLM calls, no octos-agent dependencies.

use std::collections::HashSet;
use std::io::Read as _;
use std::time::Duration;

use chrono::Utc;
use reqwest::blocking::Client;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Source types
// ---------------------------------------------------------------------------

enum SourceKind {
    /// Google News RSS -- titles are "Headline - Source", links are Google redirects.
    GoogleRss(&'static str),
    /// Generic RSS/Atom -- Substack, Medium, etc.
    GenericRss(&'static str),
    /// Yahoo News HTML -- fetch + scrape text (no article URL extraction).
    YahooHtml(&'static str),
    /// Hacker News Firebase API.
    HackerNewsApi,
}

struct SourceDef {
    name: &'static str,
    kind: SourceKind,
}

/// Result from fetching one source: headline text + discovered article URLs.
struct FetchResult {
    /// Formatted headlines text.
    text: String,
    /// Article URLs discovered (title, url) for deep-fetch phase.
    article_urls: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Category definitions
// ---------------------------------------------------------------------------

struct CategoryDef {
    name: &'static str,
    label_zh: &'static str,
    sources: &'static [SourceDef],
    /// Max articles to deep-fetch from this category.
    deep_fetch_limit: usize,
}

macro_rules! sources {
    ($($def:expr),+ $(,)?) => { &[$($def),+] };
}

const CATEGORIES: &[CategoryDef] = &[
    CategoryDef {
        name: "politics",
        label_zh: "美国政治",
        deep_fetch_limit: 3,
        sources: sources![
            SourceDef {
                name: "Google News",
                kind: SourceKind::GoogleRss(
                    "https://news.google.com/rss/topics/CAAqIggKIhxDQkFTRHdvSkwyMHZNRGxqTjNjd0VnSmxiaWdBUAE?hl=en-US&gl=US&ceid=US:en"
                )
            },
            SourceDef {
                name: "Yahoo News",
                kind: SourceKind::YahooHtml("https://news.yahoo.com/politics/")
            },
        ],
    },
    CategoryDef {
        name: "world",
        label_zh: "国际新闻",
        deep_fetch_limit: 3,
        sources: sources![
            SourceDef {
                name: "Google News",
                kind: SourceKind::GoogleRss(
                    "https://news.google.com/rss/topics/CAAqJggKIiBDQkFTRWdvSUwyMHZNRGx1YlY4U0FtVnVHZ0pWVXlnQVAB?hl=en-US&gl=US&ceid=US:en"
                )
            },
            SourceDef {
                name: "Yahoo News",
                kind: SourceKind::YahooHtml("https://news.yahoo.com/world/")
            },
        ],
    },
    CategoryDef {
        name: "business",
        label_zh: "商业财经",
        deep_fetch_limit: 3,
        sources: sources![
            SourceDef {
                name: "Google News",
                kind: SourceKind::GoogleRss(
                    "https://news.google.com/rss/topics/CAAqJggKIiBDQkFTRWdvSUwyMHZNRGx6TVdZU0FtVnVHZ0pWVXlnQVAB?hl=en-US&gl=US&ceid=US:en"
                )
            },
            SourceDef {
                name: "Yahoo News",
                kind: SourceKind::YahooHtml("https://news.yahoo.com/business/")
            },
        ],
    },
    CategoryDef {
        name: "technology",
        label_zh: "科技",
        deep_fetch_limit: 10,
        sources: sources![
            SourceDef {
                name: "Google News",
                kind: SourceKind::GoogleRss(
                    "https://news.google.com/rss/topics/CAAqJggKIiBDQkFTRWdvSUwyMHZNRGRqTVhZU0FtVnVHZ0pWVXlnQVAB?hl=en-US&gl=US&ceid=US:en"
                )
            },
            SourceDef {
                name: "Hacker News",
                kind: SourceKind::HackerNewsApi
            },
            SourceDef {
                name: "Substack",
                kind: SourceKind::GenericRss("https://newsletter.pragmaticengineer.com/feed")
            },
            SourceDef {
                name: "Substack (AI)",
                kind: SourceKind::GenericRss("https://www.oneusefulthing.org/feed")
            },
            SourceDef {
                name: "Medium",
                kind: SourceKind::GenericRss("https://medium.com/feed/tag/technology")
            },
            SourceDef {
                name: "Yahoo News",
                kind: SourceKind::YahooHtml("https://news.yahoo.com/technology/")
            },
        ],
    },
    CategoryDef {
        name: "science",
        label_zh: "科学",
        deep_fetch_limit: 3,
        sources: sources![
            SourceDef {
                name: "Google News",
                kind: SourceKind::GoogleRss(
                    "https://news.google.com/rss/topics/CAAqJggKIiBDQkFTRWdvSUwyMHZNRFp0Y1RjU0FtVnVHZ0pWVXlnQVAB?hl=en-US&gl=US&ceid=US:en"
                )
            },
            SourceDef {
                name: "Yahoo News",
                kind: SourceKind::YahooHtml("https://news.yahoo.com/science/")
            },
        ],
    },
    CategoryDef {
        name: "entertainment",
        label_zh: "社会娱乐",
        deep_fetch_limit: 2,
        sources: sources![
            SourceDef {
                name: "Google News",
                kind: SourceKind::GoogleRss(
                    "https://news.google.com/rss/topics/CAAqJggKIiBDQkFTRWdvSUwyMHZNREpxYW5RU0FtVnVHZ0pWVXlnQVAB?hl=en-US&gl=US&ceid=US:en"
                )
            },
            SourceDef {
                name: "Yahoo News",
                kind: SourceKind::YahooHtml("https://news.yahoo.com/entertainment/")
            },
        ],
    },
    CategoryDef {
        name: "health",
        label_zh: "健康",
        deep_fetch_limit: 2,
        sources: sources![
            SourceDef {
                name: "Google News",
                kind: SourceKind::GoogleRss(
                    "https://news.google.com/rss/topics/CAAqIQgKIhtDQkFTRGdvSUwyMHZNR3QwTlRFU0FtVnVLQUFQAQ?hl=en-US&gl=US&ceid=US:en"
                )
            },
            SourceDef {
                name: "Yahoo News",
                kind: SourceKind::YahooHtml("https://news.yahoo.com/health/")
            },
        ],
    },
    CategoryDef {
        name: "sports",
        label_zh: "体育",
        deep_fetch_limit: 2,
        sources: sources![
            SourceDef {
                name: "Google News",
                kind: SourceKind::GoogleRss(
                    "https://news.google.com/rss/topics/CAAqJggKIiBDQkFTRWdvSUwyMHZNRFp1ZEdvU0FtVnVHZ0pWVXlnQVAB?hl=en-US&gl=US&ceid=US:en"
                )
            },
            SourceDef {
                name: "Yahoo Sports",
                kind: SourceKind::YahooHtml("https://sports.yahoo.com/")
            },
        ],
    },
];

const ALIASES: &[(&str, &str)] = &[
    ("tech", "technology"),
    ("commerce", "business"),
    ("international", "world"),
    ("social", "entertainment"),
];

/// Max chars per HTML source page.
const MAX_SOURCE_CHARS: usize = 12_000;
/// Max chars per deep-fetched article.
const MAX_ARTICLE_CHARS: usize = 8_000;
/// Max HN stories to fetch in discovery phase.
const HN_TOP_STORIES: usize = 30;
/// Max RSS items to parse per feed.
const MAX_RSS_ITEMS: usize = 30;
/// Global cap on total deep-fetch articles (across all categories).
const MAX_DEEP_FETCH_TOTAL: usize = 20;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    language: Option<String>,
}

#[derive(Serialize)]
struct Output {
    output: String,
    success: bool,
}

// ---------------------------------------------------------------------------
// Category resolution
// ---------------------------------------------------------------------------

fn resolve_alias(name: &str) -> &str {
    let lower = name.to_lowercase();
    for &(alias, canonical) in ALIASES {
        if lower == alias {
            return canonical;
        }
    }
    for cat in CATEGORIES {
        if lower == cat.name {
            return cat.name;
        }
    }
    ""
}

fn find_category(name: &str) -> Option<&'static CategoryDef> {
    let canonical = resolve_alias(name);
    CATEGORIES.iter().find(|c| c.name == canonical)
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

fn build_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
             AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        )
        .build()
        .expect("failed to build HTTP client")
}

// ---------------------------------------------------------------------------
// Phase 1: Discovery (headlines + article URLs)
// ---------------------------------------------------------------------------

fn fetch_source(client: &Client, source: &SourceDef) -> Option<(&'static str, FetchResult)> {
    let result = match &source.kind {
        SourceKind::GoogleRss(url) => fetch_google_rss(client, url),
        SourceKind::GenericRss(url) => fetch_generic_rss(client, url),
        SourceKind::YahooHtml(url) => fetch_html_page(client, url),
        SourceKind::HackerNewsApi => fetch_hackernews(client),
    };
    match result {
        Ok(r) => Some((source.name, r)),
        Err(e) => {
            eprintln!("[warn] failed to fetch {}: {e}", source.name);
            None
        }
    }
}

/// Google News RSS -- titles are "Headline - Source", links are redirect URLs.
fn fetch_google_rss(client: &Client, url: &str) -> Result<FetchResult, String> {
    let response = client
        .get(url)
        .send()
        .map_err(|e| format!("RSS fetch failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("RSS HTTP {}", response.status()));
    }
    let xml = response
        .text()
        .map_err(|e| format!("failed to read RSS body: {e}"))?;

    let mut text = String::new();
    let mut urls = Vec::new();
    for (i, chunk) in xml.split("<item>").skip(1).enumerate() {
        if i >= MAX_RSS_ITEMS {
            break;
        }
        if let Some(title) = extract_xml_tag(chunk, "title") {
            let title = decode_xml_entities(&title);
            text.push_str(&format!("{}. {}\n", i + 1, title));
            if let Some(link) = extract_xml_tag(chunk, "link") {
                let link = decode_xml_entities(&link);
                urls.push((title, link));
            }
        }
    }
    if text.is_empty() {
        return Err("no items found in RSS".to_string());
    }
    Ok(FetchResult {
        text,
        article_urls: urls,
    })
}

/// Generic RSS/Atom -- for Substack, Medium, etc.
fn fetch_generic_rss(client: &Client, url: &str) -> Result<FetchResult, String> {
    let response = client
        .get(url)
        .send()
        .map_err(|e| format!("RSS fetch failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("RSS HTTP {}", response.status()));
    }
    let xml = response
        .text()
        .map_err(|e| format!("failed to read RSS body: {e}"))?;

    let mut text = String::new();
    let mut urls = Vec::new();
    let mut i = 0;

    // Try RSS <item> format first, then Atom <entry> format
    let (item_split, link_tag) = if xml.contains("<item>") {
        ("<item>", "link")
    } else if xml.contains("<entry>") {
        ("<entry>", "id") // Atom uses <id> for URL, or <link href="..."/>
    } else {
        return Err("unrecognized feed format".to_string());
    };

    for chunk in xml.split(item_split).skip(1) {
        if i >= MAX_RSS_ITEMS {
            break;
        }
        if let Some(title) = extract_xml_tag(chunk, "title") {
            let title = decode_xml_entities(&title);
            // Try to extract description/summary for richer context
            let desc = extract_xml_tag(chunk, "description")
                .or_else(|| extract_xml_tag(chunk, "summary"))
                .map(|d| {
                    let d = decode_xml_entities(&d);
                    extract_text_fallback(&d)
                });

            text.push_str(&format!("{}. {}", i + 1, title));
            if let Some(ref desc) = desc {
                let short: String = desc.chars().take(200).collect();
                text.push_str(&format!(" -- {short}"));
            }
            text.push('\n');

            // Extract link
            let link = extract_xml_tag(chunk, link_tag).or_else(|| extract_atom_link_href(chunk));
            if let Some(link) = link {
                let link = decode_xml_entities(&link);
                urls.push((title, link));
            }
            i += 1;
        }
    }
    if text.is_empty() {
        return Err("no items found in feed".to_string());
    }
    Ok(FetchResult {
        text,
        article_urls: urls,
    })
}

/// Hacker News API -- structured data with scores.
fn fetch_hackernews(client: &Client) -> Result<FetchResult, String> {
    let ids: Vec<u64> = client
        .get("https://hacker-news.firebaseio.com/v0/topstories.json")
        .send()
        .map_err(|e| format!("HN API failed: {e}"))?
        .json()
        .map_err(|e| format!("HN JSON parse failed: {e}"))?;

    let top_ids = &ids[..ids.len().min(HN_TOP_STORIES)];

    let mut text = String::new();
    let mut urls = Vec::new();

    for (i, &id) in top_ids.iter().enumerate() {
        let url = format!("https://hacker-news.firebaseio.com/v0/item/{id}.json");
        let item: serde_json::Value = match client.get(&url).send() {
            Ok(resp) => match resp.json() {
                Ok(v) => v,
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        let title = item["title"].as_str().unwrap_or("(untitled)").to_string();
        let score = item["score"].as_u64().unwrap_or(0);
        let item_url = item["url"].as_str().unwrap_or("").to_string();
        let descendants = item["descendants"].as_u64().unwrap_or(0);

        text.push_str(&format!(
            "{}. [{}pts, {}comments] {}",
            i + 1,
            score,
            descendants,
            title
        ));
        if !item_url.is_empty() {
            text.push_str(&format!(" ({})", item_url));
            urls.push((title.clone(), item_url));
        }
        text.push('\n');
    }

    Ok(FetchResult {
        text,
        article_urls: urls,
    })
}

/// Yahoo/HTML page -- extract text via scraper.
fn fetch_html_page(client: &Client, url: &str) -> Result<FetchResult, String> {
    let response = client
        .get(url)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let html = response
        .text()
        .map_err(|e| format!("failed to read body: {e}"))?;
    let cleaned = strip_scripts(&html);
    let mut text = html_to_text(&cleaned);
    truncate_utf8(&mut text, MAX_SOURCE_CHARS);
    Ok(FetchResult {
        text,
        article_urls: vec![],
    })
}

// ---------------------------------------------------------------------------
// Phase 1.5: Fetch all sources for a category
// ---------------------------------------------------------------------------

fn fetch_category(
    client: &Client,
    cat: &'static CategoryDef,
) -> (String, String, Vec<(String, String)>) {
    let mut combined = String::new();
    let mut all_urls = Vec::new();

    for source in cat.sources {
        if let Some((source_name, result)) = fetch_source(client, source) {
            eprintln!(
                "[info] fetched {}/{}: {} chars, {} URLs",
                cat.name,
                source_name,
                result.text.len(),
                result.article_urls.len()
            );
            combined.push_str(&format!("--- {source_name} ---\n{}\n\n", result.text));
            all_urls.extend(result.article_urls);
        }
    }

    (cat.label_zh.to_string(), combined, all_urls)
}

// ---------------------------------------------------------------------------
// Phase 2: Deep fetch top articles
// ---------------------------------------------------------------------------

fn deep_fetch_article(client: &Client, title: &str, url: &str) -> Option<String> {
    let response = client
        .get(url)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let html = response.text().ok()?;
    let cleaned = strip_scripts(&html);
    let mut text = html_to_text(&cleaned);
    truncate_utf8(&mut text, MAX_ARTICLE_CHARS);

    // Skip if too short (likely a paywall or redirect)
    if text.len() < 200 {
        return None;
    }

    Some(format!("### {title}\n_Source: {url}_\n\n{text}"))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    // Read JSON input from stdin
    let mut input_str = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input_str) {
        let out = Output {
            output: format!("failed to read stdin: {e}"),
            success: false,
        };
        println!("{}", serde_json::to_string(&out).unwrap());
        std::process::exit(1);
    }

    let input: Input = match serde_json::from_str(&input_str) {
        Ok(v) => v,
        Err(e) => {
            let out = Output {
                output: format!("invalid JSON input: {e}"),
                success: false,
            };
            println!("{}", serde_json::to_string(&out).unwrap());
            std::process::exit(1);
        }
    };

    let language = input.language.clone().unwrap_or_else(|| "zh".into());
    let client = build_client();

    // Resolve target categories
    let targets: Vec<&CategoryDef> = if input.categories.is_empty() {
        CATEGORIES.iter().collect()
    } else {
        let mut resolved: Vec<&CategoryDef> = Vec::new();
        for name in &input.categories {
            if let Some(cat) = find_category(name) {
                if !resolved.iter().any(|c| c.name == cat.name) {
                    resolved.push(cat);
                }
            } else {
                eprintln!("[warn] unknown news category: {name}");
            }
        }
        if resolved.is_empty() {
            let out = Output {
                output: format!(
                    "No valid categories found. Available: {}",
                    CATEGORIES
                        .iter()
                        .map(|c| c.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                success: false,
            };
            println!("{}", serde_json::to_string(&out).unwrap());
            std::process::exit(0);
        }
        resolved
    };

    // ---- Phase 1: Discover headlines ----
    let total_sources: usize = targets.iter().map(|c| c.sources.len()).sum();
    eprintln!(
        "[info] Phase 1: discovering headlines from {} categories ({} sources)",
        targets.len(),
        total_sources
    );

    let mut headlines: Vec<(String, String)> = Vec::new();
    let mut category_urls: Vec<(&CategoryDef, Vec<(String, String)>)> = Vec::new();

    for cat in &targets {
        let (label, text, urls) = fetch_category(&client, cat);
        if !text.is_empty() {
            headlines.push((label, text));
            if !urls.is_empty() {
                category_urls.push((cat, urls));
            }
        }
    }

    if headlines.is_empty() {
        let out = Output {
            output: "Failed to fetch any news sources.".to_string(),
            success: false,
        };
        println!("{}", serde_json::to_string(&out).unwrap());
        std::process::exit(0);
    }

    // ---- Phase 2: Deep fetch top articles ----
    let mut deep_fetch_targets: Vec<(String, String)> = Vec::new();
    let mut seen_urls: HashSet<String> = HashSet::new();

    for (cat, urls) in &category_urls {
        let limit = cat.deep_fetch_limit;
        let mut added = 0;
        for (title, url) in urls.iter() {
            if added >= limit {
                break;
            }
            // Skip Google News redirect URLs -- they return a JS-rendered
            // intermediate page, not the actual article content.
            if url.contains("news.google.com/") {
                continue;
            }
            if seen_urls.contains(url.as_str()) {
                continue;
            }
            if !url.starts_with("http") {
                continue;
            }
            seen_urls.insert(url.clone());
            deep_fetch_targets.push((title.clone(), url.clone()));
            added += 1;
            if deep_fetch_targets.len() >= MAX_DEEP_FETCH_TOTAL {
                break;
            }
        }
        if deep_fetch_targets.len() >= MAX_DEEP_FETCH_TOTAL {
            break;
        }
    }

    let deep_content = if !deep_fetch_targets.is_empty() {
        eprintln!(
            "[info] Phase 2: deep-fetching {} articles",
            deep_fetch_targets.len()
        );

        let mut articles: Vec<String> = Vec::new();
        for (i, (title, url)) in deep_fetch_targets.iter().enumerate() {
            eprintln!(
                "[info]   deep-fetch [{}/{}]: {} -- {}",
                i + 1,
                deep_fetch_targets.len(),
                title,
                url
            );
            if let Some(content) = deep_fetch_article(&client, title, url) {
                articles.push(content);
            }
        }
        eprintln!(
            "[info] Phase 2: got {} articles with content",
            articles.len()
        );
        articles.join("\n\n---\n\n")
    } else {
        String::new()
    };

    // ---- Build output ----
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let mut output = String::new();

    output.push_str(&format!("# Raw News Data -- {today}\n"));
    output.push_str(&format!("Language: {language}\n\n"));

    output.push_str("## HEADLINES BY CATEGORY\n\n");
    for (label, text) in &headlines {
        output.push_str(&format!("=== {label} ===\n{text}\n\n"));
    }

    if !deep_content.is_empty() {
        output.push_str("## FULL ARTICLE CONTENT (top stories)\n\n");
        output.push_str(&deep_content);
        output.push('\n');
    }

    let out = Output {
        output,
        success: true,
    };
    println!("{}", serde_json::to_string(&out).unwrap());
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract content of an XML tag (non-nested, first occurrence).
fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

/// Extract href from Atom-style `<link href="..." />` or `<link rel="alternate" href="..."/>`.
fn extract_atom_link_href(xml: &str) -> Option<String> {
    let link_start = xml.find("<link")?;
    let chunk = &xml[link_start..];
    let tag_end = chunk.find('>')?;
    let tag = &chunk[..tag_end];
    let href_start = tag.find("href=\"")? + 6;
    let href_end = tag[href_start..].find('"')? + href_start;
    Some(tag[href_start..href_end].to_string())
}

/// Decode common XML entities and strip CDATA wrappers.
fn decode_xml_entities(s: &str) -> String {
    let s = s.trim();
    // Strip CDATA wrapper: <![CDATA[...]]>
    let s = s
        .strip_prefix("<![CDATA[")
        .and_then(|inner| inner.strip_suffix("]]>"))
        .unwrap_or(s);
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

/// Strip `<script>` and `<style>` tags and their content from HTML.
fn strip_scripts(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let lower = html.to_lowercase();
    let bytes = html.as_bytes();
    let lower_bytes = lower.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if i + 7 < lower_bytes.len() && &lower_bytes[i..i + 7] == b"<script" {
            if let Some(end) = lower[i..].find("</script>") {
                i += end + 9;
                continue;
            }
        }
        if i + 6 < lower_bytes.len() && &lower_bytes[i..i + 6] == b"<style" {
            if let Some(end) = lower[i..].find("</style>") {
                i += end + 8;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Convert HTML to plain text using the `scraper` crate.
fn html_to_text(html: &str) -> String {
    let document = Html::parse_document(html);

    // Try to find article/main content first for better extraction
    let selectors = [
        "article",
        "main",
        "[role=\"main\"]",
        ".content",
        "#content",
        "body",
    ];

    for sel_str in &selectors {
        if let Ok(selector) = Selector::parse(sel_str) {
            let elements: Vec<_> = document.select(&selector).collect();
            if !elements.is_empty() {
                let mut text = String::new();
                for el in elements {
                    collect_text(&el, &mut text);
                }
                let cleaned = normalize_whitespace(&text);
                if cleaned.len() > 100 {
                    return cleaned;
                }
            }
        }
    }

    // Fallback: extract all text
    let mut text = String::new();
    collect_text(&document.root_element(), &mut text);
    normalize_whitespace(&text)
}

/// Recursively collect text content from an element, adding newlines for block elements.
fn collect_text(element: &scraper::ElementRef, out: &mut String) {
    for child in element.children() {
        if let Some(text_node) = child.value().as_text() {
            out.push_str(text_node);
        } else if let Some(el) = child.value().as_element() {
            let tag = el.name();
            let is_block = matches!(
                tag,
                "p" | "div"
                    | "br"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "li"
                    | "tr"
                    | "blockquote"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
            );
            if is_block {
                out.push('\n');
            }
            if let Some(child_ref) = scraper::ElementRef::wrap(child) {
                collect_text(&child_ref, out);
            }
            if is_block {
                out.push('\n');
            }
        }
    }
}

/// Normalize whitespace: collapse runs of whitespace, trim lines, remove excessive blank lines.
fn normalize_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut blank_lines = 0;

    for line in s.lines() {
        let trimmed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if trimmed.is_empty() {
            blank_lines += 1;
            if blank_lines <= 1 {
                result.push('\n');
            }
        } else {
            blank_lines = 0;
            result.push_str(&trimmed);
            result.push('\n');
        }
    }
    result.trim().to_string()
}

/// Simple HTML tag stripper as fallback.
fn extract_text_fallback(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
            result.push(' ');
        } else if !in_tag {
            result.push(c);
        }
    }
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate a string to at most `max_chars` on a valid UTF-8 boundary,
/// appending a truncation marker if shortened.
fn truncate_utf8(s: &mut String, max_chars: usize) {
    if s.len() <= max_chars {
        return;
    }
    let marker = "\n...(truncated)";
    let budget = max_chars.saturating_sub(marker.len());
    // Find a valid char boundary at or before `budget`
    let mut end = budget;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str(marker);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_alias() {
        assert_eq!(resolve_alias("tech"), "technology");
        assert_eq!(resolve_alias("Tech"), "technology");
        assert_eq!(resolve_alias("commerce"), "business");
        assert_eq!(resolve_alias("international"), "world");
        assert_eq!(resolve_alias("social"), "entertainment");
        assert_eq!(resolve_alias("politics"), "politics");
        assert_eq!(resolve_alias("unknown"), "");
    }

    #[test]
    fn test_find_category() {
        let cat = find_category("tech").unwrap();
        assert_eq!(cat.name, "technology");
        assert_eq!(cat.label_zh, "科技");
        assert!(cat.sources.len() >= 4);

        let cat = find_category("international").unwrap();
        assert_eq!(cat.name, "world");
        assert_eq!(cat.label_zh, "国际新闻");

        assert!(find_category("nonexistent").is_none());
    }

    #[test]
    fn test_all_categories_have_sources() {
        for cat in CATEGORIES {
            assert!(!cat.name.is_empty());
            assert!(!cat.label_zh.is_empty());
            assert!(!cat.sources.is_empty(), "{} has no sources", cat.name);
            assert!(
                cat.deep_fetch_limit > 0,
                "{} has 0 deep_fetch_limit",
                cat.name
            );
        }
    }

    #[test]
    fn test_extract_xml_tag() {
        let xml = "<title>Hello World - CNN</title><link>http://example.com</link>";
        assert_eq!(
            extract_xml_tag(xml, "title"),
            Some("Hello World - CNN".to_string())
        );
        assert_eq!(
            extract_xml_tag(xml, "link"),
            Some("http://example.com".to_string())
        );
        assert_eq!(extract_xml_tag(xml, "missing"), None);
    }

    #[test]
    fn test_extract_atom_link_href() {
        let xml = r#"<link rel="alternate" href="https://example.com/post"/>"#;
        assert_eq!(
            extract_atom_link_href(xml),
            Some("https://example.com/post".to_string())
        );

        let xml = r#"<link href="https://sub.example.com/feed" />"#;
        assert_eq!(
            extract_atom_link_href(xml),
            Some("https://sub.example.com/feed".to_string())
        );
    }

    #[test]
    fn test_decode_xml_entities() {
        assert_eq!(
            decode_xml_entities("Tom &amp; Jerry&#39;s &lt;show&gt;"),
            "Tom & Jerry's <show>"
        );
        assert_eq!(
            decode_xml_entities("<![CDATA[The Real Title]]>"),
            "The Real Title"
        );
        assert_eq!(
            decode_xml_entities("  <![CDATA[Spaced &amp; Raw]]>  "),
            "Spaced & Raw"
        );
        assert_eq!(decode_xml_entities("plain text"), "plain text");
    }

    #[test]
    fn test_strip_scripts() {
        let html = "<p>Hello</p><script>var x=1;</script><p>World</p><style>.a{}</style><p>!</p>";
        let cleaned = strip_scripts(html);
        assert_eq!(cleaned, "<p>Hello</p><p>World</p><p>!</p>");
    }

    #[test]
    fn test_extract_text_fallback() {
        let html = "<h1>Hello</h1><p>World <b>bold</b></p>";
        let text = extract_text_fallback(html);
        assert_eq!(text, "Hello World bold");
    }

    #[test]
    fn test_truncate_utf8() {
        let mut s = "Hello, world!".to_string();
        truncate_utf8(&mut s, 100);
        assert_eq!(s, "Hello, world!");

        let mut s = "Hello, world! This is a longer string for testing.".to_string();
        truncate_utf8(&mut s, 20);
        assert_eq!(s, "Hello\n...(truncated)");

        // Test with multibyte UTF-8 (must be longer than max_chars to trigger truncation)
        let mut s = "Hello 你好世界这是一个很长的中文字符串".to_string();
        truncate_utf8(&mut s, 22);
        assert!(s.ends_with("(truncated)"));
        // Should not panic or corrupt UTF-8
        assert!(std::str::from_utf8(s.as_bytes()).is_ok());
    }

    #[test]
    fn test_normalize_whitespace() {
        let input = "  Hello   world  \n\n\n\n  Foo  bar  ";
        let result = normalize_whitespace(input);
        assert_eq!(result, "Hello world\n\nFoo bar");
    }

    #[test]
    fn test_html_to_text() {
        let html =
            "<html><body><h1>Title</h1><p>Paragraph one.</p><p>Paragraph two.</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Paragraph one."));
        assert!(text.contains("Paragraph two."));
    }
}
