//! Deep multi-round web research tool.
//!
//! Performs iterative search across multiple angles, fetches pages in parallel,
//! chases most-referenced links, and produces a structured research report.
//!
//! Reads JSON from stdin, outputs JSON to stdout, progress to stderr.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
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
    /// Synthesis LLM provider config injected by the host (S2 plumbing).
    ///
    /// When present and complete, `resolve_synthesis_config` prefers this over
    /// reading API keys from environment variables. This lets the host route
    /// per-tenant or per-session credentials without requiring plist `EnvironmentVariables`.
    #[serde(default)]
    synthesis_config: Option<SynthesisConfig>,
}

/// Synthesis LLM provider config passed by the host.
///
/// Mirrors the `(endpoint, api_key, model, provider)` quadruple that
/// [`resolve_synthesis_config`] used to read from environment variables. All
/// fields are required for the args path to take precedence — partial configs
/// fall through to the env-var path so the operator can still set defaults.
#[derive(Deserialize, Clone, Debug)]
struct SynthesisConfig {
    /// OpenAI-compatible base URL, e.g. `https://api.deepseek.com/v1`.
    endpoint: String,
    /// Bearer token for the synthesis provider.
    ///
    /// Tokens MUST NOT be logged. Audit `tracing::*` and `eprintln!` paths
    /// before adding new diagnostics.
    api_key: String,
    /// Model id to request (e.g. `deepseek-chat`).
    model: String,
    /// Provider label used by the v2 cost envelope (e.g. `deepseek`).
    provider: String,
}

fn default_max_results() -> u8 {
    8
}
fn default_depth() -> u8 {
    2
}

#[derive(Serialize, Default)]
struct Output {
    output: String,
    success: bool,
    /// Plugin-protocol-v2 summary. The host's
    /// `SubAgentSummaryGenerator` consumes this to build the parent
    /// agent's view of the call without re-running an LLM.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<ResultSummary>,
    /// Plugin-protocol-v2 roll-up cost. Sums all internal LLM/API
    /// spend incurred during this invocation. Per-call costs are also
    /// emitted as stderr `cost` events for finer-grained ledger
    /// attribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    cost: Option<ResultCost>,
    /// Files the host should auto-deliver to chat. Mirrors v1
    /// behavior; we name the synthesized `_report.md` here so the
    /// chat UI shows the report file, not the search-engine dump.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files_to_send: Vec<String>,
}

/// v2 result summary: discriminator + headline + sources. Mirrors
/// `octos_plugin::protocol_v2::ResultSummary` field-for-field. Avoids a
/// dependency on `octos-plugin` from the standalone plugin binary
/// (plugin binaries should be self-contained per the SDK contract).
#[derive(Serialize, Deserialize, Default)]
struct ResultSummary {
    kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    headline: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sources: Vec<ResultSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rounds: Option<u32>,
}

#[derive(Serialize, Deserialize, Default)]
struct ResultSource {
    url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    title: String,
    #[serde(default)]
    cited: bool,
}

/// v2 roll-up cost. Mirrors `octos_plugin::protocol_v2::ResultCost`.
#[derive(Serialize, Deserialize, Default)]
struct ResultCost {
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    tokens_in: u32,
    tokens_out: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    usd: Option<f64>,
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
    // Plugin-protocol-v2 SIGTERM handler (W3.C3): on SIGTERM we stop
    // scheduling new work and exit cleanly within the 10-second host
    // budget. We don't carry long-lived browsers in-process here
    // (deep_crawl spawns its own), so the cleanup path is light: emit
    // a final progress event, kill in-flight HTTP via dropping the
    // client, and exit 130 (128 + SIGTERM=2).
    install_sigterm_handler();

    let mut stdin_buf = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut stdin_buf) {
        print_output(&Output {
            output: format!("Failed to read stdin: {e}"),
            success: false,
            ..Default::default()
        });
        return;
    }

    let input: Input = match serde_json::from_str(&stdin_buf) {
        Ok(v) => v,
        Err(e) => {
            print_output(&Output {
                output: format!("Invalid input JSON: {e}"),
                success: false,
                ..Default::default()
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
            input.synthesis_config.as_ref(),
        ),
    )
    .await;

    match result {
        Ok(output) => print_output(&output),
        Err(_) => print_output(&Output {
            output: format!("Deep search timed out after {}s", timeout.as_secs()),
            success: false,
            ..Default::default()
        }),
    }
}

/// Install a SIGTERM handler that emits a final v2 progress event and
/// exits with status 130 within the host's 10-second cancel budget.
///
/// On Windows there is no SIGTERM; the host falls back to job-object
/// kill which doesn't run user code. The handler is therefore a no-op
/// on Windows and the host's SIGKILL handles cleanup.
#[cfg(unix)]
fn install_sigterm_handler() {
    use tokio::signal::unix::{signal, SignalKind};
    tokio::spawn(async {
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[deep_search] failed to install SIGTERM handler: {e}");
                return;
            }
        };
        if term.recv().await.is_some() {
            // Best-effort final progress event so the operator sees
            // why we're exiting in the chat UI.
            emit_v2_progress(
                "cleanup",
                "SIGTERM received, shutting down deep_search",
                None,
            );
            // 130 = 128 + SIGTERM(2). Convention for "killed by signal 2".
            std::process::exit(130);
        }
    });
}

#[cfg(not(unix))]
fn install_sigterm_handler() {
    // No SIGTERM on Windows; deep_search exits via host SIGKILL.
}

