---
name: deep-search
description: Deep multi-round web research with parallel fetching. Triggers: deep search, research, 深度搜索, 调研, investigate, deep research.
version: 2.0.0
author: octos
always: true
---

# Deep Search

## Overview

The `search` tool does multi-round research: free sources first (GDELT and Google News RSS for news, a self-hosted SearXNG if configured, then any search API key you added), polite page reading (robots.txt, identifiable User-Agent, a real browser only to render JS-heavy pages), reference chasing, and a cited report plus structured items. News works with no API key; general queries need a SearXNG instance or a search key. This file is always in context, so it stays short; the tool's input schema lists every parameter.

## Parameters

- **query** (required); **depth** 1-3 (default 2; 10/30/50 pages); **max_results** per provider per round (default 8).
- **output**: `report` (default, Markdown) or `items` (structured JSON; the report is still written).
- **lang**: BCP-47 code(s), e.g. `"en"` or `["en", "zh-CN", "es"]`. Each language is searched separately and results are kept to those languages (unknown-language results are kept).
- **region**: ISO country, e.g. `US`, `TW` (Google News edition, Brave/Serper country).
- **since**: ISO date or `24h` / `7d` / `2w` / `3m` / `1y`. Sent to providers that support it and applied to feed/page dates (undated results are kept). Use this instead of adding years or "latest" to the query.
- **category**: `news`, `general` or `auto` (default: news when `since` ≤ 31 days or the query mentions news/latest/today).
- **max_per_domain**, **domains_allow**, **domains_deny**: source limits (Google News links count against the publisher).
- **render**: `auto` (default) or `off` (plain HTTP only).
- **search_engine**: one provider to try first (`gdelt`, `google_news_rss`, `searxng`, `serper`, `tavily`, `perplexity`, `brave`, `you`) or `all`.

Example: `{"query": "COP31 climate summit", "lang": ["en", "es"], "since": "7d", "max_per_domain": 2, "output": "items"}`

## Providers and policy (OctoSense ADR 0002 §6)

1. GDELT DOC 2.0 (at most one call per 5s) + Google News search RSS, for news.
2. SearXNG when `SEARXNG_URL` is set (instance must enable the `json` format).
3. Keyed APIs: Serper, Tavily, Perplexity, Brave, You.com.

Each tier runs only if the previous ones returned fewer than `max_results`. If none returns anything, the result is **empty** and says which providers were tried and how to add SearXNG or a key; search-results pages are never scraped silently.

**Search-results scraping is off.** DuckDuckGo HTML and the headless-Chrome Bing page are search-engine results pages, which ADR 0002 rules out. They run only if the operator sets `OCTOS_ALLOW_SERP_SCRAPE=1` (`OCTOS_ALLOW_BROWSER_SERP=1` still works as an alias), and then last, DuckDuckGo before Bing; asking for them via `search_engine` without the flag is refused. Google News article links are `news.google.com` redirects that robots.txt disallows, so they are cited as **headline-only** sources (title, publisher, date), never fetched.

Reading: robots.txt (token `octos-research`) is checked per origin first; disallowed or unreachable-robots URLs are skipped and recorded (unreachable is re-checked after an hour). At least 1s between requests to a host (`Crawl-delay` honoured), 15s timeouts, 3 MB cap, private/internal addresses blocked on every hop with DNS pinning. Main text and metadata come from a readability extractor; pages with no main text over HTTP are rendered once by the `deep_crawl` browser (no automation hiding, private destinations blocked inside the browser, and the rendered page's final URL and navigation chain re-checked before its HTML is used). 403/429 and bot challenges are not bypassed.

## Output

- Report: synthesis with `[N]` citations (when a model is configured), then sources with title, publisher, date and language, then `Report saved to:` and `Items saved to:`.
- Items (`schema: octos.research.items.v1`): `items[]` with `url` (canonical), `title`, `source`, `domain`, `lang`, `published` (ISO), `summary` + `summary_kind` (`extractive` | `model` | `snippet` | `none`), `snippet`, `fetched_at`, `provider`, `read`, `rendered`, `citation` (the report's `[N]`), `cited`, `file`; plus `skipped[]` (`url`, `reason`: `robots`, `domain_deny`, `per_domain_cap`, `lang`, `older_than_since`, `fetch_error: …`), `providers`, `report`, `items_file`.
- Files under `./research/<query-slug>/`: `<slug>_report.md`, `<slug>_report.items.json`, `_search_results.md` (raw results + provider notes), `01_<domain>.md`… (page main text with url/title/source/lang/published front matter). Use `read_file` on them for detail.

Environment: `SEARXNG_URL`; optional keys `SERPER_API_KEY`, `TAVILY_API_KEY`, `PERPLEXITY_API_KEY`, `BRAVE_API_KEY`, `YDC_API_KEY`; `DEEP_SEARCH_HOST_INTERVAL_MS` (default 1000); `DEEP_SEARCH_MAX_BROWSERS` (default 3); `OCTOS_ALLOW_SERP_SCRAPE` (off; alias `OCTOS_ALLOW_BROWSER_SERP`).
