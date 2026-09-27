//! Deep search tool: web search + parallel crawl, saving results to disk.
//!
//! Saves each crawled page as a markdown file under `.octos/research/<query-slug>/`.
//! Returns a concise index so the LLM can selectively read files for synthesis,
//! or (`output: "items"`) the structured items document, which is also always
//! written as `items.json` next to the pages.
//!
//! Pages are read politely (OctoSense ADR 0002 §6): robots.txt checked per
//! origin first, an identifiable User-Agent, at least 1s between requests to
//! one host, a body-size cap, and a real browser only to render JS-heavy pages
//! that plain HTTP returns without main text.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;

use crate::harness_events::emit_registered_progress_event;
use crate::tools::TOOL_CTX;

use super::web_search::WebSearchTool;
use super::{Tool, ToolResult};

/// Identifiable UA for page reads (no browser disguise).
const DEEP_SEARCH_USER_AGENT: &str = octos_research::USER_AGENT;

/// Page-fetch timeout.
const DEEP_SEARCH_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Largest page body read.
const MAX_PAGE_BYTES: usize = 3 * 1024 * 1024;

/// Minimum spacing between requests to one host.
const HOST_INTERVAL: Duration = Duration::from_secs(1);

pub struct DeepSearchTool {
    search: WebSearchTool,
    /// Research output base directory (e.g. ~/.octos/research/ or a sub-agent's research_dir).
    research_base: PathBuf,
}

impl DeepSearchTool {
    pub fn new(research_base: impl Into<PathBuf>) -> Self {
        Self {
            search: WebSearchTool::new(),
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
    /// `index` (default) or `items`.
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    lang: octos_research::OneOrMany,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    max_per_domain: Option<usize>,
    #[serde(default)]
    domains_allow: Vec<String>,
    #[serde(default)]
    domains_deny: Vec<String>,
}

/// A page read for the research index.
struct PageRead {
    /// Main text (readability), or the Markdown of the whole page when no
    /// article was found.
    content: String,
    final_url: String,
    meta: octos_research::extract::PageMeta,
    rendered: bool,
    fetched_at: String,
}

/// Per-call politeness state: robots.txt cache and per-host spacing.
struct Politeness {
    robots: octos_research::RobotsCache,
    throttle: octos_research::HostThrottle,
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
        "search"
    }

    fn description(&self) -> &str {
        "Search the web (free news sources first) and read the result pages in parallel, respecting robots.txt. Saves each page's main text as a markdown file under .octos/research/<query>/ plus items.json (title, url, source, lang, published, summary). Returns an index of saved files (or the items with output=items) — use read_file to examine specific pages for synthesis."
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
                    "description": "Max characters to extract per page (default: 20000)"
                },
                "output": {
                    "type": "string",
                    "enum": ["index", "items"],
                    "description": "index (default): text index with previews. items: the structured items JSON (also saved as items.json)"
                },
                "lang": {
                    "description": "BCP-47 language(s), e.g. \"en\" or [\"en\", \"es\"]",
                    "anyOf": [
                        {"type": "string"},
                        {"type": "array", "items": {"type": "string"}}
                    ]
                },
                "region": {
                    "type": "string",
                    "description": "ISO 3166-1 alpha-2 region, e.g. US"
                },
                "since": {
                    "type": "string",
                    "description": "Only results published since an ISO date or a span: 24h, 7d, 2w, 3m, 1y"
                },
                "category": {
                    "type": "string",
                    "enum": ["auto", "news", "general"],
                    "description": "news uses GDELT + Google News first (default auto)"
                },
                "max_per_domain": {
                    "type": "integer",
                    "description": "At most this many pages per domain"
                },
                "domains_allow": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Only read these domains (subdomains included)"
                },
                "domains_deny": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Never read these domains (subdomains included)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input = serde_json::from_value(args.clone()).wrap_err("invalid search input")?;

