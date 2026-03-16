use std::collections::HashSet;
use std::time::Duration;

const MAX_RSS_ITEMS: usize = 30;
const HN_TOP_STORIES: usize = 30;
const MAX_SOURCE_CHARS: usize = 12_000;
const MAX_ARTICLE_CHARS: usize = 8_000;

fn decode_xml_entities(s: &str) -> String {
    let s = s.trim();
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

fn extract_xml_tag(chunk: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = chunk.find(&open)? + open.len();
    let end = chunk[start..].find(&close)? + start;
    Some(chunk[start..end].to_string())
}

fn extract_atom_link_href(chunk: &str) -> Option<String> {
    let idx = chunk.find("<link")?;
    let tag_end = chunk[idx..].find('>')? + idx;
    let tag = &chunk[idx..=tag_end];
    if tag.contains("rel=\"alternate\"") || !tag.contains("rel=") {
        let href_start = tag.find("href=\"")? + 6;
        let href_end = tag[href_start..].find('"')? + href_start;
        return Some(tag[href_start..href_end].to_string());
    }
    None
}

fn strip_scripts(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let lower = html.to_lowercase();
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(pos) = lower[i..].find("<script") {
            result.push_str(&html[i..i + pos]);
            if let Some(end) = lower[i + pos..].find("</script>") {
                i = i + pos + end + 9;
            } else {
                break;
            }
        } else if let Some(pos) = lower[i..].find("<style") {
            result.push_str(&html[i..i + pos]);
            if let Some(end) = lower[i + pos..].find("</style>") {
                i = i + pos + end + 8;
            } else {
                break;
            }
        } else {
            result.push_str(&html[i..]);
            break;
        }
    }
    result
}

fn extract_text_fallback(html: &str) -> String {
    let mut r = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
            r.push(' ');
        } else if !in_tag {
            r.push(c);
        }
    }
    r.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_xml_entities() {
        assert_eq!(decode_xml_entities("&amp;"), "&");
        assert_eq!(decode_xml_entities("&lt;b&gt;"), "<b>");
        assert_eq!(decode_xml_entities("&quot;hi&quot;"), "\"hi\"");
        assert_eq!(decode_xml_entities("&#39;test&#39;"), "'test'");
        assert_eq!(decode_xml_entities("plain text"), "plain text");
    }

    #[test]
    fn test_decode_xml_entities_cdata() {
        assert_eq!(
            decode_xml_entities("<![CDATA[some content]]>"),
            "some content"
        );
    }

    #[test]
    fn test_extract_xml_tag() {
        let chunk = "<title>Hello World</title><link>https://example.com</link>";
        assert_eq!(extract_xml_tag(chunk, "title"), Some("Hello World".into()));
        assert_eq!(
            extract_xml_tag(chunk, "link"),
            Some("https://example.com".into())
        );
        assert_eq!(extract_xml_tag(chunk, "missing"), None);
    }

    #[test]
    fn test_extract_atom_link_href() {
        let chunk = r#"<link rel="alternate" href="https://example.com/post"/>"#;
        assert_eq!(
            extract_atom_link_href(chunk),
            Some("https://example.com/post".into())
        );
    }

    #[test]
    fn test_extract_atom_link_no_rel() {
        let chunk = r#"<link href="https://example.com/feed"/>"#;
        assert_eq!(
            extract_atom_link_href(chunk),
            Some("https://example.com/feed".into())
        );
    }

    #[test]
    fn test_strip_scripts() {
        let html = "before<script>alert('xss')</script>after";
        assert_eq!(strip_scripts(html), "beforeafter");
    }

    #[test]
    fn test_strip_styles() {
        let html = "<style>.x{}</style>content";
        assert_eq!(strip_scripts(html), "content");
    }

    #[test]
    fn test_strip_script_then_style() {
        // Script before style: both stripped
        let html = "a<script>code</script>b<style>.x{}</style>c";
        assert_eq!(strip_scripts(html), "abc");
    }

    #[test]
    fn test_strip_scripts_none() {
        assert_eq!(strip_scripts("plain text"), "plain text");
    }

    #[test]
    fn test_extract_text_fallback() {
        let html = "<p>Hello</p><div>World</div>";
        assert_eq!(extract_text_fallback(html), "Hello World");
    }

    #[test]
    fn test_extract_text_fallback_nested() {
        let html = "<div><span>Nested</span> <b>text</b></div>";
        assert_eq!(extract_text_fallback(html), "Nested text");
    }
}

