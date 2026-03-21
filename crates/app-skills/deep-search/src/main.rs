//! Deep multi-round web research tool.
//!
//! Performs iterative search across multiple angles, fetches pages in parallel,
//! chases most-referenced links, and produces a structured research report.
//!
//! Reads JSON from stdin, outputs JSON to stdout, progress to stderr.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Input / Output types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Input {
    query: String,
    #[serde(default = "default_max_results")]
    max_results: u8,
    #[serde(default)]
    search_engine: Option<String>,
    /// Research depth: 1=quick (single search), 2=standard (3 rounds), 3=thorough (5 rounds).
    #[serde(default = "default_depth")]
    depth: u8,
}

fn default_max_results() -> u8 {
    8
}
fn default_depth() -> u8 {
    2
}

#[derive(Serialize)]
struct Output {
    output: String,
    success: bool,
}

// ---------------------------------------------------------------------------
// Search result types (provider-specific)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWebResults>,
}
#[derive(Deserialize)]
struct BraveWebResults {
    results: Vec<BraveWebResult>,
}
#[derive(Deserialize)]
struct BraveWebResult {
    title: String,
    url: String,
    description: String,
}

#[derive(Deserialize)]
struct YouResponse {
    results: Option<YouResults>,
}
#[derive(Deserialize)]
struct YouResults {
    web: Option<Vec<YouWebResult>>,
}
#[derive(Deserialize)]
struct YouWebResult {
    title: String,
    url: String,
    description: String,
    #[serde(default)]
    snippets: Vec<String>,
}

#[derive(Deserialize)]
struct PerplexityResponse {
    choices: Option<Vec<PerplexityChoice>>,
    #[serde(default)]
    citations: Vec<String>,
}
#[derive(Deserialize)]
struct PerplexityChoice {
    message: Option<PerplexityMessage>,
}
#[derive(Deserialize)]
struct PerplexityMessage {
    content: Option<String>,
}

// ---------------------------------------------------------------------------
// Unified search result
// ---------------------------------------------------------------------------

struct SearchResult {
    output: String,
    success: bool,
}

/// A single crawled page.
struct CrawledPage {
    url: String,
    filename: String,
    content: String,
    outbound_links: Vec<String>,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let mut stdin_buf = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut stdin_buf) {
        print_output(&Output {
            output: format!("Failed to read stdin: {e}"),
            success: false,
        });
        return;
    }

    let input: Input = match serde_json::from_str(&stdin_buf) {
        Ok(v) => v,
        Err(e) => {
            print_output(&Output {
                output: format!("Invalid input JSON: {e}"),
                success: false,
            });
            return;
        }
    };

    let depth = input.depth.clamp(1, 3);
    let max_results = input.max_results.clamp(1, 10);
    let client = build_client();

    // Apply overall timeout based on depth
    let timeout = match depth {
        1 => Duration::from_secs(60),
        2 => Duration::from_secs(180),
        _ => Duration::from_secs(300),
    };

    let result = tokio::time::timeout(
        timeout,
        run_deep_search(
            &client,
            &input.query,
            max_results,
            depth,
            input.search_engine.as_deref(),
        ),
    )
    .await;

    match result {
        Ok(output) => print_output(&output),
        Err(_) => print_output(&Output {
            output: format!("Deep search timed out after {}s", timeout.as_secs()),
            success: false,
        }),
    }
}

