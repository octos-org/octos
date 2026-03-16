---
name: deep-search
description: Deep multi-round web research with parallel fetching. Triggers: deep search, research, 深度搜索, 调研, investigate, deep research.
version: 2.0.0
author: octos
always: true
---

# Deep Search

## Overview

The `deep_search` tool performs deep multi-round web research. It iteratively searches from multiple angles, fetches pages in parallel, chases the most-referenced external sources, and produces a structured research report.

## Usage

Call `deep_search` with a query and optional depth. The tool will:

1. **Round 1**: Search the web using Perplexity Sonar (preferred) or other available engines
2. **Rounds 2+**: Generate follow-up queries from different angles (time-qualified, subtopic-focused, controversy/analysis)
3. **Parallel fetch**: Crawl all discovered URLs concurrently (8 connections)
4. **Reference chasing**: Extract outbound links from crawled pages, fetch the most-cited external sources
5. **Report**: Build a structured report with overview, source previews, and search query log
6. **Save**: Everything saved under `./research/<query-slug>/`

### Parameters

- **query** (required, string): The research topic or question to investigate.
- **depth** (optional, integer, default: 2): Research depth:
  - `1` = Quick: single search round + crawl (~1 min, up to 10 pages)
  - `2` = Standard: 3 search rounds + reference chasing (~3 min, up to 30 pages)
  - `3` = Thorough: 5 search rounds + aggressive link chasing (~5 min, up to 50 pages)
- **max_results** (optional, integer, default: 8): Number of search results per round (1-10).
- **search_engine** (optional, string): Preferred search engine. Options: `perplexity`, `duckduckgo`, `brave`, `you`. Defaults to auto-detection (prefers Perplexity).

### Example

```json
{
  "query": "AI regulations worldwide 2026",
  "depth": 2
}
```

### Output

Returns a structured research report including:
- Overview (initial search answer)
- Source details with inline previews (first 2000 chars of each page)
- List of all search queries used
- Summary with page count and save location

Use `read_file` on specific source files for full content when you need detailed synthesis.

### Saved Files

Results are saved to `./research/<query-slug>/`:

- `_report.md` -- structured research report
- `_search_results.md` -- combined raw search results from all rounds
- `01_<domain>.md` -- full page content from first source
- `02_<domain>.md` -- full page content from second source
- etc.

### Environment Variables

- `PERPLEXITY_API_KEY` -- enables Perplexity Sonar (recommended, best for deep research)
- `BRAVE_API_KEY` -- enables Brave Search
- `YDC_API_KEY` -- enables You.com search

Without API keys, DuckDuckGo HTML search is used as fallback.