async fn run_deep_search(
    client: &reqwest::Client,
    query: &str,
    max_results: u8,
    depth: u8,
    engine: Option<&str>,
    synthesis_config: Option<&SynthesisConfig>,
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
            ..Default::default()
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
            ..Default::default()
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
    progress_simple(
        ProgressPhase::Fetch,
        &format!("Fetching {total_fetch} pages in parallel..."),
    );

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
        ranked.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        let chase_urls: Vec<String> = ranked
            .into_iter()
            .take(chase_limit)
            .filter(|(_, count)| *count >= 2) // Only chase links referenced by 2+ pages
            .map(|(url, _)| url)
            .collect();

        if !chase_urls.is_empty() {
            progress_simple(
                ProgressPhase::Fetch,
                &format!("Chasing {} most-referenced sources...", chase_urls.len()),
            );
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
        ranked_domains.sort_by_key(|entry| std::cmp::Reverse(entry.1.len()));

        let to_crawl: Vec<String> = ranked_domains
            .into_iter()
            .take(crawl_domains)
            .flat_map(|(domain, links)| {
                let take = links.len().min(pages_per_domain);
                progress_simple(
                    ProgressPhase::Fetch,
                    &format!(
                        "Site crawl: {} ({} internal links, fetching {})",
                        domain,
                        links.len(),
                        take
                    ),
                );
                links.into_iter().take(pages_per_domain)
            })
            .collect();

        if !to_crawl.is_empty() {
            progress_simple(
                ProgressPhase::Fetch,
                &format!(
                    "Site crawl: fetching {} additional pages from top domains...",
                    to_crawl.len()
                ),
            );

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
    // Synthesize an answer from the raw search dump + crawled excerpts.
    //
    // This is the W3.C1 "highest user-impact" change: instead of returning
    // a wall of search snippets, we hand the corpus to an LLM and ask it
    // to write a coherent multi-paragraph answer with `[N]` citations
    // pointing at our `Sources` list. The LLM call is best-effort: if no
    // API key is configured we fall back to the v1 behavior so the plugin
    // still works in airgapped/dev setups.
    // -----------------------------------------------------------------------
    progress_simple(ProgressPhase::Synthesize, "Synthesizing report...");
    emit_v2_progress(
        "synthesizing",
        "Synthesizing report from sources...",
        Some(0.85),
    );

    let synthesis_input = SynthesisInput {
        query,
        rounds: search_queries.len(),
        sources: saved_files
            .iter()
            .enumerate()
            .map(|(i, (_, url, preview))| SynthesisSource {
                index: i + 1,
                url: url.clone(),
                excerpt: preview.clone(),
            })
            .collect(),
    };

    let synthesis = synthesize(client, &synthesis_input, synthesis_config).await;

    progress_simple(ProgressPhase::ReportBuild, "Building report...");
    emit_v2_progress(
        "building_report",
        "Assembling final document...",
        Some(0.95),
    );

    let report_path = dir.join("_report.md");
    let report = build_report(
        query,
        synthesis.as_ref(),
        &initial_answer,
        &saved_files,
        &search_queries,
        &dir,
        &report_path,
    );

    // Save report
    let _ = fs::write(dir.join("_report.md"), &report);

    progress_simple_with_fraction(ProgressPhase::Completion, "Deep search complete", Some(1.0));
    emit_v2_progress("complete", "Deep search complete", Some(1.0));

    // Build the v2 result summary from the synthesis output (if any) so
    // the parent agent can render a useful tool-call pill without
    // re-parsing the report markdown.
    let cited_indexes: HashSet<usize> = synthesis
        .as_ref()
        .map(|s| s.cited_indexes())
        .unwrap_or_default();
    let mut summary_sources = Vec::with_capacity(saved_files.len());
    for (i, (_filename, url, _)) in saved_files.iter().enumerate() {
        summary_sources.push(ResultSource {
            url: url.clone(),
            title: String::new(),
            cited: cited_indexes.contains(&(i + 1)),
        });
    }
    let summary = ResultSummary {
        kind: "deep_research".to_string(),
        headline: synthesis
            .as_ref()
            .map(|s| s.headline.clone())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "Researched '{query}' across {} sources in {} rounds",
                    saved_files.len(),
                    search_queries.len()
                )
            }),
        confidence: synthesis.as_ref().and_then(|s| s.confidence),
        sources: summary_sources,
        rounds: Some(search_queries.len() as u32),
    };

    let cost = synthesis.as_ref().map(|s| ResultCost {
        provider: Some(s.provider.clone()),
        model: Some(s.model.clone()),
        tokens_in: s.tokens_in,
        tokens_out: s.tokens_out,
        usd: s.usd,
    });

    Output {
        output: report,
        success: true,
        summary: Some(summary),
        cost,
        files_to_send: vec![report_path.display().to_string()],
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
    // Priority: serper > tavily > perplexity > brave > bing_cdp > duckduckgo
    let mut available: Vec<&str> = Vec::new();
    if std::env::var("SERPER_API_KEY")
        .ok()
        .is_some_and(|k| !k.is_empty())
    {
        available.push("serper");
    }
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
    available.push("bing_cdp"); // free, uses headless Chrome
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

    successful.sort_by_key(|entry| std::cmp::Reverse(entry.1.output.len()));
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

    if let Ok(k) = std::env::var("SERPER_API_KEY") {
        if !k.is_empty() {
            let c = client.clone();
            let q = query.to_string();
            handles.push(tokio::spawn(async move {
                ("serper", serper_search(&c, &q, count, &k).await)
            }));
        }
    }
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
        let q = query.to_string();
        handles.push(tokio::spawn(async move {
            ("bing_cdp", bing_cdp_search(&q, count).await)
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

    successful.sort_by_key(|entry| std::cmp::Reverse(entry.1.output.len()));
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
        "serper" => {
            let key = std::env::var("SERPER_API_KEY").ok()?;
            serper_search(client, query, count, &key).await
        }
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
        "bing_cdp" => bing_cdp_search(query, count).await,
        _ => return None,
    };
    if r.success && !r.output.contains("No results found") {
        Some(r)
    } else {
        None
    }
}

/// Serper.dev Google Search API.
async fn serper_search(
    client: &reqwest::Client,
    query: &str,
    count: u8,
    api_key: &str,
) -> SearchResult {
    let response = match client
        .post("https://google.serper.dev/search")
        .header("X-API-KEY", api_key)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({"q": query, "num": count.min(10)}))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return SearchResult {
                output: format!("Serper error: {e}"),
                success: false,
            };
        }
    };

    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return SearchResult {
            output: format!("Serper HTTP {status}: {}", {
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
                output: format!("Serper parse error: {e}"),
                success: false,
            };
        }
    };

    let mut output = String::new();

    if let Some(kg) = data.get("knowledgeGraph") {
        if let Some(title) = kg["title"].as_str() {
            output.push_str(&format!("**{}**", title));
            if let Some(desc) = kg["description"].as_str() {
                output.push_str(&format!(": {}", desc));
            }
            output.push_str("\n\n");
        }
    }

    if let Some(results) = data["organic"].as_array() {
        for (i, r) in results.iter().enumerate() {
            let title = r["title"].as_str().unwrap_or("Untitled");
            let link = r["link"].as_str().unwrap_or("");
            let snippet = r["snippet"].as_str().unwrap_or("");
            output.push_str(&format!(
                "{}. [{}]({})\n{}\n\n",
                i + 1,
                title,
                link,
                snippet
            ));
        }
    }

    let success = !output.is_empty();
    SearchResult { output, success }
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
// Bing CDP Search (headless Chrome via deep-crawl binary)
// ---------------------------------------------------------------------------

/// Cap on the number of concurrent `deep_crawl` invocations (and thus
/// concurrent headless Chromium processes) per `deep_search` invocation.
///
/// Pre-W3.D1 history: with `parallel_all_engines` + per-round retries,
/// a single deep_search call could spawn 6+ Chromium processes at once,
/// pegging memory on small VMs. The semaphore caps it at 3.
///
/// Operators can override via `DEEP_SEARCH_MAX_BROWSERS` (1..16). Useful
/// when deep_search itself runs N times in parallel (e.g. swarm mode):
/// the cap on the binary's own scope-internal concurrency stays at the
/// configured value, so total chromiums = N × cap.
fn max_browsers() -> usize {
    std::env::var("DEEP_SEARCH_MAX_BROWSERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.clamp(1, 16))
        .unwrap_or(3)
}

fn browser_semaphore() -> &'static tokio::sync::Semaphore {
    static SEMAPHORE: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SEMAPHORE.get_or_init(|| tokio::sync::Semaphore::new(max_browsers()))
}

