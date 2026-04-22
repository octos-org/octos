//! Deep search tool: web search + parallel crawl, saving results to disk.
//!
//! Saves each crawled page as a markdown file under `.octos/research/<query-slug>/`.
//! Returns a concise index so the LLM can selectively read files for synthesis.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use reqwest::Client;
use serde::Deserialize;

use crate::harness_events::emit_registered_progress_event;
use crate::tools::TOOL_CTX;

use super::web_search::WebSearchTool;
use super::{Tool, ToolResult};

pub struct DeepSearchTool {
    search: WebSearchTool,
    client: Client,
    /// Research output base directory (e.g. ~/.octos/research/ or a sub-agent's research_dir).
    research_base: PathBuf,
}

impl DeepSearchTool {
    pub fn new(research_base: impl Into<PathBuf>) -> Self {
        Self {
            search: WebSearchTool::new(),
            client: Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                .build()
                .unwrap_or_else(|_| Client::new()),
            research_base: research_base.into(),
        }
    }

    pub fn with_provider_keys(mut self, provider_keys: HashMap<String, String>) -> Self {
        self.search = self.search.with_provider_keys(provider_keys);
        self
    }

    /// Directory where research results are saved.
    fn research_dir(&self, slug: &str) -> PathBuf {
        self.research_base.join(slug)
    }
}

#[derive(Deserialize)]
struct Input {
    query: String,
    #[serde(default = "default_count")]
    count: u8,
    #[serde(default = "default_max_chars_per_page")]
    max_chars_per_page: usize,
}

fn default_count() -> u8 {
    5
}

fn default_max_chars_per_page() -> usize {
    20_000
}

#[async_trait]
impl Tool for DeepSearchTool {
    fn name(&self) -> &str {
        "deep_search"
    }

    fn description(&self) -> &str {
        "Search the web and crawl all result URLs in parallel. Saves each page as a markdown file under .octos/research/<query>/. Returns an index of saved files — use read_file to examine specific pages for synthesis."
    }

