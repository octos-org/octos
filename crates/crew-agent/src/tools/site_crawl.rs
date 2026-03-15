//! Deep crawl tool: CDP-based recursive site crawler.
//!
//! Launches an ephemeral headless Chrome, performs BFS crawl starting from a seed URL,
//! extracts rendered text from each page, and saves results to disk.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chromiumoxide::Page;
use chromiumoxide::browser::{Browser, BrowserConfig};
use eyre::{Result, WrapErr};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use super::ssrf::check_ssrf;
use super::{Tool, ToolResult};
use crate::sandbox::BLOCKED_ENV_VARS;

const MAX_OUTPUT_CHARS: usize = 50_000;
const PAGE_SETTLE_MS: u64 = 3000;
const PAGE_SETTLE_RETRY_MS: u64 = 5000;
const NAV_TIMEOUT_SECS: u64 = 30;
const MAX_PAGE_TEXT_CHARS: usize = 200_000;
const PREVIEW_CHARS: usize = 2000;
/// Minimum meaningful text length — pages shorter than this are likely empty/blocked.
const MIN_USEFUL_TEXT_LEN: usize = 200;
/// Maximum retries for near-empty pages.
const MAX_EMPTY_RETRIES: u32 = 2;

/// Common user agent to avoid headless detection.
const STEALTH_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// CDP-based recursive site crawler.
pub struct DeepCrawlTool {
    research_base: PathBuf,
    config: Option<Arc<super::tool_config::ToolConfigStore>>,
}

impl DeepCrawlTool {
    #[cfg(test)]
    pub fn new(research_base: impl Into<PathBuf>) -> Self {
        Self {
            research_base: research_base.into(),
            config: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    url: String,
    max_depth: u32,
    max_pages: u32,
    #[serde(default)]
    path_prefix: Option<String>,
}

struct CrawledPage {
    url: String,
    depth: u32,
    text: String,
    links: Vec<String>,
    error: Option<String>,
}

/// Launch an ephemeral Chrome browser session.
async fn launch_browser() -> Result<(
    Browser,
    Page,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
)> {
    let temp_dir = tempfile::Builder::new()
        .prefix("crew-crawl-")
        .tempdir()
        .wrap_err("failed to create temp dir for Chrome")?;

    let mut builder = BrowserConfig::builder()
        .user_data_dir(temp_dir.path())
        .arg("--disable-dev-shm-usage")
        .arg("--disable-extensions")
        .arg("--disable-background-networking")
        // Stealth: avoid headless detection by bot-protection services
        .arg("--disable-blink-features=AutomationControlled")
        .arg(format!("--user-agent={STEALTH_USER_AGENT}"))
        .arg("--disable-features=AutomationControlled")
        .arg("--disable-infobars");

    for var in BLOCKED_ENV_VARS {
        builder = builder.env(*var, "");
    }

    let config = builder
        .build()
        .map_err(|e| eyre::eyre!("failed to build browser config: {e}"))?;

    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| eyre::eyre!("failed to launch Chrome: {e}"))?;

    let handle = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| eyre::eyre!("failed to create page: {e}"))?;

    Ok((browser, page, handle, temp_dir))
}

/// JS to remove automation indicators (navigator.webdriver, etc.)
const STEALTH_JS: &str = r#"
    Object.defineProperty(navigator, 'webdriver', { get: () => undefined });
    Object.defineProperty(navigator, 'languages', { get: () => ['en-US', 'en'] });
    Object.defineProperty(navigator, 'plugins', { get: () => [1, 2, 3, 4, 5] });
    window.chrome = { runtime: {} };
"#;