/// Search Bing via headless Chrome. Calls the `deep_crawl` sibling binary
/// to render the SERP, then extracts result links from the text output.
/// No API key needed — just Chromium installed. Uses Bing instead of Google
/// because Google CAPTCHAs automated requests from datacenter IPs.
///
/// W3.D1: gated by [`browser_semaphore`] so this binary cannot launch
/// more than `DEEP_SEARCH_MAX_BROWSERS` concurrent chromiums.
async fn bing_cdp_search(query: &str, count: u8) -> SearchResult {
    // Acquire the semaphore before launching deep_crawl so we never
    // exceed the configured cap on concurrent chromiums.
    let _permit = match browser_semaphore().acquire().await {
        Ok(p) => p,
        Err(_) => {
            return SearchResult {
                output: "bing_cdp:browser semaphore closed".into(),
                success: false,
            };
        }
    };
    // Find deep_crawl binary: check sibling dir, cargo bin, and PATH
    let crawl_bin = {
        let candidates: Vec<std::path::PathBuf> = [
            // Sibling bundled-app-skill directory
            std::env::current_exe().ok().and_then(|p| {
                p.parent()?
                    .parent()
                    .map(|d| d.join("deep-crawl").join("main"))
            }),
            // Same directory as our binary
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("deep_crawl").to_path_buf())),
            // ~/.cargo/bin
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".cargo/bin/deep_crawl")),
        ]
        .into_iter()
        .flatten()
        .collect();

        match candidates.into_iter().find(|p| p.exists()) {
            Some(p) => p,
            None => {
                // Try PATH via which
                match std::process::Command::new("which")
                    .arg("deep_crawl")
                    .output()
                {
                    Ok(o) if o.status.success() => {
                        std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim())
                    }
                    _ => {
                        return SearchResult {
                            output: "bing_cdp:deep_crawl binary not found".into(),
                            success: false,
                        };
                    }
                }
            }
        }
    };

    // Use Bing instead of Google — Google CAPTCHAs automated requests from datacenter IPs.
    // Bing is much more lenient with headless Chrome scraping.
    let search_url = format!(
        "https://www.bing.com/search?q={}&count={}",
        urlencoded(query),
        count.min(10)
    );

    let input = serde_json::json!({
        "url": search_url,
        "max_depth": 0,
        "max_pages": 1,
        "timeout_secs": 30
    });

    let result = tokio::process::Command::new(&crawl_bin)
        .arg("deep_crawl")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();

    let mut child = match result {
        Ok(c) => c,
        Err(e) => {
            return SearchResult {
                output: format!("bing_cdp:failed to spawn deep_crawl: {e}"),
                success: false,
            };
        }
    };

    // Write input JSON to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(input.to_string().as_bytes()).await;
    }

    // Wait with timeout
    let output =
        match tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output())
            .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return SearchResult {
                    output: format!("bing_cdp:deep_crawl failed: {e}"),
                    success: false,
                };
            }
            Err(_) => {
                return SearchResult {
                    output: "bing_cdp:timeout after 60s".into(),
                    success: false,
                };
            }
        };

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse deep_crawl JSON output
    let parsed: serde_json::Value = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(_) => {
            return SearchResult {
                output: format!("bing_cdp:failed to parse output ({} bytes)", stdout.len()),
                success: false,
            };
        }
    };

    let text = parsed.get("output").and_then(|v| v.as_str()).unwrap_or("");

    if text.is_empty() {
        return SearchResult {
            output: format!("bing_cdp:empty response for: {query}"),
            success: false,
        };
    }

    // Extract URLs from the crawled text (Google SERP contains result URLs)
    let mut results = Vec::new();
    let mut current_title = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Lines with URLs
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            if !trimmed.contains("bing.com")
                && !trimmed.contains("microsoft.com")
                && !trimmed.contains("google.com")
                && !trimmed.contains("gstatic.com")
            {
                let title = if current_title.is_empty() {
                    trimmed.to_string()
                } else {
                    std::mem::take(&mut current_title)
                };
                results.push(format!("- {}\n  {}", title, trimmed));
            }
        } else if trimmed.len() > 10 && !trimmed.contains("Google") && !trimmed.contains("Bing") {
            current_title = trimmed.to_string();
        }
    }

    if results.is_empty() {
        // Fall back to returning the raw text which may have useful content
        return SearchResult {
            output: format!(
                "Bing results for: {query}\n\n{}",
                &text[..text.len().min(3000)]
            ),
            success: !text.is_empty(),
        };
    }

    let output = format!(
        "Bing results for: {query}\n\n{}",
        results
            .iter()
            .take(count as usize)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );

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
                    // Skip non-content elements entirely
                    if matches!(
                        tag,
                        "script"
                            | "style"
                            | "noscript"
                            | "nav"
                            | "footer"
                            | "aside"
                            | "iframe"
                            | "svg"
                            | "form"
                    ) {
                        continue;
                    }
                    // Skip elements with boilerplate class/id hints
                    if is_boilerplate_element(el) {
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
    let trimmed = result.trim().to_string();
    clean_boilerplate(&trimmed)
}

/// Check if an HTML element is likely boilerplate based on class/id/role.
fn is_boilerplate_element(el: &scraper::node::Element) -> bool {
    let class = el.attr("class").unwrap_or("");
    let id = el.attr("id").unwrap_or("");
    let role = el.attr("role").unwrap_or("");
    if matches!(
        role,
        "navigation" | "banner" | "complementary" | "contentinfo"
    ) {
        return true;
    }
    let combined = format!("{class} {id}").to_lowercase();
    combined.contains("cookie")
        || combined.contains("consent")
        || combined.contains("gdpr")
        || combined.contains("advertisement")
        || combined.contains("ad-slot")
        || combined.contains("sidebar")
        || combined.contains("side-bar")
        || combined.contains("newsletter")
        || combined.contains("subscribe")
        || combined.contains("popup")
        || combined.contains("modal")
        || combined.contains("overlay")
        || combined.contains("share-button")
        || combined.contains("social-share")
        || combined.contains("related-post")
        || combined.contains("comment")
        || combined.contains("breadcrumb")
        || combined.contains("pagination")
        || combined.contains("menu")
        || combined.contains("toolbar")
}

/// Remove common boilerplate noise lines from extracted text.
fn clean_boilerplate(text: &str) -> String {
    let noise: &[&str] = &[
        "accept all cookies",
        "accept cookies",
        "cookie policy",
        "cookie settings",
        "we use cookies",
        "this website uses cookies",
        "privacy policy",
        "terms of service",
        "terms and conditions",
        "sign up for our newsletter",
        "subscribe to our newsletter",
        "follow us on",
        "share this article",
        "share on facebook",
        "share on twitter",
        "advertisement",
        "skip to content",
        "skip to main content",
        "back to top",
        "loading...",
        "please enable javascript",
    ];
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| {
            let t = line.trim();
            if t.len() < 3 {
                return t.is_empty();
            }
            let lower = t.to_lowercase();
            !noise.iter().any(|p| lower.contains(p))
        })
        .collect();
    let mut result = String::with_capacity(text.len());
    let mut blank_count = 0;
    for line in lines {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(line);
            result.push('\n');
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
    let base = std::env::var("OCTOS_WORK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("research").join(slug)
}

// ---------------------------------------------------------------------------
// Report assembly (W3.C1)
// ---------------------------------------------------------------------------

/// Build the full markdown research report from the synthesis output and
/// the raw crawled corpus.
///
/// Pure function: doesn't touch the filesystem or stderr, so it's easy to
/// unit-test the structural guarantees ("must contain a `## Synthesis`
/// section when synthesis is available", "must list all sources",
/// "must end with the report path").
fn build_report(
    query: &str,
    synthesis: Option<&SynthesisResult>,
    initial_answer: &str,
    saved_files: &[(String, String, String)],
    search_queries: &[String],
    dir: &Path,
    report_path: &Path,
) -> String {
    let mut report = String::new();
    report.push_str(&format!("# Deep Research: {query}\n\n"));

    // Synthesis section — prose with citations, replaces the old "Overview".
    match synthesis {
        Some(syn) if !syn.synthesis.trim().is_empty() => {
            if !syn.headline.is_empty() {
                report.push_str(&format!("_{}_\n\n", syn.headline));
            }
            report.push_str("## Synthesis\n\n");
            report.push_str(syn.synthesis.trim());
            report.push_str("\n\n");
            if let Some(conf) = syn.confidence {
                report.push_str(&format!("_Self-reported confidence: {conf:.2}_\n\n"));
            }
        }
        _ => {
            // Fallback when no LLM is available or synthesis was empty:
            // keep the v1 "Overview" but label it so it's clear we did
            // NOT synthesize, and operators know the result is raw.
            report.push_str("## Overview\n\n");
            report.push_str("_LLM synthesis unavailable — showing raw search results below._\n\n");
            report.push_str(initial_answer);
            report.push_str("\n\n");
        }
    }

    // Source details with inline previews. These are always present so
    // the synthesis citations resolve to concrete URLs and the operator
    // can verify each claim.
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

    // Summary footer — kept for v1 compatibility (the host's
    // `Report saved to: ...` detector keys off this line).
    report.push_str(&format!(
        "\n---\n{} pages crawled across {} search rounds.\n\
         Report saved to: {}\n",
        saved_files.len(),
        search_queries.len(),
        report_path.display(),
    ));

    report
}

// ---------------------------------------------------------------------------
// Synthesis (W3.C1)
// ---------------------------------------------------------------------------

/// Inputs to the synthesis LLM call: the original query plus the corpus of
/// crawled excerpts the LLM should ground its answer on.
struct SynthesisInput<'a> {
    query: &'a str,
    rounds: usize,
    sources: Vec<SynthesisSource>,
}

struct SynthesisSource {
    /// 1-based index used in `[N]` citations the LLM emits.
    index: usize,
    url: String,
    excerpt: String,
}

