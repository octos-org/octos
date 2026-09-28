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

use octos_research::item::{ItemsDocument, ResearchItem, SkippedUrl, SummaryKind};
use octos_research::{DomainCap, SearchHit};
use serde::{Deserialize, Serialize};

mod research;

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
    /// `report` (default): Markdown report in `output`. `items`: the
    /// structured items document (JSON) in `output`; the report is still
    /// written. `items.json` is written next to the report either way.
    #[serde(default)]
    output: Option<String>,
    /// BCP-47 language(s): one string, a list, or comma-separated. Each
    /// language gets its own queries (GDELT `sourcelang`, Google News
    /// edition, SearXNG/Brave/Serper language) and results are filtered to
    /// them (unknown-language results are kept).
    #[serde(default)]
    lang: research::LangInput,
    /// ISO 3166-1 alpha-2 region (Google News edition, Brave/Serper country).
    #[serde(default)]
    region: Option<String>,
    /// Only results published since: ISO date/datetime or `24h`, `7d`,
    /// `2w`, `3m`, `1y`. Passed to providers that support it and applied to
    /// feed/page publication dates (undated results are kept).
    #[serde(default)]
    since: Option<String>,
    /// `news`, `general` or `auto` (default: news when the query or `since`
    /// looks news-ish). News enables the GDELT + Google News free tier.
    #[serde(default)]
    category: Option<String>,
    /// At most this many sources per domain.
    #[serde(default)]
    max_per_domain: Option<usize>,
    /// Only use these domains (subdomains included).
    #[serde(default)]
    domains_allow: Vec<String>,
    /// Never use these domains (subdomains included).
    #[serde(default)]
    domains_deny: Vec<String>,
    /// `auto` (default): render JS-heavy pages in the headless browser when
    /// plain HTTP yields no main text. `off`: plain HTTP only.
    #[serde(default)]
    render: Option<String>,
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
    /// behavior; we name the synthesized topic-named report
    /// (`<slug>_report.md`, see `report_filename`) here so the chat UI
    /// shows the canonical report file, not the search-engine dump.
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
    /// `ok`, or `partial` when the synthesis was cut off. Passed through by
    /// the host as a kind-specific extra field.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    /// Human-readable problems with this result (cut-off synthesis,
    /// uncited sentences, failed synthesis call).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    diagnostics: Vec<String>,
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

    let opts = match research::Options::from_input(&input, chrono::Utc::now()) {
        Ok(o) => o,
        Err(e) => {
            print_output(&Output {
                output: format!("Invalid input: {e}"),
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
    // Budgets include polite reading (robots.txt, per-host spacing, GDELT's
    // 5s interval) and the occasional browser render; still well inside the
    // manifest's 600s.
    // Synthesis has no output-token cap and may retry once, so each budget
    // leaves up to two minutes for it; the deepest still fits the
    // manifest's 600s.
    let timeout = match depth {
        1 => Duration::from_secs(240),
        2 => Duration::from_secs(360),
        _ => Duration::from_secs(480),
    };
    let _ = RUN_DEADLINE.set(std::time::Instant::now() + timeout);

    let result = tokio::time::timeout(
        timeout,
        run_deep_search(
            &client,
            &input.query,
            max_results,
            depth,
            input.search_engine.as_deref(),
            input.synthesis_config.as_ref(),
            &opts,
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

/// Accumulates search rounds: hits (deduped), the query log, provider
/// names/errors, the raw dump and any model-written answer text.
#[derive(Default)]
struct SearchLog {
    hits: Vec<SearchHit>,
    seen: HashSet<String>,
    queries: Vec<String>,
    providers: Vec<String>,
    tried: Vec<String>,
    errors: Vec<String>,
    dump: String,
    answer: String,
}

impl SearchLog {
    fn add(&mut self, query: &str, lang: Option<&str>, round: research::RoundOut) {
        let label = match lang {
            Some(l) => format!("{query} [{l}]"),
            None => query.to_string(),
        };
        self.queries.push(label.clone());
        for p in round.providers {
            if !self.providers.contains(&p) {
                self.providers.push(p);
            }
        }
        self.errors.extend(round.errors);
        for t in round.tried {
            if !self.tried.contains(&t) {
                self.tried.push(t);
            }
        }
        if !round.answer.trim().is_empty() {
            self.answer.push_str(round.answer.trim());
            self.answer.push_str("\n\n");
            self.dump.push_str(round.answer.trim());
            self.dump.push_str("\n\n");
        }
        self.dump
            .push_str(&octos_research::providers::format_hits(&label, &round.hits));
        self.dump.push('\n');
        for h in round.hits {
            if self.seen.insert(octos_research::urls::dedup_key(&h.url)) {
                self.hits.push(h);
            }
        }
    }
}

/// A page read for citation, with the hit that led to it.
struct CitedSource {
    hit: SearchHit,
    page: research::ReadPage,
}

/// Mutable state while reading pages.
struct ReadState {
    sources: Vec<CitedSource>,
    unread: Vec<SearchHit>,
    skipped: Vec<SkippedUrl>,
    cap: DomainCap,
    seen_canonical: HashSet<String>,
}

/// Read `hits` and admit the pages that pass the controls (language and
/// `since` re-checked against page metadata, per-domain cap, canonical
/// dedupe). Unreadable hits are recorded as skipped and kept as unread items.
async fn read_into(
    reader: &research::Reader,
    opts: &research::Options,
    hits: Vec<SearchHit>,
    st: &mut ReadState,
) {
    let urls: Vec<String> = hits.iter().map(|h| h.url.clone()).collect();
    let pages = reader.read_all(&urls).await;
    for (hit, page) in hits.into_iter().zip(pages) {
        match page {
            Ok(page) => {
                let lang = page.meta.lang.clone().or_else(|| hit.lang.clone());
                let published = hit
                    .published
                    .clone()
                    .or_else(|| page.meta.published.clone());
                if let Err(reason) =
                    opts.filters
                        .check(&page.final_url, lang.as_deref(), published.as_deref())
                {
                    st.skipped.push(SkippedUrl {
                        url: hit.url.clone(),
                        reason: reason.to_string(),
                    });
                    continue;
                }
                let canonical = page.canonical_url();
                if !st
                    .seen_canonical
                    .insert(octos_research::urls::dedup_key(&canonical))
                {
                    continue;
                }
                if !st.cap.admit(&canonical) {
                    st.skipped.push(SkippedUrl {
                        url: hit.url.clone(),
                        reason: "per_domain_cap".to_string(),
                    });
                    continue;
                }
                st.sources.push(CitedSource { hit, page });
            }
            Err(reason) => {
                st.skipped.push(SkippedUrl {
                    url: hit.url.clone(),
                    reason,
                });
                st.unread.push(hit);
            }
        }
    }
}

fn link_hit(url: String, provider: &str) -> SearchHit {
    SearchHit {
        url,
        provider: provider.to_string(),
        ..Default::default()
    }
}

/// `_Title — Source · 2026-09-26 · es_` line shown above each source.
fn source_meta_line(s: &CitedSource) -> String {
    let item = source_item(s, 0, false, "");
    let mut parts: Vec<String> = vec![item.source.clone()];
    if let Some(p) = &item.published {
        parts.push(p.clone());
    }
    if let Some(l) = &item.lang {
        parts.push(l.clone());
    }
    if item.rendered {
        parts.push("rendered".to_string());
    }
    format!("_{} — {}_", item.title.replace('_', " "), parts.join(" · "))
}

fn one_line(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("---", "—")
}

/// Structured item for a read source.
fn source_item(s: &CitedSource, citation: usize, cited: bool, file: &str) -> ResearchItem {
    let url = s.page.canonical_url();
    let domain = octos_research::urls::domain_of(&url).unwrap_or_default();
    let meta = &s.page.meta;
    let title = meta
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| Some(s.hit.title.clone()).filter(|t| !t.is_empty()))
        .unwrap_or_else(|| url.clone());
    let summary = octos_research::item::extractive_summary(&s.page.text, 400);
    ResearchItem {
        title: one_line(&title),
        source: meta
            .site_name
            .clone()
            .or_else(|| s.hit.source.clone())
            .unwrap_or_else(|| domain.clone()),
        domain,
        lang: meta.lang.clone().or_else(|| s.hit.lang.clone()),
        published: s.hit.published.clone().or_else(|| meta.published.clone()),
        summary_kind: if summary.is_empty() {
            SummaryKind::None
        } else {
            SummaryKind::Extractive
        },
        summary,
        snippet: if s.hit.snippet.is_empty() {
            meta.excerpt.clone().unwrap_or_default()
        } else {
            s.hit.snippet.clone()
        },
        fetched_at: Some(s.page.fetched_at.clone()),
        provider: s.hit.provider.clone(),
        read: true,
        rendered: s.page.rendered,
        citation: (citation > 0).then_some(citation),
        cited,
        file: (!file.is_empty()).then(|| file.to_string()),
        url,
    }
}

/// Structured item for a hit whose page could not be read (robots.txt,
/// fetch error, no main text): headline-level information only. Headline
/// sources listed in the report carry their `[N]` citation.
fn unread_item(hit: &SearchHit, citation: Option<usize>, cited: bool) -> ResearchItem {
    let url = octos_research::urls::canonicalize(&hit.url);
    let domain = hit
        .source_url
        .as_deref()
        .and_then(octos_research::urls::domain_of)
        .or_else(|| octos_research::urls::domain_of(&url))
        .unwrap_or_default();
    ResearchItem {
        title: one_line(&hit.title),
        source: hit.source.clone().unwrap_or_else(|| domain.clone()),
        domain,
        lang: hit.lang.clone(),
        published: hit.published.clone(),
        summary: hit.snippet.clone(),
        summary_kind: if hit.snippet.is_empty() {
            SummaryKind::None
        } else {
            SummaryKind::Snippet
        },
        snippet: hit.snippet.clone(),
        fetched_at: None,
        provider: hit.provider.clone(),
        read: false,
        rendered: false,
        citation,
        cited,
        file: None,
        url,
    }
}

/// Most headline-only sources (pages we may not read) listed in a report.
const MAX_HEADLINE_SOURCES: usize = 10;

/// `_Title — Source · date · lang · headline only_` for an unread hit.
fn headline_meta_line(hit: &SearchHit) -> String {
    let item = unread_item(hit, None, false);
    let mut parts: Vec<String> = vec![item.source.clone()];
    if let Some(p) = &item.published {
        parts.push(p.clone());
    }
    if let Some(l) = &item.lang {
        parts.push(l.clone());
    }
    parts.push("headline only, page not read".to_string());
    format!("_{} — {}_", item.title.replace('_', " "), parts.join(" · "))
}

fn skipped_summary(skipped: &[SkippedUrl]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for s in skipped {
        let key = s.reason.split(':').next().unwrap_or("").to_string();
        match counts.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => counts.push((key, 1)),
        }
    }
    counts
        .iter()
        .map(|(k, n)| format!("{k}: {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[allow(clippy::too_many_arguments)]
async fn run_deep_search(
    client: &reqwest::Client,
    query: &str,
    max_results: u8,
    depth: u8,
    engine: Option<&str>,
    synthesis_config: Option<&SynthesisConfig>,
    opts: &research::Options,
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

    let langs = opts.search_langs(query);
    let mut log = SearchLog::default();

    // -----------------------------------------------------------------------
    // Round 1: the query, once per requested language
    // -----------------------------------------------------------------------
    progress(1, max_rounds, &format!("Searching: \"{query}\""));
    for lang in &langs {
        let round = research::search_round(opts, engine, query, lang.as_deref(), max_results).await;
        log.add(query, lang.as_deref(), round);
    }
    if log.hits.is_empty() {
        // Empty result, not a scrape: say what was tried and how to get
        // results (SearXNG, a key, or `category: news`).
        return no_results_output(query, &log, opts);
    }
    let initial_answer = log.dump.clone();

    // -----------------------------------------------------------------------
    // Rounds 2+: follow-up angles (recency comes from `since`, not from
    // query text)
    // -----------------------------------------------------------------------
    if depth >= 2 {
        let follow_ups = generate_follow_up_queries(query, &log.answer, depth);
        let rounds_left = max_rounds - 1;
        for (i, fq) in follow_ups.into_iter().take(rounds_left).enumerate() {
            progress(i + 2, max_rounds, &format!("Searching: \"{fq}\""));
            for lang in &langs {
                let round =
                    research::search_round(opts, engine, &fq, lang.as_deref(), max_results).await;
                log.add(&fq, lang.as_deref(), round);
            }
        }
    }

    if !log.errors.is_empty() {
        log.dump.push_str("## Provider notes\n\n");
        for e in &log.errors {
            log.dump.push_str(&format!("- {e}\n"));
        }
    }
    let _ = fs::write(dir.join("_search_results.md"), &log.dump);

    // -----------------------------------------------------------------------
    // Controls: dedupe, domain lists, lang/since, per-domain cap; then
    // interleave languages so the page budget covers all of them.
    // -----------------------------------------------------------------------
    let (kept, skipped) = opts.filters.apply(std::mem::take(&mut log.hits));
    let kept = octos_research::filter::interleave_by(kept, |h| {
        h.lang
            .as_deref()
            .map(octos_research::lang::primary)
            .unwrap_or_default()
    });
    let reader = research::Reader::new(opts.render);
    let mut st = ReadState {
        sources: Vec::new(),
        unread: Vec::new(),
        skipped,
        cap: DomainCap::new(opts.filters.max_per_domain),
        seen_canonical: HashSet::new(),
    };
    // robots.txt first (only when the operator enabled it; a no-op
    // otherwise), so disallowed links (e.g. Google News article redirects)
    // become headline-only sources instead of using the budget.
    let (readable, denied) = reader.robots_partition(kept).await;
    for (hit, reason) in denied {
        st.skipped.push(SkippedUrl {
            url: hit.url.clone(),
            reason,
        });
        st.unread.push(hit);
    }
    let to_read: Vec<SearchHit> = readable.into_iter().take(max_pages).collect();
    let mut seen_urls: HashSet<String> = to_read.iter().map(|h| normalize_url(&h.url)).collect();

    progress_simple(
        ProgressPhase::Fetch,
        &format!("Reading {} pages in parallel...", to_read.len()),
    );
    read_into(&reader, opts, to_read, &mut st).await;

    // -----------------------------------------------------------------------
    // Reference chasing (depth >= 2): links cited by 2+ read pages
    // -----------------------------------------------------------------------
    if depth >= 2 {
        let mut link_counts: HashMap<String, u32> = HashMap::new();
        for s in &st.sources {
            for link in &s.page.links {
                if !seen_urls.contains(&normalize_url(link)) {
                    *link_counts.entry(link.clone()).or_insert(0) += 1;
                }
            }
        }
        let chase_limit = match depth {
            2 => 5,
            _ => 10,
        };
        let mut ranked: Vec<(String, u32)> = link_counts.into_iter().collect();
        ranked.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        let chase_urls: Vec<String> = ranked
            .into_iter()
            .filter(|(url, count)| {
                *count >= 2 && !is_non_content_url(url) && opts.filters.check_domain(url).is_ok()
            })
            .take(chase_limit)
            .map(|(url, _)| url)
            .collect();
        if !chase_urls.is_empty() {
            progress_simple(
                ProgressPhase::Fetch,
                &format!("Chasing {} most-referenced sources...", chase_urls.len()),
            );
            for url in &chase_urls {
                seen_urls.insert(normalize_url(url));
            }
            let hits = chase_urls
                .into_iter()
                .map(|u| link_hit(u, "reference"))
                .collect();
            read_into(&reader, opts, hits, &mut st).await;
        }
    }

    // -----------------------------------------------------------------------
    // Site crawl: follow internal links on high-value domains (depth >= 2)
    // -----------------------------------------------------------------------
    if depth >= 2 {
        let mut domain_links: HashMap<String, Vec<String>> = HashMap::new();
        for s in &st.sources {
            let internal = same_origin_links(&s.page.final_url, &s.page.links, &seen_urls);
            if internal.is_empty() {
                continue;
            }
            let origin = url::Url::parse(&s.page.final_url)
                .ok()
                .map(|u| u.origin().ascii_serialization())
                .unwrap_or_default();
            if !origin.is_empty() {
                domain_links.entry(origin).or_default().extend(internal);
            }
        }
        for links in domain_links.values_mut() {
            let mut dedup_set = HashSet::new();
            links.retain(|l| dedup_set.insert(normalize_url(l)));
        }
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
            .filter(|u| opts.filters.check_domain(u).is_ok())
            .collect();
        if !to_crawl.is_empty() {
            progress_simple(
                ProgressPhase::Fetch,
                &format!(
                    "Site crawl: fetching {} additional pages from top domains...",
                    to_crawl.len()
                ),
            );
            for url in &to_crawl {
                seen_urls.insert(normalize_url(url));
            }
            let hits = to_crawl
                .into_iter()
                .map(|u| link_hit(u, "site_crawl"))
                .collect();
            read_into(&reader, opts, hits, &mut st).await;
        }
    }

    // -----------------------------------------------------------------------
    // Save read pages (main text + metadata front matter)
    // -----------------------------------------------------------------------
    let mut saved_files: Vec<(String, String, String)> = Vec::new(); // (filename, url, preview)
                                                                     // What the synthesis model sees per source, parallel to `saved_files`:
                                                                     // the full main text (the prompt builder trims it to a character budget),
                                                                     // not the short report preview.
    let mut synthesis_texts: Vec<String> = Vec::new();
    for (i, s) in st.sources.iter().enumerate() {
        let filename = format!("{:02}_{}.md", i + 1, host_slug(&s.page.final_url));
        let item = source_item(s, i + 1, false, &filename);
        let page_content = format!(
            "---\nurl: {}\nfetched_url: {}\ntitle: {}\nsource: {}\nlang: {}\npublished: {}\nrendered: {}\n---\n\n{}",
            item.url,
            one_line(&s.page.final_url),
            item.title,
            one_line(&item.source),
            item.lang.as_deref().unwrap_or(""),
            item.published.as_deref().unwrap_or(""),
            item.rendered,
            s.page.text
        );
        let _ = fs::write(dir.join(&filename), &page_content);
        let preview = format!(
            "{}\n\n{}",
            source_meta_line(s),
            truncate_utf8(&s.page.text, 2000, "\n... (truncated)")
        );
        synthesis_texts.push(format!("{}\n\n{}", source_meta_line(s), s.page.text));
        saved_files.push((filename, item.url, preview));
    }

    // Headline-only sources: provider hits whose page we may not (robots)
    // or could not read. Listed after the read pages, clearly marked, so a
    // news digest can still cite dated headlines from every language.
    let headline_hits: Vec<SearchHit> = st
        .unread
        .iter()
        .filter(|h| !h.title.is_empty())
        .take(MAX_HEADLINE_SOURCES)
        .cloned()
        .collect();
    let read_count = saved_files.len();
    for hit in &headline_hits {
        let mut preview = headline_meta_line(hit);
        if !hit.snippet.is_empty() {
            preview.push_str("\n\n");
            preview.push_str(&hit.snippet);
        }
        synthesis_texts.push(preview.clone());
        saved_files.push((
            String::new(),
            octos_research::urls::canonicalize(&hit.url),
            preview,
        ));
    }

    // -----------------------------------------------------------------------
    // Fail loudly if every search round + read phase produced zero sources.
    // Without this guard the synthesizer would still run with `sources=0`
    // and the LLM would write a report from prior knowledge. Surface the
    // failure (with why pages were skipped) instead.
    // -----------------------------------------------------------------------
    if saved_files.is_empty() {
        let rounds = log.queries.len();
        let skipped_note = if st.skipped.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nSkipped URLs ({}): {}.",
                st.skipped.len(),
                skipped_summary(&st.skipped)
            )
        };
        return Output {
            output: format!(
                "Deep search failed: 0 usable sources across {rounds} search round(s) for query \"{query}\".{skipped_note}\n\nThis usually means the providers returned no results, only blocked domains, or pages whose robots.txt disallows automated reading. Refusing to synthesize a report from prior knowledge — try a different `search_engine`, widen `since`/`lang`, rephrase the query, or switch to `run_pipeline` with multiple search nodes."
            ),
            success: false,
            ..Default::default()
        };
    }

    // -----------------------------------------------------------------------
    // Synthesize an answer from the crawled excerpts, with `[N]` citations
    // pointing at our `Sources` list. Best-effort: without an API key we
    // fall back to the raw-results report.
    // -----------------------------------------------------------------------
    progress_simple(ProgressPhase::Synthesize, "Synthesizing report...");
    emit_v2_progress(
        "synthesizing",
        "Synthesizing report from sources...",
        Some(0.85),
    );

    let synthesis_input = SynthesisInput {
        query,
        rounds: log.queries.len(),
        sources: saved_files
            .iter()
            .zip(synthesis_texts)
            .enumerate()
            .map(|(i, ((_, url, _), text))| SynthesisSource {
                index: i + 1,
                url: url.clone(),
                excerpt: text,
            })
            .collect(),
    };

    let (synthesis, diagnostics) =
        synthesis_diagnostics(synthesize(client, &synthesis_input, synthesis_config).await);
    let partial = synthesis.as_ref().is_some_and(|s| s.truncated.is_some());

    progress_simple(ProgressPhase::ReportBuild, "Building report...");
    emit_v2_progress(
        "building_report",
        "Assembling final document...",
        Some(0.95),
    );

    let report_path = unique_report_path(&dir, &slug);
    let report = build_report(
        query,
        synthesis.as_ref(),
        &initial_answer,
        &saved_files,
        &log.queries,
        &dir,
        &report_path,
    );
    let _ = fs::write(&report_path, &report);

    // -----------------------------------------------------------------------
    // Structured items (always written next to the report)
    // -----------------------------------------------------------------------
    let cited_indexes: HashSet<usize> = synthesis
        .as_ref()
        .map(|s| s.cited_indexes())
        .unwrap_or_default();
    let items_path = items_path_for(&report_path);
    let mut doc = ItemsDocument::new(query, opts.controls_json());
    for (i, s) in st.sources.iter().enumerate() {
        doc.items.push(source_item(
            s,
            i + 1,
            cited_indexes.contains(&(i + 1)),
            &saved_files[i].0,
        ));
    }
    let headline_keys: HashSet<String> = headline_hits
        .iter()
        .map(|h| octos_research::urls::dedup_key(&h.url))
        .collect();
    for (j, hit) in headline_hits.iter().enumerate() {
        let n = read_count + j + 1;
        doc.items
            .push(unread_item(hit, Some(n), cited_indexes.contains(&n)));
    }
    for hit in &st.unread {
        if !headline_keys.contains(&octos_research::urls::dedup_key(&hit.url)) {
            doc.items.push(unread_item(hit, None, false));
        }
    }
    doc.skipped = std::mem::take(&mut st.skipped);
    doc.providers = log.providers.clone();
    doc.report = Some(report_path.display().to_string());
    doc.items_file = Some(items_path.display().to_string());
    doc.diagnostics = diagnostics.clone();
    let items_json = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string());
    let _ = fs::write(&items_path, &items_json);

    progress_simple_with_fraction(ProgressPhase::Completion, "Deep search complete", Some(1.0));
    emit_v2_progress("complete", "Deep search complete", Some(1.0));

    let summary_sources = doc
        .items
        .iter()
        .filter(|it| it.citation.is_some())
        .map(|it| ResultSource {
            url: it.url.clone(),
            title: it.title.clone(),
            cited: it.cited,
        })
        .collect();
    let headline = if partial {
        format!("Incomplete report on '{query}': the synthesis was cut off")
    } else {
        synthesis
            .as_ref()
            .map(|s| s.headline.clone())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "Researched '{query}' across {} sources in {} rounds",
                    saved_files.len(),
                    log.queries.len()
                )
            })
    };
    let summary = ResultSummary {
        kind: "deep_research".to_string(),
        headline,
        confidence: synthesis.as_ref().and_then(|s| s.confidence),
        sources: summary_sources,
        rounds: Some(log.queries.len() as u32),
        status: Some(if partial { "partial" } else { "ok" }.to_string()),
        diagnostics: diagnostics.clone(),
    };

    let cost = synthesis.as_ref().map(|s| ResultCost {
        provider: Some(s.provider.clone()),
        model: Some(s.model.clone()),
        tokens_in: s.tokens_in,
        tokens_out: s.tokens_out,
        usd: s.usd,
    });

    let output = if opts.items_mode {
        items_json
    } else {
        format!("{report}Items saved to: {}\n", items_path.display())
    };
    assemble_output(output, &diagnostics, partial, summary, cost, &report_path)
}

/// Split a synthesis outcome into the usable result and the diagnostics a
/// caller must see (failed call, cut-off reply, uncited sentences).
fn synthesis_diagnostics(outcome: SynthesisOutcome) -> (Option<SynthesisResult>, Vec<String>) {
    let mut diagnostics = Vec::new();
    let synthesis = match outcome {
        SynthesisOutcome::NotConfigured => None,
        SynthesisOutcome::Failed(reason) => {
            diagnostics.push(format!(
                "Synthesis failed ({reason}); the report lists the sources without a synthesized answer."
            ));
            None
        }
        SynthesisOutcome::Done(s) => Some(s),
    };
    if let Some(s) = &synthesis {
        if let Some(reason) = &s.truncated {
            diagnostics.push(format!(
                "Synthesis incomplete: {reason} (after {} attempt(s)). The report's synthesis is cut off; do not treat it as a complete answer.",
                s.attempts
            ));
        }
        if s.uncited_flagged > 0 {
            diagnostics.push(format!(
                "{} sentence(s) in the synthesis have no [N] citation and are marked [citation needed].",
                s.uncited_flagged
            ));
        }
    }
    (synthesis, diagnostics)
}

/// Final plugin result. An incomplete synthesis (`partial`) is never
/// success, and is not auto-delivered to chat as if it were a finished
/// report (its path is still in `output`). Diagnostics lead the output so
/// the calling agent reads them first.
fn assemble_output(
    output: String,
    diagnostics: &[String],
    partial: bool,
    summary: ResultSummary,
    cost: Option<ResultCost>,
    report_path: &Path,
) -> Output {
    let output = if diagnostics.is_empty() {
        output
    } else {
        let lead = if partial {
            "Deep search partial: the report is incomplete."
        } else {
            "Deep search notes:"
        };
        format!("{lead}\n- {}\n\n{output}", diagnostics.join("\n- "))
    };
    Output {
        output,
        success: !partial,
        summary: Some(summary),
        cost,
        files_to_send: if partial {
            Vec::new()
        } else {
            vec![report_path.display().to_string()]
        },
    }
}

/// Empty (successful) result when no allowed provider returned anything.
fn no_results_output(query: &str, log: &SearchLog, opts: &research::Options) -> Output {
    let mut message = octos_research::no_results_message(query, &log.tried);
    if !log.errors.is_empty() {
        message.push_str("\nProvider notes:\n- ");
        message.push_str(&log.errors.join("\n- "));
        message.push('\n');
    }
    let output = if opts.items_mode {
        let mut doc = ItemsDocument::new(query, opts.controls_json());
        doc.providers = log.tried.clone();
        doc.note = Some(message);
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
    } else {
        message
    };
    Output {
        output,
        success: true,
        summary: Some(ResultSummary {
            kind: "deep_research".to_string(),
            headline: format!("No results for '{query}' from the allowed providers"),
            rounds: Some(log.queries.len() as u32),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `<report-stem>.items.json` next to the report, so repeated runs keep
/// their own items like they keep their own reports.
fn items_path_for(report_path: &Path) -> PathBuf {
    let stem = report_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("_report");
    report_path.with_file_name(format!("{stem}.items.json"))
}

// ---------------------------------------------------------------------------
// Follow-up query generation (heuristic, no LLM)
// ---------------------------------------------------------------------------

fn generate_follow_up_queries(original: &str, search_output: &str, depth: u8) -> Vec<String> {
    let mut queries = Vec::new();

    // 1. Extract bold/header topics from Perplexity's answer
    let subtopics = extract_subtopics(search_output);

    // 2. Recency is the `since` control's job (provider date filters), not
    //    a hard-coded year in the query text.

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
// HTTP client (synthesis LLM calls)
// ---------------------------------------------------------------------------

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(octos_research::USER_AGENT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ---------------------------------------------------------------------------
// DuckDuckGo HTML search (keyless last resort)
// ---------------------------------------------------------------------------

/// DuckDuckGo's no-JavaScript HTML results page. **Opt-in only**
/// (`OCTOS_ALLOW_SERP_SCRAPE=1`): it is a search-results page, which ADR
/// 0002 rules out scraping by default. Requested with the identifiable
/// research User-Agent; if DuckDuckGo declines, that is a clean miss.
async fn ddg_search(query: &str, count: u8) -> Result<Vec<SearchHit>, String> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoded(query));
    let response = research::api_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("DuckDuckGo error: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("DuckDuckGo HTTP {}", response.status()));
    }
    let html = response.text().await.unwrap_or_default();
    Ok(parse_ddg_results(&html, count as usize)
        .into_iter()
        .map(|(title, url, snippet)| SearchHit {
            url,
            title,
            snippet,
            provider: "duckduckgo".to_string(),
            ..Default::default()
        })
        .collect())
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
// Headless browser (the `deep_crawl` sibling binary)
// ---------------------------------------------------------------------------

/// Cap on the number of concurrent `deep_crawl` invocations (and thus
/// concurrent headless Chromium processes) per `deep_search` invocation.
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

/// Locate the `deep_crawl` binary: bundled sibling skill dir, next to this
/// binary, `~/.cargo/bin`, then `PATH`.
fn find_deep_crawl_bin() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = [
        std::env::current_exe().ok().and_then(|p| {
            p.parent()?
                .parent()
                .map(|d| d.join("deep-crawl").join("main"))
        }),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("deep_crawl"))),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo/bin/deep_crawl")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if let Some(p) = candidates.into_iter().find(|p| p.exists()) {
        return Some(p);
    }
    let o = std::process::Command::new("which")
        .arg("deep_crawl")
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
}

/// How long a `deep_crawl` that is being stopped gets to close its browser
/// before it is killed.
const DEEP_CRAWL_STOP_GRACE: Duration = Duration::from_secs(5);

/// A running `deep_crawl` that is stopped gracefully however its caller
/// ends (finished, timed out, or its future dropped).
///
/// deep_crawl starts its own headless Chrome and kills it in its SIGTERM
/// handler. A SIGKILL (what `kill_on_drop` sends) gives it no chance to, and
/// the browser was left running with no parent. So: SIGTERM first, then
/// SIGKILL if it has not exited within [`DEEP_CRAWL_STOP_GRACE`]. The child
/// is held until it is reaped, so its pid cannot be reused in between.
/// deep_crawl stays in this process's group, so a host that kills this
/// plugin's process group still reaches it and its browser.
struct DeepCrawlChild(Option<tokio::process::Child>);

impl DeepCrawlChild {
    fn child(&mut self) -> &mut tokio::process::Child {
        self.0
            .as_mut()
            .expect("deep_crawl child is present until drop")
    }
}

impl Drop for DeepCrawlChild {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    if tokio::time::timeout(DEEP_CRAWL_STOP_GRACE, child.wait())
                        .await
                        .is_err()
                    {
                        let _ = child.kill().await;
                    }
                });
            }
            Err(_) => {
                let _ = child.start_kill();
            }
        }
    }
}