async fn run_deep_search(
    client: &reqwest::Client,
    query: &str,
    max_results: u8,
    depth: u8,
    engine: Option<&str>,
) -> Output {
    let max_rounds = match depth {
        1 => 1,
        2 => 3,
        _ => 5,
    };
    let max_pages: usize = match depth {
        1 => 10,
        2 => 30,
        _ => 50,
    };

    let slug = slugify(query);
    let dir = research_dir(&slug);
    if let Err(e) = fs::create_dir_all(&dir) {
        return Output {
            output: format!("Failed to create research directory: {e}"),
            success: false,
        };
    }

    let mut all_urls: Vec<String> = Vec::new();
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut search_queries: Vec<String> = Vec::new();
    let mut initial_answer = String::new();
    let mut all_search_output = String::new();

    // -----------------------------------------------------------------------
    // Round 1: Initial broad search
    // -----------------------------------------------------------------------
    progress(1, max_rounds, &format!("Searching: \"{query}\""));
    search_queries.push(query.to_string());

    let r1 = web_search(client, query, max_results, engine).await;
    if !r1.success {
        return Output {
            output: r1.output,
            success: false,
        };
    }

    all_search_output.push_str(&r1.output);
    all_search_output.push_str("\n\n");
    initial_answer.push_str(&r1.output);

    for url in extract_urls(&r1.output) {
        let norm = normalize_url(&url);
        if seen_urls.insert(norm) {
            all_urls.push(url);
        }
    }

    // -----------------------------------------------------------------------
    // Rounds 2+: Follow-up searches from different angles
    // -----------------------------------------------------------------------
    if depth >= 2 {
        let follow_ups = generate_follow_up_queries(query, &r1.output, depth);
        let rounds_left = max_rounds - 1;

        for (i, fq) in follow_ups.into_iter().take(rounds_left).enumerate() {
            let round = i + 2;
            progress(round, max_rounds, &format!("Searching: \"{fq}\""));
            search_queries.push(fq.clone());

            let r = web_search(client, &fq, max_results, engine).await;
            if r.success {
                all_search_output.push_str(&r.output);
                all_search_output.push_str("\n\n");
                for url in extract_urls(&r.output) {
                    let norm = normalize_url(&url);
                    if seen_urls.insert(norm) {
                        all_urls.push(url);
                    }
                }
            }
        }
    }

    // Save combined search results
    let _ = fs::write(dir.join("_search_results.md"), &all_search_output);

    // -----------------------------------------------------------------------
    // Parallel page fetching
    // -----------------------------------------------------------------------
    let urls_to_fetch: Vec<String> = all_urls.into_iter().take(max_pages).collect();
    let total_fetch = urls_to_fetch.len();
    progress_simple(&format!("Fetching {total_fetch} pages in parallel..."));

    let crawled_pages = fetch_pages_parallel(client, &urls_to_fetch, 20_000).await;

    // Save pages to disk
    let mut saved_files: Vec<(String, String, String)> = Vec::new(); // (filename, url, preview)
    for page in crawled_pages.iter() {
        let page_content = format!("---\nurl: {}\n---\n\n{}", page.url, page.content);
        let _ = fs::write(dir.join(&page.filename), &page_content);
        let preview = truncate_utf8(&page.content, 2000, "\n... (truncated)");
        saved_files.push((page.filename.clone(), page.url.clone(), preview));
    }

    // -----------------------------------------------------------------------
    // Reference chasing (depth >= 2)
    // -----------------------------------------------------------------------
    let mut all_crawled_pages: Vec<&CrawledPage> = crawled_pages.iter().collect();
    let mut chased_pages: Vec<CrawledPage> = Vec::new();

    if depth >= 2 {
        let mut link_counts: HashMap<String, u32> = HashMap::new();
        for page in &crawled_pages {
            for link in &page.outbound_links {
                let norm = normalize_url(link);
                if !seen_urls.contains(&norm) {
                    *link_counts.entry(link.clone()).or_insert(0) += 1;
                }
            }
        }

        // Get top referenced links
        let chase_limit = match depth {
            2 => 5,
            _ => 10,
        };
        let mut ranked: Vec<(String, u32)> = link_counts.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1));
        let chase_urls: Vec<String> = ranked
            .into_iter()
            .take(chase_limit)
            .filter(|(_, count)| *count >= 2) // Only chase links referenced by 2+ pages
            .map(|(url, _)| url)
            .collect();

        if !chase_urls.is_empty() {
            progress_simple(&format!(
                "Chasing {} most-referenced sources...",
                chase_urls.len()
            ));
            // Mark chased URLs as seen
            for url in &chase_urls {
                seen_urls.insert(normalize_url(url));
            }
            chased_pages = fetch_pages_parallel(client, &chase_urls, 20_000).await;
            let offset = saved_files.len();
            for (i, page) in chased_pages.iter().enumerate() {
                let filename = format!("{:02}_{}.md", offset + i + 1, host_slug(&page.url));
                let page_content = format!("---\nurl: {}\n---\n\n{}", page.url, page.content);
                let _ = fs::write(dir.join(&filename), &page_content);
                let preview = truncate_utf8(&page.content, 2000, "\n... (truncated)");
                saved_files.push((filename, page.url.clone(), preview));
            }
        }
    }

    // Combine all crawled pages for site crawl link extraction
    all_crawled_pages.extend(chased_pages.iter());

    // -----------------------------------------------------------------------
    // Site crawl: follow internal links on high-value domains (depth >= 2)
    // -----------------------------------------------------------------------
    if depth >= 2 {
        let mut domain_links: HashMap<String, Vec<String>> = HashMap::new();

        for page in &all_crawled_pages {
            let internal = same_origin_links(&page.url, &page.outbound_links, &seen_urls);
            if internal.is_empty() {
                continue;
            }
            let origin = url::Url::parse(&page.url)
                .ok()
                .map(|u| u.origin().ascii_serialization())
                .unwrap_or_default();
            if !origin.is_empty() {
                domain_links.entry(origin).or_default().extend(internal);
            }
        }

        // Deduplicate links within each domain
        for links in domain_links.values_mut() {
            let mut dedup_set = HashSet::new();
            links.retain(|l| {
                let norm = normalize_url(l);
                dedup_set.insert(norm)
            });
        }

        // Rank domains by internal link count, pick top N
        let crawl_domains: usize = match depth {
            2 => 3,
            _ => 5,
        };
        let pages_per_domain: usize = match depth {
            2 => 3,
            _ => 5,
        };

        let mut ranked_domains: Vec<(String, Vec<String>)> = domain_links.into_iter().collect();
        ranked_domains.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

        let to_crawl: Vec<String> = ranked_domains
            .into_iter()
            .take(crawl_domains)
            .flat_map(|(domain, links)| {
                let take = links.len().min(pages_per_domain);
                progress_simple(&format!(
                    "Site crawl: {} ({} internal links, fetching {})",
                    domain,
                    links.len(),
                    take
                ));
                links.into_iter().take(pages_per_domain)
            })
            .collect();

        if !to_crawl.is_empty() {
            progress_simple(&format!(
                "Site crawl: fetching {} additional pages from top domains...",
                to_crawl.len()
            ));

            // Mark as seen
            for url in &to_crawl {
                seen_urls.insert(normalize_url(url));
            }

            let site_pages = fetch_pages_parallel(client, &to_crawl, 20_000).await;
            let offset = saved_files.len();
            for (i, page) in site_pages.iter().enumerate() {
                let filename = format!("{:02}_{}.md", offset + i + 1, host_slug(&page.url));
                let page_content = format!("---\nurl: {}\n---\n\n{}", page.url, page.content);
                let _ = fs::write(dir.join(&filename), &page_content);
                let preview = truncate_utf8(&page.content, 2000, "\n... (truncated)");
                saved_files.push((filename, page.url.clone(), preview));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Build structured report
    // -----------------------------------------------------------------------
    progress_simple("Building report...");

    let mut report = String::new();
    report.push_str(&format!("# Deep Research: {query}\n\n"));

    // Overview section
    report.push_str("## Overview\n\n");
    report.push_str(&initial_answer);
    report.push_str("\n\n");

    // Source details with inline previews
    report.push_str(&format!(
        "## Sources ({} pages crawled)\n\n",
        saved_files.len()
    ));
    for (i, (filename, url, preview)) in saved_files.iter().enumerate() {
        report.push_str(&format!("### Source [{}]: {}\n", i + 1, url));
        report.push_str(&format!(
            "_Full content: {}/{}_\n\n",
            dir.display(),
            filename
        ));
        report.push_str(preview);
        report.push_str("\n\n---\n\n");
    }

    // Search queries used
    report.push_str("## Search Queries Used\n\n");
    for (i, q) in search_queries.iter().enumerate() {
        report.push_str(&format!("{}. {}\n", i + 1, q));
    }
    report.push('\n');

    // Summary line
    report.push_str(&format!(
        "\n---\n{} pages crawled across {} search rounds. Results saved to: {}\n\
         Use read_file to get full content from specific sources for detailed synthesis.\n",
        saved_files.len(),
        search_queries.len(),
        dir.display(),
    ));

    // Save report
    let _ = fs::write(dir.join("_report.md"), &report);

    Output {
        output: report,
        success: true,
    }
}

// ---------------------------------------------------------------------------
// Follow-up query generation (heuristic, no LLM)
// ---------------------------------------------------------------------------

fn generate_follow_up_queries(original: &str, search_output: &str, depth: u8) -> Vec<String> {
    let mut queries = Vec::new();

    // 1. Extract bold/header topics from Perplexity's answer
    let subtopics = extract_subtopics(search_output);

    // 2. Time-qualified variant
    queries.push(format!("{original} 2026 latest"));

    // 3. Subtopic-based queries (combine original topic with extracted subtopics)
    for topic in subtopics.iter().take(3) {
        if topic.len() > 3 && topic.len() < 60 {
            queries.push(format!("{original} {topic}"));
        }
    }

    // 4. For depth 3: add controversy/analysis angles
    if depth >= 3 {
        queries.push(format!("{original} analysis controversy"));
        queries.push(format!("{original} expert opinion"));

        // More subtopic variants
        for topic in subtopics.iter().skip(3).take(2) {
            if topic.len() > 3 && topic.len() < 60 {
                queries.push(format!("{original} {topic}"));
            }
        }
    }

    // Deduplicate
    let mut seen = HashSet::new();
    queries.retain(|q| {
        let key = q.to_lowercase();
        seen.insert(key)
    });

    queries
}

/// Extract subtopics from search output by finding **bold** text and ### headers.
fn extract_subtopics(text: &str) -> Vec<String> {
    let mut topics = Vec::new();

    // Extract **bold** text
    let mut pos = 0;
    while let Some(start) = text[pos..].find("**") {
        let start = pos + start + 2;
        if let Some(end) = text[start..].find("**") {
            let topic = text[start..start + end].trim().to_string();
            if !topic.is_empty() && topic.len() < 80 {
                topics.push(topic);
            }
            pos = start + end + 2;
        } else {
            break;
        }
    }

    // Extract ### headers
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(header) = trimmed.strip_prefix("###") {
            let h = header.trim().trim_start_matches('#').trim();
            if !h.is_empty() && h.len() < 80 {
                topics.push(h.to_string());
            }
        }
    }

    // Deduplicate
    let mut seen = HashSet::new();
    topics.retain(|t| {
        let key = t.to_lowercase();
        seen.insert(key)
    });

    topics
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ---------------------------------------------------------------------------
// Web search (multi-provider, async)
// ---------------------------------------------------------------------------

async fn web_search(
    client: &reqwest::Client,
    query: &str,
    count: u8,
    engine: Option<&str>,
) -> SearchResult {
    // If a specific engine is requested, use it directly
    if let Some(eng) = engine {
        if eng == "all" {
            // Fire ALL engines in parallel — use sparingly (high resource cost)
            return parallel_all_engines(client, query, count).await;
        }
        if let Some(r) = try_engine(client, query, count, eng).await {
            return r;
        }
    }

    // Default: race top 2 available engines in parallel.
    // Gives redundancy + speed without the resource explosion of fire-all.
    // Priority: tavily > perplexity > brave > duckduckgo
    let mut available: Vec<&str> = Vec::new();
    if std::env::var("TAVILY_API_KEY")
        .ok()
        .is_some_and(|k| !k.is_empty())
    {
        available.push("tavily");
    }
    if std::env::var("PERPLEXITY_API_KEY")
        .ok()
        .is_some_and(|k| !k.is_empty())
    {
        available.push("perplexity");
    }
    if std::env::var("BRAVE_API_KEY")
        .ok()
        .is_some_and(|k| !k.is_empty())
    {
        available.push("brave");
    }
    available.push("duckduckgo"); // always available (free)

    // Race top 2
    let top2: Vec<&str> = available.into_iter().take(2).collect();
    let mut handles = Vec::new();
    for eng in &top2 {
        let c = client.clone();
        let q = query.to_string();
        let cnt = count;
        let name = eng.to_string();
        handles.push(tokio::spawn(async move {
            (name.clone(), try_engine(&c, &q, cnt, &name).await)
        }));
    }

    let results = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        futures::future::join_all(handles),
    )
    .await
    .unwrap_or_default();

    // Pick the best result (richest successful output)
    let mut successful: Vec<(String, SearchResult)> = results
        .into_iter()
        .filter_map(|r| r.ok())
        .filter_map(|(name, opt)| opt.map(|r| (name, r)))
        .collect();

    if successful.is_empty() {
        return SearchResult {
            output: format!("No results found from any search engine for: {query}"),
            success: false,
        };
    }

    successful.sort_by(|a, b| b.1.output.len().cmp(&a.1.output.len()));
    let mut primary = successful.remove(0);

    // Append runner-up if it has substantial unique content
    if let Some((name, other)) = successful.first() {
        if other.output.len() > 200 {
            primary
                .1
                .output
                .push_str(&format!("\n\n--- Also from {name} ---\n"));
            primary.1.output.push_str(&other.output);
        }
    }

    primary.1
}

/// Fire ALL available engines in parallel and merge results.
/// Use sparingly — with many concurrent workers this creates hundreds of connections.
async fn parallel_all_engines(client: &reqwest::Client, query: &str, count: u8) -> SearchResult {
    let mut handles = Vec::new();

    if let Ok(k) = std::env::var("TAVILY_API_KEY") {
        if !k.is_empty() {
            let c = client.clone();
            let q = query.to_string();
            handles.push(tokio::spawn(async move {
                ("tavily", tavily_search(&c, &q, count, &k).await)
            }));
        }
    }
    if let Ok(k) = std::env::var("PERPLEXITY_API_KEY") {
        if !k.is_empty() {
            let c = client.clone();
            let q = query.to_string();
            handles.push(tokio::spawn(async move {
                ("perplexity", perplexity_search(&c, &q, &k).await)
            }));
        }
    }
    if let Ok(k) = std::env::var("BRAVE_API_KEY") {
        let c = client.clone();
        let q = query.to_string();
        handles.push(tokio::spawn(async move {
            ("brave", brave_search(&c, &q, count, &k).await)
        }));
    }
    {
        let c = client.clone();
        let q = query.to_string();
        handles.push(tokio::spawn(async move {
            ("duckduckgo", ddg_search(&c, &q, count).await)
        }));
    }

    let results = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        futures::future::join_all(handles),
    )
    .await
    .unwrap_or_default();

    let mut successful: Vec<(String, SearchResult)> = results
        .into_iter()
        .filter_map(|r| r.ok())
        .filter(|(_, r)| r.success && !r.output.contains("No results found"))
        .map(|(n, r)| (n.to_string(), r))
        .collect();

    if successful.is_empty() {
        return SearchResult {
            output: format!("No results from any engine for: {query}"),
            success: false,
        };
    }

    successful.sort_by(|a, b| b.1.output.len().cmp(&a.1.output.len()));
    let mut primary = successful.remove(0);
    for (name, other) in &successful {
        if other.output.len() > 100 {
            primary
                .1
                .output
                .push_str(&format!("\n\n--- Additional ({name}) ---\n"));
            primary.1.output.push_str(&other.output);
        }
    }
    primary.1
}

