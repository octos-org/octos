---
name: deep-search
description: Deep multi-round web research with parallel fetching. Triggers: deep search, research, 深度搜索, 调研, investigate, deep research.
version: 2.0.0
author: octos
always: true
---

# Deep Search

## Overview

The `search` tool does multi-round research: octos's key-less metasearch first, then a self-hosted SearXNG if configured, then any search API key you added, polite page reading (honest User-Agent, per-host spacing, a real browser only to render JS-heavy pages), reference chasing, and a cited report plus structured items. News, science and software searches work with no API key; key-less general search gets web results from results-page search (DuckDuckGo, then Bing; on unless the operator turned it off) alongside Wikipedia and Wikidata. This file is always in context, so it stays short; the tool's input schema lists every parameter.

## Parameters

Use this native Rust tool for deep research; its search engines use sandboxed
OctoScript internally. Do not route ordinary research to `run_pipeline`, DOT,
or graph IR. The host supplies synthesis from the saved `strong` research
lane, or the chat provider when no strong lane is configured. Use `deep_crawl`
for bounded reading of a specific site's rendered pages, then consolidate
that evidence into the final report when needed.

- **query** (required); **depth** 1-3 (default 2; 10/30/50 pages); **max_results** per provider per round (default 8).
- **output**: `report` (default, Markdown) or `items` (structured JSON; the report is still written).
- **lang**: BCP-47 code(s), e.g. `"en"` or `["en", "zh-CN", "es"]`. Each language is searched separately and results are kept to those languages (unknown-language results are kept).
- **query_by_lang**: the query in each language's own words, e.g. `{"zh": "人工智能 监管"}` with query `"AI regulation"`. Without it every language gets the same text, so an English query finds few Chinese pages; translate the query yourself when researching several languages.
- **region**: ISO country, e.g. `US`, `TW` (Google News edition, Brave/Serper country).
- **since**: ISO date or `24h` / `7d` / `2w` / `3m` / `1y`. Sent to providers that support it and applied to feed/page dates (undated results are kept). Use this instead of adding years or "latest" to the query.
- **category**: `news`, `general`, `science`, `it`, `social` or `auto` (default: news when `since` ≤ 31 days or the query mentions news/latest/today, else general).
- **max_per_domain**, **domains_allow**, **domains_deny**: source limits (Google News links count against the publisher).
- **render**: `auto` (default) or `off` (plain HTTP only).
- **search_engine**: one provider to try first (`metasearch`, `gdelt`, `google_news_rss`, `searxng`, `serper`, `tavily`, `perplexity`, `brave`, `you`) or `all`.

Example: `{"query": "COP31 climate summit", "lang": ["en", "es"], "since": "7d", "max_per_domain": 2, "output": "items"}`

## Providers and policy (OctoSense ADR 0002 §6)

1. Metasearch: sandboxed engines over official APIs and feeds, run in parallel, merged and ranked. news: GDELT, Google News RSS (headlines), publisher RSS feeds (en/zh), Hacker News, Mastodon hashtags; general: DuckDuckGo, Bing, Brave web and Google results pages (on by default, see below), Wikipedia, Wikidata; science: arXiv, OpenAlex; it: Hacker News, GitHub, Stack Exchange; social: Mastodon; Brave joins general/news when `BRAVE_API_KEY` is set. Each provider's rate limit is enforced; failing engines are suspended with backoff. `OCTOS_METASEARCH=0` turns it off (GDELT + Google News RSS are then called directly for news).
2. SearXNG when `SEARXNG_URL` is set (instance must enable the `json` format).
3. Keyed APIs: Serper, Tavily, Perplexity, Brave, You.com.
4. Without the metasearch (`OCTOS_METASEARCH=0`), results-page search for general web results: DuckDuckGo's HTML page, then Bing in headless Chrome. With it, the metasearch's own results-page engines cover them.

Each tier runs only if the previous ones returned fewer than `max_results`. If none returns anything, the result is **empty** and says which providers were tried and how to add SearXNG or a key; results-page search runs last and says when an engine answered with a challenge.