/// Run `deep_crawl` with `input` on stdin; returns its stdout.
async fn run_deep_crawl(
    bin: &Path,
    input: &serde_json::Value,
    limit: Duration,
) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let child = tokio::process::Command::new(bin)
        .arg("deep_crawl")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // Last resort only: `DeepCrawlChild` stops it first.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn deep_crawl: {e}"))?;
    let mut crawl = DeepCrawlChild(Some(child));
    let work = async {
        if let Some(mut stdin) = crawl.child().stdin.take() {
            let _ = stdin.write_all(input.to_string().as_bytes()).await;
        }
        let mut stdout = Vec::new();
        if let Some(mut out) = crawl.child().stdout.take() {
            out.read_to_end(&mut stdout)
                .await
                .map_err(|e| format!("deep_crawl failed: {e}"))?;
        }
        crawl
            .child()
            .wait()
            .await
            .map_err(|e| format!("deep_crawl failed: {e}"))?;
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    };
    match tokio::time::timeout(limit, work).await {
        Ok(result) => result,
        // `crawl` is dropped on return: deep_crawl gets SIGTERM and closes
        // its browser.
        Err(_) => Err(format!("deep_crawl timed out after {}s", limit.as_secs())),
    }
}

/// Bing results page rendered in headless Chrome, then scraped.
///
/// **Opt-in only** (`OCTOS_ALLOW_SERP_SCRAPE=1`): scraping a search
/// engine's results page with a browser is the "disguised search" OctoSense
/// ADR 0002 §6 rules out, so it is never part of the automatic provider
/// order. Kept for operators who explicitly accept that on their own box.
async fn bing_cdp_search(query: &str, count: u8) -> Result<Vec<SearchHit>, String> {
    if !research::serp_scrape_allowed() {
        return Err("disabled".to_string());
    }
    let _permit = browser_semaphore()
        .acquire()
        .await
        .map_err(|_| "browser semaphore closed".to_string())?;
    let bin = find_deep_crawl_bin().ok_or("deep_crawl binary not found")?;
    let locale = detect_bing_locale(query);
    let search_url = format!(
        "https://www.bing.com/search?q={}&count={}&mkt={}&setlang={}",
        urlencoded(query),
        count.min(10),
        locale,
        locale,
    );
    let input = serde_json::json!({"url": search_url, "max_depth": 0, "max_pages": 1});
    let stdout = run_deep_crawl(&bin, &input, Duration::from_secs(60)).await?;
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|_| format!("unparseable deep_crawl output ({} bytes)", stdout.len()))?;
    let text = parsed.get("output").and_then(|v| v.as_str()).unwrap_or("");
    Ok(extract_bing_results(text)
        .iter()
        .take(count as usize)
        .filter_map(|row| {
            let (title, url) = row.trim_start_matches("- ").split_once("\n  ")?;
            Some(SearchHit {
                url: url.trim().to_string(),
                title: title.trim().to_string(),
                provider: "bing_cdp".to_string(),
                ..Default::default()
            })
        })
        .collect())
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