/// Try a specific search engine by name.
async fn try_engine(
    client: &reqwest::Client,
    query: &str,
    count: u8,
    engine: &str,
) -> Option<SearchResult> {
    let r = match engine {
        "tavily" => {
            let key = std::env::var("TAVILY_API_KEY").ok()?;
            tavily_search(client, query, count, &key).await
        }
        "perplexity" => {
            let key = std::env::var("PERPLEXITY_API_KEY").ok()?;
            perplexity_search(client, query, &key).await
        }
        "brave" => {
            let key = std::env::var("BRAVE_API_KEY").ok()?;
            brave_search(client, query, count, &key).await
        }
        "you" => {
            let key = std::env::var("YDC_API_KEY").ok()?;
            you_search(client, query, count, &key).await
        }
        "duckduckgo" => ddg_search(client, query, count).await,
        _ => return None,
    };
    if r.success && !r.output.contains("No results found") {
        Some(r)
    } else {
        None
    }
}

/// Tavily AI-optimized search.
async fn tavily_search(
    client: &reqwest::Client,
    query: &str,
    count: u8,
    api_key: &str,
) -> SearchResult {
    let body = serde_json::json!({
        "api_key": api_key,
        "query": query,
        "max_results": count.min(10),
        "include_answer": true,
        "include_raw_content": false,
    });

    let response = match client
        .post("https://api.tavily.com/search")
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return SearchResult {
                output: format!("Tavily error: {e}"),
                success: false,
            };
        }
    };

    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return SearchResult {
            output: format!("Tavily HTTP {status}: {}", {
                let mut end = text.len().min(200);
                while !text.is_char_boundary(end) && end > 0 {
                    end -= 1;
                }
                &text[..end]
            }),
            success: false,
        };
    }

    let data: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return SearchResult {
                output: format!("Tavily parse error: {e}"),
                success: false,
            };
        }
    };

    let mut output = String::new();

    // Include AI-generated answer if available
    if let Some(answer) = data["answer"].as_str() {
        if !answer.is_empty() {
            output.push_str("**AI Summary:**\n");
            output.push_str(answer);
            output.push_str("\n\n");
        }
    }

    // Include search results
    if let Some(results) = data["results"].as_array() {
        for (i, r) in results.iter().enumerate() {
            let title = r["title"].as_str().unwrap_or("Untitled");
            let url = r["url"].as_str().unwrap_or("");
            let content = r["content"].as_str().unwrap_or("");
            output.push_str(&format!("{}. [{}]({})\n{}\n\n", i + 1, title, url, content));
        }
    }

    SearchResult {
        output,
        success: true,
    }
}

