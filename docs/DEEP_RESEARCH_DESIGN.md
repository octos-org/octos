# Deep Research: Architecture & Implementation Specification

> Status: Design Phase — Ready for Implementation
> Date: 2026-03-01
> Codename: **Deep Research** (flagship offering)
> Authors: yuechen, claude

---

## Table of Contents

1. [Problem Statement](#1-problem-statement)
2. [Competitive Landscape](#2-competitive-landscape)
3. [Architecture Overview](#3-architecture-overview)
4. [Component 1: Search Source Registry](#4-component-1-search-source-registry)
5. [Component 2: Search Engine Backends](#5-component-2-search-engine-backends)
6. [Component 3: Research Orchestrator](#6-component-3-research-orchestrator)
7. [Component 4: Collection Sub-Agents](#7-component-4-collection-sub-agents)
8. [Component 5: Synthesis Agent](#8-component-5-synthesis-agent)
9. [Component 6: Report Output](#9-component-6-report-output)
10. [Data Structures & Type Definitions](#10-data-structures--type-definitions)
11. [Prompt Templates](#11-prompt-templates)
12. [Integration with Existing Codebase](#12-integration-with-existing-codebase)
13. [File Layout](#13-file-layout)
14. [Configuration](#14-configuration)
15. [Error Handling & Edge Cases](#15-error-handling--edge-cases)
16. [Testing Strategy](#16-testing-strategy)
17. [Build Order & Milestones](#17-build-order--milestones)
18. [Why RL Training Matters (Future)](#18-why-rl-training-matters-future)
19. [References](#19-references)

---

## 1. Problem Statement

Our current pipeline (`deep_search` + `synthesize_research`) produces **sketchy, summary-level output** (~6K chars) because it:

1. **Searches once** with generic English queries through a single search engine (DuckDuckGo)
2. **Never reflects** — no gap detection, no follow-up searches for missing information
3. **Treats all content equally** — no relevance scoring, no quality filtering
4. **Summarizes instead of synthesizing** — map-reduce compression loses detail

Deep Research solves this by searching **deeply and broadly** (multiple engines, languages, regions, specialized sources), **reflecting on gaps**, **filtering for quality**, and **synthesizing with self-critique**.

---

## 2. Competitive Landscape

### Gemini Deep Research (Google)

- **80-160 search queries** per session, **100+ full pages** read
- **Orchestrator + parallel sub-agents** per subtopic
- **Reflection loop**: search → reflect → detect gaps → search again (iterative)
- **Multi-pass self-critique** in synthesis + citation validation
- **1M token context window** for holding raw source material during synthesis
- **5-60 minutes** per research task
- **RL-trained** specifically for multi-step search optimization
- Open-source reference: [gemini-fullstack-langgraph-quickstart](https://github.com/google-gemini/gemini-fullstack-langgraph-quickstart)

### Kimi K2.5 Agent Swarm (Moonshot)

- **74 keywords**, **206 URLs explored**, **only top 3.2% retained**
- **Up to 100 parallel sub-agents** via PARL (Parallel-Agent RL)
- **1,500 coordinated tool calls** per session
- **End-to-end RL** — no hardcoded workflow, all behavior learned from reward signals
- **10,000+ word reports** with **26 traceable citations** (clickable to highlighted source)
- **3-5 minutes** (4.5x faster than single-agent via parallelism)
- Technical report: [Kimi-Researcher](https://moonshotai.github.io/Kimi-Researcher/)

### Our Target

| Metric | Current (Deep Search v1) | Target (Deep Research) |
|---|---|---|
| Search queries per session | ~10 | 50-100+ |
| Pages read | ~30 | 100-200+ |
| Languages searched | 1 (English) | 2-6 per query |
| Search engines used | 1 (DuckDuckGo) | 3-5 (Google, Bing, Baidu, etc.) |
| Sub-agents | 0 | 5-20 (dynamic) |
| Reflection cycles per sub-agent | 0 | 3-5 |
| Quality filtering | None | Top 5-10% retained |
| Report length | ~6K chars | 10K-30K+ words |
| Citations | 0 (inline URLs only) | 20+ traceable |
| Time budget | ~6 min | 5-20 min |

---

## 3. Architecture Overview

```
┌──────────────────────────────────────────────────────────────────────┐
│                         USER QUERY                                   │
│  "Who will win the 2026 World Cup?"                                  │
└──────────────────────┬───────────────────────────────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────────────────────────────┐
│  PHASE 1: PLANNING  (DeepResearchOrchestrator tool)                  │
│                                                                      │
│  1. Detect topics via keyword matching against Source Registry        │
│  2. Call LLM to generate ResearchPlan:                               │
│     - N research angles (5-20)                                       │
│     - Per angle: assigned search engines, languages, portals         │
│  3. (Optional) Present plan to user for review via channel message   │
│                                                                      │
│  Output: ResearchPlan { angles: Vec<ResearchAngle> }                 │
└──────────────────────┬───────────────────────────────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────────────────────────────┐
│  PHASE 2: COLLECTION  (Parallel sub-agents)                          │
│                                                                      │
│  For each ResearchAngle, spawn a tokio task:                         │
│                                                                      │
│  ┌────────────────────────────┐  ┌────────────────────────────────┐  │
│  │  Sub-Agent 1               │  │  Sub-Agent 2                   │  │
│  │  "English sports press"    │  │  "Spanish press (Marca, AS)"   │  │
│  │                            │  │                                │  │
│  │  loop (max 5 cycles) {     │  │  loop (max 5 cycles) {        │  │
│  │    search(google_news,     │  │    search(google_news,         │  │
│  │           lang=en)         │  │           lang=es)             │  │
│  │    fetch_pages(parallel)   │  │    fetch_pages(parallel)       │  │
│  │    score_relevance()       │  │    score_relevance()           │  │
│  │    discard_low_quality()   │  │    discard_low_quality()       │  │
│  │    reflect: gaps?          │  │    reflect: gaps?              │  │
│  │    if gaps → new queries   │  │    if gaps → new queries       │  │
│  │    else → write partial    │  │    else → write partial        │  │
│  │  }                         │  │  }                             │  │
│  └────────────────────────────┘  └────────────────────────────────┘  │
│  ┌────────────────────────────┐  ┌────────────────────────────────┐  │
│  │  Sub-Agent 3               │  │  Sub-Agent N...                │  │
│  │  "Statistics (FBref)"      │  │  "Prediction markets"          │  │
│  │  ...                       │  │  ...                           │  │
│  └────────────────────────────┘  └────────────────────────────────┘  │
│                                                                      │
│  Wall-time = max(sub-agent durations)                                │
│  Output: research/{slug}/partial_01.md ... partial_N.md              │
└──────────────────────┬───────────────────────────────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────────────────────────────┐
│  PHASE 3: SYNTHESIS  (Fresh LLM context, no search history)          │
│                                                                      │
│  1. Read all partial_*.md files                                      │
│  2. If total > context limit: batch into 80K char chunks             │
│  3. Map phase: extract key findings per batch (LLM call)             │
│  4. Reduce phase: merge all findings into structured report (LLM)    │
│  5. Self-critique: review draft → identify weak sections (LLM)       │
│  6. (Optional) Request targeted gap-fill searches if critical        │
│  7. Final report with citations, tables, cross-source analysis       │
│                                                                      │
│  Output: research/{slug}/report.md                                   │
└──────────────────────┬───────────────────────────────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────────────────────────────┐
│  REPORT OUTPUT                                                       │
│                                                                      │
│  Canonical: Markdown (report.md)                                     │
│  Optional conversions (separate tools, not in pipeline):             │
│  - PPTX via existing make-slide skill                                │
│  - DOCX (future)                                                     │
│  - HTML/website (future)                                             │
│  - Infographic (future)                                              │
└──────────────────────────────────────────────────────────────────────┘
```

---

## 4. Component 1: Search Source Registry

### Purpose

A structured database mapping **topics → search engines + languages + portals + data APIs**. The orchestrator queries this to decide which sources each sub-agent should use. Without this, every sub-agent hits the same DuckDuckGo in English.

### File Location

```
crates/octos-agent/src/source_registry.rs       # Rust structs + lookup logic
crates/octos-agent/data/source_registry.toml     # Data file (embedded via include_str!)
```

### Rust Data Structures

```rust
// crates/octos-agent/src/source_registry.rs

use serde::Deserialize;
use std::collections::HashMap;

/// The full registry, deserialized from source_registry.toml.
#[derive(Debug, Deserialize)]
pub struct SourceRegistry {
    pub search_engines: Vec<SearchEngine>,
    pub topic_sources: Vec<TopicSource>,
}

/// A search engine backend that can be queried programmatically.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchEngine {
    /// Unique identifier (e.g., "google", "baidu", "bing_news").
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// How to access: "api", "rss", "scrape".
    #[serde(rename = "type")]
    pub engine_type: EngineType,
    /// URL template. Placeholders: {query}, {lang}, {region}, {count}.
    pub endpoint: String,
    /// Environment variable name for API key. Empty string = no key needed.
    #[serde(default)]
    pub key_env: String,
    /// Additional env vars needed (e.g., Google CSE ID).
    #[serde(default)]
    pub extra_env: HashMap<String, String>,
    /// Whether this engine supports language filtering.
    #[serde(default)]
    pub supports_language: bool,
    /// Whether this engine supports region/country filtering.
    #[serde(default)]
    pub supports_region: bool,
    /// If set, this engine only supports these languages (e.g., Baidu = ["zh"]).
    /// Empty = supports all languages.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Rate limit description (informational).
    #[serde(default)]
    pub rate_limit: String,
    /// Lower = prefer when available.
    #[serde(default = "default_priority")]
    pub priority: u8,
    /// For proxy engines: which engine to route through (e.g., "serper").
    #[serde(default)]
    pub proxy_engine: Option<String>,
    /// For proxy engines: query template with {query} placeholder
    /// (e.g., "site:x.com {query}").
    #[serde(default)]
    pub query_template: Option<String>,
    /// Informational notes about the engine.
    #[serde(default)]
    pub notes: Option<String>,
}

fn default_priority() -> u8 { 5 }

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EngineType {
    Api,
    Rss,
    Scrape,
    /// Proxy search: routes queries through another engine with site: operator.
    /// Used for walled-garden platforms (X/Twitter, Reddit, LinkedIn).
    Proxy,
}

/// Maps a topic domain to specific sources, languages, and portals.
#[derive(Debug, Clone, Deserialize)]
pub struct TopicSource {
    /// Topic identifier (e.g., "middle_east", "football", "finance").
    pub topic: String,
    /// Keywords that trigger this topic (matched against user query, case-insensitive).
    pub keywords: Vec<String>,
    /// Which search engine IDs to use for this topic.
    pub search_engines: Vec<String>,
    /// Languages to search in (ISO 639-1 codes).
    pub languages: Vec<String>,
    /// Specific news/content portals to browse directly.
    #[serde(default)]
    pub portals: Vec<Portal>,
    /// Think tanks or institutional sources.
    #[serde(default)]
    pub think_tanks: Vec<String>,
    /// Structured data APIs (e.g., "Yahoo Finance API", "Semantic Scholar API").
    #[serde(default)]
    pub data_sources: Vec<String>,
}

/// A specific website/portal to browse directly via web_fetch.
#[derive(Debug, Clone, Deserialize)]
pub struct Portal {
    pub name: String,
    pub url: String,
    pub lang: String,
    /// Optional: RSS feed URL for this portal.
    #[serde(default)]
    pub rss: Option<String>,
}

impl SourceRegistry {
    /// Load from embedded TOML data.
    pub fn load() -> Self {
        let data = include_str!("../data/source_registry.toml");
        toml::from_str(data).expect("source_registry.toml is invalid")
    }

    /// Find all topics that match the given query (by keyword matching).
    /// Returns matched TopicSource entries sorted by match strength
    /// (number of keyword hits).
    pub fn match_topics(&self, query: &str) -> Vec<&TopicSource> {
        let query_lower = query.to_lowercase();
        let query_words: Vec<&str> = query_lower.split_whitespace().collect();

        let mut scored: Vec<(&TopicSource, usize)> = self
            .topic_sources
            .iter()
            .filter_map(|ts| {
                let hits = ts.keywords.iter().filter(|kw| {
                    let kw_lower = kw.to_lowercase();
                    // Match if keyword appears as substring in query
                    // OR any query word matches keyword
                    query_lower.contains(&kw_lower)
                        || query_words.iter().any(|w| kw_lower.contains(w))
                }).count();
                if hits > 0 { Some((ts, hits)) } else { None }
            })
            .collect();

        scored.sort_by(|a, b| b.1.cmp(&a.1));
        scored.into_iter().map(|(ts, _)| ts).collect()
    }

    /// Look up a search engine by ID.
    pub fn engine(&self, id: &str) -> Option<&SearchEngine> {
        self.search_engines.iter().find(|e| e.id == id)
    }

    /// Get all engines that are available (have required API keys set).
    pub fn available_engines(&self) -> Vec<&SearchEngine> {
        self.search_engines
            .iter()
            .filter(|e| {
                e.key_env.is_empty() || std::env::var(&e.key_env).is_ok()
            })
            .collect()
    }

    /// For a query, compute the full set of engines, languages, and portals
    /// to use across all matched topics. Merges and deduplicates.
    pub fn plan_sources(&self, query: &str) -> SourcePlan {
        let topics = self.match_topics(query);
        let mut engines = Vec::new();
        let mut languages = Vec::new();
        let mut portals = Vec::new();
        let mut think_tanks = Vec::new();
        let mut data_sources = Vec::new();

        for ts in &topics {
            for eid in &ts.search_engines {
                if !engines.contains(eid) {
                    engines.push(eid.clone());
                }
            }
            for lang in &ts.languages {
                if !languages.contains(lang) {
                    languages.push(lang.clone());
                }
            }
            for portal in &ts.portals {
                if !portals.iter().any(|p: &Portal| p.url == portal.url) {
                    portals.push(portal.clone());
                }
            }
            for tt in &ts.think_tanks {
                if !think_tanks.contains(tt) {
                    think_tanks.push(tt.clone());
                }
            }
            for ds in &ts.data_sources {
                if !data_sources.contains(ds) {
                    data_sources.push(ds.clone());
                }
            }
        }

        // Always include at least one general-purpose engine
        if engines.is_empty() {
            engines.push("google".to_string());
            engines.push("duckduckgo".to_string());
        }
        if languages.is_empty() {
            languages.push("en".to_string());
        }

        SourcePlan {
            matched_topics: topics.iter().map(|t| t.topic.clone()).collect(),
            engines,
            languages,
            portals,
            think_tanks,
            data_sources,
        }
    }
}

/// Computed source plan for a query.
#[derive(Debug, Clone)]
pub struct SourcePlan {
    pub matched_topics: Vec<String>,
    pub engines: Vec<String>,
    pub languages: Vec<String>,
    pub portals: Vec<Portal>,
    pub think_tanks: Vec<String>,
    pub data_sources: Vec<String>,
}
```

### TOML Data File

Location: `crates/octos-agent/data/source_registry.toml`

```toml
# =============================================================================
# Search Source Registry
# =============================================================================
# Maps topics to search engines, languages, portals, and data sources.
# The orchestrator uses this to decide which sources each sub-agent should use.
#
# To add a new topic:
#   1. Add a [[topic_sources]] block with keywords
#   2. Reference existing search_engine IDs or add new [[search_engines]] blocks
#   3. List portals with name, url, lang, and optional rss
#
# To add a new search engine:
#   1. Add a [[search_engines]] block with id, name, type, endpoint
#   2. Set key_env to the env var name (empty string = no key needed)
#   3. Reference the ID from topic_sources.search_engines
# =============================================================================

# ---------------------------------------------------------------------------
# SEARCH ENGINES
# ---------------------------------------------------------------------------

[[search_engines]]
id = "google"
name = "Google Custom Search"
type = "api"
endpoint = "https://www.googleapis.com/customsearch/v1?q={query}&cx={cx}&gl={region}&lr=lang_{lang}&num={count}"
key_env = "GOOGLE_CSE_API_KEY"
extra_env = { cx = "GOOGLE_CSE_ID" }
supports_language = true
supports_region = true
rate_limit = "100/day free, 10K/day paid ($5/1K)"
priority = 1

[[search_engines]]
id = "google_news"
name = "Google News RSS"
type = "rss"
endpoint = "https://news.google.com/rss/search?q={query}&hl={lang}&gl={region}&ceid={region}:{lang}"
key_env = ""
supports_language = true
supports_region = true
rate_limit = "unlimited"
priority = 1

[[search_engines]]
id = "bing"
name = "Bing Web Search v7"
type = "api"
endpoint = "https://api.bing.microsoft.com/v7.0/search?q={query}&mkt={lang}-{region}&count={count}"
key_env = "BING_API_KEY"
supports_language = true
supports_region = true
rate_limit = "1K/month free (S1), 10K/month ($7)"
priority = 2

[[search_engines]]
id = "bing_news"
name = "Bing News Search v7"
type = "api"
endpoint = "https://api.bing.microsoft.com/v7.0/news/search?q={query}&mkt={lang}-{region}&count={count}"
key_env = "BING_API_KEY"
supports_language = true
supports_region = true
rate_limit = "1K/month free"
priority = 2

[[search_engines]]
id = "baidu"
name = "Baidu Search"
type = "scrape"
endpoint = "https://www.baidu.com/s?wd={query}&rn={count}"
key_env = ""
supports_language = false
supports_region = false
languages = ["zh"]
rate_limit = "unlimited (scrape, may get blocked)"
priority = 1

[[search_engines]]
id = "yandex"
name = "Yandex Search XML"
type = "api"
endpoint = "https://yandex.com/search/xml?query={query}&lr={region}&l10n={lang}"
key_env = "YANDEX_API_KEY"
extra_env = { user = "YANDEX_USER" }
supports_language = true
supports_region = true
languages = ["ru", "uk", "kk", "be", "en"]
rate_limit = "1K/day"
priority = 3

[[search_engines]]
id = "naver"
name = "Naver Search"
type = "api"
endpoint = "https://openapi.naver.com/v1/search/webkr.json?query={query}&display={count}"
key_env = "NAVER_CLIENT_ID"
extra_env = { secret = "NAVER_CLIENT_SECRET" }
supports_language = false
supports_region = false
languages = ["ko"]
rate_limit = "25K/day"
priority = 3

[[search_engines]]
id = "duckduckgo"
name = "DuckDuckGo HTML"
type = "scrape"
endpoint = "https://html.duckduckgo.com/html/?q={query}"
key_env = ""
supports_language = false
supports_region = false
rate_limit = "unlimited"
priority = 10

[[search_engines]]
id = "brave"
name = "Brave Search"
type = "api"
endpoint = "https://api.search.brave.com/res/v1/web/search?q={query}&count={count}"
key_env = "BRAVE_API_KEY"
supports_language = false
supports_region = false
rate_limit = "2K/month free"
priority = 5

[[search_engines]]
id = "serper"
name = "Serper.dev (Google Proxy)"
type = "api"
endpoint = "https://google.serper.dev/search"
key_env = "SERPER_API_KEY"
supports_language = true
supports_region = true
rate_limit = "$50/mo for 50K queries (recommended primary)"
priority = 1
notes = "Returns Google results via JSON API. No CSE setup needed. Also supports /news, /images, /scholar endpoints."

[[search_engines]]
id = "serper_news"
name = "Serper.dev News"
type = "api"
endpoint = "https://google.serper.dev/news"
key_env = "SERPER_API_KEY"
supports_language = true
supports_region = true
rate_limit = "shared with serper quota"
priority = 1

[[search_engines]]
id = "perplexity"
name = "Perplexity Sonar"
type = "api"
endpoint = "https://api.perplexity.ai/chat/completions"
key_env = "PERPLEXITY_API_KEY"
supports_language = false
supports_region = false
rate_limit = "pay-per-use"
priority = 2
notes = "AI-powered meta-search. Returns synthesized answers with citations. Use for fact verification and gap-filling, not primary collection."

# --- Social Media & Platform-Specific Proxy Searches ---
# These are NOT direct API searches. They use Google/Serper with site: operators
# to search within walled-garden platforms that have prohibitively expensive APIs.

[[search_engines]]
id = "x_proxy"
name = "X/Twitter (via Google site: proxy)"
type = "proxy"
proxy_engine = "serper"
query_template = "site:x.com {query}"
key_env = "SERPER_API_KEY"
supports_language = true
supports_region = true
rate_limit = "shared with serper quota"
priority = 3
notes = "X Basic API is $200/mo (7-day only), Pro is $5K/mo. Using site:x.com via Google is far cheaper and searches all public history. Good for trending topics, expert opinions, breaking news reactions."

[[search_engines]]
id = "reddit_proxy"
name = "Reddit (via Google site: proxy)"
type = "proxy"
proxy_engine = "serper"
query_template = "site:reddit.com {query}"
key_env = "SERPER_API_KEY"
supports_language = true
supports_region = true
rate_limit = "shared with serper quota"
priority = 3
notes = "Reddit API requires OAuth + rate limits. Google indexes Reddit deeply. Good for community sentiment, technical discussions, product reviews."

[[search_engines]]
id = "linkedin_proxy"
name = "LinkedIn (via Google site: proxy)"
type = "proxy"
proxy_engine = "serper"
query_template = "site:linkedin.com {query}"
key_env = "SERPER_API_KEY"
supports_language = false
supports_region = false
rate_limit = "shared with serper quota"
priority = 4
notes = "LinkedIn has no public search API. Google indexes public profiles and posts. Good for industry expert opinions and company announcements."

[[search_engines]]
id = "semantic_scholar"
name = "Semantic Scholar"
type = "api"
endpoint = "https://api.semanticscholar.org/graph/v1/paper/search?query={query}&limit={count}"
key_env = ""
supports_language = false
supports_region = false
rate_limit = "100/5min unauthenticated"
priority = 3

[[search_engines]]
id = "arxiv"
name = "arXiv Search"
type = "api"
endpoint = "http://export.arxiv.org/api/query?search_query=all:{query}&max_results={count}"
key_env = ""
supports_language = false
supports_region = false
languages = ["en"]
rate_limit = "unlimited (3s between requests)"
priority = 3

# ---------------------------------------------------------------------------
# TOPIC → SOURCE MAPPINGS
# ---------------------------------------------------------------------------

[[topic_sources]]
topic = "middle_east"
keywords = ["iran", "iraq", "syria", "lebanon", "israel", "palestine", "saudi", "yemen", "gulf", "khamenei", "hezbollah", "hamas", "ayatollah", "tehran", "riyadh", "中东", "伊朗", "以色列"]
search_engines = ["serper", "serper_news", "google_news", "x_proxy"]
languages = ["en", "ar", "fa", "he"]
think_tanks = ["IISS", "RAND", "Brookings", "CFR", "CSIS", "Chatham House"]
data_sources = []

[[topic_sources.portals]]
name = "Al Jazeera (Arabic)"
url = "https://www.aljazeera.net"
lang = "ar"
rss = "https://www.aljazeera.net/rss"

[[topic_sources.portals]]
name = "Al Jazeera (English)"
url = "https://www.aljazeera.com/news"
lang = "en"
rss = "https://www.aljazeera.com/xml/rss/all.xml"

[[topic_sources.portals]]
name = "Al Arabiya"
url = "https://www.alarabiya.net"
lang = "ar"

[[topic_sources.portals]]
name = "Times of Israel"
url = "https://www.timesofisrael.com"
lang = "en"
rss = "https://www.timesofisrael.com/feed/"

[[topic_sources.portals]]
name = "IRNA"
url = "https://www.irna.ir"
lang = "fa"

[[topic_sources.portals]]
name = "Fars News"
url = "https://www.farsnews.ir"
lang = "fa"

[[topic_sources.portals]]
name = "Middle East Eye"
url = "https://www.middleeasteye.net"
lang = "en"

# ---

[[topic_sources]]
topic = "football"
keywords = ["world cup", "FIFA", "soccer", "football", "premier league", "la liga", "champions league", "euros", "euro 2028", "bundesliga", "serie a", "ligue 1", "足球", "世界杯"]
search_engines = ["serper", "serper_news", "google_news", "x_proxy", "reddit_proxy"]
languages = ["en", "es", "pt", "fr", "de", "it", "ar"]
think_tanks = []
data_sources = ["FBref", "WhoScored", "SofaScore", "Transfermarkt"]

[[topic_sources.portals]]
name = "ESPN FC"
url = "https://www.espn.com/soccer/"
lang = "en"

[[topic_sources.portals]]
name = "BBC Sport Football"
url = "https://www.bbc.com/sport/football"
lang = "en"
rss = "https://feeds.bbci.co.uk/sport/football/rss.xml"

[[topic_sources.portals]]
name = "Marca"
url = "https://www.marca.com/futbol.html"
lang = "es"

[[topic_sources.portals]]
name = "AS"
url = "https://as.com/futbol/"
lang = "es"

[[topic_sources.portals]]
name = "Mundo Deportivo"
url = "https://www.mundodeportivo.com/futbol"
lang = "es"

[[topic_sources.portals]]
name = "A Bola"
url = "https://www.abola.pt"
lang = "pt"

[[topic_sources.portals]]
name = "Globo Esporte"
url = "https://ge.globo.com/futebol/"
lang = "pt"

[[topic_sources.portals]]
name = "L'Equipe Football"
url = "https://www.lequipe.fr/Football/"
lang = "fr"

[[topic_sources.portals]]
name = "Gazzetta dello Sport"
url = "https://www.gazzetta.it/Calcio/"
lang = "it"

[[topic_sources.portals]]
name = "Kicker"
url = "https://www.kicker.de"
lang = "de"

[[topic_sources.portals]]
name = "Olé"
url = "https://www.ole.com.ar/futbol/"
lang = "es"

[[topic_sources.portals]]
name = "FBref"
url = "https://fbref.com"
lang = "en"

[[topic_sources.portals]]
name = "Transfermarkt"
url = "https://www.transfermarkt.com"
lang = "en"

# ---

[[topic_sources]]
topic = "china"
keywords = ["china", "beijing", "shanghai", "xi jinping", "CPC", "CCP", "chinese economy", "中国", "北京", "上海", "习近平"]
search_engines = ["baidu", "serper", "serper_news", "x_proxy"]
languages = ["zh", "en"]
think_tanks = ["Brookings", "CSIS", "RAND", "Carnegie"]
data_sources = []

[[topic_sources.portals]]
name = "Xinhua"
url = "https://www.xinhuanet.com"
lang = "zh"

[[topic_sources.portals]]
name = "SCMP"
url = "https://www.scmp.com"
lang = "en"

[[topic_sources.portals]]
name = "36Kr"
url = "https://36kr.com"
lang = "zh"

[[topic_sources.portals]]
name = "Caixin"
url = "https://www.caixin.com"
lang = "zh"

[[topic_sources.portals]]
name = "Guancha"
url = "https://www.guancha.cn"
lang = "zh"

# ---

[[topic_sources]]
topic = "russia_ukraine"
keywords = ["russia", "ukraine", "putin", "zelensky", "moscow", "kyiv", "NATO", "crimea", "donbas", "俄罗斯", "乌克兰", "俄乌"]
search_engines = ["serper", "serper_news", "google_news", "yandex", "x_proxy"]
languages = ["en", "ru", "uk"]
think_tanks = ["IISS", "RAND", "Chatham House", "ISW"]
data_sources = []

[[topic_sources.portals]]
name = "TASS (English)"
url = "https://tass.com"
lang = "en"

[[topic_sources.portals]]
name = "TASS (Russian)"
url = "https://tass.ru"
lang = "ru"

[[topic_sources.portals]]
name = "Ukrainska Pravda"
url = "https://www.pravda.com.ua"
lang = "uk"

[[topic_sources.portals]]
name = "Kyiv Independent"
url = "https://kyivindependent.com"
lang = "en"

[[topic_sources.portals]]
name = "ISW"
url = "https://www.understandingwar.org"
lang = "en"

# ---

[[topic_sources]]
topic = "finance"
keywords = ["stock", "market", "GDP", "inflation", "fed", "interest rate", "crypto", "bitcoin", "IPO", "earnings", "bond", "equity", "S&P 500", "nasdaq", "dow jones", "股票", "市场", "经济"]
search_engines = ["serper", "serper_news", "x_proxy", "reddit_proxy"]
languages = ["en", "zh"]
think_tanks = ["IMF", "World Bank", "BIS"]
data_sources = ["Yahoo Finance API", "CoinGecko API", "World Bank API", "FRED API"]

[[topic_sources.portals]]
name = "Bloomberg"
url = "https://www.bloomberg.com"
lang = "en"

[[topic_sources.portals]]
name = "Reuters Business"
url = "https://www.reuters.com/business/"
lang = "en"

[[topic_sources.portals]]
name = "Financial Times"
url = "https://www.ft.com"
lang = "en"

[[topic_sources.portals]]
name = "Yahoo Finance"
url = "https://finance.yahoo.com"
lang = "en"

[[topic_sources.portals]]
name = "EastMoney"
url = "https://www.eastmoney.com"
lang = "zh"

# ---

[[topic_sources]]
topic = "technology"
keywords = ["AI", "LLM", "GPU", "startup", "tech", "SaaS", "cloud", "semiconductor", "chip", "NVIDIA", "Apple", "Google", "Microsoft", "Meta", "OpenAI", "人工智能", "芯片"]
search_engines = ["serper", "serper_news", "x_proxy", "reddit_proxy", "linkedin_proxy"]
languages = ["en", "zh"]
think_tanks = []
data_sources = ["HN API", "arXiv API", "Semantic Scholar API"]

[[topic_sources.portals]]
name = "Hacker News"
url = "https://news.ycombinator.com"
lang = "en"

[[topic_sources.portals]]
name = "Ars Technica"
url = "https://arstechnica.com"
lang = "en"

[[topic_sources.portals]]
name = "TechCrunch"
url = "https://techcrunch.com"
lang = "en"

[[topic_sources.portals]]
name = "The Verge"
url = "https://www.theverge.com"
lang = "en"

[[topic_sources.portals]]
name = "36Kr"
url = "https://36kr.com"
lang = "zh"

[[topic_sources.portals]]
name = "MIT Technology Review"
url = "https://www.technologyreview.com"
lang = "en"

# ---

[[topic_sources]]
topic = "academic"
keywords = ["research", "study", "paper", "journal", "peer-reviewed", "clinical trial", "meta-analysis", "systematic review", "论文", "研究"]
search_engines = ["serper", "semantic_scholar", "arxiv"]
languages = ["en"]
think_tanks = []
data_sources = ["Semantic Scholar API", "arXiv API", "PubMed API", "Google Scholar (scrape)"]

# ---

[[topic_sources]]
topic = "prediction"
keywords = ["predict", "forecast", "odds", "betting", "who will win", "probability", "chances", "outlook", "预测"]
search_engines = ["serper", "x_proxy", "reddit_proxy", "perplexity"]
languages = ["en"]
think_tanks = []
data_sources = []

[[topic_sources.portals]]
name = "Polymarket"
url = "https://polymarket.com"
lang = "en"

[[topic_sources.portals]]
name = "Metaculus"
url = "https://www.metaculus.com"
lang = "en"

# ---

[[topic_sources]]
topic = "japan_korea"
keywords = ["japan", "tokyo", "korea", "seoul", "japanese", "korean", "日本", "韩国", "東京"]
search_engines = ["serper", "serper_news", "google_news", "naver"]
languages = ["en", "ja", "ko"]
think_tanks = []
data_sources = []

[[topic_sources.portals]]
name = "NHK World"
url = "https://www3.nhk.or.jp/nhkworld/"
lang = "en"

[[topic_sources.portals]]
name = "Yonhap (English)"
url = "https://en.yna.co.kr"
lang = "en"

# ---

[[topic_sources]]
topic = "health"
keywords = ["health", "disease", "vaccine", "pandemic", "WHO", "FDA", "drug", "treatment", "cancer", "clinical", "医疗", "健康", "疫苗"]
search_engines = ["serper", "serper_news", "perplexity"]
languages = ["en"]
think_tanks = ["WHO", "CDC", "NIH"]
data_sources = ["PubMed API"]

[[topic_sources.portals]]
name = "WHO"
url = "https://www.who.int"
lang = "en"

[[topic_sources.portals]]
name = "CDC"
url = "https://www.cdc.gov"
lang = "en"

[[topic_sources.portals]]
name = "STAT News"
url = "https://www.statnews.com"
lang = "en"

# ---

[[topic_sources]]
topic = "climate_energy"
keywords = ["climate", "carbon", "renewable", "solar", "wind", "nuclear", "oil", "OPEC", "energy", "emission", "net zero", "气候", "能源"]
search_engines = ["serper", "serper_news", "google_news"]
languages = ["en"]
think_tanks = ["IEA", "IRENA", "WRI"]
data_sources = []

[[topic_sources.portals]]
name = "Carbon Brief"
url = "https://www.carbonbrief.org"
lang = "en"

[[topic_sources.portals]]
name = "IEA"
url = "https://www.iea.org"
lang = "en"
```

### How to Add New Topics

Adding a new topic requires zero code changes — just add to the TOML file:

```toml
[[topic_sources]]
topic = "basketball"
keywords = ["NBA", "basketball", "playoffs", "finals", "MVP", "篮球"]
search_engines = ["serper", "serper_news", "x_proxy", "reddit_proxy"]
languages = ["en", "zh"]

[[topic_sources.portals]]
name = "ESPN NBA"
url = "https://www.espn.com/nba/"
lang = "en"

[[topic_sources.portals]]
name = "The Ringer"
url = "https://www.theringer.com/nba"
lang = "en"
```

Rebuild and the orchestrator picks it up automatically.

---

## 5. Component 2: Search Engine Backends

### Purpose

Implement the actual HTTP/scrape logic for each search engine in the registry. Each backend takes a query + language + region and returns structured results.

### File Location

```
crates/octos-agent/src/search/
    mod.rs              # SearchBackend trait + factory
    serper.rs           # Serper.dev (Google proxy) + SiteProxyBackend (X, Reddit, LinkedIn)
    google.rs           # Google Custom Search + Google News RSS (legacy, CSE closing Jan 2027)
    bing.rs             # Bing Web + News Search (Grounding with Bing, Azure-only)
    baidu.rs            # Baidu HTML scrape
    yandex.rs           # Yandex XML API
    naver.rs            # Naver OpenAPI
    perplexity.rs       # Perplexity Sonar (AI meta-search, for verification)
    rss.rs              # Generic RSS feed parser (used by Google News, portals)
```

### SearchBackend Trait

```rust
// crates/octos-agent/src/search/mod.rs

use eyre::Result;
use serde::{Deserialize, Serialize};

/// A single search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// Source engine that produced this result.
    pub source: String,
    /// Language of the result page (if known).
    pub language: Option<String>,
}

/// Options for a search query.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub query: String,
    pub count: u8,
    /// ISO 639-1 language code (e.g., "en", "ar", "zh").
    pub language: Option<String>,
    /// ISO 3166-1 alpha-2 region code (e.g., "US", "IR", "CN").
    pub region: Option<String>,
}

#[async_trait::async_trait]
pub trait SearchBackend: Send + Sync {
    /// Unique engine ID matching source_registry.toml.
    fn engine_id(&self) -> &str;

    /// Execute a search query.
    async fn search(&self, options: &SearchOptions) -> Result<Vec<SearchResult>>;

    /// Check if this backend is available (API key present, etc.).
    fn is_available(&self) -> bool;
}

/// Factory: create a SearchBackend from a registry SearchEngine entry.
pub fn create_backend(engine: &crate::source_registry::SearchEngine) -> Option<Box<dyn SearchBackend>> {
    match engine.id.as_str() {
        // --- Tier 1: Primary search engines ---
        "serper" | "serper_news" => Some(Box::new(serper::SerperBackend::new(engine))),
        "google" => Some(Box::new(google::GoogleSearchBackend::new(engine))),
        "google_news" => Some(Box::new(google::GoogleNewsBackend::new(engine))),
        "bing" | "bing_news" => Some(Box::new(bing::BingSearchBackend::new(engine))),

        // --- Tier 2: Regional / language-specific ---
        "baidu" => Some(Box::new(baidu::BaiduSearchBackend::new())),
        "yandex" => Some(Box::new(yandex::YandexSearchBackend::new(engine))),
        "naver" => Some(Box::new(naver::NaverSearchBackend::new(engine))),

        // --- Tier 3: Social media proxy searches (site: operator via Serper) ---
        "x_proxy" | "reddit_proxy" | "linkedin_proxy" =>
            Some(Box::new(serper::SiteProxyBackend::new(engine))),

        // --- Tier 4: AI-powered meta-search ---
        "perplexity" => Some(Box::new(perplexity::PerplexityBackend::new(engine))),

        // --- Fallback: free/no-key backends ---
        "duckduckgo" => Some(Box::new(duckduckgo::DdgSearchBackend::new())),
        "brave" => Some(Box::new(brave::BraveSearchBackend::new())),
        _ => None,
    }
}
```

### Example: Google Custom Search Implementation

```rust
// crates/octos-agent/src/search/google.rs

pub struct GoogleSearchBackend {
    client: reqwest::Client,
    api_key: Option<String>,
    cx: Option<String>,
}

impl GoogleSearchBackend {
    pub fn new(engine: &SearchEngine) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: std::env::var(&engine.key_env).ok(),
            cx: engine.extra_env.get("cx")
                .and_then(|env_name| std::env::var(env_name).ok()),
        }
    }
}

#[async_trait]
impl SearchBackend for GoogleSearchBackend {
    fn engine_id(&self) -> &str { "google" }

    fn is_available(&self) -> bool {
        self.api_key.is_some() && self.cx.is_some()
    }

    async fn search(&self, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        let api_key = self.api_key.as_ref().ok_or_else(|| eyre::eyre!("GOOGLE_CSE_API_KEY not set"))?;
        let cx = self.cx.as_ref().ok_or_else(|| eyre::eyre!("GOOGLE_CSE_ID not set"))?;

        let mut params = vec![
            ("q", options.query.clone()),
            ("key", api_key.clone()),
            ("cx", cx.clone()),
            ("num", options.count.min(10).to_string()),
        ];

        if let Some(lang) = &options.language {
            params.push(("lr", format!("lang_{lang}")));
        }
        if let Some(region) = &options.region {
            params.push(("gl", region.clone()));
        }

        let resp = self.client
            .get("https://www.googleapis.com/customsearch/v1")
            .query(&params)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!("Google CSE error {status}: {body}");
        }

        let data: serde_json::Value = resp.json().await?;
        let items = data.get("items").and_then(|v| v.as_array());

        Ok(items.map(|items| {
            items.iter().filter_map(|item| {
                Some(SearchResult {
                    title: item.get("title")?.as_str()?.to_string(),
                    url: item.get("link")?.as_str()?.to_string(),
                    snippet: item.get("snippet").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                    source: "google".to_string(),
                    language: options.language.clone(),
                })
            }).collect()
        }).unwrap_or_default())
    }
}
```

### Example: Google News RSS Implementation

```rust
pub struct GoogleNewsBackend {
    client: reqwest::Client,
}

#[async_trait]
impl SearchBackend for GoogleNewsBackend {
    fn engine_id(&self) -> &str { "google_news" }
    fn is_available(&self) -> bool { true } // No API key needed

    async fn search(&self, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        let lang = options.language.as_deref().unwrap_or("en");
        let region = options.region.as_deref().unwrap_or("US");
        let encoded_query = urlencoded(&options.query);

        let url = format!(
            "https://news.google.com/rss/search?q={encoded_query}&hl={lang}&gl={region}&ceid={region}:{lang}"
        );

        let resp = self.client.get(&url).send().await?;
        let xml = resp.text().await?;

        // Parse RSS XML — extract <item> elements
        // Each item has <title>, <link>, <description>
        parse_rss_items(&xml, options.count as usize, "google_news", Some(lang))
    }
}

/// Generic RSS parser reusable for any RSS feed.
pub fn parse_rss_items(
    xml: &str,
    max: usize,
    source: &str,
    language: Option<&str>,
) -> Result<Vec<SearchResult>> {
    let mut results = Vec::new();
    // Simple XML parsing without a full XML library:
    // Find each <item>...</item> block, extract <title>, <link>, <description>
    let mut pos = 0;
    while results.len() < max {
        let item_start = match xml[pos..].find("<item>") {
            Some(p) => pos + p,
            None => break,
        };
        let item_end = match xml[item_start..].find("</item>") {
            Some(p) => item_start + p + 7,
            None => break,
        };
        let item = &xml[item_start..item_end];
        pos = item_end;

        let title = extract_xml_tag(item, "title").unwrap_or_default();
        let link = extract_xml_tag(item, "link").unwrap_or_default();
        let desc = extract_xml_tag(item, "description").unwrap_or_default();

        if !link.is_empty() {
            results.push(SearchResult {
                title: decode_html_entities(&title),
                url: link,
                snippet: decode_html_entities(&strip_tags(&desc)),
                source: source.to_string(),
                language: language.map(|s| s.to_string()),
            });
        }
    }
    Ok(results)
}
```

### Example: Baidu Scrape Implementation

```rust
// crates/octos-agent/src/search/baidu.rs

pub struct BaiduSearchBackend {
    client: reqwest::Client,
}

impl BaiduSearchBackend {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait]
impl SearchBackend for BaiduSearchBackend {
    fn engine_id(&self) -> &str { "baidu" }
    fn is_available(&self) -> bool { true }

    async fn search(&self, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        let encoded = urlencoded(&options.query);
        let url = format!("https://www.baidu.com/s?wd={encoded}&rn={}", options.count.min(10));

        let resp = self.client.get(&url).send().await?;
        let html = resp.text().await?;

        // Parse Baidu HTML results
        // Baidu uses <div class="result c-container"> for each result
        // Title in <h3 class="t"><a href="...">Title</a></h3>
        // Snippet in <span class="content-right_8Zs40">...</span>
        // URLs are Baidu redirects — follow them or parse data-url attribute
        parse_baidu_html(&html, options.count as usize)
    }
}
```

### Example: Serper.dev Implementation (Recommended Primary)

Serper.dev is our **recommended primary search backend**. It provides Google results via a clean JSON API without needing to set up Google Custom Search Engine (which is closing to new customers by Jan 2027). At $50/month for 50K queries, it's the best cost/quality ratio.

```rust
// crates/octos-agent/src/search/serper.rs

pub struct SerperBackend {
    client: reqwest::Client,
    api_key: Option<String>,
    endpoint: String,  // "/search" or "/news"
}

impl SerperBackend {
    pub fn new(engine: &SearchEngine) -> Self {
        let endpoint = if engine.id == "serper_news" {
            "https://google.serper.dev/news"
        } else {
            "https://google.serper.dev/search"
        };
        Self {
            client: reqwest::Client::new(),
            api_key: std::env::var(&engine.key_env).ok(),
            endpoint: endpoint.to_string(),
        }
    }
}

#[async_trait]
impl SearchBackend for SerperBackend {
    fn engine_id(&self) -> &str { "serper" }
    fn is_available(&self) -> bool { self.api_key.is_some() }

    async fn search(&self, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        let api_key = self.api_key.as_ref()
            .ok_or_else(|| eyre::eyre!("SERPER_API_KEY not set"))?;

        let mut body = serde_json::json!({
            "q": options.query,
            "num": options.count.min(10),
        });
        if let Some(lang) = &options.language {
            body["hl"] = serde_json::Value::String(lang.clone());
        }
        if let Some(region) = &options.region {
            body["gl"] = serde_json::Value::String(region.clone());
        }

        let resp = self.client
            .post(&self.endpoint)
            .header("X-API-KEY", api_key)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!("Serper API error {status}: {body}");
        }

        let data: serde_json::Value = resp.json().await?;
        let items = data.get("organic").and_then(|v| v.as_array());

        Ok(items.map(|items| {
            items.iter().filter_map(|item| {
                Some(SearchResult {
                    title: item.get("title")?.as_str()?.to_string(),
                    url: item.get("link")?.as_str()?.to_string(),
                    snippet: item.get("snippet").and_then(|s| s.as_str())
                        .unwrap_or("").to_string(),
                    source: "serper".to_string(),
                    language: options.language.clone(),
                })
            }).collect()
        }).unwrap_or_default())
    }
}
```

### Example: Site Proxy Backend (X/Twitter, Reddit, LinkedIn)

Instead of paying $200+/mo for X API or dealing with Reddit's OAuth, we use Google's existing index via `site:` operator searches through Serper. This gives us full public history search for the cost of a regular Serper query.

**When to use proxy searches:**
- **X/Twitter** (`site:x.com`): Trending topics, expert opinions, breaking news reactions, public discourse analysis
- **Reddit** (`site:reddit.com`): Community sentiment, technical discussions, product comparisons, grassroots opinions
- **LinkedIn** (`site:linkedin.com`): Industry expert takes, company announcements, professional network signals

```rust
// crates/octos-agent/src/search/serper.rs (same file as SerperBackend)

/// Proxy backend that searches within a specific platform using site: operator.
/// Reuses Serper as the underlying search engine.
pub struct SiteProxyBackend {
    inner: SerperBackend,
    engine_id: String,
    query_template: String,  // e.g. "site:x.com {query}"
}

impl SiteProxyBackend {
    pub fn new(engine: &SearchEngine) -> Self {
        let query_template = engine.query_template.clone()
            .unwrap_or_else(|| format!("site:{} {{query}}", engine.id.replace("_proxy", ".com")));
        Self {
            inner: SerperBackend {
                client: reqwest::Client::new(),
                api_key: std::env::var(&engine.key_env).ok(),
                endpoint: "https://google.serper.dev/search".to_string(),
            },
            engine_id: engine.id.clone(),
            query_template,
        }
    }
}

#[async_trait]
impl SearchBackend for SiteProxyBackend {
    fn engine_id(&self) -> &str { &self.engine_id }
    fn is_available(&self) -> bool { self.inner.is_available() }

    async fn search(&self, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        // Rewrite query with site: operator
        let proxied_query = self.query_template.replace("{query}", &options.query);
        let proxied_options = SearchOptions {
            query: proxied_query,
            ..options.clone()
        };
        let mut results = self.inner.search(&proxied_options).await?;
        // Tag results with the proxy engine ID
        for r in &mut results {
            r.source = self.engine_id.clone();
        }
        Ok(results)
    }
}
```

### Example: Perplexity Sonar Backend

Perplexity provides AI-synthesized answers with source citations. Unlike traditional search engines that return links, Perplexity returns pre-digested analysis. Best used for **verification** and **gap-filling** rather than primary collection.

```rust
// crates/octos-agent/src/search/perplexity.rs

pub struct PerplexityBackend {
    client: reqwest::Client,
    api_key: Option<String>,
}

impl PerplexityBackend {
    pub fn new(engine: &SearchEngine) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: std::env::var(&engine.key_env).ok(),
        }
    }
}

#[async_trait]
impl SearchBackend for PerplexityBackend {
    fn engine_id(&self) -> &str { "perplexity" }
    fn is_available(&self) -> bool { self.api_key.is_some() }

    async fn search(&self, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        let api_key = self.api_key.as_ref()
            .ok_or_else(|| eyre::eyre!("PERPLEXITY_API_KEY not set"))?;

        // Perplexity uses chat completions format
        let body = serde_json::json!({
            "model": "sonar",
            "messages": [{"role": "user", "content": options.query}],
        });

        let resp = self.client
            .post("https://api.perplexity.ai/chat/completions")
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!("Perplexity API error {status}: {body}");
        }

        let data: serde_json::Value = resp.json().await?;

        // Extract citations from the response
        let citations = data.get("citations")
            .and_then(|c| c.as_array())
            .map(|urls| {
                urls.iter().filter_map(|u| u.as_str()).enumerate().map(|(i, url)| {
                    SearchResult {
                        title: format!("Perplexity citation {}", i + 1),
                        url: url.to_string(),
                        snippet: data.get("choices")
                            .and_then(|c| c.get(0))
                            .and_then(|c| c.get("message"))
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .chars().take(500).collect::<String>(),
                        source: "perplexity".to_string(),
                        language: None,
                    }
                }).collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(citations)
    }
}
```

### MultiSearcher: Query Multiple Engines in Parallel

```rust
// crates/octos-agent/src/search/mod.rs

pub struct MultiSearcher {
    backends: Vec<Box<dyn SearchBackend>>,
}

impl MultiSearcher {
    /// Create from a list of engine IDs. Only includes backends that are available.
    pub fn from_engine_ids(registry: &SourceRegistry, engine_ids: &[String]) -> Self {
        let backends: Vec<_> = engine_ids
            .iter()
            .filter_map(|id| registry.engine(id))
            .filter_map(|engine| create_backend(engine))
            .filter(|b| b.is_available())
            .collect();
        Self { backends }
    }

    /// Search all backends in parallel, merge and deduplicate results.
    pub async fn search_all(&self, options: &SearchOptions) -> Vec<SearchResult> {
        let futures: Vec<_> = self.backends
            .iter()
            .map(|b| b.search(options))
            .collect();

        let all_results = futures::future::join_all(futures).await;

        let mut seen_urls = std::collections::HashSet::new();
        let mut merged = Vec::new();

        for result in all_results.into_iter().flatten() {
            for item in result {
                let normalized = normalize_url(&item.url);
                if seen_urls.insert(normalized) {
                    merged.push(item);
                }
            }
        }

        merged
    }

    /// Search with multiple language variants in parallel.
    pub async fn search_multilingual(
        &self,
        query: &str,
        languages: &[String],
        count_per_lang: u8,
    ) -> Vec<SearchResult> {
        let futures: Vec<_> = languages
            .iter()
            .flat_map(|lang| {
                self.backends.iter().map(move |b| {
                    let options = SearchOptions {
                        query: query.to_string(),
                        count: count_per_lang,
                        language: Some(lang.clone()),
                        region: lang_to_region(lang),
                    };
                    b.search(&options)
                })
            })
            .collect();

        let all_results = futures::future::join_all(futures).await;
        // Merge and deduplicate same as above
        // ...
    }
}

/// Map language codes to typical region codes.
fn lang_to_region(lang: &str) -> Option<String> {
    match lang {
        "en" => Some("US".to_string()),
        "ar" => Some("AE".to_string()),
        "fa" => Some("IR".to_string()),
        "he" => Some("IL".to_string()),
        "zh" => Some("CN".to_string()),
        "ru" => Some("RU".to_string()),
        "uk" => Some("UA".to_string()),
        "ja" => Some("JP".to_string()),
        "ko" => Some("KR".to_string()),
        "es" => Some("ES".to_string()),
        "pt" => Some("BR".to_string()),
        "fr" => Some("FR".to_string()),
        "de" => Some("DE".to_string()),
        "it" => Some("IT".to_string()),
        _ => None,
    }
}
```

---

## 6. Component 3: Research Orchestrator

### Purpose

The single entry-point tool that the main agent calls. Plans the research, spawns sub-agents, collects results, runs synthesis.

### File Location

```
crates/octos-agent/src/tools/deep_research_v2.rs
```

### Tool Registration Name

`deep_research` — replaces the existing `DeepResearchTool` (which is retired).

### Input Schema

```json
{
  "type": "object",
  "properties": {
    "query": {
      "type": "string",
      "description": "The research question or topic to investigate thoroughly"
    },
    "depth": {
      "type": "string",
      "enum": ["standard", "thorough", "exhaustive"],
      "description": "Research depth: standard (5-8 agents, ~5min), thorough (8-15 agents, ~10min), exhaustive (15-20 agents, ~20min). Default: thorough"
    },
    "languages": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Override: specific languages to search in (ISO 639-1 codes). If omitted, auto-detected from query and source registry."
    },
    "background": {
      "type": "boolean",
      "description": "If true, run in background and notify when done. Default: true"
    }
  },
  "required": ["query"]
}
```

### Rust Struct

```rust
// crates/octos-agent/src/tools/deep_research_v2.rs

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use tracing::{info, warn};

use crate::agent::{Agent, AgentConfig, AgentId};
use crate::search::{MultiSearcher, SearchOptions, SearchResult};
use crate::source_registry::{Portal, SourcePlan, SourceRegistry};
use crate::tools::{Tool, ToolPolicy, ToolRegistry, ToolResult};
use octos_core::{Message, MessageRole, Task, TaskContext, TaskKind, TokenUsage};
use octos_llm::config::ChatConfig;
use octos_llm::provider::LlmProvider;
use octos_llm::types::ToolSpec;
use octos_memory::EpisodeStore;

/// Constants
const MAX_AGENTS: usize = 20;
const MAX_REFLECTION_CYCLES: u32 = 5;
const PAGES_PER_AGENT: usize = 30;
const AGENT_TIMEOUT_SECS: u64 = 600;
const AGENT_MAX_ITERATIONS: u32 = 30;

// Synthesis constants (same as synthesize_research.rs)
const BATCH_CHAR_LIMIT: usize = 80_000;
const TOTAL_CHAR_LIMIT: usize = 500_000;

pub struct DeepResearchOrchestrator {
    llm: Arc<dyn LlmProvider>,
    memory: Arc<EpisodeStore>,
    data_dir: PathBuf,
    registry: SourceRegistry,
    notify_tx: tokio::sync::mpsc::Sender<ResearchNotification>,
}

#[derive(Debug, Clone)]
pub struct ResearchNotification {
    pub question: String,
    pub report_path: PathBuf,
    pub success: bool,
    pub summary: String,
}

#[derive(Deserialize)]
struct Input {
    query: String,
    #[serde(default = "default_depth")]
    depth: ResearchDepth,
    #[serde(default)]
    languages: Option<Vec<String>>,
    #[serde(default = "default_true")]
    background: bool,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum ResearchDepth {
    Standard,   // 5-8 agents, ~5 min
    Thorough,   // 8-15 agents, ~10 min
    Exhaustive, // 15-20 agents, ~20 min
}

fn default_depth() -> ResearchDepth { ResearchDepth::Thorough }
fn default_true() -> bool { true }

impl ResearchDepth {
    fn max_agents(&self) -> usize {
        match self {
            Self::Standard => 8,
            Self::Thorough => 15,
            Self::Exhaustive => 20,
        }
    }

    fn reflection_cycles(&self) -> u32 {
        match self {
            Self::Standard => 3,
            Self::Thorough => 4,
            Self::Exhaustive => 5,
        }
    }
}
```

### Phase 1: Planning (LLM Call)

The orchestrator first queries the source registry for topic matches, then asks the LLM to generate a structured research plan.

```rust
impl DeepResearchOrchestrator {
    /// Generate a research plan by combining registry data with LLM planning.
    async fn plan_research(
        &self,
        query: &str,
        depth: ResearchDepth,
        language_override: Option<&[String]>,
    ) -> Result<ResearchPlan> {
        // Step 1: Get source plan from registry
        let source_plan = self.registry.plan_sources(query);

        let languages = language_override
            .map(|l| l.to_vec())
            .unwrap_or(source_plan.languages.clone());

        // Step 2: Build context for the LLM planner
        let portal_list: String = source_plan.portals
            .iter()
            .map(|p| format!("- {} ({}) [{}]", p.name, p.url, p.lang))
            .collect::<Vec<_>>()
            .join("\n");

        let engine_list = source_plan.engines.join(", ");
        let lang_list = languages.join(", ");
        let topic_list = source_plan.matched_topics.join(", ");

        let prompt = format!(
            include_str!("../prompts/research_planner.txt"),
            query = query,
            max_agents = depth.max_agents(),
            topics = topic_list,
            engines = engine_list,
            languages = lang_list,
            portals = portal_list,
            think_tanks = source_plan.think_tanks.join(", "),
            data_sources = source_plan.data_sources.join(", "),
        );

        let messages = vec![
            Message {
                role: MessageRole::System,
                content: "You are a research planning strategist. Output ONLY valid JSON.".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::User,
                content: prompt,
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        let config = ChatConfig {
            max_tokens: Some(4096),
            temperature: Some(0.3),
            ..Default::default()
        };

        let response = self.llm.chat(&messages, &[], &config).await?;
        let plan: ResearchPlan = serde_json::from_str(
            response.content.as_deref().unwrap_or("{}"),
        ).wrap_err("failed to parse research plan JSON")?;

        // Clamp to max agents
        let mut plan = plan;
        plan.angles.truncate(depth.max_agents());

        Ok(plan)
    }
}
```

### Phase 2: Collection (Parallel Sub-Agents)

```rust
impl DeepResearchOrchestrator {
    async fn execute_collection(
        &self,
        plan: &ResearchPlan,
        research_dir: &Path,
        depth: ResearchDepth,
    ) -> Result<(Vec<PartialResult>, TokenUsage)> {
        let mut handles = Vec::new();
        let mut total_tokens = TokenUsage::default();

        for (i, angle) in plan.angles.iter().enumerate() {
            let llm = self.llm.clone();
            let memory = self.memory.clone();
            let angle = angle.clone();
            let res_dir = research_dir.to_path_buf();
            let partial_path = res_dir.join(format!("partial_{:02}.md", i + 1));
            let max_cycles = depth.reflection_cycles();

            let handle = tokio::spawn(async move {
                let result = Self::run_sub_agent(
                    llm,
                    memory,
                    &angle,
                    &res_dir,
                    &partial_path,
                    max_cycles,
                    i,
                ).await;

                (i, angle.title.clone(), result, partial_path)
            });

            handles.push(handle);
        }

        info!(
            agents = handles.len(),
            research_dir = %research_dir.display(),
            "all sub-agents spawned, waiting for completion"
        );

        let results = futures::future::join_all(handles).await;

        let mut partials = Vec::new();
        for join_result in results {
            let (i, title, result, partial_path) = match join_result {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "sub-agent join error");
                    continue;
                }
            };

            match result {
                Ok((content, usage)) => {
                    total_tokens.input_tokens += usage.input_tokens;
                    total_tokens.output_tokens += usage.output_tokens;

                    // Read from disk if written, otherwise use returned content
                    let final_content = if partial_path.exists() {
                        tokio::fs::read_to_string(&partial_path)
                            .await
                            .unwrap_or(content)
                    } else {
                        content
                    };

                    if !final_content.is_empty() {
                        partials.push(PartialResult {
                            agent_index: i,
                            angle_title: title,
                            content: final_content,
                        });
                    }
                }
                Err(e) => {
                    warn!(agent = i, title = %title, error = %e, "sub-agent failed");
                }
            }
        }

        Ok((partials, total_tokens))
    }

    /// Run a single sub-agent with its own Agent instance.
    async fn run_sub_agent(
        llm: Arc<dyn LlmProvider>,
        memory: Arc<EpisodeStore>,
        angle: &ResearchAngle,
        research_dir: &Path,
        partial_path: &Path,
        max_cycles: u32,
        agent_index: usize,
    ) -> Result<(String, TokenUsage)> {
        // Build tool registry for this sub-agent
        let mut tools = ToolRegistry::new();
        tools.register(crate::tools::WebSearchTool::new());
        tools.register(crate::tools::WebFetchTool::new());
        tools.register(crate::tools::ReadFileTool::new(research_dir));
        tools.register(crate::tools::WriteFileTool::new(research_dir));
        tools.register(crate::tools::GlobTool::new(research_dir));
        tools.register(crate::tools::GrepTool::new(research_dir));

        // If browser feature enabled:
        #[cfg(feature = "browser")]
        tools.register(crate::tools::BrowserTool::new());

        // Block dangerous tools
        let policy = ToolPolicy {
            deny: vec![
                "shell".into(),
                "spawn".into(),
                "edit_file".into(),
                "deep_research".into(),
            ],
            ..Default::default()
        };
        tools.apply_policy(&policy);

        // Build system prompt with angle-specific instructions
        let system_prompt = format!(
            include_str!("../prompts/research_collector.txt"),
            title = angle.title,
            description = angle.description,
            search_queries = angle.suggested_queries.join("\n  - "),
            portals = angle.portals.iter()
                .map(|p| format!("{} ({})", p.name, p.url))
                .collect::<Vec<_>>()
                .join("\n  - "),
            languages = angle.languages.join(", "),
            max_cycles = max_cycles,
            partial_path = partial_path.display(),
        );

        let agent_id = AgentId::new(format!("researcher-{}", agent_index));
        let agent = Agent::new(agent_id, llm, tools, memory)
            .with_config(AgentConfig {
                max_iterations: AGENT_MAX_ITERATIONS,
                max_timeout: Some(Duration::from_secs(AGENT_TIMEOUT_SECS)),
                save_episodes: false,
                ..Default::default()
            })
            .with_system_prompt(system_prompt);

        let task = Task::new(
            TaskKind::Code {
                instruction: format!(
                    "Research this angle thoroughly and write your findings.\n\n\
                     Angle: {}\n\n\
                     Description: {}\n\n\
                     Save your findings to: {}\n\n\
                     Include specific data, numbers, dates, quotes with source URLs.",
                    angle.title,
                    angle.description,
                    partial_path.file_name().unwrap().to_string_lossy(),
                ),
                files: vec![],
            },
            TaskContext {
                working_dir: research_dir.to_path_buf(),
                ..Default::default()
            },
        );

        let result = agent.run_task(&task).await?;
        Ok((result.content, result.token_usage))
    }
}
```

### Phase 3: Synthesis

Reuses the map-reduce pattern from `synthesize_research.rs` but adds a self-critique pass.

```rust
impl DeepResearchOrchestrator {
    async fn synthesize(
        &self,
        query: &str,
        partials: &[PartialResult],
        research_dir: &Path,
    ) -> Result<(String, TokenUsage)> {
        let mut total_tokens = TokenUsage::default();

        // Collect all partial content
        let contents: Vec<(String, String)> = partials
            .iter()
            .map(|p| (p.angle_title.clone(), p.content.clone()))
            .collect();

        let total_chars: usize = contents.iter().map(|(_, c)| c.len()).sum();
        info!(
            partials = contents.len(),
            total_chars,
            "starting synthesis phase"
        );

        // --- Map phase (if needed) ---
        let findings = if total_chars <= BATCH_CHAR_LIMIT {
            // Single batch — extract directly
            let (findings, usage) = self.extract_findings(query, &contents).await?;
            total_tokens.input_tokens += usage.input_tokens;
            total_tokens.output_tokens += usage.output_tokens;
            findings
        } else {
            // Partition into batches
            let batches = partition_batches(&contents);
            info!(batches = batches.len(), "map phase: processing batches");

            let mut batch_findings = Vec::new();
            for (batch_idx, batch) in batches.iter().enumerate() {
                let batch_contents: Vec<(String, String)> = batch
                    .iter()
                    .map(|&i| contents[i].clone())
                    .collect();

                match self.extract_findings_batch(
                    query,
                    &batch_contents,
                    batch_idx + 1,
                    batches.len(),
                ).await {
                    Ok((findings, usage)) => {
                        total_tokens.input_tokens += usage.input_tokens;
                        total_tokens.output_tokens += usage.output_tokens;
                        batch_findings.push(findings);
                    }
                    Err(e) => {
                        warn!(batch = batch_idx, error = %e, "batch extraction failed");
                    }
                }
            }

            if batch_findings.is_empty() {
                eyre::bail!("all batch extractions failed");
            }

            // Reduce: merge batch findings
            let (merged, usage) = self.merge_findings(query, &batch_findings).await?;
            total_tokens.input_tokens += usage.input_tokens;
            total_tokens.output_tokens += usage.output_tokens;
            merged
        };

        // --- Self-critique pass ---
        let (final_report, critique_usage) = self.self_critique(query, &findings).await?;
        total_tokens.input_tokens += critique_usage.input_tokens;
        total_tokens.output_tokens += critique_usage.output_tokens;

        // Save final report
        let report_path = research_dir.join("report.md");
        tokio::fs::write(&report_path, &final_report).await?;
        info!(
            report_path = %report_path.display(),
            report_chars = final_report.len(),
            "final report saved"
        );

        Ok((final_report, total_tokens))
    }

    /// Self-critique: review the draft and improve it.
    async fn self_critique(
        &self,
        query: &str,
        draft: &str,
    ) -> Result<(String, TokenUsage)> {
        let prompt = format!(
            include_str!("../prompts/research_critic.txt"),
            query = query,
            draft = draft,
        );

        let messages = vec![
            Message {
                role: MessageRole::System,
                content: "You are a senior research editor. Review and improve this research report.".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::User,
                content: prompt,
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        let config = ChatConfig {
            max_tokens: Some(16384), // Longer output for comprehensive report
            temperature: Some(0.0),
            ..Default::default()
        };

        let response = self.llm.chat(&messages, &[], &config).await?;
        let content = response.content.unwrap_or_default();
        Ok((content, response.usage))
    }
}
```

### Tool Trait Implementation

```rust
#[async_trait]
impl Tool for DeepResearchOrchestrator {
    fn name(&self) -> &str { "deep_research" }

    fn description(&self) -> &str {
        "Perform comprehensive multi-source research on a topic. Searches across multiple \
         engines (Google, Bing, Baidu, etc.), languages, and specialized portals. \
         Spawns 5-20 parallel research agents, each with reflection loops for gap detection. \
         Produces a detailed, citation-rich report (10K+ words). Takes 5-20 minutes."
    }

    fn tags(&self) -> &[&str] { &["web"] }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The research question or topic to investigate thoroughly"
                },
                "depth": {
                    "type": "string",
                    "enum": ["standard", "thorough", "exhaustive"],
                    "description": "Research depth. standard: 5-8 agents ~5min, thorough: 8-15 agents ~10min, exhaustive: 15-20 agents ~20min. Default: thorough"
                },
                "languages": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Override languages to search in (ISO 639-1). Auto-detected if omitted."
                },
                "background": {
                    "type": "boolean",
                    "description": "Run in background and notify when done. Default: true"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input = serde_json::from_value(args.clone())?;
        let slug = slugify(&input.query);
        let research_dir = self.data_dir.join("research").join(&slug);
        tokio::fs::create_dir_all(&research_dir).await?;

        let report_path = research_dir.join("report.md");

        if input.background {
            // Clone what we need for the background task
            let llm = self.llm.clone();
            let memory = self.memory.clone();
            let data_dir = self.data_dir.clone();
            let registry = SourceRegistry::load(); // Re-load for the background task
            let notify_tx = self.notify_tx.clone();
            let query = input.query.clone();
            let depth = input.depth;
            let languages = input.languages.clone();
            let rp = report_path.clone();
            let rd = research_dir.clone();

            tokio::spawn(async move {
                let tool = DeepResearchOrchestrator {
                    llm, memory, data_dir, registry, notify_tx: notify_tx.clone(),
                };

                let result = tool.execute_full(
                    &query, depth, languages.as_deref(), &rd,
                ).await;

                let (success, summary) = match result {
                    Ok((report, tokens)) => {
                        (true, format!(
                            "Research complete. Report: {} ({} chars, {} input + {} output tokens)",
                            rp.display(), report.len(),
                            tokens.input_tokens, tokens.output_tokens,
                        ))
                    }
                    Err(e) => (false, format!("Research failed: {e}")),
                };

                let _ = notify_tx.send(ResearchNotification {
                    question: query,
                    report_path: rp,
                    success,
                    summary,
                }).await;
            });

            return Ok(ToolResult {
                output: format!(
                    "Deep Research started in background.\n\
                     Research directory: {}\n\
                     Report will be saved to: {}\n\
                     You'll be notified when it's ready. Continue chatting in the meantime.",
                    research_dir.display(),
                    report_path.display(),
                ),
                success: true,
                ..Default::default()
            });
        }

        // Synchronous mode
        let (report, tokens) = self.execute_full(
            &input.query,
            input.depth,
            input.languages.as_deref(),
            &research_dir,
        ).await?;

        Ok(ToolResult {
            output: report,
            success: true,
            tokens_used: Some(tokens),
            ..Default::default()
        })
    }
}

impl DeepResearchOrchestrator {
    async fn execute_full(
        &self,
        query: &str,
        depth: ResearchDepth,
        languages: Option<&[String]>,
        research_dir: &Path,
    ) -> Result<(String, TokenUsage)> {
        let mut total_tokens = TokenUsage::default();

        // Phase 1: Plan
        info!(query, ?depth, "phase 1: generating research plan");
        let plan = self.plan_research(query, depth, languages).await?;
        info!(
            angles = plan.angles.len(),
            "research plan generated"
        );

        // Save plan for debugging
        let plan_json = serde_json::to_string_pretty(&plan)?;
        tokio::fs::write(research_dir.join("_plan.json"), &plan_json).await?;

        // Phase 2: Collect
        info!("phase 2: spawning {} sub-agents", plan.angles.len());
        let (partials, collection_tokens) = self
            .execute_collection(&plan, research_dir, depth)
            .await?;
        total_tokens.input_tokens += collection_tokens.input_tokens;
        total_tokens.output_tokens += collection_tokens.output_tokens;
        info!(
            successful_agents = partials.len(),
            total_agents = plan.angles.len(),
            "collection phase complete"
        );

        if partials.is_empty() {
            eyre::bail!("all sub-agents failed, no data collected");
        }

        // Phase 3: Synthesize
        info!("phase 3: synthesizing {} partial reports", partials.len());
        let (report, synthesis_tokens) = self
            .synthesize(query, &partials, research_dir)
            .await?;
        total_tokens.input_tokens += synthesis_tokens.input_tokens;
        total_tokens.output_tokens += synthesis_tokens.output_tokens;

        Ok((report, total_tokens))
    }
}
```

---

## 7. Component 4: Collection Sub-Agents

### What Each Sub-Agent Does

Each sub-agent is a full `Agent` instance with its own tool registry and system prompt. It runs an iterative search-reflect loop:

```
┌─────────────────────────────────────────────────┐
│  Sub-Agent Lifecycle (max N cycles)              │
│                                                  │
│  1. Generate search queries from angle spec      │
│     - Use suggested queries from plan            │
│     - Add language-specific variants             │
│     - Add site:-scoped queries for portals       │
│                                                  │
│  2. Execute searches via web_search tool         │
│     - Multiple queries in sequence               │
│     - Collect URLs                               │
│                                                  │
│  3. Fetch full pages via web_fetch tool          │
│     - Browse assigned portals directly            │
│     - Fetch search result URLs                   │
│                                                  │
│  4. REFLECT (built into system prompt):          │
│     "Review what you've found so far.             │
│      Score relevance of each source (1-10).       │
│      Identify gaps: what's still missing?         │
│      If gaps remain and cycles left, search       │
│      again with targeted queries."                │
│                                                  │
│  5. Quality filter:                              │
│     - Discard sources scored < 5                 │
│     - Keep only the most relevant 30%            │
│                                                  │
│  6. Write findings to partial_NN.md              │
│     - Structured with ## headers                 │
│     - Include specific data, quotes, numbers     │
│     - Cite every claim with [source](url)        │
└─────────────────────────────────────────────────┘
```

### Sub-Agent Available Tools

| Tool | Purpose |
|---|---|
| `web_search` | Search via configured backends (DuckDuckGo/Brave/etc.) |
| `web_fetch` | Fetch and read a full web page |
| `browser` | Interactive browsing (click, scroll, screenshot) for JS-heavy sites |
| `read_file` | Read saved research files |
| `write_file` | Write findings to partial file |
| `glob` | List files in research directory |
| `grep` | Search within saved files |

### Blocked Tools

`shell`, `spawn`, `edit_file`, `deep_research` (no recursion).

---

## 8. Component 5: Synthesis Agent

### What Makes It Different from Current synthesize_research

| Feature | Current (synthesize_research) | New (Deep Research synthesis) |
|---|---|---|
| Context | Inherits search history | **Fresh context** (zero history) |
| Input | Raw source files from disk | **Curated partial reports** from sub-agents |
| Quality of input | All content, unfiltered | **Pre-filtered** by sub-agents (top 30%) |
| Processing | Map-reduce only | Map-reduce + **self-critique** pass |
| Output length | max_tokens=8192 | max_tokens=**16384** |
| Citation validation | None | **Explicit citation check** in critique pass |
| Cross-source analysis | None | **Contradiction detection**, consensus finding |

### Self-Critique Pass

After the initial synthesis, the critic prompt asks:

1. **Completeness**: Are there obvious gaps? Any angle not well-covered?
2. **Specificity**: Are claims backed by specific data (numbers, dates, quotes)?
3. **Citation quality**: Does every major claim have a source URL?
4. **Structure**: Is the report well-organized with clear sections?
5. **Contradictions**: Are conflicting sources properly noted?

The critic either returns the improved report directly, or flags critical gaps that would need additional searching (future: can trigger targeted gap-fill agents).

---

## 9. Component 6: Report Output

### Canonical Format: Markdown

```markdown
# [Research Title]

> Research completed: 2026-03-01 | Sources: 47 pages across 12 sub-agents | Duration: 8m 32s

## Executive Summary

[2-3 paragraph summary of key findings]

## [Section 1: Major Theme]

### [Subsection 1.1]

[Detailed findings with specific data points]

According to [Source Name](https://url), the market grew 23.4% in Q3 2025...

### [Subsection 1.2]

| Comparison | Team A | Team B | Team C |
|---|---|---|---|
| Win Rate | 78% | 65% | 71% |
| Goals/Game | 2.4 | 1.8 | 2.1 |

## [Section 2: Another Theme]

...

## Contradictions & Open Questions

- Source A claims X, while Source B argues Y. The discrepancy may be due to...
- No consensus on Z — requires further investigation.

## Sources

1. [Source Title](https://url) — used in sections 1.1, 2.3
2. [Source Title](https://url) — used in sections 1.2, 3.1
...

---

_Generated by Deep Research | {N} sub-agents | {M} sources analyzed | {T} tokens used_
```

### Future Conversions (Not Part of Pipeline)

The report is a standalone markdown artifact. Conversion tools are separate:

- **PPTX**: Existing `make-slide` skill can be invoked separately
- **DOCX**: Future `make-doc` skill using `docx-rs` crate
- **HTML/Website**: Future `make-site` skill using static site generator
- **Infographic**: Future skill using SVG generation

---

## 10. Data Structures & Type Definitions

```rust
/// The research plan generated by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchPlan {
    /// The original query.
    pub query: String,
    /// Research angles to investigate in parallel.
    pub angles: Vec<ResearchAngle>,
}

/// A single research angle assigned to one sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchAngle {
    /// Short title (e.g., "Spanish sports press").
    pub title: String,
    /// Detailed description of what to investigate.
    pub description: String,
    /// Suggested initial search queries.
    pub suggested_queries: Vec<String>,
    /// Languages to search in.
    pub languages: Vec<String>,
    /// Specific portals to browse directly.
    pub portals: Vec<Portal>,
    /// Search engine IDs to use.
    pub search_engines: Vec<String>,
    /// Priority: 1=critical, 2=important, 3=supplementary.
    pub priority: u8,
}

/// Result from one sub-agent.
#[derive(Debug)]
pub struct PartialResult {
    pub agent_index: usize,
    pub angle_title: String,
    pub content: String,
}
```

---

## 11. Prompt Templates

All prompts live in `crates/octos-agent/src/prompts/`. They use `{placeholder}` syntax filled by `format!()`.

### `research_planner.txt`

```
You are planning a comprehensive research investigation.

Query: {query}
Maximum research agents: {max_agents}

Available information:
- Detected topics: {topics}
- Available search engines: {engines}
- Languages to cover: {languages}
- Known portals:
{portals}
- Think tanks: {think_tanks}
- Data sources: {data_sources}

Generate a research plan as a JSON object with this EXACT structure:
{{
  "query": "<the original query>",
  "angles": [
    {{
      "title": "<short descriptive title, 3-8 words>",
      "description": "<what this agent should investigate, 1-2 sentences>",
      "suggested_queries": ["<search query 1>", "<search query 2>", ...],
      "languages": ["en", "es", ...],
      "portals": [
        {{"name": "<portal name>", "url": "<portal url>", "lang": "<lang>"}}
      ],
      "search_engines": ["google", "google_news", ...],
      "priority": 1
    }}
  ]
}}

Rules:
1. Create {max_agents} research angles maximum. Use fewer if the query is simple.
2. Each angle should be INDEPENDENT — no overlap between agents.
3. Distribute languages across angles: one agent per major language/region perspective.
4. Include at least one angle for each detected topic ({topics}).
5. Assign specific portals from the list above to relevant angles.
6. Include angle-specific search queries (3-5 per angle), not generic queries.
   Good: "FIFA 2026 World Cup group stage predictions site:marca.com"
   Bad: "world cup 2026"
7. Include site:-scoped queries for specific portals.
8. For current events, include time-qualified queries (e.g., "topic 2026 latest").
9. Vary query formulations: factual, analytical, opinion, statistical.
10. Priority 1 = core angles, 2 = important supplementary, 3 = nice-to-have.

Output ONLY the JSON object. No explanation, no markdown fences.
```

### `research_collector.txt`

```
You are Research Agent #{agent_index}: "{title}"

Your assignment: {description}

## Instructions

Search thoroughly from YOUR specific angle. You have up to {max_cycles} search-reflect cycles.

### Assigned Resources
- Languages: {languages}
- Suggested search queries:
  - {search_queries}
- Portals to browse directly:
  - {portals}

### Workflow (repeat up to {max_cycles} times)

**SEARCH**: Execute 3-5 searches using web_search. Use your suggested queries first,
then generate follow-up queries based on what you find. Include site:-scoped queries
for your assigned portals. Search in ALL your assigned languages.

**BROWSE**: For each assigned portal, use web_fetch to read the homepage or relevant
section directly. Don't rely only on search results — browse the portals.

**REFLECT**: After each search round, explicitly think:
- What specific data points have I found? (numbers, dates, percentages, quotes)
- What's still MISSING that I need for a thorough report?
- Are there contradictions between sources?
- Have I covered all my assigned languages?
If gaps remain and you have cycles left, search again with targeted queries.

**QUALITY FILTER**: Not everything you find is useful. For each source:
- Score relevance 1-10 (10 = directly answers the research question with specific data)
- DISCARD sources scoring below 5
- Keep only the most relevant ~30% of what you find

### Output Format

Write your findings to: {partial_path}

Structure:
```markdown
# [Your Angle Title]

## Key Findings

[Most important discoveries with specific data]

## Detailed Analysis

### [Sub-topic 1]
[Detailed findings with citations]

According to [Source Name](url), ...
The data shows 23.4% growth ([Source](url))...

### [Sub-topic 2]
...

## Contradictions / Uncertainties

[Where sources disagree]

## Sources Used

1. [Title](url) - relevance: 9/10 - [language]
2. [Title](url) - relevance: 8/10 - [language]
...
```

Rules:
- EVERY claim must have a [source](url) citation
- Include SPECIFIC numbers, percentages, dates, and direct quotes
- Do NOT summarize vaguely — be precise and data-rich
- Write in the SAME LANGUAGE as the original query
- Include data from ALL your assigned languages (translate key findings if needed)
- ALWAYS save your findings to the file — do not just return text
```

### `research_critic.txt`

```
Review this research report and improve it.

Original question: {query}

Draft report:
{draft}

Evaluate the draft on these criteria and produce an IMPROVED version:

1. **Completeness**: Are there obvious gaps? Any promised section that's thin?
   - If a section has only 1-2 sentences, it needs more substance.

2. **Specificity**: Replace any vague statements with specific data.
   - BAD: "The market grew significantly"
   - GOOD: "The market grew 23.4% YoY to $47.2B in Q3 2025 (Source)"

3. **Citations**: Every major claim MUST have a [source](url).
   - Count the citations. A good report has 20+ sources.
   - If claims lack citations, note "[citation needed]".

4. **Structure**: Reorganize if needed.
   - Clear ## and ### headers
   - Markdown tables for any comparisons
   - Executive summary at the top

5. **Cross-source analysis**: Note where sources agree/disagree.
   - Add a "Contradictions & Open Questions" section if missing.

6. **Language**: Match the language of the original query.

Output the COMPLETE improved report (not just the changes).
Include a metadata line at the end:
_Reviewed and improved. Original: N words, Improved: M words. Citations: K sources._
```

---

## 12. Integration with Existing Codebase

### Files to Create

| File | Purpose |
|---|---|
| `crates/octos-agent/src/source_registry.rs` | Source registry structs + lookup logic |
| `crates/octos-agent/data/source_registry.toml` | Source database (embedded via `include_str!`) |
| `crates/octos-agent/src/search/mod.rs` | SearchBackend trait + factory + MultiSearcher |
| `crates/octos-agent/src/search/serper.rs` | Serper.dev backend + SiteProxyBackend (X, Reddit, LinkedIn) |
| `crates/octos-agent/src/search/google.rs` | Google CSE + Google News RSS backends (legacy) |
| `crates/octos-agent/src/search/bing.rs` | Bing Web + News backends (legacy) |
| `crates/octos-agent/src/search/baidu.rs` | Baidu scrape backend |
| `crates/octos-agent/src/search/perplexity.rs` | Perplexity Sonar AI meta-search backend |
| `crates/octos-agent/src/search/rss.rs` | Generic RSS parser |
| `crates/octos-agent/src/tools/deep_research_v2.rs` | Orchestrator tool |
| `crates/octos-agent/src/prompts/research_planner.txt` | Planning prompt |
| `crates/octos-agent/src/prompts/research_collector.txt` | Sub-agent prompt |
| `crates/octos-agent/src/prompts/research_critic.txt` | Self-critique prompt |

### Files to Modify

| File | Change |
|---|---|
| `crates/octos-agent/src/lib.rs` | Add `pub mod source_registry;` and `pub mod search;`, re-export `DeepResearchOrchestrator` |
| `crates/octos-agent/src/tools/mod.rs` | Add `pub mod deep_research_v2;` and `pub use deep_research_v2::DeepResearchOrchestrator;` |
| `crates/octos-agent/Cargo.toml` | Add `toml` dependency (for registry parsing) |
| `crates/octos-cli/src/commands/gateway.rs` | Register `DeepResearchOrchestrator` tool, update system prompt |
| `crates/octos-cli/src/commands/chat.rs` | Register `DeepResearchOrchestrator` tool |

### Dependencies to Add

```toml
# crates/octos-agent/Cargo.toml
[dependencies]
toml = "0.8"   # For source_registry.toml parsing
```

No other new dependencies — all HTTP, async, serde, etc. are already in the workspace.

### Existing Code to Reuse

| Pattern | Source File | What to Reuse |
|---|---|---|
| Agent creation for sub-agents | `deep_research.rs:228-284` | `Agent::new().with_config().with_system_prompt()` |
| Parallel tokio::spawn + join_all | `deep_research.rs:228-284` | Spawn handles, `join_all`, filter `JoinError` |
| Map-reduce synthesis | `synthesize_research.rs` | Batch partitioning, extract/merge prompts |
| LLM direct calls | `synthesize_research.rs:145-262` | `ChatConfig`, `Message` construction, `llm.chat()` |
| Page fetching | `deep_search.rs`, `web_fetch.rs` | SSRF check, `htmd::convert()`, truncation |
| File saving | `deep_search.rs` | `---\nurl: {}\n---` header format |
| Task construction | `deep_research.rs` | `Task::new(TaskKind::Code { ... })` |
| Token accumulation | `synthesize_research.rs` | `TokenUsage::default()` then `+=` |
| Background mode | `deep_research.rs:552-646` | `tokio::spawn` + notification channel |
| Slug generation | `deep_search.rs` | `slugify()` function |
| Policy application | `deep_research.rs:build_research_tools()` | `ToolPolicy { deny: [...] }` |

---

## 13. File Layout

### Research Directory Structure (on disk)

```
~/.octos/research/
    2026-world-cup-predictions/
        _plan.json               # Research plan (JSON, for debugging)
        partial_01.md            # Sub-agent 1 findings: "English sports press"
        partial_02.md            # Sub-agent 2 findings: "Spanish press"
        partial_03.md            # Sub-agent 3 findings: "Portuguese press"
        partial_04.md            # Sub-agent 4 findings: "Statistics"
        partial_05.md            # Sub-agent 5 findings: "Prediction markets"
        partial_06.md            # Sub-agent 6 findings: "Historical analysis"
        01_espn-com.md           # Full page content (from sub-agents' web_fetch)
        02_marca-com.md
        03_fbref-com.md
        ...
        _search_results.md       # Aggregated search results (if deep_search used)
        report.md                # FINAL synthesized report
```

### Source Code Layout

```
crates/octos-agent/
    src/
        source_registry.rs       # Registry structs + lookup
        search/
            mod.rs               # SearchBackend trait, factory, MultiSearcher
            serper.rs            # Serper.dev + SiteProxyBackend (X, Reddit, LinkedIn)
            google.rs            # Legacy (CSE closing Jan 2027)
            bing.rs              # Legacy (retired Aug 2025)
            baidu.rs
            perplexity.rs        # AI meta-search (verification & gap-filling)
            rss.rs
        tools/
            deep_research_v2.rs  # Orchestrator (new)
            deep_search.rs       # Existing (v1, kept as-is)
            synthesize_research.rs  # Existing (kept, used by v1)
        prompts/
            research_planner.txt
            research_collector.txt
            research_critic.txt
            worker.txt           # Existing
    data/
        source_registry.toml
```

---

## 14. Configuration

### Environment Variables (API Keys)

| Variable | Required | Engine | Cost / Free Tier |
|---|---|---|---|
| `SERPER_API_KEY` | **Recommended** | Serper.dev (Google proxy) + X/Reddit/LinkedIn proxy | $50/mo for 50K queries |
| `PERPLEXITY_API_KEY` | Optional | Perplexity Sonar (AI meta-search) | Pay-per-use |
| `GOOGLE_CSE_API_KEY` | Legacy | Google Custom Search | 100/day free; **CSE closing to new customers Jan 2027** |
| `GOOGLE_CSE_ID` | Legacy | Google Custom Search | — |
| `BING_API_KEY` | Legacy | Bing Web + News | **Retired Aug 2025**; replaced by "Grounding with Bing" (Azure-only, $35/1K) |
| `BRAVE_API_KEY` | Optional | Brave Search | 2K/month free |
| `YANDEX_API_KEY` | Optional | Yandex | 1K/day |
| `YANDEX_USER` | Optional | Yandex | — |
| `NAVER_CLIENT_ID` | Optional | Naver | 25K/day |
| `NAVER_CLIENT_SECRET` | Optional | Naver | — |

**Recommended setup**: `SERPER_API_KEY` alone gets you Google web search, Google news, AND social media proxy searches (X/Twitter, Reddit, LinkedIn) — all for $50/month. The system falls back to DuckDuckGo if no keys are set, but quality improves dramatically with Serper.

**Why Serper over Google CSE**: Google Custom Search Engine is closing to new customers (must migrate by Jan 2027). Serper provides the same Google results via a simpler JSON API with no CSE setup required.

**Social media proxy strategy**: Instead of paying $200/mo for X Basic API (7-day search only) or $5K/mo for X Pro (full archive), we use `site:x.com` queries via Serper. This searches all public tweets indexed by Google at regular Serper query cost. Same approach for Reddit and LinkedIn.

### Gateway Config (config.json)

```json
{
  "tools": {
    "deep_research": {
      "default_depth": "thorough",
      "max_agents": 15,
      "max_reflection_cycles": 5,
      "agent_timeout_secs": 600,
      "background": true
    }
  }
}
```

---

## 15. Error Handling & Edge Cases

| Scenario | Handling |
|---|---|
| All sub-agents fail | Return error: "all sub-agents failed, no data collected" |
| Some sub-agents fail | Continue with successful ones, warn in logs |
| Sub-agent timeout (600s) | `tokio::time::timeout` kills it, others continue |
| No API keys for any engine | Falls back to DuckDuckGo scraping |
| No topics matched in registry | Uses default engines (google, duckduckgo) + English |
| LLM fails to generate valid plan JSON | Retry once with stricter prompt; fall back to simple 3-angle plan |
| Synthesis batch extraction fails | Skip failed batch, continue with others |
| Research directory already exists | Reuse it (idempotent) |
| Total source chars > 500K | Truncate to TOTAL_CHAR_LIMIT, process prefix |
| Portal returns 403/paywall | Log warning, skip portal, note "paywall" in findings |
| SSRF check fails | Skip URL silently (same as existing pattern) |
| Background task panics | Notification sent with `success: false` |
| Rate limit on search engine | Retry with backoff (reuse RetryProvider pattern) |

---

## 16. Testing Strategy

### Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_registry_load() {
        let registry = SourceRegistry::load();
        assert!(!registry.search_engines.is_empty());
        assert!(!registry.topic_sources.is_empty());
    }

    #[test]
    fn test_topic_matching_football() {
        let registry = SourceRegistry::load();
        let topics = registry.match_topics("Who will win the 2026 World Cup?");
        assert!(topics.iter().any(|t| t.topic == "football"));
        assert!(topics.iter().any(|t| t.topic == "prediction"));
    }

    #[test]
    fn test_topic_matching_iran() {
        let registry = SourceRegistry::load();
        let topics = registry.match_topics("Iran succession after Khamenei");
        assert!(topics.iter().any(|t| t.topic == "middle_east"));
    }

    #[test]
    fn test_topic_matching_no_match() {
        let registry = SourceRegistry::load();
        let topics = registry.match_topics("random gibberish xyzzy");
        // Should still return empty or generic
        // plan_sources() adds defaults
    }

    #[test]
    fn test_plan_sources_football() {
        let registry = SourceRegistry::load();
        let plan = registry.plan_sources("World Cup 2026 predictions");
        assert!(plan.languages.contains(&"es".to_string()));
        assert!(plan.languages.contains(&"pt".to_string()));
        assert!(plan.portals.iter().any(|p| p.name.contains("Marca")));
    }

    #[test]
    fn test_plan_sources_defaults() {
        let registry = SourceRegistry::load();
        let plan = registry.plan_sources("random topic");
        // Should have at least default engines
        assert!(!plan.engines.is_empty());
        assert!(plan.languages.contains(&"en".to_string()));
    }

    #[test]
    fn test_available_engines() {
        let registry = SourceRegistry::load();
        let available = registry.available_engines();
        // DuckDuckGo and Baidu should always be available (no key needed)
        assert!(available.iter().any(|e| e.id == "duckduckgo"));
        assert!(available.iter().any(|e| e.id == "baidu"));
    }

    // Google News RSS parser tests
    #[test]
    fn test_parse_rss_items() {
        let xml = r#"
        <rss><channel>
        <item>
            <title>Test Article</title>
            <link>https://example.com/article</link>
            <description>This is a test article about football.</description>
        </item>
        </channel></rss>"#;

        let results = parse_rss_items(xml, 10, "test", Some("en")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Test Article");
        assert_eq!(results[0].url, "https://example.com/article");
    }

    // Batch partitioning tests (reuse from synthesize_research)
    #[test]
    fn test_partition_batches_single() {
        let files = vec![("a".to_string(), "x".repeat(1000))];
        let batches = partition_batches(&files);
        assert_eq!(batches.len(), 1);
    }

    #[test]
    fn test_partition_batches_split() {
        let files: Vec<_> = (0..10)
            .map(|i| (format!("f{i}"), "x".repeat(20_000)))
            .collect();
        let batches = partition_batches(&files);
        assert!(batches.len() >= 2); // 200K total > 80K batch limit
    }
}
```

### Integration Tests

```rust
// crates/octos-agent/tests/deep_research_integration.rs

#[tokio::test]
#[ignore] // Requires API keys
async fn test_google_search_backend() {
    if std::env::var("GOOGLE_CSE_API_KEY").is_err() {
        return; // Skip without key
    }
    let registry = SourceRegistry::load();
    let engine = registry.engine("google").unwrap();
    let backend = google::GoogleSearchBackend::new(engine);

    let results = backend.search(&SearchOptions {
        query: "Rust programming language".to_string(),
        count: 5,
        language: Some("en".to_string()),
        region: Some("US".to_string()),
    }).await.unwrap();

    assert!(!results.is_empty());
    assert!(results[0].url.starts_with("http"));
}

#[tokio::test]
async fn test_google_news_rss() {
    // Google News RSS doesn't require API key
    let backend = google::GoogleNewsBackend::new();
    let results = backend.search(&SearchOptions {
        query: "World Cup 2026".to_string(),
        count: 5,
        language: Some("en".to_string()),
        region: Some("US".to_string()),
    }).await.unwrap();

    assert!(!results.is_empty());
}
```

### End-to-End Test

```bash
# Manual test via gateway (after deploy):
# 1. Send message on Telegram: "深入研究：2026年世界杯谁会夺冠？"
# 2. Verify:
#    - _plan.json shows multi-angle plan with Spanish, Portuguese, English sources
#    - partial_*.md files have findings in multiple languages
#    - report.md is 10K+ words with 20+ citations
#    - Total time: 5-15 minutes
```

---

## 17. Build Order & Milestones

### Milestone 0: Source Registry (1-2 days)

**Files**: `source_registry.rs`, `data/source_registry.toml`

Deliverables:
- [ ] `SourceRegistry` struct with `load()`, `match_topics()`, `plan_sources()`
- [ ] TOML data file with 10+ topics
- [ ] Unit tests for topic matching
- [ ] `cargo test -p octos-agent` passes

### Milestone 1: Search Engine Backends (3-5 days)

**Files**: `search/mod.rs`, `search/google.rs`, `search/bing.rs`, `search/baidu.rs`, `search/rss.rs`

Deliverables:
- [ ] `SearchBackend` trait + factory
- [ ] Google Custom Search backend
- [ ] Google News RSS backend
- [ ] Bing Web + News backend
- [ ] Baidu HTML scrape backend
- [ ] `MultiSearcher` with parallel search + dedup
- [ ] Integration tests (with API keys)

### Milestone 2: Research Orchestrator — Planning Phase (2-3 days)

**Files**: `tools/deep_research_v2.rs`, `prompts/research_planner.txt`

Deliverables:
- [ ] `DeepResearchOrchestrator` struct
- [ ] Planning LLM call → `ResearchPlan` JSON
- [ ] Plan saved to `_plan.json`
- [ ] Tool registered in gateway + chat
- [ ] Manual test: verify plan quality for 5 different query types

### Milestone 3: Collection Sub-Agents (3-5 days)

**Files**: `tools/deep_research_v2.rs` (collection methods), `prompts/research_collector.txt`

Deliverables:
- [ ] Sub-agent creation with per-angle system prompt
- [ ] Parallel execution via `tokio::spawn` + `join_all`
- [ ] Partial results saved to `partial_*.md`
- [ ] Timeout handling (600s per agent)
- [ ] Manual test: verify sub-agents produce quality findings

### Milestone 4: Synthesis + Self-Critique (2-3 days)

**Files**: `tools/deep_research_v2.rs` (synthesis methods), `prompts/research_critic.txt`

Deliverables:
- [ ] Map-reduce synthesis (reuse pattern from `synthesize_research.rs`)
- [ ] Self-critique LLM pass
- [ ] Final report saved to `report.md`
- [ ] Background mode with notification
- [ ] Token tracking across all phases

### Milestone 5: Integration + Deploy (1-2 days)

Deliverables:
- [ ] Register tool in `gateway.rs` and `chat.rs`
- [ ] Update system prompt to use `deep_research` for complex queries
- [ ] `cargo build --workspace` passes
- [ ] `cargo test --workspace` passes
- [ ] `./scripts/deploy.sh` to Mac Mini
- [ ] End-to-end test via Telegram

### Total: ~12-18 days

---

## 18. Why RL Training Matters (Future)

### What We Build Now (Prompt-Based)

The architecture above uses **prompt engineering** for all agent behaviors:
- Search query generation: prompted ("generate 3-5 targeted queries for this angle")
- Gap detection: prompted ("reflect on what's missing")
- Quality filtering: prompted ("score relevance 1-10, discard below 5")
- Stopping: fixed rule (max N cycles)

This works and will be a massive improvement over v1. But it has a ceiling.

### What RL Adds

| Behavior | Prompt-Based | RL-Trained |
|---|---|---|
| **Query formulation** | "Generate good queries" — generic | Model has learned which query patterns yield the most information across millions of training runs |
| **When to stop** | Fixed: 3-5 cycles per agent | Learned: 5 cycles for hard topics, 1 cycle for easy ones — optimized for efficiency |
| **Quality filtering** | "Score 1-10" — subjective, varies by run | Learned relevance model — trained to maximize final report quality |
| **Resource allocation** | Uniform: same budget per angle | Adaptive: more cycles for harder angles, fewer for easy ones |
| **Trajectory optimization** | Each step optimized independently | Entire 23-step trajectory optimized end-to-end |

### The Chess Analogy

Prompt-based agents are like a chess player who knows the rules and some strategy tips ("control the center", "develop your pieces"). They play decent games.

RL-trained agents are like a chess engine that has played 10 million games against itself. It has discovered counterintuitive strategies that no human would program. It doesn't just know the rules — it has an intuitive feel for which moves lead to winning positions 20 moves later.

Kimi started at 8.6% on HLE and reached 26.9% through RL alone — a 3x improvement with zero change to the architecture or prompts. That's the gap between "following rules" and "having learned what works."

### Our RL Roadmap (Phase 2)

1. **Collect trajectories**: Log all search queries, pages fetched, quality scores, and final reports from production usage
2. **Define reward signal**: Human rating of report quality (1-5), citation accuracy verification, completeness scoring
3. **Training pipeline**: REINFORCE/PPO on collected trajectories with the reward signal
4. **Train what**: The orchestrator's planning (which angles to create), sub-agent's search behavior (what queries, when to stop), quality scoring
5. **Requires**: Training infrastructure (GPU cluster), evaluation benchmark, 1000+ rated trajectories

This is a separate project, probably 2-3 months of work. The architecture we build now is the prerequisite.

---

## 19. References

### Competitive Systems

- [Gemini Deep Research API](https://ai.google.dev/gemini-api/docs/deep-research)
- [Google LangGraph Reference Implementation](https://github.com/google-gemini/gemini-fullstack-langgraph-quickstart)
- [Kimi-Researcher Technical Report](https://moonshotai.github.io/Kimi-Researcher/)
- [Kimi K2.5 Technical Report (arXiv)](https://arxiv.org/html/2602.02276v1)
- [DeepSearchQA Benchmark (arXiv)](https://www.arxiv.org/pdf/2601.20975)
- [ByteByteGo: How OpenAI, Gemini, and Claude Use Agents for Deep Research](https://blog.bytebytego.com/p/how-openai-gemini-and-claude-use)

### Internal Codebase References

- `crates/octos-agent/src/tools/deep_research.rs` — Existing sub-agent spawning patterns
- `crates/octos-agent/src/tools/synthesize_research.rs` — Map-reduce synthesis patterns
- `crates/octos-agent/src/tools/deep_search.rs` — Page fetching and file saving patterns
- `crates/octos-agent/src/tools/web_search.rs` — DuckDuckGo/Brave/You.com/Perplexity backends
- `crates/octos-agent/src/tools/web_fetch.rs` — SSRF protection, HTML-to-markdown
- `crates/octos-agent/src/agent.rs` — Agent, AgentConfig, Agent::new() builder pattern
- `crates/octos-agent/src/plugins/tool.rs` — Plugin tool stdin/stdout protocol
- `crates/octos-agent/src/bootstrap.rs` — Bundled app-skill bootstrap pattern