/// Private/internal link targets are dropped (shared SSRF classification
/// from `octos_research::net`; the reader re-checks with DNS before any read).
fn is_private_url(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(octos_research::net::is_private_host))
        .unwrap_or(false)
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
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
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

/// Keep at most `max_chars` characters (not bytes, so CJK text gets the same
/// room as English) and append `suffix` when anything was cut.
fn truncate_utf8(s: &str, max_chars: usize, suffix: &str) -> String {
    octos_research::text::truncate_chars(s, max_chars, suffix)
}

/// Extract `(title, url)` pairs from a Bing SERP text dump.
///
/// Bing's rendered SERP collapses each result onto a long line shaped
/// roughly like `<domain><url>›<crumb>...<title><snippet>`, e.g.
/// `thesaurus.comhttps://www.thesaurus.com › browse › hatesHATES Synonyms...`.
/// The old line-prefix heuristic only found URLs that started a line and
/// so missed every inline match. This pass uses a regex to find every
/// `http(s)://` occurrence, walks back to the most recent newline or `>`
/// breadcrumb separator to seed a title, strips Bing/MS noise domains,
/// and de-duplicates by normalized URL.
///
/// The returned strings are pre-formatted as `- <title>\n  <url>`, which
/// `bing_cdp_search` splits into hits. (Opt-in provider only.)
fn extract_bing_results(text: &str) -> Vec<String> {
    use regex::Regex;
    // URL terminator set chosen empirically against Bing's inline SERP:
    // whitespace, common HTML chars, fenced punctuation, parentheses, and
    // the `›` Bing breadcrumb glyph that follows the URL.
    let re = Regex::new(r#"https?://[^\s<>"\)\(›]+"#).expect("static URL regex");
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
    for m in re.find_iter(text) {
        let mut raw = m.as_str();
        while let Some(stripped) =
            raw.strip_suffix(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')'))
        {
            raw = stripped;
        }
        if raw.is_empty() {
            continue;
        }
        let url = raw.to_string();
        let lower = url.to_lowercase();
        if lower.contains("bing.com")
            || lower.contains("microsoft.com")
            || lower.contains("google.com")
            || lower.contains("gstatic.com")
            || lower.contains("msn.com/spartan")
        {
            continue;
        }
        if !seen.insert(normalize_url(&url)) {
            continue;
        }
        // Build a title from the text immediately before this URL on the
        // same logical line. Bing's domain prefix (`thesaurus.com` ahead
        // of `https://...`) is the closest thing to a title we get, so
        // we grab the last 80 chars of the prefix up to the previous
        // separator. Falls back to the bare URL when no prefix exists.
        let start = m.start();
        // The `›` Bing breadcrumb glyph is 3 bytes in UTF-8 (U+203A), so a
        // naive `i + 1` after `rfind` lands inside the glyph and `&str`
        // indexing panics. Step past the *full* UTF-8 width of whichever
        // separator we hit.
        let prefix_start = text[..start]
            .rfind(['\n', '›', '|'])
            .map(|i| {
                let sep_len = text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                i + sep_len
            })
            .unwrap_or(0);
        let raw_prefix = text[prefix_start..start].trim();
        let title = if raw_prefix.is_empty() {
            url.clone()
        } else {
            // Cap title length so we don't drag in adjacent results.
            let mut s = raw_prefix.to_string();
            if s.len() > 80 {
                let mut cut = 80;
                while cut > 0 && !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                s.truncate(cut);
            }
            s
        };
        results.push(format!("- {title}\n  {url}"));
    }
    results
}