/// Output of a successful synthesis call: prose with citations + metadata
/// for the v2 result envelope.
struct SynthesisResult {
    /// Multi-paragraph synthesized answer with `[N]` citations.
    synthesis: String,
    /// Optional one-line headline. Used for the parent's tool-call pill.
    headline: String,
    /// Self-reported confidence in `[0, 1]`. Heuristic; the LLM is asked
    /// to assess source quality and agreement.
    confidence: Option<f64>,
    /// Provider / model used. Reported in the v2 cost envelope.
    provider: String,
    model: String,
    tokens_in: u32,
    tokens_out: u32,
    usd: Option<f64>,
}

impl SynthesisResult {
    /// Extract the set of `[N]` citation indexes referenced in the prose.
    fn cited_indexes(&self) -> HashSet<usize> {
        let mut out = HashSet::new();
        let bytes = self.synthesis.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'[' {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1 && j < bytes.len() && bytes[j] == b']' {
                    if let Ok(s) = std::str::from_utf8(&bytes[i + 1..j]) {
                        if let Ok(n) = s.parse::<usize>() {
                            out.insert(n);
                        }
                    }
                    i = j + 1;
                    continue;
                }
            }
            i += 1;
        }
        out
    }
}

/// Run the synthesis LLM call. Returns `None` when no API key is
/// configured, the call fails, or the response is unusable. The deep_search
/// flow falls back to the v1 raw-dump report in that case.
async fn synthesize(
    client: &reqwest::Client,
    input: &SynthesisInput<'_>,
    args_config: Option<&SynthesisConfig>,
) -> Option<SynthesisResult> {
    let (endpoint, api_key, model, provider) = resolve_synthesis_config(args_config)?;

    let prompt = build_synthesis_prompt(input);
    let prompt_chars = prompt.len();

    let body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": SYNTHESIS_SYSTEM_PROMPT
            },
            {
                "role": "user",
                "content": prompt
            }
        ],
        "max_tokens": 1500,
        "temperature": 0.3,
    });

    let response = match client
        .post(format!("{endpoint}/chat/completions"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(Duration::from_secs(60))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[synthesis] LLM call failed: {e}");
            emit_v2_progress("synthesizing", &format!("LLM call failed: {e}"), None);
            return None;
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        eprintln!(
            "[synthesis] HTTP {status}: {}",
            truncate_utf8(&text, 300, "")
        );
        emit_v2_progress("synthesizing", &format!("LLM HTTP {status}"), None);
        return None;
    }

    let json: serde_json::Value = match response.json().await {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[synthesis] failed to parse response: {e}");
            return None;
        }
    };

    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim();
    if content.is_empty() {
        eprintln!("[synthesis] empty response");
        return None;
    }

    // OpenAI-compatible providers return token usage under "usage".
    let tokens_in = json["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32;
    let tokens_out = json["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32;
    let usd = project_usd(&model, tokens_in, tokens_out);

    let (synthesis_text, headline, confidence) = parse_synthesis_response(content);

    // Emit a v2 cost event so the host can attribute spend.
    emit_v2_cost(&provider, &model, tokens_in, tokens_out, usd);

    eprintln!(
        "[synthesis] ok: prompt_chars={} sources={} tokens_in={} tokens_out={} synthesis_chars={}",
        prompt_chars,
        input.sources.len(),
        tokens_in,
        tokens_out,
        synthesis_text.len()
    );

    Some(SynthesisResult {
        synthesis: synthesis_text,
        headline,
        confidence,
        provider,
        model,
        tokens_in,
        tokens_out,
        usd,
    })
}

/// System prompt for the synthesis call.
///
/// The output format is a strict 3-section markdown doc:
/// 1. `## Headline` — one line summarizing the answer
/// 2. `## Confidence` — numeric in `[0, 1]`
/// 3. `## Synthesis` — multi-paragraph prose with `[N]` citations
///
/// We parse this in [`parse_synthesis_response`] so we can lift each piece
/// into the v2 result envelope.
const SYNTHESIS_SYSTEM_PROMPT: &str = "\
You are a research analyst. You write grounded, cited answers from the source \
material the user provides. Rules: (1) Every factual claim MUST end with one or \
more `[N]` citations referencing the numbered sources. (2) Use multiple \
paragraphs; do NOT bulletpoint the entire answer. (3) Acknowledge contradiction \
between sources when present. (4) If the sources don't cover an aspect of the \
question, say so explicitly rather than guess. (5) Output exactly three \
sections, in this order:\n\n\
## Headline\n\
<one-line answer, no citations>\n\n\
## Confidence\n\
<a number from 0.0 to 1.0 reflecting source agreement and depth>\n\n\
## Synthesis\n\
<3-6 paragraphs of cited prose>\n";

fn build_synthesis_prompt(input: &SynthesisInput<'_>) -> String {
    let mut prompt = String::new();
    prompt.push_str(&format!("Question: {}\n\n", input.query));
    prompt.push_str(&format!(
        "Researcher gathered {} sources across {} search rounds. Source excerpts \
         (truncated):\n\n",
        input.sources.len(),
        input.rounds
    ));
    // Cap the excerpts so the prompt stays within reasonable LLM context.
    // 1500 chars * 12 sources = 18k chars ≈ 4-5k tokens.
    const PER_SOURCE_CHARS: usize = 1500;
    const MAX_SOURCES: usize = 12;
    for src in input.sources.iter().take(MAX_SOURCES) {
        prompt.push_str(&format!("---\n[{}] {}\n", src.index, src.url));
        let excerpt = if src.excerpt.len() > PER_SOURCE_CHARS {
            truncate_utf8(&src.excerpt, PER_SOURCE_CHARS, "\n... (truncated)")
        } else {
            src.excerpt.clone()
        };
        prompt.push_str(&excerpt);
        prompt.push_str("\n\n");
    }
    if input.sources.len() > MAX_SOURCES {
        prompt.push_str(&format!(
            "---\n(+ {} more sources omitted from this prompt for brevity; \
             they are still listed in the final report.)\n",
            input.sources.len() - MAX_SOURCES
        ));
    }
    prompt.push_str(
        "---\n\nWrite the answer. Cite each numbered source at least once if it \
         is used. Sources you do not cite will not appear in the final summary.\n",
    );
    prompt
}

/// Parse the strict 3-section synthesis response.
///
/// Tolerant of section reordering and missing sections. The synthesis body
/// is the section labeled `## Synthesis` (or, falling back, everything
/// after the first heading we don't recognize).
fn parse_synthesis_response(text: &str) -> (String, String, Option<f64>) {
    let mut headline = String::new();
    let mut confidence: Option<f64> = None;
    let mut synthesis = String::new();
    let mut current = SectionTag::None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("##") {
            let label = rest.trim().to_lowercase();
            current = match label.as_str() {
                "headline" => SectionTag::Headline,
                "confidence" => SectionTag::Confidence,
                "synthesis" | "answer" | "report" => SectionTag::Synthesis,
                _ => SectionTag::Other,
            };
            continue;
        }
        match current {
            SectionTag::Headline if !trimmed.is_empty() && headline.is_empty() => {
                headline = trimmed.to_string();
            }
            SectionTag::Confidence if confidence.is_none() && !trimmed.is_empty() => {
                // Find the first run of digits/decimal/sign so we can
                // tolerate prose like "Confidence: ~0.85 (high)" while
                // still preserving negative signs (which we then clamp
                // to 0).
                let cleaned =
                    trimmed.trim_matches(|c: char| !c.is_ascii_digit() && c != '.' && c != '-');
                if let Ok(v) = cleaned.parse::<f64>() {
                    confidence = Some(v.clamp(0.0, 1.0));
                }
            }
            SectionTag::Synthesis => {
                synthesis.push_str(line);
                synthesis.push('\n');
            }
            _ => {}
        }
    }

    let synthesis = synthesis.trim().to_string();
    // Fallback: if we didn't find an explicit `## Synthesis` section, the
    // whole text is treated as the synthesis. Keeps the renderer robust to
    // model misbehavior.
    let synthesis = if synthesis.is_empty() {
        text.trim().to_string()
    } else {
        synthesis
    };
    (synthesis, headline, confidence)
}