// ---------------------------------------------------------------------------
// DuckDuckGo HTML search
// ---------------------------------------------------------------------------

async fn ddg_search(client: &reqwest::Client, query: &str, count: u8) -> SearchResult {
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoded(query));
    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return SearchResult {
                output: format!("DuckDuckGo error: {e}"),
                success: false,
            }
        }
    };
    if !response.status().is_success() {
        return SearchResult {
            output: format!("DuckDuckGo HTTP {}", response.status()),
            success: false,
        };
    }
    let html = response.text().await.unwrap_or_default();
    let results = parse_ddg_results(&html, count as usize);
    if results.is_empty() {
        return SearchResult {
            output: format!("No results found for: {query}"),
            success: true,
        };
    }
    let mut output = format!("Results for: {query}\n\n");
    for (i, (title, url, snippet)) in results.iter().enumerate() {
        output.push_str(&format!("{}. {title}\n   {url}\n   {snippet}\n\n", i + 1));
    }
    SearchResult {
        output,
        success: true,
    }
}

fn parse_ddg_results(html: &str, max: usize) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
    let marker = "class=\"result__a\"";
    let mut search_from = 0;
    while results.len() < max {
        let pos = match html[search_from..].find(marker) {
            Some(p) => search_from + p + marker.len(),
            None => break,
        };
        search_from = pos;
        let chunk = &html[pos..];
        let raw_href = match extract_attr(chunk, "href=\"") {
            Some(h) => h,
            None => continue,
        };
        let url = decode_ddg_url(&raw_href);
        if !url.starts_with("http") || url.contains("duckduckgo.com/y.js") {
            continue;
        }
        let title = match chunk.find('>') {
            Some(gt) => {
                let after = &chunk[gt + 1..];
                match after.find("</a>") {
                    Some(end) => strip_tags(&after[..end]),
                    None => continue,
                }
            }
            None => continue,
        };
        if title.is_empty() {
            continue;
        }
        let snippet_marker = "class=\"result__snippet\"";
        let snippet = if let Some(sp) = chunk.find(snippet_marker) {
            let after_marker = &chunk[sp + snippet_marker.len()..];
            match after_marker.find('>') {
                Some(gt) => {
                    let content = &after_marker[gt + 1..];
                    match content.find("</a>") {
                        Some(end) => strip_tags(&content[..end]),
                        None => String::new(),
                    }
                }
                None => String::new(),
            }
        } else {
            String::new()
        };
        results.push((title, url, snippet));
    }
    results
}