#[tokio::test]
#[ignore] // Network-dependent: fetches live data from Google News, HN, Substack, Medium, Yahoo
async fn test_tech_news_discovery() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .build()
        .unwrap();

    println!("\n========================================");
    println!("Phase 1: DISCOVERY — Technology Sources");
    println!("========================================\n");

    let mut all_urls: Vec<(String, String)> = Vec::new();

    // --- Google News RSS ---
    {
        println!("--- Google News RSS (Technology) ---");
        let url = "https://news.google.com/rss/topics/CAAqJggKIiBDQkFTRWdvSUwyMHZNRGRqTVhZU0FtVnVHZ0pWVXlnQVAB?hl=en-US&gl=US&ceid=US:en";
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                let xml = resp.text().await.unwrap_or_default();
                let mut count = 0;
                for (i, chunk) in xml.split("<item>").skip(1).enumerate() {
                    if i >= MAX_RSS_ITEMS {
                        break;
                    }
                    if let Some(title) = extract_xml_tag(chunk, "title") {
                        let title = decode_xml_entities(&title);
                        println!("  {}. {}", i + 1, title);
                        if let Some(link) = extract_xml_tag(chunk, "link") {
                            all_urls.push((title, decode_xml_entities(&link)));
                        }
                        count += 1;
                    }
                }
                println!("  → {count} headlines (status: {status})\n");
            }
            Err(e) => println!("  FAILED: {e}\n"),
        }
    }

    // --- Hacker News API ---
    {
        println!("--- Hacker News API ---");
        match client
            .get("https://hacker-news.firebaseio.com/v0/topstories.json")
            .send()
            .await
        {
            Ok(resp) => {
                let ids: Vec<u64> = resp.json().await.unwrap_or_default();
                let top = &ids[..ids.len().min(HN_TOP_STORIES)];
                let fetches: Vec<_> = top
                    .iter()
                    .map(|&id| {
                        let c = &client;
                        async move {
                            let url =
                                format!("https://hacker-news.firebaseio.com/v0/item/{id}.json");
                            c.get(&url)
                                .send()
                                .await
                                .ok()?
                                .json::<serde_json::Value>()
                                .await
                                .ok()
                        }
                    })
                    .collect();
                let results = futures::future::join_all(fetches).await;
                for (i, item) in results.into_iter().flatten().enumerate() {
                    let title = item["title"].as_str().unwrap_or("(untitled)").to_string();
                    let score = item["score"].as_u64().unwrap_or(0);
                    let url = item["url"].as_str().unwrap_or("").to_string();
                    let comments = item["descendants"].as_u64().unwrap_or(0);
                    println!("  {}. [{}pts, {}c] {}", i + 1, score, comments, title);
                    if !url.is_empty() {
                        all_urls.push((title, url));
                    }
                }
                println!("  → {} stories\n", top.len());
            }
            Err(e) => println!("  FAILED: {e}\n"),
        }
    }

    // --- Substack (Pragmatic Engineer) ---
    {
        println!("--- Substack (Pragmatic Engineer) ---");
        let url = "https://newsletter.pragmaticengineer.com/feed";
        match client.get(url).send().await {
            Ok(resp) => {
                let xml = resp.text().await.unwrap_or_default();
                let mut count = 0;
                let item_split = if xml.contains("<item>") {
                    "<item>"
                } else {
                    "<entry>"
                };
                for (i, chunk) in xml.split(item_split).skip(1).enumerate() {
                    if i >= 10 {
                        break;
                    }
                    if let Some(title) = extract_xml_tag(chunk, "title") {
                        let title = decode_xml_entities(&title);
                        let link = extract_xml_tag(chunk, "link")
                            .or_else(|| extract_atom_link_href(chunk));
                        println!("  {}. {}", i + 1, title);
                        if let Some(link) = link {
                            all_urls.push((title, decode_xml_entities(&link)));
                        }
                        count += 1;
                    }
                }
                println!("  → {count} items\n");
            }
            Err(e) => println!("  FAILED: {e}\n"),
        }
    }

    // --- Substack (One Useful Thing / AI) ---
    {
        println!("--- Substack (One Useful Thing — AI) ---");
        let url = "https://www.oneusefulthing.org/feed";
        match client.get(url).send().await {
            Ok(resp) => {
                let xml = resp.text().await.unwrap_or_default();
                let mut count = 0;
                let item_split = if xml.contains("<item>") {
                    "<item>"
                } else {
                    "<entry>"
                };
                for (i, chunk) in xml.split(item_split).skip(1).enumerate() {
                    if i >= 10 {
                        break;
                    }
                    if let Some(title) = extract_xml_tag(chunk, "title") {
                        let title = decode_xml_entities(&title);
                        println!("  {}. {}", i + 1, title);
                        count += 1;
                    }
                }
                println!("  → {count} items\n");
            }
            Err(e) => println!("  FAILED: {e}\n"),
        }
    }

    // --- Medium (technology tag) ---
    {
        println!("--- Medium (technology) ---");
        let url = "https://medium.com/feed/tag/technology";
        match client.get(url).send().await {
            Ok(resp) => {
                let xml = resp.text().await.unwrap_or_default();
                let mut count = 0;
                let item_split = if xml.contains("<item>") {
                    "<item>"
                } else {
                    "<entry>"
                };
                for (i, chunk) in xml.split(item_split).skip(1).enumerate() {
                    if i >= 10 {
                        break;
                    }
                    if let Some(title) = extract_xml_tag(chunk, "title") {
                        let title = decode_xml_entities(&title);
                        println!("  {}. {}", i + 1, title);
                        count += 1;
                    }
                }
                println!("  → {count} items\n");
            }
            Err(e) => println!("  FAILED: {e}\n"),
        }
    }

    // --- Yahoo News (Technology) ---
    {
        println!("--- Yahoo News (Technology) ---");
        let url = "https://news.yahoo.com/technology/";
        match client
            .get(url)
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                let html = resp.text().await.unwrap_or_default();
                let cleaned = strip_scripts(&html);
                let mut text =
                    htmd::convert(&cleaned).unwrap_or_else(|_| extract_text_fallback(&cleaned));
                let orig_len = text.len();
                text.truncate(2000);
                println!(
                    "  Status: {status}, converted: {} chars (showing first 2000)",
                    orig_len
                );
                println!("  Preview:\n{text}\n");
            }
            Err(e) => println!("  FAILED: {e}\n"),
        }
    }

    // Deduplicate URLs and filter out Google News redirect URLs
    let mut seen = HashSet::new();
    all_urls.retain(|(_, url)| !url.contains("news.google.com/") && seen.insert(url.clone()));

    println!("========================================");
    println!("Phase 2: DEEP FETCH — Top 5 articles (no Google redirects)");
    println!("========================================\n");
    println!("Total unique direct article URLs: {}\n", all_urls.len());

    // Deep-fetch top 5
    let limit = 5.min(all_urls.len());
    for (i, (title, url)) in all_urls[..limit].iter().enumerate() {
        println!("--- [{}/{}] {} ---", i + 1, limit, title);
        println!("  URL: {url}");
        match client
            .get(url)
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    println!("  HTTP {status} — skipping\n");
                    continue;
                }
                let html = resp.text().await.unwrap_or_default();
                let cleaned = strip_scripts(&html);
                let mut text =
                    htmd::convert(&cleaned).unwrap_or_else(|_| extract_text_fallback(&cleaned));
                let orig_len = text.len();
                if text.len() < 200 {
                    println!("  Too short ({} chars) — likely paywall\n", text.len());
                    continue;
                }
                text.truncate(MAX_ARTICLE_CHARS);
                // Show preview
                let preview: String = text.chars().take(500).collect();
                println!("  {orig_len} chars (showing 500):");
                println!("  {preview}...\n");
            }
            Err(e) => println!("  FAILED: {e}\n"),
        }
    }

    println!("========================================");
    println!(
        "DONE — discovery found {} articles, deep-fetched {}",
        all_urls.len(),
        limit
    );
    println!("========================================");
}