        let count = input.count.clamp(1, 10);
        let max_chars = input.max_chars_per_page.clamp(1000, 200_000);
        let items_mode = match input.output.as_deref().map(str::trim) {
            None | Some("") | Some("index") | Some("report") => false,
            Some("items") => true,
            Some(other) => {
                return Ok(invalid_input(&format!(
                    "invalid output {other:?} (use \"index\" or \"items\")"
                )));
            }
        };
        let filters = match build_filters(&input) {
            Ok(f) => f,
            Err(e) => return Ok(invalid_input(&e)),
        };

        // Step 1: Search (free news sources first inside web_search)
        emit_deep_research_progress(
            "search",
            &format!("Searching: \"{}\"", input.query),
            Some(0.1),
        );
        let mut search_args = serde_json::json!({
            "query": input.query,
            "count": count
        });
        for (key, value) in [
            (
                "lang",
                serde_json::to_value(&input.lang).unwrap_or_default(),
            ),
            ("region", serde_json::json!(input.region)),
            ("since", serde_json::json!(input.since)),
            ("category", serde_json::json!(input.category)),
        ] {
            if !value.is_null() {
                search_args[key] = value;
            }
        }
        let search_result = self.search.execute(&search_args).await?;

        if !search_result.success {
            return Ok(search_result);
        }

        // Extract URLs from search results, then apply the domain controls.
        let mut skipped: Vec<octos_research::SkippedUrl> = Vec::new();
        let mut cap = octos_research::DomainCap::new(filters.max_per_domain);
        let mut urls = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for url in extract_urls(&search_result.output) {
            if !seen.insert(octos_research::urls::dedup_key(&url)) {
                continue;
            }
            if let Err(reason) = filters.check_domain(&url) {
                skipped.push(skip(&url, reason));
            } else if !cap.admit(&url) {
                skipped.push(skip(&url, "per_domain_cap"));
            } else {
                urls.push(url);
            }
        }

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

        // Step 2: Parallel, polite reads of all URLs
        emit_deep_research_progress(
            "fetch",
            &format!(
                "Reading {} pages in parallel (robots.txt respected)...",
                urls.len()
            ),
            Some(0.4),
        );
        let polite = Politeness {
            robots: octos_research::RobotsCache::new(),
            throttle: octos_research::HostThrottle::new(HOST_INTERVAL),
        };
        let fetches: Vec<_> = urls
            .iter()
            .map(|url| self.fetch_page(url, max_chars, &polite))
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
        let mut failed_files: Vec<String> = Vec::new();
        let mut items: Vec<octos_research::ResearchItem> = Vec::new();
        for (i, (url, page)) in urls.iter().zip(pages.iter()).enumerate() {
            let filename = format!("{:02}_{}.md", i + 1, host_slug(url));
            let filepath = dir.join(&filename);

            match page {
                Ok(page) if !page.content.is_empty() => {
                    let item = page_item(url, page, i + 1, &filename);
                    if let Err(reason) =
                        filters.check(&item.url, item.lang.as_deref(), item.published.as_deref())
                    {
                        skipped.push(skip(url, reason));
                        continue;
                    }
                    // Save full content to disk
                    let page_content = format!(
                        "---\nurl: {url}\ncanonical: {}\ntitle: {}\nsource: {}\nlang: {}\npublished: {}\n---\n\n{}",
                        item.url,
                        one_line(&item.title),
                        one_line(&item.source),
                        item.lang.as_deref().unwrap_or(""),
                        item.published.as_deref().unwrap_or(""),
                        page.content
                    );
                    let _ = tokio::fs::write(&filepath, &page_content).await;
                    saved_count += 1;
                    saved_files.push(format!("  - {} ({})", filepath.display(), url));

                    // Return truncated preview inline to keep context small
                    output.push_str(&format!("## Source [{}]: {}\n", i + 1, url));
                    output.push_str(&format!(
                        "_{} — {}{}{}_\n",
                        one_line(&item.title),
                        one_line(&item.source),
                        item.published
                            .as_deref()
                            .map(|p| format!(" · {p}"))
                            .unwrap_or_default(),
                        item.lang
                            .as_deref()
                            .map(|l| format!(" · {l}"))
                            .unwrap_or_default(),
                    ));
                    output.push_str(&format!("_Full content: {}_\n\n", filepath.display()));
                    let mut preview = page.content.clone();
                    octos_core::truncate_utf8(
                        &mut preview,
                        INLINE_CHARS_PER_PAGE,
                        "\n... (truncated, use read_file for full content)",
                    );
                    output.push_str(&preview);
                    output.push_str("\n\n---\n\n");
                    items.push(item);
                }
                Ok(_) => {}
                Err(e) => {
                    // `fetch_page` propagates robots.txt refusals, 403/500,
                    // transport, and body-read failures. Persist the error
                    // artifact AND surface it in the returned index —
                    // otherwise a failed (or all-failed) crawl hands the agent
                    // an index that silently omits the source, with no path
                    // or reason to inspect.
                    let err_content = format!("---\nurl: {url}\nerror: {e}\n---\n");
                    let _ = tokio::fs::write(&filepath, &err_content).await;
                    failed_files.push(format!("  - {} ({url}): {e}", filepath.display()));
                    output.push_str(&format!("## Source [{}]: {url} — FETCH FAILED\n", i + 1));
                    output.push_str(&format!(
                        "_Error: {e}. Saved error artifact: {}_\n\n---\n\n",
                        filepath.display()
                    ));
                    skipped.push(skip(url, &e.to_string()));
                }
            }
        }