fn decode_ddg_url(raw: &str) -> String {
    if let Some(start) = raw.find("uddg=") {
        let encoded = &raw[start + 5..];
        let end = encoded.find('&').unwrap_or(encoded.len());
        urldecoded(&encoded[..end])
    } else {
        raw.to_string()
    }
}

// ---------------------------------------------------------------------------
// Brave Search
// ---------------------------------------------------------------------------

async fn brave_search(
    client: &reqwest::Client,
    query: &str,
    count: u8,
    api_key: &str,
) -> SearchResult {
    let response = match client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", api_key)
        .header("Accept", "application/json")
        .query(&[("q", query), ("count", &count.to_string())])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return SearchResult {
                output: format!("Brave error: {e}"),
                success: false,
            }
        }
    };
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return SearchResult {
            output: format!("Brave ({status}): {body}"),
            success: false,
        };
    }
    let brave: BraveResponse = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            return SearchResult {
                output: format!("Brave parse error: {e}"),
                success: false,
            }
        }
    };
    let results = brave.web.map(|w| w.results).unwrap_or_default();
    if results.is_empty() {
        return SearchResult {
            output: format!("No results found for: {query}"),
            success: true,
        };
    }
    let mut output = format!("Results for: {query}\n\n");
    for (i, r) in results.iter().enumerate() {
        output.push_str(&format!(
            "{}. {}\n   {}\n   {}\n\n",
            i + 1,
            r.title,
            r.url,
            r.description
        ));
    }
    SearchResult {
        output,
        success: true,
    }
}