#[derive(Clone, Copy)]
enum SectionTag {
    None,
    Headline,
    Confidence,
    Synthesis,
    Other,
}

/// Resolve synthesis provider config: prefer host-injected args over env.
///
/// S2 plumbing: when the host populates `Input::synthesis_config`, we use it
/// directly so secrets stay in the agent's typed config instead of operator
/// plists. When it's missing or incomplete (any of the four fields blank), we
/// fall back to environment variables in the legacy priority order. This
/// preserves backward compat with operators who still set `DEEPSEEK_API_KEY`
/// in the launchd plist.
///
/// Returns `(endpoint, api_key, model, provider)`.
///
/// Tokens MUST NOT be logged. The function only emits a `provider` label on
/// success so debugging the resolution path doesn't leak credentials.
fn resolve_synthesis_config(
    args_config: Option<&SynthesisConfig>,
) -> Option<(String, String, String, String)> {
    // Args path: take everything from the host-injected struct when all four
    // fields are non-empty. Allow operators to still override the model via
    // env even when the args path is used — keeps the
    // `DEEP_SEARCH_SYNTHESIS_MODEL` knob meaningful.
    if let Some(cfg) = args_config {
        if !cfg.endpoint.is_empty()
            && !cfg.api_key.is_empty()
            && !cfg.model.is_empty()
            && !cfg.provider.is_empty()
        {
            let model = std::env::var("DEEP_SEARCH_SYNTHESIS_MODEL")
                .ok()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| cfg.model.clone());
            eprintln!("[synthesis] using host-injected provider: {}", cfg.provider);
            return Some((
                cfg.endpoint.clone(),
                cfg.api_key.clone(),
                model,
                cfg.provider.clone(),
            ));
        }
    }

    // Env path: legacy fallback for operators who haven't migrated to S2.
    let model_override = std::env::var("DEEP_SEARCH_SYNTHESIS_MODEL").ok();

    let configs: &[(&str, &str, &str, &str)] = &[
        (
            "DEEPSEEK_API_KEY",
            "https://api.deepseek.com/v1",
            "deepseek-chat",
            "deepseek",
        ),
        (
            "KIMI_API_KEY",
            "https://api.moonshot.ai/v1",
            "kimi-2.5",
            "moonshot",
        ),
        (
            "DASHSCOPE_API_KEY",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "qwen-plus",
            "dashscope",
        ),
        (
            "OPENAI_API_KEY",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
            "openai",
        ),
        (
            "GEMINI_API_KEY",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "gemini-2.0-flash",
            "google",
        ),
        (
            "ANTHROPIC_API_KEY",
            "https://api.anthropic.com/v1",
            "claude-3-5-haiku-20241022",
            "anthropic",
        ),
    ];
    for &(env_var, endpoint, default_model, provider) in configs {
        if let Ok(key) = std::env::var(env_var) {
            if !key.is_empty() {
                let model = model_override
                    .clone()
                    .unwrap_or_else(|| default_model.to_string());
                return Some((endpoint.to_string(), key, model, provider.to_string()));
            }
        }
    }
    None
}

/// Project a USD cost from token counts. Conservative estimate for the
/// known small/cheap models. Returns `None` for unknown models so the
/// host's pricing catalog can fill in (or operators can compute it
/// post-hoc).
fn project_usd(model: &str, tokens_in: u32, tokens_out: u32) -> Option<f64> {
    let lower = model.to_lowercase();
    let (input_per_million, output_per_million) = match lower.as_str() {
        "deepseek-chat" | "deepseek-coder" => (0.27, 1.10),
        m if m.starts_with("kimi") => (0.20, 0.80),
        m if m.starts_with("qwen-plus") => (0.20, 0.60),
        "gpt-4o-mini" => (0.15, 0.60),
        "gemini-2.0-flash" | "gemini-1.5-flash" => (0.075, 0.30),
        "claude-3-5-haiku-20241022" => (1.0, 5.0),
        _ => return None,
    };
    let cost = (tokens_in as f64) * input_per_million / 1_000_000.0
        + (tokens_out as f64) * output_per_million / 1_000_000.0;
    Some(cost)
}

// ---------------------------------------------------------------------------
// Plugin-protocol-v2 stderr events
// ---------------------------------------------------------------------------

/// Emit a v2 `progress` event on stderr. Best-effort: serialization is
/// infallible for these small structs in practice; if it ever does fail
/// we fall back to a legacy free-form line.
fn emit_v2_progress(stage: &str, message: &str, progress: Option<f64>) {
    let event = serde_json::json!({
        "type": "progress",
        "stage": stage,
        "message": message,
        "progress": progress,
    });
    match serde_json::to_string(&event) {
        Ok(line) => eprintln!("{line}"),
        Err(_) => eprintln!("[{stage}] {message}"),
    }
}

/// Emit a v2 `cost` event on stderr.
fn emit_v2_cost(provider: &str, model: &str, tokens_in: u32, tokens_out: u32, usd: Option<f64>) {
    let event = serde_json::json!({
        "type": "cost",
        "provider": provider,
        "model": model,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "usd": usd,
    });
    match serde_json::to_string(&event) {
        Ok(line) => eprintln!("{line}"),
        Err(_) => eprintln!("[cost] {provider}/{model} in={tokens_in} out={tokens_out}"),
    }
}

// ---------------------------------------------------------------------------
// Progress output (stderr for gateway to stream)
// ---------------------------------------------------------------------------

fn progress(step: usize, total: usize, msg: &str) {
    eprintln!("[{step}/{total}] {msg}");
    let progress_fraction = if total == 0 {
        None
    } else {
        Some((step as f64 / total as f64).min(0.95))
    };
    emit_progress_event(ProgressPhase::Search, msg, progress_fraction);
    // Plugin-protocol-v2 mirror: structured event so downstream
    // consumers don't have to scrape `[step/total] message`.
    emit_v2_progress(ProgressPhase::Search.v2_stage(), msg, progress_fraction);
}

fn progress_simple(phase: ProgressPhase, msg: &str) {
    progress_simple_with_fraction(phase, msg, None);
}

fn progress_simple_with_fraction(phase: ProgressPhase, msg: &str, progress_fraction: Option<f64>) {
    eprintln!("[*] {msg}");
    emit_progress_event(phase, msg, progress_fraction);
    emit_v2_progress(phase.v2_stage(), msg, progress_fraction);
}

#[derive(Copy, Clone)]
enum ProgressPhase {
    Search,
    Fetch,
    Synthesize,
    ReportBuild,
    Completion,
}

impl ProgressPhase {
    fn as_str(self) -> &'static str {
        match self {
            ProgressPhase::Search => "search",
            ProgressPhase::Fetch => "fetch",
            ProgressPhase::Synthesize => "synthesize",
            ProgressPhase::ReportBuild => "report_build",
            ProgressPhase::Completion => "completion",
        }
    }

    /// Plugin-protocol-v2 stage label. Slightly different from the
    /// internal harness phase name (which is preserved for backwards
    /// compatibility with the existing harness sink schema).
    fn v2_stage(self) -> &'static str {
        match self {
            ProgressPhase::Search => "searching",
            ProgressPhase::Fetch => "fetching",
            ProgressPhase::Synthesize => "synthesizing",
            ProgressPhase::ReportBuild => "building_report",
            ProgressPhase::Completion => "complete",
        }
    }
}

#[derive(Serialize)]
struct HarnessProgressEvent<'a> {
    schema: &'static str,
    kind: &'static str,
    session_id: &'a str,
    task_id: &'a str,
    workflow: &'static str,
    phase: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<f64>,
}

struct HarnessContext {
    sink: PathBuf,
    session_id: String,
    task_id: String,
}