        // Structured items (always written).
        let items_path = dir.join("items.json");
        let mut doc = octos_research::ItemsDocument::new(&input.query, filters.to_json());
        doc.items = items;
        doc.skipped = skipped;
        doc.items_file = Some(items_path.display().to_string());
        let items_json = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into());
        let _ = tokio::fs::write(&items_path, &items_json).await;

        output.push_str(&format!(
            "{saved_count} pages crawled and saved to: {}\n\nSaved files:\n{}\n\n\
            Structured items: {}\n\n\
            Use read_file to get full content from specific sources for detailed synthesis.\n",
            dir.display(),
            saved_files.join("\n"),
            items_path.display()
        ));
        output.push_str(&render_failed_sources_block(&failed_files));

        emit_deep_research_progress("completion", "Deep search complete", Some(1.0));

        Ok(ToolResult {
            output: if items_mode { items_json } else { output },
            success: true,
            ..Default::default()
        })
    }
}

fn invalid_input(msg: &str) -> ToolResult {
    ToolResult {
        output: format!("Invalid search input: {msg}"),
        success: false,
        ..Default::default()
    }
}

fn skip(url: &str, reason: &str) -> octos_research::SkippedUrl {
    octos_research::SkippedUrl {
        url: url.to_string(),
        reason: reason.to_string(),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("---", "—")
}

fn build_filters(input: &Input) -> std::result::Result<octos_research::Filters, String> {
    let since = match input.since.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(s) => Some(octos_research::date::Since::parse(s, chrono::Utc::now())?),
    };
    octos_research::Filters::new(
        input.lang.clone().into_vec(),
        since,
        input.domains_allow.clone(),
        input.domains_deny.clone(),
        input.max_per_domain,
    )
}

/// Structured item for a read page (`citation` = its `Source [N]`).
fn page_item(
    url: &str,
    page: &PageRead,
    citation: usize,
    file: &str,
) -> octos_research::ResearchItem {
    let canonical = octos_research::urls::canonicalize(
        page.meta.canonical.as_deref().unwrap_or(&page.final_url),
    );
    let domain = octos_research::urls::domain_of(&canonical).unwrap_or_default();
    let summary = octos_research::item::extractive_summary(&page.content, 400);
    octos_research::ResearchItem {
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
            octos_research::SummaryKind::None
        } else {
            octos_research::SummaryKind::Extractive
        },
        summary,
        snippet: page.meta.excerpt.clone().unwrap_or_default(),
        fetched_at: Some(page.fetched_at.clone()),
        provider: "web_search".to_string(),
        read: true,
        rendered: page.rendered,
        citation: Some(citation),
        cited: false,
        file: Some(file.to_string()),
        url: canonical,
    }
}