    fn tags(&self) -> &[&str] {
        &["web"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "count": {
                    "type": "integer",
                    "description": "Number of results to search and crawl (1-10, default: 5)"
                },
                "max_chars_per_page": {
                    "type": "integer",
                    "description": "Max characters to extract per page (default: 50000)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid deep_search input")?;

        let count = input.count.clamp(1, 10);
        let max_chars = input.max_chars_per_page.clamp(1000, 200_000);

        // Step 1: Search
        emit_deep_research_progress(
            "search",
            &format!("Searching: \"{}\"", input.query),
            Some(0.1),
        );
        let search_args = serde_json::json!({
            "query": input.query,
            "count": count
        });
        let search_result = self.search.execute(&search_args).await?;

        if !search_result.success {
            return Ok(search_result);
        }

        // Extract URLs from search results
        let urls = extract_urls(&search_result.output);

        if urls.is_empty() {
            emit_deep_research_progress("completion", "Deep search complete", Some(1.0));
            return Ok(search_result);
        }

        // Create output directory
        let slug = slugify(&input.query);
        let dir = self.research_dir(&slug);
        tokio::fs::create_dir_all(&dir)
            .await
            .wrap_err("failed to create research directory")?;

        // Step 2: Parallel fetch all URLs
        emit_deep_research_progress(
            "fetch",
            &format!("Fetching {} pages in parallel...", urls.len()),
            Some(0.4),
        );
        let fetches: Vec<_> = urls
            .iter()
            .map(|url| self.fetch_page(url, max_chars))
            .collect();

        let pages = futures::future::join_all(fetches).await;

        // Step 3: Save search results summary
        let search_file = dir.join("_search_results.md");
        tokio::fs::write(&search_file, &search_result.output)
            .await
            .wrap_err("failed to write search results")?;

        // Step 4: Save full content to disk, return truncated preview inline
        // This keeps the LLM context small while full data is on disk.
        emit_deep_research_progress("report_build", "Building research index...", Some(0.8));
        const INLINE_CHARS_PER_PAGE: usize = 3000;

        let mut output = search_result.output;
        output.push_str("\n---\n\n");

        let mut saved_count = 0u32;
        let mut saved_files = Vec::new();
        for (i, (url, page)) in urls.iter().zip(pages.iter()).enumerate() {
            let filename = format!("{:02}_{}.md", i + 1, host_slug(url));
            let filepath = dir.join(&filename);

            match page {
                Ok(content) if !content.is_empty() => {
                    // Save full content to disk
                    let page_content = format!("---\nurl: {url}\n---\n\n{content}");
                    let _ = tokio::fs::write(&filepath, &page_content).await;
                    saved_count += 1;
                    saved_files.push(format!("  - {} ({})", filepath.display(), url));

                    // Return truncated preview inline to keep context small
                    output.push_str(&format!("## Source [{}]: {}\n", i + 1, url));
                    output.push_str(&format!("_Full content: {}_\n\n", filepath.display()));
                    let mut preview = content.clone();
                    octos_core::truncate_utf8(
                        &mut preview,
                        INLINE_CHARS_PER_PAGE,
                        "\n... (truncated, use read_file for full content)",
                    );
                    output.push_str(&preview);
                    output.push_str("\n\n---\n\n");
                }
                Ok(_) => {}
                Err(e) => {
                    let err_content = format!("---\nurl: {url}\nerror: {e}\n---\n");
                    let _ = tokio::fs::write(&filepath, &err_content).await;
                }
            }
        }

        output.push_str(&format!(
            "{saved_count} pages crawled and saved to: {}\n\nSaved files:\n{}\n\n\
            Use read_file to get full content from specific sources for detailed synthesis.\n",
            dir.display(),
            saved_files.join("\n")
        ));

        emit_deep_research_progress("completion", "Deep search complete", Some(1.0));

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }
}

fn emit_deep_research_progress(phase: &str, message: &str, progress: Option<f64>) {
    if let Ok(Some(sink)) = TOOL_CTX.try_with(|ctx| ctx.harness_event_sink.clone()) {
        let _ =
            emit_registered_progress_event(sink, Some("deep_research"), phase, message, progress);
    }
}

impl DeepSearchTool {
    async fn fetch_page(&self, url: &str, max_chars: usize) -> Result<String> {
        // SSRF check
        if let Ok(parsed) = reqwest::Url::parse(url) {
            if let Some(host) = parsed.host_str() {
                if super::ssrf::is_private_host(host) {
                    return Ok(String::new());
                }
                let port = parsed.port_or_known_default().unwrap_or(443);
                if let Ok(addrs) = tokio::net::lookup_host(format!("{host}:{port}")).await {
                    for addr in addrs {
                        if super::ssrf::is_private_ip(&addr.ip()) {
                            return Ok(String::new());
                        }
                    }
                }
            }
        }

        let response = self.client.get(url).send().await.wrap_err("fetch failed")?;

        if !response.status().is_success() {
            eyre::bail!("HTTP {}", response.status());
        }

        let body = response.text().await.wrap_err("read body failed")?;

        // Convert HTML to markdown
        let mut content = htmd::convert(&body).unwrap_or_else(|_| extract_text_simple(&body));

        octos_core::truncate_utf8(&mut content, max_chars, "\n... (truncated)");

        Ok(content)
    }
}

/// Convert a query string to a filesystem-safe slug.
fn slugify(s: &str) -> String {
    let mut slug = String::with_capacity(s.len());
    for ch in s.chars().take(60) {
        if ch.is_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if (ch == ' ' || ch == '-' || ch == '_') && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

/// Extract a short slug from a URL's hostname.
fn host_slug(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.strip_prefix("www.").unwrap_or(h).replace('.', "-"))
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Extract URLs from search result output.
fn extract_urls(output: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            urls.push(trimmed.to_string());
        }
        // "[N] url" format from Perplexity citations
        if let Some(rest) = trimmed.strip_prefix('[') {
            if let Some(after_bracket) = rest.find("] ") {
                let url = &rest[after_bracket + 2..];
                if url.starts_with("http") {
                    urls.push(url.to_string());
                }
            }
        }
    }
    urls
}

/// Simple text extraction fallback.
fn extract_text_simple(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        if c == '<' {
            in_tag = true;
            continue;
        }
        if c == '>' {
            in_tag = false;
            result.push(' ');
            continue;
        }
        if !in_tag {
            result.push(c);
        }
    }
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("top AI startups 2025"), "top-ai-startups-2025");
        assert_eq!(slugify("NVIDIA stock price!"), "nvidia-stock-price");
        assert_eq!(slugify("  spaces  "), "spaces");
    }

    #[test]
    fn test_host_slug() {
        assert_eq!(host_slug("https://www.example.com/page"), "example-com");
        assert_eq!(host_slug("https://api.you.com/search"), "api-you-com");
        assert_eq!(host_slug("https://nerdwallet.com"), "nerdwallet-com");
    }

    #[test]
    fn test_extract_urls_from_search_results() {
        let output = "Results for: test\n\n1. Title\n   https://example.com/page\n   Description\n\n2. Title 2\n   https://other.com\n   Desc\n";
        let urls = extract_urls(output);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://example.com/page");
        assert_eq!(urls[1], "https://other.com");
    }

    #[test]
    fn test_extract_urls_from_perplexity_citations() {
        let output =
            "Answer text\n\nSources:\n  [1] https://example.com\n  [2] https://other.com\n";
        let urls = extract_urls(output);
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn test_extract_urls_empty() {
        assert!(extract_urls("no urls here").is_empty());
    }

    #[tokio::test]
    async fn test_invalid_input() {
        let tool = DeepSearchTool::new("/tmp");
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