fn emit_progress_event(phase: ProgressPhase, message: &str, progress_fraction: Option<f64>) {
    let Some(context) = harness_context_from_env() else {
        return;
    };

    let event = HarnessProgressEvent {
        schema: "octos.harness.event.v1",
        kind: "progress",
        session_id: &context.session_id,
        task_id: &context.task_id,
        workflow: "deep_research",
        phase: phase.as_str(),
        message,
        progress: progress_fraction,
    };

    if let Err(err) = write_progress_event_to_sink(&context.sink, &event) {
        eprintln!(
            "[progress] failed to write structured event to {}: {err}",
            context.sink.display()
        );
    }
}

fn harness_context_from_env() -> Option<HarnessContext> {
    let raw_sink = std::env::var_os("OCTOS_EVENT_SINK")?;
    if raw_sink.is_empty() {
        return None;
    }
    let session_id = std::env::var("OCTOS_HARNESS_SESSION_ID")
        .or_else(|_| std::env::var("OCTOS_SESSION_ID"))
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    let task_id = std::env::var("OCTOS_HARNESS_TASK_ID")
        .or_else(|_| std::env::var("OCTOS_TASK_ID"))
        .ok()
        .filter(|value| !value.trim().is_empty())?;

    Some(HarnessContext {
        sink: sink_path_from_env_value(raw_sink),
        session_id,
        task_id,
    })
}

fn sink_path_from_env_value(raw_sink: std::ffi::OsString) -> PathBuf {
    let raw = raw_sink.to_string_lossy();
    if let Some(rest) = raw.strip_prefix("file://") {
        return PathBuf::from(rest.strip_prefix("localhost").unwrap_or(rest));
    }
    PathBuf::from(raw_sink)
}