/// Pick a Bing locale tag (`mkt`/`setlang` value) from the query script.
///
/// Bing has been observed returning English-locale synonym pages (e.g.
/// thesaurus.com "hates" synonyms for `美国和伊朗和谈`) when handed a CJK
/// query from a US-geolocated IP without any locale hint. Forcing
/// `mkt=zh-CN&setlang=zh-CN` for CJK input keeps the results aligned with
/// the user's actual language.
///
/// Detection uses a single pass over the chars, ordered by precedence:
/// Hangul → Korean, Hiragana/Katakana → Japanese, CJK Unified → Chinese.
/// Any other script falls through to `en-US`.
fn detect_bing_locale(query: &str) -> &'static str {
    let mut has_cjk_unified = false;
    let mut has_kana = false;
    let mut has_hangul = false;
    for ch in query.chars() {
        let c = ch as u32;
        if (0xAC00..=0xD7AF).contains(&c) {
            has_hangul = true;
        } else if (0x3040..=0x309F).contains(&c) || (0x30A0..=0x30FF).contains(&c) {
            has_kana = true;
        } else if (0x4E00..=0x9FFF).contains(&c) {
            has_cjk_unified = true;
        }
    }
    if has_hangul {
        "ko-KR"
    } else if has_kana {
        "ja-JP"
    } else if has_cjk_unified {
        "zh-CN"
    } else {
        "en-US"
    }
}