// ---------------------------------------------------------------------------
// You.com Search
// ---------------------------------------------------------------------------

async fn you_search(
    client: &reqwest::Client,
    query: &str,
    count: u8,
    api_key: &str,
) -> SearchResult {
    let response = match client
        .get("https://ydc-index.io/v1/search")
        .header("X-API-Key", api_key)
        .query(&[("query", query), ("count", &count.to_string())])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return SearchResult {
                output: format!("You.com error: {e}"),
                success: false,
            }
        }
    };
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return SearchResult {
            output: format!("You.com ({status}): {body}"),
            success: false,
        };
    }
    let you: YouResponse = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            return SearchResult {
                output: format!("You.com parse error: {e}"),
                success: false,
            }
        }
    };
    let results = you.results.and_then(|r| r.web).unwrap_or_default();
    if results.is_empty() {
        return SearchResult {
            output: format!("No results found for: {query}"),
            success: true,
        };
    }
    let mut output = format!("Results for: {query}\n\n");
    for (i, r) in results.iter().enumerate() {
        output.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.description.is_empty() {
            output.push_str(&format!("   {}\n", r.description));
        }
        if let Some(snippet) = r.snippets.first() {
            output.push_str(&format!("   {}\n", truncate_utf8(snippet, 300, "...")));
        }
        output.push('\n');
    }
    SearchResult {
        output,
        success: true,
    }
}

// ---------------------------------------------------------------------------
// Perplexity Sonar Search
// ---------------------------------------------------------------------------

async fn perplexity_search(client: &reqwest::Client, query: &str, api_key: &str) -> SearchResult {
    let body = serde_json::json!({
        "model": "sonar",
        "messages": [{"role": "user", "content": query}],
        "max_tokens": 1024
    });
    let response = match client
        .post("https://api.perplexity.ai/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return SearchResult {
                output: format!("Perplexity error: {e}"),
                success: false,
            }
        }
    };
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return SearchResult {
            output: format!("Perplexity ({status}): {body}"),
            success: false,
        };
    }
    let pplx: PerplexityResponse = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            return SearchResult {
                output: format!("Perplexity parse error: {e}"),
                success: false,
            }
        }
    };
    let answer = pplx
        .choices
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.message)
        .and_then(|m| m.content)
        .unwrap_or_default();
    if answer.is_empty() {
        return SearchResult {
            output: format!("No results found for: {query}"),
            success: true,
        };
    }
    let mut output = format!("Search: {query}\n\n{answer}");
    if !pplx.citations.is_empty() {
        output.push_str("\n\nSources:\n");
        for (i, url) in pplx.citations.iter().enumerate() {
            output.push_str(&format!("  [{}] {}\n", i + 1, url));
        }
    }
    SearchResult {
        output,
        success: true,
    }
}

// ---------------------------------------------------------------------------
// Parallel page fetching
// ---------------------------------------------------------------------------