/// JS to extract links from the page.
const EXTRACT_LINKS_JS: &str = r#"
    (() => {
        const urls = new Set();
        // Standard <a href> links
        document.querySelectorAll('a[href]').forEach(a => {
            if (a.href && !a.href.startsWith('javascript:') && !a.href.startsWith('mailto:'))
                urls.add(a.href);
        });
        // SPA router links (Vue router-link, React Link, etc.)
        document.querySelectorAll('[data-href], [data-url], [data-link]').forEach(el => {
            const href = el.getAttribute('data-href')
                || el.getAttribute('data-url')
                || el.getAttribute('data-link');
            if (href) {
                try { urls.add(new URL(href, location.origin).href); } catch {}
            }
        });
        return Array.from(urls);
    })()
"#;

/// Extract innerText from the page.
async fn extract_text(page: &Page) -> Result<String, String> {
    match page.evaluate("document.body.innerText").await {
        Ok(result) => {
            let mut t = result.into_value::<String>().unwrap_or_default();
            if t.len() > MAX_PAGE_TEXT_CHARS {
                crew_core::truncate_utf8(&mut t, MAX_PAGE_TEXT_CHARS, "\n\n... (truncated)");
            }
            Ok(t)
        }
        Err(e) => Err(format!("Failed to extract text: {e}")),
    }
}

/// Check if text looks like a bot-protection page.
fn is_bot_blocked(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("performing security verification")
        || lower.contains("press & hold to confirm you are")
        || lower.contains("please verify you are a human")
        || lower.contains("checking your browser")
        || lower.contains("just a moment...")
        || lower.contains("attention required! | cloudflare")
        || lower.contains("enable javascript and cookies to continue")
}