fn emit_deep_research_progress(phase: &str, message: &str, progress: Option<f64>) {
    if let Ok(Some(sink)) = TOOL_CTX.try_with(|ctx| ctx.harness_event_sink.clone()) {
        let _ =
            emit_registered_progress_event(sink, Some("deep_research"), phase, message, progress);
    }
}

impl DeepSearchTool {
    async fn robots_allows(polite: &Politeness, url: &str) -> Result<Option<Duration>> {
        let decision = polite
            .robots
            .check(url, octos_research::AGENT_TOKEN, |robots_url| async move {
                let resp = super::ssrf::ssrf_safe_send(
                    &robots_url,
                    super::ssrf::SSRF_MAX_REDIRECTS,
                    |b| {
                        b.timeout(DEEP_SEARCH_FETCH_TIMEOUT)
                            .user_agent(DEEP_SEARCH_USER_AGENT)
                    },
                    |client, current_url| client.get(current_url),
                )
                .await;
                match resp {
                    Ok(r) => {
                        let status = r.status().as_u16();
                        let body = read_capped(r, 512 * 1024).await.unwrap_or_default();
                        (Some(status), body)
                    }
                    Err(_) => (None, String::new()),
                }
            })
            .await;
        if decision.allowed {
            Ok(decision.crawl_delay)
        } else {
            eyre::bail!("skipped: {} (robots.txt)", decision.reason)
        }
    }

    async fn fetch_page(
        &self,
        url: &str,
        max_chars: usize,
        polite: &Politeness,
    ) -> Result<PageRead> {
        // robots.txt first (per origin, cached), then per-host spacing.
        let crawl_delay = Self::robots_allows(polite, url).await?;
        let host = octos_research::urls::domain_of(url).unwrap_or_default();
        polite.throttle.wait(&host, crawl_delay).await;

        // SSRF is re-validated on EVERY redirect hop (the initial-URL-only
        // check this replaced let an allowed URL 302 to 169.254.169.254 /
        // 10.x through). `ssrf_safe_send` disables auto-redirects, re-checks +
        // DNS-pins each hop, and re-issues the GET.
        //
        // Failures (SSRF-blocked, transport error, non-2xx, body-read) return
        // Err so the caller's save loop records an error artifact for the
        // skipped source — collapsing them to Ok("") would silently drop
        // 403/500 pages from the research index.
        let response = super::ssrf::ssrf_safe_send(
            url,
            super::ssrf::SSRF_MAX_REDIRECTS,
            |builder| {
                builder
                    .timeout(DEEP_SEARCH_FETCH_TIMEOUT)
                    .user_agent(DEEP_SEARCH_USER_AGENT)
            },
            |client, current_url| client.get(current_url),
        )
        .await
        .map_err(|e| eyre::eyre!("{e}"))?;

        if !response.status().is_success() {
            eyre::bail!("HTTP {}", response.status());
        }
        let final_url = response.url().to_string();
        let body = read_capped(response, MAX_PAGE_BYTES)
            .await
            .map_err(|e| eyre::eyre!("read body failed: {e}"))?;
        let fetched_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        // Main text (readability); else the Markdown of the whole page.
        let mut extracted = octos_research::extract::extract(&body, &final_url);
        let mut page_url = final_url;
        let mut rendered = false;
        if extracted.is_empty_text() {
            let markdown = htmd::convert(&body).unwrap_or_else(|_| extract_text_simple(&body));
            if markdown.chars().filter(|c| !c.is_whitespace()).count()
                >= octos_research::extract::MIN_MAIN_TEXT_CHARS
            {
                extracted.text = markdown;
            }
        }
        // JS-heavy page: render it in a real browser (reading, not search).
        #[cfg(feature = "browser")]
        if extracted.is_empty_text() {
            if let Ok((rurl, rhtml)) = render_page_html(&page_url, Duration::from_secs(45)).await {
                let same_site = octos_research::urls::domain_of(&rurl)
                    == octos_research::urls::domain_of(&page_url);
                if same_site || Self::robots_allows(polite, &rurl).await.is_ok() {
                    let rex = octos_research::extract::extract(&rhtml, &rurl);
                    if !rex.is_empty_text() {
                        extracted = rex;
                        page_url = rurl;
                        rendered = true;
                    }
                }
            }
        }

        let mut content = extracted.text;
        octos_core::truncate_utf8(&mut content, max_chars, "\n... (truncated)");

        Ok(PageRead {
            content,
            final_url: page_url,
            meta: extracted.meta,
            rendered,
            fetched_at,
        })
    }
}