fn research_dir(slug: &str) -> PathBuf {
    let base = std::env::var("OCTOS_WORK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("research").join(slug)
}

/// Issue #261: derive the canonical report filename from the topic slug
/// so each research run produces a topic-named markdown file rather than
/// the legacy hardcoded `_report.md`. The wrapper directory still keeps
/// the slug, and intermediate sidecars (`_search_results.md`,
/// `01_*.md`, …) keep their leading-`_`/index prefix shapes so a
/// directory listing groups by run.
///
/// `octos_agent::tools::research_utils::read_sources` excludes files with
/// this report suffix when collecting "source" inputs for the
/// `synthesize_research` map-reduce. Without that companion skip, the new
/// topic-named report would be re-ingested as a source on the next
/// synthesis run.
///
/// Falls back to `_report.md` only if the slug is empty (degenerate
/// query). Keeps the file extension uniformly `.md`.
fn report_filename(slug: &str) -> String {
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "_report.md".to_string()
    } else {
        format!("{trimmed}_report.md")
    }
}

fn unique_report_path(dir: &Path, slug: &str) -> PathBuf {
    let base = report_filename(slug);
    let first = dir.join(&base);
    if !first.exists() {
        return first;
    }

    let stem = base.strip_suffix(".md").unwrap_or(&base);
    for index in 2.. {
        let candidate = dir.join(format!("{stem}-{index}.md"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("unbounded suffix search must find an unused report path")
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
            if let Some(reason) = &syn.truncated {
                report.push_str(&format!(
                    "> **Incomplete:** this synthesis was cut off ({reason}). Treat it as partial and check the sources below.\n\n"
                ));
            }
            report.push_str(syn.synthesis.trim());
            report.push_str("\n\n");
            if syn.truncated.is_some() {
                report.push_str("_[synthesis cut off here]_\n\n");
            }
            if let Some(conf) = syn.confidence {
                report.push_str(&format!("_Self-reported confidence: {conf:.2}_\n\n"));
            }
        }
        _ => {
            // Fallback when no LLM is available or synthesis was empty:
            // keep the v1 "Overview" but label it so it's clear we did
            // NOT synthesize, and operators know the result is raw.
            report.push_str("## Overview\n\n");
            if let Some(reason) = synthesis.and_then(|s| s.truncated.as_ref()) {
                report.push_str(&format!(
                    "> **Incomplete:** the synthesis produced no usable text ({reason}).\n\n"
                ));
            }
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
        if filename.is_empty() {
            report.push_str(
                "_Headline only: the page was not read (robots.txt or fetch failure)._\n\n",
            );
        } else {
            report.push_str(&format!(
                "_Full content: {}/{}_\n\n",
                dir.display(),
                filename
            ));
        }
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

/// Output of a synthesis call: prose with citations + metadata for the v2
/// result envelope, plus whether the reply was complete.
#[derive(Default)]
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
    /// Summed over every attempt.
    tokens_in: u32,
    tokens_out: u32,
    usd: Option<f64>,
    /// Why the synthesis is incomplete (`finish_reason: length`, or it ends
    /// mid-sentence / mid-structure) after the last attempt. `None` when
    /// the reply finished cleanly. An incomplete synthesis never counts as
    /// success.
    truncated: Option<String>,
    /// Model calls made (1, or 2 when a cut-off reply was retried).
    attempts: u32,
    /// Sentences with no `[N]` citation, marked `[citation needed]`.
    uncited_flagged: usize,
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

/// What the synthesis step produced.
enum SynthesisOutcome {
    /// No provider configured: the raw-results report is the designed
    /// output, not an error.
    NotConfigured,
    /// The call failed (transport, HTTP, unparseable or empty reply).
    Failed(String),
    /// The model replied. Check `truncated` before trusting it.
    Done(SynthesisResult),
}

/// Upper bound for one synthesis call. There is no output-token cap (see
/// [`synthesis_request_body`]), so a reasoning model can think for a while.
const SYNTHESIS_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Deadline of the whole run, set by `main`. The synthesis step keeps its
/// calls (and the retry) inside it so a slow model yields an honest
/// "incomplete" result instead of the whole run timing out with nothing.
static RUN_DEADLINE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Optional output-token cap for the synthesis call, from
/// `DEEP_SEARCH_SYNTHESIS_MAX_TOKENS`. Unset by default: a fixed cap cut
/// off reasoning models (their thinking tokens count against it) in 3 of 6
/// validation runs. Output size is bounded by the prompt and the source
/// budget instead.
fn synthesis_max_tokens() -> Option<u32> {
    std::env::var("DEEP_SEARCH_SYNTHESIS_MAX_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
}

/// OpenAI-compatible chat request. `max_tokens` is sent only when an
/// operator configured one.
fn synthesis_request_body(model: &str, prompt: &str, max_tokens: Option<u32>) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": SYNTHESIS_SYSTEM_PROMPT},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.3,
    });
    if let Some(n) = max_tokens {
        body["max_tokens"] = serde_json::json!(n);
    }
    body
}

/// Appended to the prompt when the first reply was cut off.
const SYNTHESIS_RETRY_NOTE: &str = "\n\nA previous attempt at this answer was cut off before it \
finished. Write a shorter answer: at most three paragraphs, every sentence complete, and \
finish with the Gaps line.";

/// One model reply.
struct ModelReply {
    content: String,
    finish_reason: Option<String>,
    tokens_in: u32,
    tokens_out: u32,
}

async fn call_synthesis_model(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<ModelReply, String> {
    let response = client
        .post(format!("{endpoint}/chat/completions"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(body)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("LLM call failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        eprintln!(
            "[synthesis] HTTP {status}: {}",
            truncate_utf8(&text, 300, "")
        );
        return Err(format!("LLM HTTP {status}"));
    }
    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("failed to parse response: {e}"))?;
    Ok(ModelReply {
        content: json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string(),
        finish_reason: json["choices"][0]["finish_reason"]
            .as_str()
            .map(str::to_string),
        // OpenAI-compatible providers return token usage under "usage".
        tokens_in: json["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32,
        tokens_out: json["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32,
    })
}

/// Why a reply is incomplete, or `None` when it finished cleanly.
fn truncation_reason(reply: &ModelReply, synthesis: &str) -> Option<String> {
    if reply.finish_reason.as_deref() == Some("length") {
        return Some("the model hit its output limit (finish_reason: length)".to_string());
    }
    let has_headline = reply.content.lines().any(|l| {
        l.trim()
            .trim_start_matches('#')
            .trim()
            .eq_ignore_ascii_case("headline")
    });
    if has_headline && !has_synthesis_section(&reply.content) {
        return Some("the reply stopped before its Synthesis section".to_string());
    }
    octos_research::text::looks_cut_off(synthesis).map(|r| format!("the synthesis {r}"))
}

fn has_synthesis_section(text: &str) -> bool {
    text.lines().any(|l| {
        l.trim()
            .strip_prefix("##")
            .map(|r| {
                matches!(
                    r.trim().to_lowercase().as_str(),
                    "synthesis" | "answer" | "report"
                )
            })
            .unwrap_or(false)
    })
}

/// Run the synthesis LLM call, retrying once when the reply is cut off.
///
/// Returns [`SynthesisOutcome::NotConfigured`] without an API key, in which
/// case the deep_search flow falls back to the v1 raw-dump report.
async fn synthesize(
    client: &reqwest::Client,
    input: &SynthesisInput<'_>,
    args_config: Option<&SynthesisConfig>,
) -> SynthesisOutcome {
    let Some((endpoint, api_key, model, provider)) = resolve_synthesis_config(args_config) else {
        return SynthesisOutcome::NotConfigured;
    };

    let prompt = build_synthesis_prompt(input);
    let max_tokens = synthesis_max_tokens();
    let mut result = SynthesisResult {
        provider: provider.clone(),
        model: model.clone(),
        ..Default::default()
    };
    let mut last_error = None;

    for attempt in 1..=2u32 {
        let remaining = RUN_DEADLINE
            .get()
            .map(|d| d.saturating_duration_since(std::time::Instant::now()))
            .unwrap_or(SYNTHESIS_CALL_TIMEOUT + Duration::from_secs(10))
            .saturating_sub(Duration::from_secs(10));
        if remaining < Duration::from_secs(15) {
            if attempt == 1 {
                return SynthesisOutcome::Failed("no time left in the run budget".to_string());
            }
            if let Some(reason) = &mut result.truncated {
                reason.push_str("; no time left to retry");
            }
            break;
        }
        let user_prompt = if attempt == 1 {
            prompt.clone()
        } else {
            format!("{prompt}{SYNTHESIS_RETRY_NOTE}")
        };
        let body = synthesis_request_body(&model, &user_prompt, max_tokens);
        let reply = match call_synthesis_model(
            client,
            &endpoint,
            &api_key,
            &body,
            remaining.min(SYNTHESIS_CALL_TIMEOUT),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[synthesis] attempt {attempt}: {e}");
                emit_v2_progress("synthesizing", &e, None);
                last_error = Some(e);
                // Keep a cut-off first reply rather than nothing.
                if result.attempts > 0 {
                    break;
                }
                return SynthesisOutcome::Failed(last_error.unwrap_or_default());
            }
        };
        result.attempts = attempt;
        result.tokens_in += reply.tokens_in;
        result.tokens_out += reply.tokens_out;

        let (synthesis_text, headline, confidence) = if reply.content.is_empty() {
            (String::new(), String::new(), None)
        } else {
            parse_synthesis_response(&reply.content)
        };
        if reply.content.is_empty() && reply.finish_reason.as_deref() != Some("length") {
            eprintln!("[synthesis] attempt {attempt}: empty response");
            if result.attempts > 1 || !result.synthesis.is_empty() {
                break;
            }
            return SynthesisOutcome::Failed("empty reply".to_string());
        }
        let truncated = truncation_reason(&reply, &synthesis_text);
        eprintln!(
            "[synthesis] attempt {attempt}: prompt_chars={} sources={} tokens_in={} tokens_out={} \
             finish_reason={} synthesis_chars={} complete={}",
            user_prompt.chars().count(),
            input.sources.len(),
            reply.tokens_in,
            reply.tokens_out,
            reply.finish_reason.as_deref().unwrap_or("-"),
            synthesis_text.chars().count(),
            truncated.is_none()
        );
        // A complete reply always wins; between two cut-off replies keep
        // the longer one.
        if truncated.is_none() || synthesis_text.len() >= result.synthesis.len() {
            result.synthesis = synthesis_text;
            result.headline = headline;
            result.confidence = confidence;
        }
        result.truncated = truncated;
        if result.truncated.is_none() {
            break;
        }
        if attempt == 1 {
            emit_v2_progress(
                "synthesizing",
                "Synthesis was cut off; retrying once with a shorter answer...",
                None,
            );
        }
    }
    if let Some(e) = last_error {
        if let Some(reason) = &mut result.truncated {
            reason.push_str(&format!("; the retry failed: {e}"));
        }
    }

    let (flagged, count) = octos_research::text::flag_uncited_sentences(&result.synthesis);
    result.synthesis = flagged;
    result.uncited_flagged = count;
    result.usd = project_usd(&model, result.tokens_in, result.tokens_out);
    // Emit a v2 cost event so the host can attribute spend.
    emit_v2_cost(
        &provider,
        &model,
        result.tokens_in,
        result.tokens_out,
        result.usd,
    );
    SynthesisOutcome::Done(result)
}

/// System prompt for the synthesis call.
///
/// The output format is a strict 3-section markdown doc:
/// 1. `## Headline` — one line summarizing the answer
/// 2. `## Confidence` — numeric in `[0, 1]`
/// 3. `## Synthesis` — multi-paragraph prose with `[N]` citations, ending
///    with one `Gaps:` line
///
/// We parse this in [`parse_synthesis_response`] so we can lift each piece
/// into the v2 result envelope. In validation every unsupported or wrongly
/// cited claim was an uncited sentence (often about the sources rather than
/// the topic), so the rules forbid both and give "what's missing" one
/// exempt line.
const SYNTHESIS_SYSTEM_PROMPT: &str = "\
You are a research analyst. You write grounded, cited answers using only the numbered source \
material the user provides. Rules:\n\
(1) Every sentence that states a fact, figure, date, quote, cause or consequence MUST end with \
one or more `[N]` citations to the sources that support it. This includes sentences that \
restate, connect or compare facts. If no source supports a sentence, do not write it.\n\
(2) Write about the topic, not about the sources: no sentences about how many sources there \
are, their quality, or what they do or do not say. The only place for that is the final Gaps \
line.\n\
(3) Use only the sources; add nothing from memory.\n\
(4) Use multiple paragraphs; do NOT bulletpoint the entire answer.\n\
(5) When sources disagree, say so and cite each side.\n\
(6) Write in the language of the question.\n\
(7) Finish every sentence. Output exactly three sections, in this order:\n\n\
## Headline\n\
<one-line answer, no citations>\n\n\
## Confidence\n\
<a number from 0.0 to 1.0 reflecting source agreement and depth>\n\n\
## Synthesis\n\
<2-6 paragraphs of cited prose>\n\
Gaps: <one line naming what the sources do not cover, or \"none\"; keep the English word \
\"Gaps:\" whatever the language>\n";

/// Characters (not bytes) of each source's text in the synthesis prompt.
/// About one full news article: the old 1500-*byte* cap kept roughly 500
/// Chinese characters, often just the lede.
const PER_SOURCE_CHARS: usize = 6_000;
/// Total source text in one prompt, shared evenly when many sources come
/// back (12 sources get 4000 characters each). 48k characters is about 12k
/// tokens of English or 30-48k of CJK text, well inside current 128k-token
/// context windows, and about US$0.01 of input at DeepSeek Flash prices.
const TOTAL_SOURCE_CHARS: usize = 48_000;
/// Sources beyond this are listed in the report but not sent to the model.
const MAX_SYNTHESIS_SOURCES: usize = 12;

/// Per-source character budget when `n` sources go into the prompt.
fn per_source_chars(n: usize) -> usize {
    if n == 0 {
        return PER_SOURCE_CHARS;
    }
    (TOTAL_SOURCE_CHARS / n).min(PER_SOURCE_CHARS)
}

fn build_synthesis_prompt(input: &SynthesisInput<'_>) -> String {
    let mut prompt = String::new();
    prompt.push_str(&format!("Question: {}\n\n", input.query));
    prompt.push_str(&format!(
        "Researcher gathered {} sources across {} search rounds. Source excerpts \
         (long pages are trimmed):\n\n",
        input.sources.len(),
        input.rounds
    ));
    let included = input.sources.len().min(MAX_SYNTHESIS_SOURCES);
    let budget = per_source_chars(included);
    for src in input.sources.iter().take(MAX_SYNTHESIS_SOURCES) {
        prompt.push_str(&format!("---\n[{}] {}\n", src.index, src.url));
        prompt.push_str(&truncate_utf8(&src.excerpt, budget, "\n... (truncated)"));
        prompt.push_str("\n\n");
    }
    if input.sources.len() > MAX_SYNTHESIS_SOURCES {
        prompt.push_str(&format!(
            "---\n(+ {} more sources omitted from this prompt for brevity; \
             they are still listed in the final report.)\n",
            input.sources.len() - MAX_SYNTHESIS_SOURCES
        ));
    }
    prompt.push_str(
        "---\n\nWrite the answer. Every factual sentence ends with its [N] citation(s); \
         cite each numbered source you use. Sources you do not cite will not appear in the \
         final summary. Sources marked \"headline only\" were not read: cite them only for \
         what the headline itself says. Mention publication dates when they matter.\n",
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
    schema_version: u32,
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
        schema_version: 1,
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

    /// A deep_crawl that times out is asked to stop (SIGTERM) and gets to
    /// clean up its browser, instead of being SIGKILLed and leaving the
    /// browser behind.
    #[cfg(unix)]
    #[tokio::test]
    async fn should_stop_a_timed_out_deep_crawl_gracefully() {
        let dir = std::env::temp_dir().join(format!("deep-crawl-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("cleaned");
        let script = dir.join("deep_crawl");
        // A stand-in deep_crawl: a long-running "browser" child, killed by
        // its TERM handler, which also leaves a marker.
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nsleep 300 &\nbrowser=$!\ntrap 'kill $browser; echo $browser > {m}; exit 130' TERM\nwait $browser\n",
                m = marker.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = run_deep_crawl(&script, &serde_json::json!({}), Duration::from_millis(500))
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        let mut browser = None;
        for _ in 0..50 {
            if let Ok(pid) = std::fs::read_to_string(&marker) {
                browser = Some(pid.trim().to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let browser = browser.expect("deep_crawl was not asked to clean up");
        let alive = std::process::Command::new("kill")
            .args(["-0", &browser])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the stand-in browser {browser} was left running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("top AI startups 2025"), "top-AI-startups-2025");
        assert_eq!(slugify("NVIDIA stock price!"), "NVIDIA-stock-price");
        assert_eq!(slugify("  spaces  "), "spaces");
        // CJK preserved
        assert!(slugify("伊朗哈梅内伊").contains("伊朗"));
    }

    #[test]
    fn extract_bing_results_finds_inline_serp_urls() {
        // Reproduces the actual Bing SERP dump observed on mini5 for a
        // CJK query — domain prefix glued to the URL, breadcrumbs, then
        // title and snippet all on one logical line. Previous extractor
        // only saw `https://...` when it started a line and so returned
        // empty for this entire SERP.
        let sample = "About 50 results...thesaurus.comhttps://www.thesaurus.com › browse › hatesHATES Synonyms - 113 wordsthesaurus.comhttps://www.thesaurus.comSynonyms and Antonyms of Words | Thesaurus.com";
        let results = extract_bing_results(sample);
        // We expect at least the two thesaurus.com URLs to come through
        // (de-duplication keeps only the deeper one because the second
        // hit normalizes to a prefix of the first under our rules — the
        // exact dedup count isn't load-bearing, but ≥1 is required).
        assert!(!results.is_empty(), "expected inline URLs to be extracted");
        // Result rows must be in "- <title>\n  <url>" shape so
        // `bing_cdp_search` can split them into hits.
        for row in &results {
            assert!(row.contains("https://"), "row missing URL: {row}");
            assert!(row.starts_with("- "), "row missing title prefix: {row}");
        }
        // Noise-domain filter must drop bing.com / google.com / etc.
        let noisy = "bing.com results bing.comhttps://www.bing.com/results microsoft.comhttps://microsoft.com/help https://example.com/real";
        let only_real = extract_bing_results(noisy);
        assert_eq!(only_real.len(), 1, "expected only example.com to survive");
        assert!(only_real[0].contains("example.com"));
    }

    #[test]
    fn detect_bing_locale_routes_cjk_scripts_distinctly() {
        // Chinese → zh-CN: prevents Bing from interpreting `美国和伊朗和谈`
        // as English "hates" and returning thesaurus.com synonym pages.
        assert_eq!(detect_bing_locale("美国和伊朗和谈"), "zh-CN");
        assert_eq!(detect_bing_locale("深度研究"), "zh-CN");
        // Korean (Hangul) takes precedence so the U+4E00 fallback never
        // misroutes mixed CJK loanwords back to Chinese.
        assert_eq!(detect_bing_locale("미국과 이란 협상"), "ko-KR");
        // Japanese (Hiragana/Katakana) likewise routes to ja-JP even when
        // the query also contains Han characters.
        assert_eq!(detect_bing_locale("アメリカとイラン"), "ja-JP");
        assert_eq!(detect_bing_locale("米国とイラン交渉"), "ja-JP");
        // English / Latin defaults to en-US.
        assert_eq!(detect_bing_locale("US Iran peace talks 2026"), "en-US");
        assert_eq!(detect_bing_locale(""), "en-US");
    }

    #[test]
    fn should_save_report_with_topic_named_filename() {
        // Issue #261: deep_search no longer hardcodes `_report.md`. The
        // canonical report file inside `research/<slug>/` is named after
        // the topic slug so it is self-describing when downloaded or
        // attached, and so the LLM has a stable name to read back on the
        // next turn.
        assert_eq!(
            report_filename("rust-async-runtimes-2026"),
            "rust-async-runtimes-2026_report.md"
        );
        assert_eq!(
            report_filename("top-AI-startups-2025"),
            "top-AI-startups-2025_report.md"
        );
        // CJK preserved (slugify keeps unicode > U+007F).
        assert_eq!(
            report_filename("新能源汽车走势分析"),
            "新能源汽车走势分析_report.md"
        );
        assert!(report_filename("foo-bar").ends_with("_report.md"));
        assert!(report_filename("中美关系").ends_with("_report.md"));
    }

    #[test]
    fn report_filename_falls_back_when_slug_is_empty() {
        // Defensive: a degenerate empty slug (after `slugify` strips
        // everything) keeps the historical filename so we never produce
        // a zero-name `.md` file.
        assert_eq!(report_filename(""), "_report.md");
        assert_eq!(report_filename("---"), "_report.md");
    }

    #[test]
    fn unique_report_path_adds_suffix_on_collision() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "deep-search-report-collision-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("topic_report.md"), "first").unwrap();
        std::fs::write(dir.join("topic_report-2.md"), "second").unwrap();

        let candidate = unique_report_path(&dir, "topic");
        assert_eq!(
            candidate.file_name().and_then(|name| name.to_str()),
            Some("topic_report-3.md")
        );

        std::fs::remove_dir_all(&dir).unwrap();
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
    fn should_build_items_document_with_read_and_unread_items() {
        let hit = SearchHit {
            url: "https://elpais.com/clima/cumbre.html?utm_source=gdelt".into(),
            title: "La cumbre del clima".into(),
            source: Some("elpais.com".into()),
            lang: Some("es".into()),
            published: Some("2026-09-26T10:15:00Z".into()),
            provider: "gdelt".into(),
            ..Default::default()
        };
        let page = research::ReadPage {
            final_url: "https://elpais.com/clima/cumbre.html".into(),
            text: "Los delegados de 190 países alcanzaron un acuerdo preliminar el jueves por la noche. Otra frase.".into(),
            meta: octos_research::extract::PageMeta {
                title: Some("Cumbre: acuerdo preliminar".into()),
                site_name: Some("EL PAÍS".into()),
                lang: Some("es-ES".into()),
                ..Default::default()
            },
            links: vec![],
            rendered: true,
            fetched_at: "2026-09-27T12:00:00Z".into(),
        };
        let src = CitedSource { hit, page };
        let item = source_item(&src, 3, true, "03_elpais-com.md");
        assert_eq!(item.url, "https://elpais.com/clima/cumbre.html");
        assert_eq!(item.title, "Cumbre: acuerdo preliminar");
        assert_eq!(item.source, "EL PAÍS");
        assert_eq!(item.domain, "elpais.com");
        assert_eq!(item.lang.as_deref(), Some("es-ES"));
        assert_eq!(item.published.as_deref(), Some("2026-09-26T10:15:00Z"));
        assert_eq!(item.summary_kind, SummaryKind::Extractive);
        assert!(item.summary.starts_with("Los delegados"));
        assert_eq!(item.citation, Some(3));
        assert!(item.cited && item.read && item.rendered);
        assert!(
            source_meta_line(&src).contains("EL PAÍS · 2026-09-26T10:15:00Z · es-ES · rendered")
        );

        let gnews = SearchHit {
            url: "https://news.google.com/rss/articles/CBMi?oc=5".into(),
            title: "Headline".into(),
            source: Some("Reuters".into()),
            source_url: Some("https://www.reuters.com".into()),
            published: Some("2026-09-25T14:05:00Z".into()),
            provider: "google_news_rss".into(),
            ..Default::default()
        };
        let unread = unread_item(&gnews, Some(4), false);
        assert!(!unread.read);
        assert_eq!(unread.domain, "reuters.com");
        assert_eq!(unread.source, "Reuters");
        assert_eq!(unread.citation, Some(4));
        assert!(headline_meta_line(&gnews).contains("Reuters · 2026-09-25T14:05:00Z"));

        let mut doc = ItemsDocument::new("cumbre", serde_json::json!({"lang": ["es"]}));
        doc.items = vec![item, unread];
        doc.skipped = vec![SkippedUrl {
            url: gnews.url.clone(),
            reason: "robots".into(),
        }];
        let v = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["schema"], "octos.research.items.v1");
        assert_eq!(v["items"].as_array().unwrap().len(), 2);
        assert_eq!(v["items"][0]["summary_kind"], "extractive");
        assert_eq!(v["skipped"][0]["reason"], "robots");
        assert_eq!(skipped_summary(&doc.skipped), "robots: 1");
        assert_eq!(
            items_path_for(Path::new("/r/topic_report-2.md")),
            PathBuf::from("/r/topic_report-2.items.json")
        );
    }

    #[test]
    fn should_return_empty_result_with_guidance_when_nothing_is_configured() {
        let mut opts = research::Options::from_input(
            &serde_json::from_value(serde_json::json!({"query": "q", "output": "items"})).unwrap(),
            chrono::Utc::now(),
        )
        .unwrap();
        let log = SearchLog {
            queries: vec!["q".into()],
            errors: vec!["duckduckgo: disabled (scraping search-results pages needs OCTOS_ALLOW_SERP_SCRAPE=1)".into()],
            ..Default::default()
        };
        let out = no_results_output("q", &log, &opts);
        assert!(out.success, "empty result, not a failure");
        let doc: serde_json::Value = serde_json::from_str(&out.output).unwrap();
        assert!(doc["items"].as_array().unwrap().is_empty());
        let note = doc["note"].as_str().unwrap();
        assert!(note.contains("Providers tried: none"), "{note}");
        assert!(note.contains("SEARXNG_URL") && note.contains("OCTOS_ALLOW_SERP_SCRAPE"));

        opts.items_mode = false;
        let text = no_results_output("q", &log, &opts).output;
        assert!(text.contains("add a search API key"), "{text}");
    }

    #[test]
    fn test_generate_follow_up_queries() {
        let queries = generate_follow_up_queries("AI regulations", "**Ethics** and **Safety**", 2);
        assert!(queries.len() >= 2);
        assert!(queries.iter().any(|q| q.contains("Ethics")));
        assert!(
            !queries
                .iter()
                .any(|q| q.contains("latest") || q.contains("2026")),
            "no hard-coded recency in follow-ups: {queries:?}"
        );
    }

    #[test]
    fn test_truncate_utf8() {
        assert_eq!(truncate_utf8("Hello, world!", 100, "..."), "Hello, world!");
        assert_eq!(truncate_utf8("Hello, world!", 5, "..."), "Hello...");
        // Characters, not bytes: 5 Han characters are 15 bytes.
        assert_eq!(truncate_utf8("台风正在逼近广东", 5, "..."), "台风正在逼...");
    }

    #[test]
    fn test_is_private_url() {
        assert!(is_private_url("http://localhost/x"));
        assert!(is_private_url("http://127.0.0.1/x"));
        assert!(is_private_url("http://[::1]/x"));
        assert!(!is_private_url("https://example.com/x"));
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
                schema_version: 1,
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
                schema_version: 1,
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
                schema_version: 1,
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
                schema_version: 1,
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
                schema_version: 1,
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
        // #2267 (Windows): autocrlf checkouts give include_str! a CRLF fixture
        // while the sink writes LF — compare normalized, the byte content of
        // each event is what the fixture pins.
        let norm = |s: &str| s.replace("\r\n", "\n");
        assert_eq!(norm(&actual), norm(fixture));
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
            ..Default::default()
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
            ..Default::default()
        };
        let cited = result.cited_indexes();
        assert!(cited.contains(&1));
        assert_eq!(cited.len(), 1);
    }

    #[test]
    fn build_synthesis_prompt_caps_per_source_chars() {
        let huge_excerpt = "x".repeat(PER_SOURCE_CHARS + 1_000);
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
        // Per-source cap + suffix → cap is enforced
        assert!(prompt.contains("(truncated)"));
        assert!(prompt.contains(&"x".repeat(PER_SOURCE_CHARS)));
        assert!(!prompt.contains(&"x".repeat(PER_SOURCE_CHARS + 1)));
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
            ..Default::default()
        };
        let dir = std::path::PathBuf::from("/tmp/research/topic");
        // Issue #261: canonical report filename now derives from the
        // topic slug, not the literal `_report.md`.
        let report_path = dir.join(report_filename("topic"));
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
        // Trailer with report path stays for v1 host compatibility,
        // but now references the topic-named filename (issue #261).
        assert!(
            report.contains(&format!(
                "Report saved to: {}",
                dir.join("topic_report.md").display()
            )),
            "expected topic-named report path in trailer: {report}"
        );
        // Belt-and-suspenders: the legacy literal must NOT leak back in.
        assert!(
            !report.contains("/topic/_report.md"),
            "legacy `_report.md` must not appear once slug is set: {report}"
        );
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
        // Issue #261: topic-named filename, no longer `_report.md`.
        let report_path = dir.join(report_filename("topic"));
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
        // Trailer must reference the topic-named file.
        assert!(
            report.contains(&format!(
                "Report saved to: {}",
                dir.join("topic_report.md").display()
            )),
            "expected topic-named trailer: {report}"
        );
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
            ..Default::default()
        };
        // Issue #261: even in the degenerate-synthesis fallback the
        // file is named after the topic slug.
        let topic_report = format!("/tmp/{}", report_filename("topic"));
        let report = build_report(
            "topic",
            Some(&syn),
            "raw initial",
            &[],
            &["topic".to_string()],
            std::path::Path::new("/tmp"),
            std::path::Path::new(&topic_report),
        );
        assert!(!report.contains("## Synthesis"));
        assert!(report.contains("LLM synthesis unavailable"));
        assert!(report.contains("raw initial"));
        // Trailer references the topic-named filename.
        assert!(
            report.contains("Report saved to: /tmp/topic_report.md"),
            "expected topic-named trailer in fallback path: {report}"
        );
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
                ..Default::default()
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

    // --- Synthesis completeness (validation findings, 27 Sep 2026) ---

    /// A fake OpenAI-compatible endpoint: serves `replies` in order, one per
    /// connection, and records each request body.
    fn fake_model(
        replies: Vec<serde_json::Value>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = bodies.clone();
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).unwrap();
                seen.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap());
                let text = reply.to_string();
                let mut stream = stream;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                )
                .unwrap();
            }
        });
        (endpoint, bodies)
    }

    fn model_reply(content: &str, finish_reason: &str) -> serde_json::Value {
        serde_json::json!({
            "choices": [{"message": {"content": content}, "finish_reason": finish_reason}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 50}
        })
    }

    fn fake_config(endpoint: &str) -> SynthesisConfig {
        SynthesisConfig {
            endpoint: endpoint.to_string(),
            api_key: "test-key".to_string(),
            model: "deepseek-v4-flash".to_string(),
            provider: "deepseek".to_string(),
        }
    }

    fn one_source_input() -> SynthesisInput<'static> {
        SynthesisInput {
            query: "EU AI Act",
            rounds: 1,
            sources: vec![SynthesisSource {
                index: 1,
                url: "https://a.example/x".to_string(),
                excerpt: "Spain's regulator warned a company before launch.".to_string(),
            }],
        }
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    const CUT_OFF: &str = "## Headline\nRegulators act early\n\n## Confidence\n0.4\n\n## Synthesis\n\
        Spain's regulator warned a company before launch [1]. Notably, the warning came before the tool";
    const COMPLETE: &str =
        "## Headline\nRegulators act early\n\n## Confidence\n0.4\n\n## Synthesis\n\
        Spain's regulator warned a company before launch [1].\n\nGaps: fines imposed so far.";

    #[test]
    fn synthesis_request_sends_no_max_tokens_by_default() {
        let body = synthesis_request_body("m", "p", None);
        assert!(body.get("max_tokens").is_none(), "{body}");
        let body = synthesis_request_body("m", "p", Some(4000));
        assert_eq!(body["max_tokens"], 4000, "operator opt-in still works");
    }

    #[tokio::test]
    async fn length_cut_off_reply_is_retried_and_never_success() {
        let (endpoint, bodies) = fake_model(vec![
            model_reply(CUT_OFF, "length"),
            model_reply(CUT_OFF, "length"),
        ]);
        let cfg = fake_config(&endpoint);
        let outcome = synthesize(&test_client(), &one_source_input(), Some(&cfg)).await;
        let SynthesisOutcome::Done(result) = outcome else {
            panic!("expected a reply");
        };
        assert_eq!(result.attempts, 2, "one retry");
        assert!(
            result
                .truncated
                .as_deref()
                .unwrap()
                .contains("finish_reason: length"),
            "{:?}",
            result.truncated
        );
        assert_eq!(
            (result.tokens_in, result.tokens_out),
            (200, 100),
            "both calls counted"
        );

        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        for b in bodies.iter() {
            assert!(b.get("max_tokens").is_none(), "no output cap sent: {b}");
        }
        let retry_prompt = bodies[1]["messages"][1]["content"].as_str().unwrap();
        assert!(retry_prompt.contains("was cut off"), "{retry_prompt}");

        // The run result built from it is partial, not success.
        let (synthesis, diagnostics) = synthesis_diagnostics(SynthesisOutcome::Done(result));
        let syn = synthesis.unwrap();
        assert!(diagnostics
            .iter()
            .any(|d| d.starts_with("Synthesis incomplete")));
        let out = assemble_output(
            "report".to_string(),
            &diagnostics,
            syn.truncated.is_some(),
            ResultSummary::default(),
            None,
            Path::new("/tmp/r.md"),
        );
        assert!(!out.success, "a cut-off report must not report success");
        assert!(out.output.starts_with("Deep search partial"));
        assert!(out.files_to_send.is_empty());

        let report = build_report(
            "EU AI Act",
            Some(&syn),
            "",
            &[],
            &[],
            Path::new("/tmp"),
            Path::new("/tmp/r.md"),
        );
        assert!(report.contains("**Incomplete:**"), "{report}");
        assert!(report.contains("_[synthesis cut off here]_"), "{report}");
    }

    #[tokio::test]
    async fn mid_sentence_reply_without_length_flag_is_caught_and_retry_can_fix_it() {
        // Some providers say "stop" even when the text is cut off.
        let (endpoint, _bodies) = fake_model(vec![
            model_reply(CUT_OFF, "stop"),
            model_reply(COMPLETE, "stop"),
        ]);
        let cfg = fake_config(&endpoint);
        let SynthesisOutcome::Done(result) =
            synthesize(&test_client(), &one_source_input(), Some(&cfg)).await
        else {
            panic!("expected a reply");
        };
        assert_eq!(result.attempts, 2);
        assert!(result.truncated.is_none(), "{:?}", result.truncated);
        assert!(result.synthesis.ends_with("Gaps: fines imposed so far."));
        assert_eq!(result.uncited_flagged, 0);
        let (_, diagnostics) = synthesis_diagnostics(SynthesisOutcome::Done(result));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[tokio::test]
    async fn reasoning_model_with_empty_content_on_length_is_incomplete_not_fallback() {
        let (endpoint, _bodies) =
            fake_model(vec![model_reply("", "length"), model_reply("", "length")]);
        let cfg = fake_config(&endpoint);
        let SynthesisOutcome::Done(result) =
            synthesize(&test_client(), &one_source_input(), Some(&cfg)).await
        else {
            panic!("an output-limit hit is a cut-off reply, not a missing one");
        };
        assert!(result.truncated.is_some());
    }

    #[tokio::test]
    async fn uncited_sentences_are_flagged_in_a_complete_reply() {
        let reply = "## Headline\nVucic resigns\n\n## Confidence\n0.7\n\n## Synthesis\n\
            Vucic resigned on Friday [1]. The immediate political context is nearly two years of protest.\n\n\
            Gaps: none";
        let (endpoint, _bodies) = fake_model(vec![model_reply(reply, "stop")]);
        let cfg = fake_config(&endpoint);
        let SynthesisOutcome::Done(result) =
            synthesize(&test_client(), &one_source_input(), Some(&cfg)).await
        else {
            panic!("expected a reply");
        };
        assert_eq!(result.attempts, 1);
        assert!(result.truncated.is_none());
        assert_eq!(result.uncited_flagged, 1);
        assert!(result
            .synthesis
            .contains("two years of protest. [citation needed]"));
        let (_, diagnostics) = synthesis_diagnostics(SynthesisOutcome::Done(result));
        assert!(diagnostics[0].contains("[citation needed]"));
    }

    #[test]
    fn synthesis_prompt_trims_cjk_sources_by_characters() {
        // 5000 Han characters (15000 bytes) fit the 6000-character budget
        // whole; the old 1500-byte cap kept 500 of them.
        let zh = "台风".repeat(2_500);
        let input = SynthesisInput {
            query: "台风",
            rounds: 1,
            sources: vec![SynthesisSource {
                index: 1,
                url: "https://news.example/zh".to_string(),
                excerpt: zh.clone(),
            }],
        };
        let prompt = build_synthesis_prompt(&input);
        assert!(prompt.contains(&zh), "whole CJK source kept");
        assert!(!prompt.contains("(truncated)"));

        // With 12 sources the total budget is shared: 4000 characters each.
        assert_eq!(per_source_chars(12), TOTAL_SOURCE_CHARS / 12);
        assert_eq!(per_source_chars(3), PER_SOURCE_CHARS);
        let sources = (1..=12)
            .map(|i| SynthesisSource {
                index: i,
                url: format!("https://x{i}"),
                excerpt: "台".repeat(9_000),
            })
            .collect();
        let prompt = build_synthesis_prompt(&SynthesisInput {
            query: "台风",
            rounds: 1,
            sources,
        });
        assert!(prompt.contains(&"台".repeat(4_000)));
        assert!(!prompt.contains(&"台".repeat(4_001)));
        assert!(prompt.chars().count() < TOTAL_SOURCE_CHARS + 3_000);
    }

    #[test]
    fn system_prompt_requires_citation_on_every_factual_sentence() {
        assert!(SYNTHESIS_SYSTEM_PROMPT.contains("Every sentence that states a fact"));
        assert!(SYNTHESIS_SYSTEM_PROMPT.contains(octos_research::text::GAPS_PREFIX));
    }
}