fn write_progress_event_to_sink(
    sink: impl AsRef<Path>,
    event: &HarnessProgressEvent<'_>,
) -> io::Result<()> {
    let sink = sink.as_ref();
    let mut file = OpenOptions::new().create(true).append(true).open(sink)?;
    let json = serde_json::to_string(event)
        .map_err(|err| io::Error::other(format!("serialize progress event: {err}")))?;
    writeln!(file, "{json}")?;
    file.flush()
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

    // ---- S2: synthesis_config plumbing ---------------------------------
    //
    // These tests share process-wide env-var state, so they serialize on a
    // local mutex. Every test snapshots the env keys it touches before the
    // case and restores them on exit so no test leaks into another.

    /// Mutex serializing synthesis-config env tests in this module.
    fn synthesis_env_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// All synthesis-related env keys that the resolver consults.
    /// We snapshot+restore these so concurrently-running test orderings stay safe.
    const SYNTHESIS_ENV_KEYS: &[&str] = &[
        "DEEPSEEK_API_KEY",
        "KIMI_API_KEY",
        "DASHSCOPE_API_KEY",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "ANTHROPIC_API_KEY",
        "DEEP_SEARCH_SYNTHESIS_MODEL",
    ];

    fn snapshot_synthesis_env() -> Vec<(&'static str, Option<String>)> {
        SYNTHESIS_ENV_KEYS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect()
    }

    fn clear_synthesis_env() {
        for key in SYNTHESIS_ENV_KEYS {
            // SAFETY: tests serialize on `synthesis_env_lock()`.
            unsafe { std::env::remove_var(key) };
        }
    }

    fn restore_synthesis_env(snapshot: Vec<(&'static str, Option<String>)>) {
        for (key, value) in snapshot {
            // SAFETY: tests serialize on `synthesis_env_lock()`.
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }

    #[test]
    fn test_input_deserialization_with_synthesis_config() {
        let json = r#"{
            "query": "test",
            "synthesis_config": {
                "endpoint": "https://api.example.com/v1",
                "api_key": "sk-host-injected",
                "model": "deepseek-chat",
                "provider": "deepseek"
            }
        }"#;
        let input: Input = serde_json::from_str(json).unwrap();
        let cfg = input.synthesis_config.expect("synthesis_config parsed");
        assert_eq!(cfg.endpoint, "https://api.example.com/v1");
        assert_eq!(cfg.api_key, "sk-host-injected");
        assert_eq!(cfg.model, "deepseek-chat");
        assert_eq!(cfg.provider, "deepseek");
    }

    #[test]
    fn test_synthesis_config_args_path_takes_precedence_over_env() {
        let _guard = synthesis_env_lock().lock().unwrap();
        let snapshot = snapshot_synthesis_env();
        clear_synthesis_env();
        // Set BOTH a real env key (would normally win) and pass an args
        // config: args must take precedence, leaving env untouched.
        // SAFETY: test holds `synthesis_env_lock` for the duration of the case.
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "from-env") };

        let args = SynthesisConfig {
            endpoint: "https://api.host-injected.example/v1".to_string(),
            api_key: "from-args".to_string(),
            model: "host-model".to_string(),
            provider: "host-provider".to_string(),
        };
        let resolved = resolve_synthesis_config(Some(&args)).expect("resolves");
        assert_eq!(resolved.0, "https://api.host-injected.example/v1");
        assert_eq!(resolved.1, "from-args");
        assert_eq!(resolved.2, "host-model");
        assert_eq!(resolved.3, "host-provider");

        restore_synthesis_env(snapshot);
    }

    #[test]
    fn test_synthesis_config_args_path_with_no_env() {
        let _guard = synthesis_env_lock().lock().unwrap();
        let snapshot = snapshot_synthesis_env();
        clear_synthesis_env();

        let args = SynthesisConfig {
            endpoint: "https://api.example.com/v1".to_string(),
            api_key: "sk-args-only".to_string(),
            model: "args-model".to_string(),
            provider: "args-provider".to_string(),
        };
        let resolved = resolve_synthesis_config(Some(&args)).expect("resolves from args");
        assert_eq!(resolved.1, "sk-args-only");
        assert_eq!(resolved.3, "args-provider");

        restore_synthesis_env(snapshot);
    }

    #[test]
    fn test_synthesis_config_falls_back_to_env_when_args_missing() {
        let _guard = synthesis_env_lock().lock().unwrap();
        let snapshot = snapshot_synthesis_env();
        clear_synthesis_env();
        // SAFETY: test holds `synthesis_env_lock` for the duration of the case.
        unsafe { std::env::set_var("KIMI_API_KEY", "kimi-from-env") };

        let resolved = resolve_synthesis_config(None).expect("env path resolves");
        assert_eq!(resolved.0, "https://api.moonshot.ai/v1");
        assert_eq!(resolved.1, "kimi-from-env");
        assert_eq!(resolved.3, "moonshot");

        restore_synthesis_env(snapshot);
    }

    #[test]
    fn test_synthesis_config_falls_back_to_env_when_args_incomplete() {
        let _guard = synthesis_env_lock().lock().unwrap();
        let snapshot = snapshot_synthesis_env();
        clear_synthesis_env();
        // SAFETY: test holds `synthesis_env_lock` for the duration of the case.
        unsafe { std::env::set_var("OPENAI_API_KEY", "openai-from-env") };

        // Args missing api_key → fall through to env.
        let args = SynthesisConfig {
            endpoint: "https://api.example.com/v1".to_string(),
            api_key: "".to_string(),
            model: "some-model".to_string(),
            provider: "some-provider".to_string(),
        };
        let resolved = resolve_synthesis_config(Some(&args)).expect("env path resolves");
        assert_eq!(resolved.1, "openai-from-env");
        assert_eq!(resolved.3, "openai");

        restore_synthesis_env(snapshot);
    }

    #[test]
    fn test_synthesis_config_returns_none_when_neither_set() {
        let _guard = synthesis_env_lock().lock().unwrap();
        let snapshot = snapshot_synthesis_env();
        clear_synthesis_env();

        assert!(resolve_synthesis_config(None).is_none());
        // Empty args also falls through to none.
        let empty_args = SynthesisConfig {
            endpoint: "".to_string(),
            api_key: "".to_string(),
            model: "".to_string(),
            provider: "".to_string(),
        };
        assert!(resolve_synthesis_config(Some(&empty_args)).is_none());

        restore_synthesis_env(snapshot);
    }

    #[test]
    fn test_synthesis_model_env_override_applies_to_args_path() {
        let _guard = synthesis_env_lock().lock().unwrap();
        let snapshot = snapshot_synthesis_env();
        clear_synthesis_env();
        // SAFETY: test holds `synthesis_env_lock` for the duration of the case.
        unsafe { std::env::set_var("DEEP_SEARCH_SYNTHESIS_MODEL", "override-model") };

        let args = SynthesisConfig {
            endpoint: "https://api.example.com/v1".to_string(),
            api_key: "sk-args".to_string(),
            model: "default-from-args".to_string(),
            provider: "deepseek".to_string(),
        };
        let resolved = resolve_synthesis_config(Some(&args)).expect("resolves");
        assert_eq!(resolved.2, "override-model");

        restore_synthesis_env(snapshot);
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

    #[test]
    fn test_structured_progress_events_match_fixture() {
        let mut sink = std::env::temp_dir();
        let unique = format!(
            "deep-search-progress-events-{}-{}.ndjson",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        sink.push(unique);
        let _ = std::fs::remove_file(&sink);

        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/progress_events.ndjson"
        ));

        let events = [
            HarnessProgressEvent {
                schema: "octos.harness.event.v1",
                kind: "progress",
                session_id: "api:session",
                task_id: "task-1",
                workflow: "deep_research",
                phase: "search",
                message: "Searching: \"rust async\"",
                progress: Some(0.25),
            },
            HarnessProgressEvent {
                schema: "octos.harness.event.v1",
                kind: "progress",
                session_id: "api:session",
                task_id: "task-1",
                workflow: "deep_research",
                phase: "fetch",
                message: "Fetching 4 pages in parallel...",
                progress: None,
            },
            HarnessProgressEvent {
                schema: "octos.harness.event.v1",
                kind: "progress",
                session_id: "api:session",
                task_id: "task-1",
                workflow: "deep_research",
                phase: "synthesize",
                message: "Synthesizing report...",
                progress: None,
            },
            HarnessProgressEvent {
                schema: "octos.harness.event.v1",
                kind: "progress",
                session_id: "api:session",
                task_id: "task-1",
                workflow: "deep_research",
                phase: "report_build",
                message: "Building report...",
                progress: None,
            },
            HarnessProgressEvent {
                schema: "octos.harness.event.v1",
                kind: "progress",
                session_id: "api:session",
                task_id: "task-1",
                workflow: "deep_research",
                phase: "completion",
                message: "Deep search complete",
                progress: Some(1.0),
            },
        ];

        for event in &events {
            write_progress_event_to_sink(&sink, event).unwrap();
        }

        let actual = std::fs::read_to_string(&sink).unwrap();
        assert_eq!(actual, fixture);
        let _ = std::fs::remove_file(&sink);
    }

    // -------------------------------------------------------------------
    // Synthesis (W3.C1) tests.
    // -------------------------------------------------------------------

    #[test]
    fn parse_synthesis_response_extracts_three_sections() {
        let raw = "## Headline\n\
Foo is a programming language [1][2].\n\n\
## Confidence\n\
0.85\n\n\
## Synthesis\n\
Foo is a programming language used for systems programming [1]. It \
emphasizes safety and performance [2].\n\n\
A second paragraph explores tooling [3].\n";
        let (synthesis, headline, confidence) = parse_synthesis_response(raw);
        assert_eq!(headline, "Foo is a programming language [1][2].");
        assert_eq!(confidence, Some(0.85));
        assert!(synthesis.contains("systems programming [1]"));
        assert!(synthesis.contains("A second paragraph"));
        // Synthesis MUST NOT contain the headline or confidence lines.
        assert!(
            !synthesis.contains("## Headline"),
            "synthesis leaked headline section"
        );
    }

    #[test]
    fn parse_synthesis_response_falls_back_when_no_sections() {
        let raw = "Just a paragraph of text without explicit sections [1]. \
And another sentence [2].";
        let (synthesis, headline, confidence) = parse_synthesis_response(raw);
        assert_eq!(headline, "");
        assert_eq!(confidence, None);
        assert!(synthesis.contains("[1]"));
        assert!(synthesis.contains("[2]"));
    }

    #[test]
    fn parse_synthesis_clamps_confidence() {
        let raw = "## Confidence\n2.5\n## Synthesis\nbody";
        let (_, _, confidence) = parse_synthesis_response(raw);
        assert_eq!(confidence, Some(1.0));

        let raw = "## Confidence\n-0.5\n## Synthesis\nbody";
        let (_, _, confidence) = parse_synthesis_response(raw);
        assert_eq!(confidence, Some(0.0));
    }

    #[test]
    fn parse_synthesis_tolerates_renamed_sections() {
        // Some models emit "## Answer" instead of "## Synthesis".
        let raw = "## Headline\nshort\n\n## Answer\nThe real body [1].\n";
        let (synthesis, headline, _) = parse_synthesis_response(raw);
        assert_eq!(headline, "short");
        assert!(synthesis.contains("real body"));
    }

    #[test]
    fn cited_indexes_extracts_referenced_sources() {
        let result = SynthesisResult {
            synthesis: "Claim one [1]. Claim two [2][3]. Repeat [1] and [10].".to_string(),
            headline: String::new(),
            confidence: None,
            provider: String::new(),
            model: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            usd: None,
        };
        let cited = result.cited_indexes();
        assert!(cited.contains(&1));
        assert!(cited.contains(&2));
        assert!(cited.contains(&3));
        assert!(cited.contains(&10));
        assert_eq!(cited.len(), 4); // [1] is deduplicated
    }

    #[test]
    fn cited_indexes_ignores_non_numeric_brackets() {
        let result = SynthesisResult {
            synthesis: "Claim one [1]. Claim with [bracketed text]. Edge [99x] case.".to_string(),
            headline: String::new(),
            confidence: None,
            provider: String::new(),
            model: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            usd: None,
        };
        let cited = result.cited_indexes();
        assert!(cited.contains(&1));
        assert_eq!(cited.len(), 1);
    }

    #[test]
    fn build_synthesis_prompt_caps_per_source_chars() {
        let huge_excerpt = "x".repeat(5_000);
        let input = SynthesisInput {
            query: "test",
            rounds: 1,
            sources: vec![SynthesisSource {
                index: 1,
                url: "https://x".to_string(),
                excerpt: huge_excerpt,
            }],
        };
        let prompt = build_synthesis_prompt(&input);
        // 1500 char cap + suffix → cap is enforced
        assert!(prompt.contains("(truncated)"));
    }

    #[test]
    fn build_synthesis_prompt_omits_sources_beyond_cap() {
        let sources = (1..=20)
            .map(|i| SynthesisSource {
                index: i,
                url: format!("https://x{i}"),
                excerpt: format!("text {i}"),
            })
            .collect();
        let input = SynthesisInput {
            query: "test",
            rounds: 1,
            sources,
        };
        let prompt = build_synthesis_prompt(&input);
        // Should mention 8 omitted (12 cap + 8 = 20)
        assert!(
            prompt.contains("8 more sources omitted"),
            "got: {}",
            &prompt[prompt.len().saturating_sub(300)..]
        );
        assert!(prompt.contains("https://x1"));
        assert!(prompt.contains("https://x12"));
        assert!(!prompt.contains("https://x13"));
    }

    #[test]
    fn project_usd_handles_known_models() {
        // Only check that costs are positive and roughly sane (sub-cent
        // for typical synthesis sizes). Brittle pricing assertions are
        // not the point — we want a sanity floor.
        let cost = project_usd("deepseek-chat", 1000, 500).unwrap();
        assert!(cost > 0.0);
        assert!(cost < 0.01); // synthesis at 1k+500 should be sub-cent

        let cost = project_usd("gpt-4o-mini", 1000, 500).unwrap();
        assert!(cost > 0.0);
    }

    #[test]
    fn project_usd_returns_none_for_unknown_model() {
        assert!(project_usd("custom-private-model-v9", 100, 100).is_none());
    }

    #[test]
    fn build_report_with_synthesis_includes_synthesis_section_and_sources() {
        let saved = vec![
            (
                "01_a.md".to_string(),
                "https://a.example/x".to_string(),
                "Full text from a [1].".to_string(),
            ),
            (
                "02_b.md".to_string(),
                "https://b.example/y".to_string(),
                "Full text from b [2].".to_string(),
            ),
        ];
        let queries = vec!["topic".to_string(), "topic 2026".to_string()];
        let syn = SynthesisResult {
            synthesis: "Foo is widely used [1]. Bar is alternative [2].\n\n\
A second paragraph elaborates on alternatives [2]."
                .to_string(),
            headline: "Foo and bar are alternatives".to_string(),
            confidence: Some(0.85),
            provider: "deepseek".to_string(),
            model: "deepseek-chat".to_string(),
            tokens_in: 1000,
            tokens_out: 200,
            usd: Some(0.0009),
        };
        let dir = std::path::PathBuf::from("/tmp/research/topic");
        let report_path = dir.join("_report.md");
        let report = build_report(
            "topic",
            Some(&syn),
            "ignored when synthesis present",
            &saved,
            &queries,
            &dir,
            &report_path,
        );

        // Critical structural guarantees the test enforces:
        assert!(report.starts_with("# Deep Research: topic\n\n"));
        assert!(
            report.contains("## Synthesis\n\nFoo is widely used [1]"),
            "expected synthesis section with citations: {report}"
        );
        assert!(
            report.contains("_Foo and bar are alternatives_"),
            "expected italic headline: {report}"
        );
        assert!(
            report.contains("_Self-reported confidence: 0.85_"),
            "expected confidence line: {report}"
        );
        // Source listing must be present.
        assert!(report.contains("### Source [1]: https://a.example/x"));
        assert!(report.contains("### Source [2]: https://b.example/y"));
        // No "LLM synthesis unavailable" disclaimer when synthesis IS available.
        assert!(
            !report.contains("LLM synthesis unavailable"),
            "synthesis fallback leaked into successful path"
        );
        // Trailer with report path stays for v1 host compatibility.
        assert!(report.contains("Report saved to: /tmp/research/topic/_report.md"));
        // Multi-paragraph structure is preserved (we have a blank line in
        // the synthesis input → there should be at least 4 newlines around
        // the synthesis body).
        let synthesis_section = report.split("## Sources").next().unwrap();
        assert!(
            synthesis_section.matches("\n\n").count() >= 3,
            "synthesis should have multi-paragraph structure: {synthesis_section}"
        );
    }

    #[test]
    fn build_report_without_synthesis_falls_back_with_disclaimer() {
        let saved = vec![(
            "01_a.md".to_string(),
            "https://a.example/x".to_string(),
            "Snippet".to_string(),
        )];
        let queries = vec!["topic".to_string()];
        let dir = std::path::PathBuf::from("/tmp/research/topic");
        let report_path = dir.join("_report.md");
        let report = build_report(
            "topic",
            None,
            "Initial Bing dump:\n1. Result one",
            &saved,
            &queries,
            &dir,
            &report_path,
        );
        assert!(report.contains("## Overview"));
        assert!(report.contains("LLM synthesis unavailable"));
        assert!(report.contains("Initial Bing dump"));
        assert!(report.contains("### Source [1]"));
        assert!(!report.contains("## Synthesis"));
    }

    #[test]
    fn build_report_with_empty_synthesis_falls_back() {
        // If the LLM returns an empty body (rare but possible), the
        // report should fall back to the raw initial answer rather than
        // emit an empty synthesis section.
        let syn = SynthesisResult {
            synthesis: "   \n  ".to_string(),
            headline: "Headline only".to_string(),
            confidence: None,
            provider: "x".to_string(),
            model: "y".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            usd: None,
        };
        let report = build_report(
            "topic",
            Some(&syn),
            "raw initial",
            &[],
            &["topic".to_string()],
            std::path::Path::new("/tmp"),
            std::path::Path::new("/tmp/_report.md"),
        );
        assert!(!report.contains("## Synthesis"));
        assert!(report.contains("LLM synthesis unavailable"));
        assert!(report.contains("raw initial"));
    }

    #[test]
    fn max_browsers_default_is_three() {
        // Avoid clobbering a real env var the developer set.
        let prev = std::env::var("DEEP_SEARCH_MAX_BROWSERS").ok();
        // SAFETY: tests are single-threaded by default.
        unsafe {
            std::env::remove_var("DEEP_SEARCH_MAX_BROWSERS");
        }
        assert_eq!(max_browsers(), 3);
        // Restore for other tests.
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("DEEP_SEARCH_MAX_BROWSERS", v);
            }
        }
    }

    #[test]
    fn max_browsers_clamps_to_range() {
        let prev = std::env::var("DEEP_SEARCH_MAX_BROWSERS").ok();
        unsafe {
            std::env::set_var("DEEP_SEARCH_MAX_BROWSERS", "0");
        }
        assert_eq!(max_browsers(), 1, "clamps zero to 1");
        unsafe {
            std::env::set_var("DEEP_SEARCH_MAX_BROWSERS", "100");
        }
        assert_eq!(max_browsers(), 16, "clamps high to 16");
        unsafe {
            std::env::set_var("DEEP_SEARCH_MAX_BROWSERS", "5");
        }
        assert_eq!(max_browsers(), 5, "passes through valid value");
        unsafe {
            std::env::set_var("DEEP_SEARCH_MAX_BROWSERS", "not-a-number");
        }
        assert_eq!(max_browsers(), 3, "falls back to default on parse fail");
        // Cleanup.
        unsafe {
            std::env::remove_var("DEEP_SEARCH_MAX_BROWSERS");
        }
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("DEEP_SEARCH_MAX_BROWSERS", v);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn browser_semaphore_caps_concurrency() {
        // The OnceLock-backed semaphore is initialized at first call.
        // We can't read its capacity directly, but we can validate that
        // permits decrement when held and that exceeding the cap blocks.
        let sem = browser_semaphore();
        let cap = sem.available_permits();
        assert!((1..=16).contains(&cap), "cap must be in [1, 16], got {cap}");

        // Hold the bindings so the permits are NOT dropped immediately.
        let permit1 = sem.try_acquire().ok();
        assert!(permit1.is_some(), "first acquire should succeed");
        let after_one = sem.available_permits();
        assert_eq!(
            after_one,
            cap - 1,
            "permit should decrement available count"
        );
        drop(permit1);
        // Restored after drop.
        assert_eq!(sem.available_permits(), cap);
    }

    #[test]
    fn output_serializes_v2_summary() {
        let mut output = Output {
            output: "report".to_string(),
            success: true,
            summary: Some(ResultSummary {
                kind: "deep_research".to_string(),
                headline: "5 sources answering test".to_string(),
                confidence: Some(0.8),
                sources: vec![ResultSource {
                    url: "https://example.com".to_string(),
                    title: "Example".to_string(),
                    cited: true,
                }],
                rounds: Some(3),
            }),
            cost: Some(ResultCost {
                provider: Some("deepseek".to_string()),
                model: Some("deepseek-chat".to_string()),
                tokens_in: 1024,
                tokens_out: 256,
                usd: Some(0.0034),
            }),
            files_to_send: vec!["/tmp/report.md".to_string()],
        };
        let json = serde_json::to_value(&output).unwrap();
        assert_eq!(json["summary"]["kind"], "deep_research");
        assert_eq!(json["summary"]["confidence"], 0.8);
        assert_eq!(json["summary"]["sources"][0]["cited"], true);
        assert_eq!(json["cost"]["tokens_in"], 1024);
        assert_eq!(json["files_to_send"][0], "/tmp/report.md");

        // Default-empty fields elide so existing v1 code keeps working.
        output.summary = None;
        output.cost = None;
        output.files_to_send.clear();
        let json = serde_json::to_value(&output).unwrap();
        assert!(json.get("summary").is_none(), "summary should be omitted");
        assert!(json.get("cost").is_none(), "cost should be omitted");
        assert!(
            json.get("files_to_send").is_none(),
            "files_to_send should be omitted"
        );
    }
}