/// Read a response body up to `cap` bytes (lossy UTF-8).
async fn read_capped(
    mut resp: reqwest::Response,
    cap: usize,
) -> std::result::Result<String, String> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = cap.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if buf.len() >= cap {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Render one page in headless Chrome (the `browser` tool's chromiumoxide
/// machinery) and return `(final_url, html)`. No automation hiding: the
/// browser's own User-Agent plus the `octos-research` token.
#[cfg(feature = "browser")]
async fn render_page_html(url: &str, bound: Duration) -> Result<(String, String)> {
    use chromiumoxide::browser::{Browser, BrowserConfig};
    use chromiumoxide::cdp::browser_protocol::network::SetUserAgentOverrideParams;
    use futures::StreamExt;

    let executable = super::web_search::detect_browser_executable()
        .ok_or_else(|| eyre::eyre!("no Chrome/Chromium executable detected"))?;
    let temp_dir = tempfile::Builder::new()
        .prefix("octos-research-render-")
        .tempdir()
        .wrap_err("failed to create temp dir for Chrome")?;
    let mut builder = BrowserConfig::builder()
        .chrome_executable(&executable)
        .user_data_dir(temp_dir.path())
        .arg("--headless=new")
        .arg("--disable-dev-shm-usage")
        .arg("--disable-extensions")
        .arg("--disable-background-networking");
    for var in crate::sandbox::BLOCKED_ENV_VARS {
        builder = builder.env(*var, "");
    }
    let config = builder
        .build()
        .map_err(|e| eyre::eyre!("failed to build browser config: {e}"))?;

    let fut = async {
        let (mut browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| eyre::eyre!("failed to launch Chrome: {e}"))?;
        let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
        let outcome = async {
            let page = browser
                .new_page("about:blank")
                .await
                .map_err(|e| eyre::eyre!("failed to open page: {e}"))?;
            let base_ua = page.user_agent().await.unwrap_or_default();
            let ua = format!("{base_ua} octos-research/1.0 (+https://github.com/octos-org/octos)");
            let _ = page
                .set_user_agent(SetUserAgentOverrideParams::new(ua.trim().to_string()))
                .await;
            page.goto(url)
                .await
                .map_err(|e| eyre::eyre!("navigation failed: {e}"))?;
            let _ = page.wait_for_navigation().await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            let html = page
                .content()
                .await
                .map_err(|e| eyre::eyre!("failed to read HTML: {e}"))?;
            let final_url = page
                .url()
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| url.to_string());
            Ok::<_, eyre::Report>((final_url, html))
        }
        .await;
        let _ = browser.close().await;
        handler_task.abort();
        outcome
    };
    tokio::time::timeout(bound, fut)
        .await
        .map_err(|_| eyre::eyre!("browser render timed out"))?
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

/// Render the "failed sources" tail appended to the research index when one or
/// more crawls fail. Each entry names the saved error-artifact path and the
/// failure reason so the agent can locate and read it. Empty when nothing
/// failed. Pure, so the failed-source surfacing is unit-testable without a
/// live crawl.
fn render_failed_sources_block(failed_files: &[String]) -> String {
    if failed_files.is_empty() {
        return String::new();
    }
    format!(
        "\n{} source(s) failed to fetch (error artifacts saved for inspection):\n{}\n",
        failed_files.len(),
        failed_files.join("\n")
    )
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

    #[test]
    fn failed_sources_block_lists_path_and_reason_and_is_empty_when_none() {
        // Codex P2: once `fetch_page` propagates fetch errors, a failed crawl
        // must surface the saved error-artifact path AND the reason in the
        // returned index — not silently omit the source. No failures → no tail.
        assert!(render_failed_sources_block(&[]).is_empty());

        let block = render_failed_sources_block(&[
            "  - /r/01_example-com.md (https://example.com): HTTP 403".to_string(),
            "  - /r/02_other-com.md (https://other.com): read body failed".to_string(),
        ]);
        assert!(block.contains("2 source(s) failed"), "block: {block}");
        assert!(block.contains("/r/01_example-com.md"), "block: {block}");
        assert!(block.contains("HTTP 403"), "block: {block}");
        assert!(block.contains("https://other.com"), "block: {block}");
    }

    #[test]
    fn should_build_item_from_page_metadata() {
        let page = PageRead {
            content: "Negotiators from nearly 200 countries agreed on a draft text late on Thursday night. More follows.".into(),
            final_url: "https://apnews.com/article/x?utm_source=feed".into(),
            meta: octos_research::extract::PageMeta {
                title: Some("Climate summit ends with draft deal".into()),
                site_name: Some("AP News".into()),
                lang: Some("en".into()),
                published: Some("2026-09-25T21:40:00Z".into()),
                ..Default::default()
            },
            rendered: false,
            fetched_at: "2026-09-27T12:00:00Z".into(),
        };
        let item = page_item("https://apnews.com/article/x", &page, 2, "02_apnews-com.md");
        assert_eq!(item.url, "https://apnews.com/article/x");
        assert_eq!(item.source, "AP News");
        assert_eq!(item.domain, "apnews.com");
        assert_eq!(item.published.as_deref(), Some("2026-09-25T21:40:00Z"));
        assert_eq!(item.citation, Some(2));
        assert_eq!(item.summary_kind, octos_research::SummaryKind::Extractive);
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["summary_kind"], "extractive");
    }

    #[tokio::test]
    async fn should_reject_invalid_controls_before_searching() {
        let tool = DeepSearchTool::new("/tmp");
        for bad in [
            serde_json::json!({"query": "q", "output": "xml"}),
            serde_json::json!({"query": "q", "since": "soon"}),
            serde_json::json!({"query": "q", "lang": ["en", "english"]}),
        ] {
            let r = tool.execute(&bad).await.unwrap();
            assert!(!r.success, "{bad}");
            assert!(r.output.starts_with("Invalid search input"), "{}", r.output);
        }
    }

    #[test]
    fn should_apply_domain_controls_from_input() {
        let input: Input = serde_json::from_value(serde_json::json!({
            "query": "q", "domains_deny": ["example.com"], "max_per_domain": 1, "lang": "en"
        }))
        .unwrap();
        let f = build_filters(&input).unwrap();
        assert_eq!(
            f.check_domain("https://www.example.com/a"),
            Err("domain_deny")
        );
        assert!(f.check_domain("https://other.org/a").is_ok());
        assert_eq!(f.max_per_domain, Some(1));
        assert_eq!(f.langs, vec!["en"]);
    }

    #[tokio::test]
    async fn test_invalid_input() {
        let tool = DeepSearchTool::new("/tmp");
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