**Results-page search is on** (ADR 0002 §6 as amended: the way SearXNG searches, no person in the loop): the metasearch's DuckDuckGo, Bing, Bing News, Brave web and Google engines read those engines' results pages. DuckDuckGo, Bing and Brave get the octos User-Agent. Google's page answers only a browser-like client, so its engine is fetched with the `legacy_mobile` client: a feature-phone User-Agent over a Chrome TLS fingerprint, as SearXNG does. No CAPTCHA is solved: a challenge page suspends that engine for a while and the others answer. Search engines' terms may not allow automated queries (Google and Bing: high risk; DuckDuckGo: robots.txt allows the HTML page, its terms promise nothing). The operator can turn it off with `OCTOS_ALLOW_SERP_SCRAPE=0` (alias `OCTOS_ALLOW_BROWSER_SERP`; any value other than 1/true/yes/on turns it off); asking for `duckduckgo`/`bing_cdp` via `search_engine` while it is off is refused.

**Automation policy (defaults).** OctoSense agents are personal assistants reading on one person's behalf, so **robots.txt is not applied by default** (maintainer decision): it is never fetched or consulted, for feeds, person-initiated reads and autonomous research alike. An operator can turn it on with `OCTOS_RESPECT_ROBOTS=1` (then disallowed/unreachable-robots URLs are skipped and recorded, and `Crawl-delay` is honoured). Always on, whatever the setting: an honest User-Agent (`octos-research/1.0`) for reading pages and for every engine except Google's results page (above), at least 1s between requests to a host, one backoff-and-retry on 429/503 honouring `Retry-After` (capped at 30s), 15s timeouts, a 3 MB cap, no paywall/login/CAPTCHA bypass, and private/internal addresses blocked on every hop with DNS pinning.

Reading: Main text and metadata come from a readability extractor; pages with no main text over HTTP (including Google News article redirects) are rendered once by the `deep_crawl` browser (no automation hiding, private destinations blocked inside the browser, and the rendered page's final URL and navigation chain re-checked before its HTML is used). A page blocked over plain HTTP (a bot challenge, 401 or 403) is read once in that browser, which is often let through; nothing is solved, and a challenge in the browser is final.

## Output

- Report: synthesis with `[N]` citations (when a model is configured), then sources with title, publisher, date and language, then `Report saved to:` and `Items saved to:`.
- Synthesis: the model sees up to 6000 characters (characters, not bytes, so CJK pages get the same room) of each source's main text, 48000 in total, shared evenly across at most 12 sources. No output-token cap is sent (set `DEEP_SEARCH_SYNTHESIS_MAX_TOKENS` to impose one). Every factual sentence must carry a `[N]` citation; sentences without one are marked `[citation needed]`, and one closing `Gaps:` line says what the sources don't cover. A reply that was cut off (`finish_reason: length`, or it ends mid-sentence or mid-structure) is retried once with a request for a shorter answer; if it is still cut off the result is **not** success: `success: false`, `summary.status: "partial"`, the report is marked incomplete, and `summary.diagnostics` / items `diagnostics` say why.
- Items (`schema: octos.research.items.v1`): `items[]` with `url` (canonical), `title`, `source`, `domain`, `lang`, `published` (ISO), `summary` + `summary_kind` (`extractive` | `model` | `snippet` | `none`), `snippet`, `fetched_at`, `provider` (`metasearch` items add `engines`, best first, and a rank `score`), `read`, `rendered`, `citation` (the report's `[N]`), `cited`, `file`; plus `skipped[]` (`url`, `reason`: `robots` (only when enabled), `ssrf_blocked`, `domain_deny`, `per_domain_cap`, `lang`, `older_than_since`, `fetch_error: …`), `providers`, `note` (e.g. key-less general search is thin), `report`, `items_file`.
- Files under `./research/<query-slug>/`: `<slug>_report.md`, `<slug>_report.items.json`, `_search_results.md` (raw results + provider notes), `01_<domain>.md`… (page main text with url/title/source/lang/published front matter). Use `read_file` on them for detail.

Environment: `OCTOS_METASEARCH` (on), `OCTOS_METASEARCH_MASTODON_INSTANCE`, optional `GITHUB_TOKEN` / `OPENALEX_API_KEY` / `STACKEXCHANGE_KEY` (raise those APIs' limits); `SEARXNG_URL`; optional keys `SERPER_API_KEY`, `TAVILY_API_KEY`, `PERPLEXITY_API_KEY`, `BRAVE_API_KEY`, `YDC_API_KEY`; `DEEP_SEARCH_HOST_INTERVAL_MS` (default 1000); `DEEP_SEARCH_MAX_BROWSERS` (default 3); `DEEP_SEARCH_SYNTHESIS_MAX_TOKENS` (unset: no output cap); `OCTOS_ALLOW_SERP_SCRAPE` (on; `0` turns it off; alias `OCTOS_ALLOW_BROWSER_SERP`).