async fn fetch_pages_parallel(
    client: &reqwest::Client,
    urls: &[String],
    max_chars: usize,
) -> Vec<CrawledPage> {
    let concurrency = 8;

    let results: Vec<Option<CrawledPage>> = stream::iter(urls.iter().enumerate())
        .map(|(i, url)| {
            let client = client.clone();
            let url = url.clone();
            let filename = format!("{:02}_{}.md", i + 1, host_slug(&url));
            async move {
                match fetch_page(&client, &url, max_chars).await {
                    Ok((content, links)) if !content.is_empty() => Some(CrawledPage {
                        url,
                        filename,
                        content,
                        outbound_links: links,
                    }),
                    _ => None,
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    results.into_iter().flatten().collect()
}

async fn fetch_page(
    client: &reqwest::Client,
    url: &str,
    max_chars: usize,
) -> Result<(String, Vec<String>), String> {
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            if is_private_host(host) {
                return Ok((String::new(), vec![]));
            }
        }
    }

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let body = response
        .text()
        .await
        .map_err(|e| format!("read body failed: {e}"))?;

    let content = html_to_text(&body);
    let links = extract_links_from_html(&body, url);
    let truncated = truncate_utf8(&content, max_chars, "\n... (truncated)");
    Ok((truncated, links))
}

/// Extract all outbound http(s) links from HTML.
fn extract_links_from_html(html: &str, base_url: &str) -> Vec<String> {
    let base = url::Url::parse(base_url).ok();
    let document = scraper::Html::parse_document(html);
    let selector = match scraper::Selector::parse("a[href]") {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let mut links = Vec::new();
    for element in document.select(&selector) {
        if let Some(href) = element.value().attr("href") {
            let resolved = if href.starts_with("http") {
                href.to_string()
            } else if let Some(ref base) = base {
                base.join(href).map(|u| u.to_string()).unwrap_or_default()
            } else {
                continue;
            };
            if resolved.starts_with("http") && !is_private_url(&resolved) {
                links.push(resolved);
            }
        }
    }
    links
}

// ---------------------------------------------------------------------------
// HTML to text
// ---------------------------------------------------------------------------

fn html_to_text(html: &str) -> String {
    let document = scraper::Html::parse_document(html);
    let mut text_parts: Vec<String> = Vec::new();

    fn extract_text(node: ego_tree::NodeRef<'_, scraper::Node>, parts: &mut Vec<String>) {
        for child in node.children() {
            match child.value() {
                scraper::Node::Text(text) => {
                    let t = text.trim();
                    if !t.is_empty() {
                        parts.push(t.to_string());
                    }
                }
                scraper::Node::Element(el) => {
                    let tag = el.name();
                    if tag == "script" || tag == "style" || tag == "noscript" {
                        continue;
                    }
                    let is_block = matches!(
                        tag,
                        "p" | "div"
                            | "h1"
                            | "h2"
                            | "h3"
                            | "h4"
                            | "h5"
                            | "h6"
                            | "li"
                            | "tr"
                            | "br"
                            | "hr"
                            | "blockquote"
                            | "pre"
                            | "section"
                            | "article"
                            | "header"
                            | "footer"
                            | "nav"
                            | "main"
                            | "aside"
                    );
                    if is_block {
                        parts.push("\n".to_string());
                    }
                    extract_text(child, parts);
                    if is_block {
                        parts.push("\n".to_string());
                    }
                }
                _ => {}
            }
        }
    }

    extract_text(document.tree.root(), &mut text_parts);
    let raw = text_parts.join(" ");

    let mut result = String::with_capacity(raw.len());
    let mut prev_newline = false;
    let mut prev_space = false;
    for ch in raw.chars() {
        if ch == '\n' {
            if !prev_newline {
                result.push('\n');
            }
            prev_newline = true;
            prev_space = false;
        } else if ch.is_whitespace() {
            if !prev_space && !prev_newline {
                result.push(' ');
            }
            prev_space = true;
        } else {
            prev_newline = false;
            prev_space = false;
            result.push(ch);
        }
    }
    result.trim().to_string()
}

// ---------------------------------------------------------------------------
// SSRF protection
// ---------------------------------------------------------------------------

fn is_private_host(host: &str) -> bool {
    if host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host == "0.0.0.0"
        || host.ends_with(".local")
        || host.ends_with(".internal")
    {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return is_private_ip(&ip);
    }
    false
}

fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64)
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.octets()[0] & 0xfe) == 0xfc
                || (v6.octets()[0] == 0xfe && (v6.octets()[1] & 0xc0) == 0x80)
        }
    }
}

fn is_private_url(url: &str) -> bool {
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            return is_private_host(host);
        }
    }
    false
}

/// Extract same-origin internal links from a page, filtering out already-seen URLs.
fn same_origin_links(
    page_url: &str,
    outbound_links: &[String],
    seen_urls: &HashSet<String>,
) -> Vec<String> {
    let origin = match url::Url::parse(page_url) {
        Ok(u) => u.origin().ascii_serialization(),
        Err(_) => return vec![],
    };

    outbound_links
        .iter()
        .filter(|link| {
            url::Url::parse(link)
                .ok()
                .map(|u| u.origin().ascii_serialization() == origin)
                .unwrap_or(false)
        })
        .filter(|link| !seen_urls.contains(&normalize_url(link)))
        .filter(|link| !is_non_content_url(link))
        .cloned()
        .collect()
}

/// Filter out URLs unlikely to have useful text content.
fn is_non_content_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.ends_with(".pdf")
        || lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".gif")
        || lower.ends_with(".svg")
        || lower.ends_with(".webp")
        || lower.ends_with(".zip")
        || lower.ends_with(".tar.gz")
        || lower.ends_with(".mp4")
        || lower.ends_with(".mp3")
        || lower.contains("/login")
        || lower.contains("/signup")
        || lower.contains("/register")
        || lower.contains("/auth/")
        || lower.contains("/api/")
        || lower.contains("/cdn-cgi/")
}

// ---------------------------------------------------------------------------
// URL helpers
// ---------------------------------------------------------------------------

fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(char::from(HEX[(b >> 4) as usize]));
                out.push(char::from(HEX[(b & 0xf) as usize]));
            }
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn urldecoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = bytes.next().and_then(hex_val);
            let lo = bytes.next().and_then(hex_val);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4 | l) as char);
            }
        } else {
            out.push(b as char);
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn extract_attr(html: &str, prefix: &str) -> Option<String> {
    let start = html.find(prefix)? + prefix.len();
    let end = html[start..].find('"')? + start;
    Some(decode_html_entities(&html[start..end]))
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    decode_html_entities(out.trim())
}

fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn extract_urls(output: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            urls.push(trimmed.to_string());
        }
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

/// Normalize a URL for deduplication (strip fragment, trailing slash, lowercase host).
fn normalize_url(url: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(url) {
        parsed.set_fragment(None);
        let s = parsed.to_string();
        s.trim_end_matches('/').to_lowercase()
    } else {
        url.to_lowercase()
    }
}

// ---------------------------------------------------------------------------
// String helpers
// ---------------------------------------------------------------------------

fn slugify(s: &str) -> String {
    let mut slug = String::with_capacity(s.len());
    for ch in s.chars().take(80) {
        if ch.is_alphanumeric() || ch > '\x7f' {
            // Keep CJK and other unicode chars as-is for readability
            slug.push(ch);
        } else if (ch == ' ' || ch == '-' || ch == '_') && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

fn host_slug(raw_url: &str) -> String {
    url::Url::parse(raw_url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.strip_prefix("www.").unwrap_or(h).replace('.', "-"))
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn truncate_utf8(s: &str, max_chars: usize, suffix: &str) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = s[..end].to_string();
    result.push_str(suffix);
    result
}

fn research_dir(slug: &str) -> PathBuf {
    PathBuf::from("./research").join(slug)
}

// ---------------------------------------------------------------------------
// Progress output (stderr for gateway to stream)
// ---------------------------------------------------------------------------

fn progress(step: usize, total: usize, msg: &str) {
    eprintln!("[{step}/{total}] {msg}");
}

fn progress_simple(msg: &str) {
    eprintln!("[*] {msg}");
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn print_output(output: &Output) {
    let json = serde_json::to_string(output).unwrap_or_else(|_| {
        r#"{"output":"Failed to serialize output","success":false}"#.to_string()
    });
    println!("{json}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("top AI startups 2025"), "top-AI-startups-2025");
        assert_eq!(slugify("NVIDIA stock price!"), "NVIDIA-stock-price");
        assert_eq!(slugify("  spaces  "), "spaces");
        // CJK preserved
        assert!(slugify("伊朗哈梅内伊").contains("伊朗"));
    }

    #[test]
    fn test_host_slug() {
        assert_eq!(host_slug("https://www.example.com/page"), "example-com");
        assert_eq!(host_slug("https://api.you.com/search"), "api-you-com");
    }

    #[test]
    fn test_normalize_url() {
        assert_eq!(
            normalize_url("https://Example.com/page#section"),
            "https://example.com/page"
        );
        assert_eq!(
            normalize_url("https://example.com/page/"),
            "https://example.com/page"
        );
    }

    #[test]
    fn test_extract_subtopics() {
        let text = "## Overview\n**Economy**: growth\n**Technology**: AI\n### Politics\nsome text";
        let topics = extract_subtopics(text);
        assert!(topics.contains(&"Economy".to_string()));
        assert!(topics.contains(&"Technology".to_string()));
        assert!(topics.contains(&"Politics".to_string()));
    }

    #[test]
    fn test_generate_follow_up_queries() {
        let queries = generate_follow_up_queries("AI regulations", "**Ethics** and **Safety**", 2);
        assert!(queries.len() >= 2);
        assert!(queries.iter().any(|q| q.contains("2026")));
    }

    #[test]
    fn test_extract_urls() {
        let output = "Results:\n   https://example.com/page\n  [1] https://other.com\n";
        let urls = extract_urls(output);
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn test_truncate_utf8() {
        assert_eq!(truncate_utf8("Hello, world!", 100, "..."), "Hello, world!");
        assert_eq!(truncate_utf8("Hello, world!", 5, "..."), "Hello...");
    }

    #[test]
    fn test_is_private_host() {
        assert!(is_private_host("localhost"));
        assert!(is_private_host("127.0.0.1"));
        assert!(!is_private_host("example.com"));
    }

    #[test]
    fn test_input_deserialization_defaults() {
        let json = r#"{"query": "test"}"#;
        let input: Input = serde_json::from_str(json).unwrap();
        assert_eq!(input.query, "test");
        assert_eq!(input.max_results, 8);
        assert_eq!(input.depth, 2);
    }

    #[test]
    fn test_input_deserialization_full() {
        let json =
            r#"{"query": "test", "max_results": 3, "depth": 3, "search_engine": "perplexity"}"#;
        let input: Input = serde_json::from_str(json).unwrap();
        assert_eq!(input.depth, 3);
        assert_eq!(input.search_engine.as_deref(), Some("perplexity"));
    }

    #[test]
    fn test_same_origin_links() {
        let seen: HashSet<String> = HashSet::new();
        let outbound = vec![
            "https://example.com/page2".to_string(),
            "https://example.com/page3".to_string(),
            "https://other.com/external".to_string(),
            "https://example.com/login".to_string(), // filtered: /login
            "https://example.com/image.png".to_string(), // filtered: .png
        ];
        let result = same_origin_links("https://example.com/page1", &outbound, &seen);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"https://example.com/page2".to_string()));
        assert!(result.contains(&"https://example.com/page3".to_string()));
    }

    #[test]
    fn test_same_origin_links_respects_seen() {
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert("https://example.com/page2".to_string());
        let outbound = vec![
            "https://example.com/page2".to_string(),
            "https://example.com/page3".to_string(),
        ];
        let result = same_origin_links("https://example.com/page1", &outbound, &seen);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "https://example.com/page3");
    }

    #[test]
    fn test_is_non_content_url() {
        assert!(is_non_content_url("https://example.com/image.png"));
        assert!(is_non_content_url("https://example.com/file.zip"));
        assert!(is_non_content_url("https://example.com/login"));
        assert!(is_non_content_url("https://example.com/auth/callback"));
        assert!(is_non_content_url("https://example.com/api/v1/data"));
        assert!(!is_non_content_url("https://example.com/docs/guide"));
        assert!(!is_non_content_url("https://example.com/blog/post-1"));
    }
}