/// Crawl a single page: navigate, wait for JS render, extract text and links.
/// Retries with longer wait if the page is near-empty or bot-blocked.
async fn crawl_single_page(page: &Page, url: &str, page_settle_ms: u64) -> CrawledPage {
    // Inject stealth JS before navigation
    let _ = page.evaluate(STEALTH_JS).await;

    // Navigate
    if let Err(e) = page.goto(url).await {
        return CrawledPage {
            url: url.to_string(),
            depth: 0,
            text: String::new(),
            links: vec![],
            error: Some(format!("Navigation failed: {e}")),
        };
    }

    // Wait for navigation + JS settle
    let _ = tokio::time::timeout(
        Duration::from_secs(NAV_TIMEOUT_SECS),
        page.wait_for_navigation(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(page_settle_ms)).await;

    // Re-inject stealth after navigation (some sites check post-load)
    let _ = page.evaluate(STEALTH_JS).await;

    // Extract text with retry for near-empty or bot-blocked pages
    let mut text = match extract_text(page).await {
        Ok(t) => t,
        Err(e) => {
            return CrawledPage {
                url: url.to_string(),
                depth: 0,
                text: String::new(),
                links: vec![],
                error: Some(e),
            };
        }
    };

    // Retry if page looks empty or bot-blocked
    for retry in 0..MAX_EMPTY_RETRIES {
        let trimmed_len = text.trim().len();
        if trimmed_len >= MIN_USEFUL_TEXT_LEN && !is_bot_blocked(&text) {
            break;
        }
        warn!(
            url = %url,
            text_len = trimmed_len,
            retry = retry + 1,
            bot_blocked = is_bot_blocked(&text),
            "page looks empty or bot-blocked, retrying with longer wait"
        );
        tokio::time::sleep(Duration::from_millis(PAGE_SETTLE_RETRY_MS)).await;
        text = match extract_text(page).await {
            Ok(t) => t,
            Err(_) => break,
        };
    }

    // Extract links
    let links = match page.evaluate(EXTRACT_LINKS_JS).await {
        Ok(result) => result.into_value::<Vec<String>>().unwrap_or_default(),
        Err(_) => vec![],
    };

    CrawledPage {
        url: url.to_string(),
        depth: 0,
        text,
        links,
        error: None,
    }
}

/// Normalize a URL: remove fragment, trailing slash, lowercase scheme+host.
fn normalize_url(url: &str) -> Option<String> {
    let mut parsed = reqwest::Url::parse(url).ok()?;
    parsed.set_fragment(None);
    let mut s = parsed.to_string();
    if s.ends_with('/') && s.len() > parsed.origin().ascii_serialization().len() + 1 {
        s.pop();
    }
    Some(s)
}

/// Generate a filesystem-safe slug from a hostname.
fn host_slug(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or("unknown");
    host.replace('.', "-")
}

/// Generate a filesystem-safe filename from a URL path.
fn page_slug(url: &reqwest::Url, index: usize) -> String {
    let path = url.path().trim_matches('/');
    let slug = if path.is_empty() {
        "index".to_string()
    } else {
        path.replace('/', "_")
            .replace(|c: char| !c.is_alphanumeric() && c != '_' && c != '-', "_")
    };
    // Truncate long slugs and prefix with index for ordering
    let truncated = if slug.len() > 80 { &slug[..80] } else { &slug };
    format!("{:03}_{truncated}", index)
}

#[async_trait]
impl Tool for DeepCrawlTool {
    fn name(&self) -> &str {
        "deep_crawl"
    }

    fn description(&self) -> &str {
        "Recursively crawl a website using a headless browser (CDP). Renders JavaScript, \
         follows same-origin links via BFS, extracts text from each page, and saves \
         results to disk. Ideal for JS-rendered SPAs and documentation sites. \
         IMPORTANT: Always ask the user how many pages (max_pages) and how deep (max_depth) \
         they want to crawl before calling this tool."
    }

    fn tags(&self) -> &[&str] {
        &["web"]
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Starting URL to crawl"
                },
                "max_depth": {
                    "type": "integer",
                    "description": "Maximum link-following depth (range: 1-10). Ask the user to choose."
                },
                "max_pages": {
                    "type": "integer",
                    "description": "Maximum number of pages to crawl (range: 1-100). Ask the user to choose."
                },
                "path_prefix": {
                    "type": "string",
                    "description": "Only follow links under this path prefix (e.g. /docs/)"
                }
            },
            "required": ["url", "max_depth", "max_pages"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid deep_crawl input")?;

        // Resolve configurable settings from config store
        let (cfg_page_settle, cfg_max_output) = match &self.config {
            Some(c) => (
                c.get_u64("deep_crawl", "page_settle_ms").await,
                c.get_usize("deep_crawl", "max_output_chars").await,
            ),
            None => (None, None),
        };
        let page_settle_ms = cfg_page_settle.unwrap_or(PAGE_SETTLE_MS);
        let max_output_chars = cfg_max_output.unwrap_or(MAX_OUTPUT_CHARS);

        let max_depth = input.max_depth.clamp(1, 10);
        let max_pages = input.max_pages.clamp(1, 100);

        // Validate seed URL
        let seed_url = match reqwest::Url::parse(&input.url) {
            Ok(u) => u,
            Err(_) => {
                return Ok(ToolResult {
                    output: "Invalid URL".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let scheme = seed_url.scheme();
        if scheme != "http" && scheme != "https" {
            return Ok(ToolResult {
                output: format!("Only http:// and https:// URLs are allowed, got {scheme}://"),
                success: false,
                ..Default::default()
            });
        }

        // SSRF check on seed URL
        if let Some(msg) = check_ssrf(&input.url).await {
            return Ok(ToolResult {
                output: msg,
                success: false,
                ..Default::default()
            });
        }

        let seed_origin = seed_url.origin().ascii_serialization();

        // Prepare output directory
        let crawl_dir = self
            .research_base
            .join(format!("crawl-{}", host_slug(&seed_url)));
        tokio::fs::create_dir_all(&crawl_dir)
            .await
            .wrap_err("failed to create crawl output directory")?;

        // Launch ephemeral browser
        let (mut browser, page, handler, _temp_dir) = match launch_browser().await {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("Failed to launch browser: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // BFS crawl
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, u32)> = VecDeque::new(); // (url, depth)
        let mut results: Vec<CrawledPage> = Vec::new();

        let seed_normalized = normalize_url(&input.url).unwrap_or_else(|| input.url.clone());
        visited.insert(seed_normalized.clone());
        queue.push_back((input.url.clone(), 0));

        info!(
            url = %input.url,
            max_depth,
            max_pages,
            path_prefix = ?input.path_prefix,
            "starting deep crawl"
        );

        while let Some((url, depth)) = queue.pop_front() {
            if results.len() >= max_pages as usize {
                break;
            }

            info!(
                url = %url,
                depth,
                visited = results.len(),
                "crawling page"
            );

            let mut crawled = crawl_single_page(&page, &url, page_settle_ms).await;
            crawled.depth = depth;

            // Enqueue discovered links
            if depth < max_depth {
                for link in &crawled.links {
                    if results.len() + queue.len() >= max_pages as usize {
                        break;
                    }

                    let normalized = match normalize_url(link) {
                        Some(n) => n,
                        None => continue,
                    };

                    if visited.contains(&normalized) {
                        continue;
                    }

                    // Same-origin check
                    let link_url = match reqwest::Url::parse(&normalized) {
                        Ok(u) => u,
                        Err(_) => continue,
                    };
                    if link_url.origin().ascii_serialization() != seed_origin {
                        continue;
                    }

                    // Path prefix filter
                    if let Some(ref prefix) = input.path_prefix {
                        if !link_url.path().starts_with(prefix) {
                            continue;
                        }
                    }

                    // SSRF check on discovered links
                    if check_ssrf(&normalized).await.is_some() {
                        continue;
                    }

                    visited.insert(normalized.clone());
                    queue.push_back((normalized, depth + 1));
                }
            }

            results.push(crawled);
        }

        // Shutdown browser
        let _ = browser.close().await;
        handler.abort();

        // Save results to disk and build output
        let mut output = format!(
            "# Deep Crawl: {}\nCrawled {} pages (max_depth: {}, max_pages: {})\n\n## Sitemap\n",
            input.url,
            results.len(),
            max_depth,
            max_pages
        );

        for (i, page) in results.iter().enumerate() {
            let status = if page.error.is_some() { "ERR" } else { "OK" };
            output.push_str(&format!(
                "{}. [depth={}] {} ({})\n",
                i + 1,
                page.depth,
                page.url,
                status
            ));
        }
        output.push('\n');

        for (i, crawled) in results.iter().enumerate() {
            // Save full content to disk
            let file_url = reqwest::Url::parse(&crawled.url).ok();
            let filename = file_url
                .as_ref()
                .map(|u| format!("{}.md", page_slug(u, i)))
                .unwrap_or_else(|| format!("{:03}_page.md", i));
            let file_path = crawl_dir.join(&filename);

            let file_content = if let Some(ref err) = crawled.error {
                format!("# {}\n\nError: {}\n", crawled.url, err)
            } else {
                format!("# {}\n\n{}\n", crawled.url, crawled.text)
            };

            if let Err(e) = tokio::fs::write(&file_path, &file_content).await {
                warn!(path = %file_path.display(), error = %e, "failed to write crawled page");
            }

            // Add preview to output
            output.push_str(&format!(
                "## Page {} [depth={}]: {}\n",
                i + 1,
                crawled.depth,
                crawled.url
            ));
            output.push_str(&format!("_Full content: {}_\n\n", file_path.display()));

            if let Some(ref err) = crawled.error {
                output.push_str(&format!("Error: {err}\n\n"));
            } else {
                let preview = if crawled.text.len() > PREVIEW_CHARS {
                    crew_core::truncated_utf8(&crawled.text, PREVIEW_CHARS, "...")
                } else {
                    crawled.text.clone()
                };
                output.push_str(&preview);
                output.push_str("\n\n");
            }
        }

        output.push_str(&format!(
            "{} pages saved to: {}\nUse read_file to examine specific pages.",
            results.len(),
            crawl_dir.display()
        ));

        // Truncate final output if needed
        crew_core::truncate_utf8(&mut output, max_output_chars, "\n\n... (truncated)");

        info!(
            pages = results.len(),
            dir = %crawl_dir.display(),
            "deep crawl complete"
        );

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_url_removes_fragment() {
        let result = normalize_url("https://example.com/docs#section").unwrap();
        assert_eq!(result, "https://example.com/docs");
    }

    #[test]
    fn test_normalize_url_removes_trailing_slash() {
        let result = normalize_url("https://example.com/docs/").unwrap();
        assert_eq!(result, "https://example.com/docs");
    }

    #[test]
    fn test_normalize_url_keeps_root_slash() {
        let result = normalize_url("https://example.com/").unwrap();
        assert_eq!(result, "https://example.com/");
    }

    #[test]
    fn test_host_slug() {
        let url = reqwest::Url::parse("https://www.example.com/docs").unwrap();
        assert_eq!(host_slug(&url), "www-example-com");
    }

    #[test]
    fn test_page_slug() {
        let url = reqwest::Url::parse("https://example.com/docs/install").unwrap();
        assert_eq!(page_slug(&url, 1), "001_docs_install");
    }

    #[test]
    fn test_page_slug_index() {
        let url = reqwest::Url::parse("https://example.com/").unwrap();
        assert_eq!(page_slug(&url, 0), "000_index");
    }

    #[test]
    fn test_page_slug_long_path() {
        let long_path = "a".repeat(200);
        let url = reqwest::Url::parse(&format!("https://example.com/{long_path}")).unwrap();
        let slug = page_slug(&url, 5);
        assert!(slug.len() <= 84); // 3 digits + _ + 80 chars
        assert!(slug.starts_with("005_"));
    }

    #[test]
    fn test_input_deserialization_required_fields() {
        let v = json!({
            "url": "https://example.com",
            "max_depth": 3,
            "max_pages": 20
        });
        let input: Input = serde_json::from_value(v).unwrap();
        assert_eq!(input.url, "https://example.com");
        assert_eq!(input.max_depth, 3);
        assert_eq!(input.max_pages, 20);
        assert!(input.path_prefix.is_none());
    }

    #[test]
    fn test_input_deserialization_with_path_prefix() {
        let v = json!({
            "url": "https://example.com",
            "max_depth": 5,
            "max_pages": 50,
            "path_prefix": "/docs/"
        });
        let input: Input = serde_json::from_value(v).unwrap();
        assert_eq!(input.max_depth, 5);
        assert_eq!(input.max_pages, 50);
        assert_eq!(input.path_prefix.as_deref(), Some("/docs/"));
    }

    #[test]
    fn test_input_missing_required_fields() {
        let v = json!({ "url": "https://example.com" });
        assert!(serde_json::from_value::<Input>(v).is_err());
    }

    #[tokio::test]
    async fn test_ssrf_blocked() {
        let tool = DeepCrawlTool::new("/tmp/test-crawl");
        let result = tool
            .execute(&json!({ "url": "http://127.0.0.1:8080", "max_depth": 1, "max_pages": 1 }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("private"));
    }

    #[tokio::test]
    async fn test_invalid_scheme() {
        let tool = DeepCrawlTool::new("/tmp/test-crawl");
        let result = tool
            .execute(&json!({ "url": "file:///etc/passwd", "max_depth": 1, "max_pages": 1 }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("Only http://"));
    }

    #[tokio::test]
    async fn test_invalid_url() {
        let tool = DeepCrawlTool::new("/tmp/test-crawl");
        let result = tool
            .execute(&json!({ "url": "not-a-url", "max_depth": 1, "max_pages": 1 }))
            .await
            .unwrap();
        assert!(!result.success);
    }
}
