---
name: deep-crawl
description: Recursively crawl websites using headless Chrome. Triggers: crawl, scrape website, 爬取, crawl site, deep crawl, website content.
version: 1.0.0
author: octos
requires_bins: google-chrome
always: false
---

# deep_crawl

## Overview

The `deep_crawl` tool recursively crawls a website using a headless Chrome browser via the Chrome DevTools Protocol (CDP). It renders JavaScript, follows same-origin links via BFS, extracts text content from each page, and saves results to disk. This is ideal for crawling JS-rendered SPAs, documentation sites, and any site that requires a full browser environment.

## Requirements

- **Google Chrome** or **Chromium** must be installed and available in PATH, or at a standard system location.
  - macOS: `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`
  - Linux: `google-chrome`, `google-chrome-stable`, or `chromium-browser`

## Usage

Call the `deep_crawl` tool with a starting URL. The crawler will follow same-origin links up to the specified depth and page limits.

### Parameters

| Parameter     | Type    | Required | Default | Description                                              |
|---------------|---------|----------|---------|----------------------------------------------------------|
| `url`         | string  | yes      | --      | The seed URL to start crawling from                      |
| `max_depth`   | integer | no       | 3       | Maximum link-following depth (1-10)                      |
| `max_pages`   | integer | no       | 50      | Maximum number of pages to crawl (1-200)                 |
| `path_prefix` | string  | no       | --      | Only follow links whose path starts with this prefix     |
| `include_html`| boolean | no       | false   | Also return each page's rendered HTML and final URL in `pages` |

### Example

```json
{
  "url": "https://docs.example.com/guide/",
  "max_depth": 3,
  "max_pages": 30,
  "path_prefix": "/guide/"
}
```

## Output

The tool returns a JSON object on stdout:

```json
{
  "output": "# Deep Crawl: https://docs.example.com/guide/\nCrawled 12 pages ...\n\n## Sitemap\n1. [depth=0] https://docs.example.com/guide/ (OK)\n...",
  "success": true
}
```

The `output` field contains:
- A **sitemap** listing all crawled pages with their depth and status
- A **content preview** (first ~2000 characters) for each page
- The **directory path** where full page contents are saved as `.md` files

Results are saved to a research directory named `crawl-<hostname>/` under the current working directory. Each page is saved as a numbered markdown file (e.g., `000_index.md`, `001_docs_install.md`).

## Behavior Details

- Only `http://` and `https://` URLs are allowed
- Only same-origin links are followed (no cross-domain crawling)
- robots.txt is **off by default** (operator setting `OCTOS_RESPECT_ROBOTS=1`; see Automation policy). When on, every page is checked (RFC 9309, product token `octos-research`): disallowed URLs are recorded as `skipped` and never opened; an unreachable robots.txt (5xx/network error) disallows the origin (re-checked after an hour); `Crawl-delay` is honoured (capped at 10s). When off, robots.txt is never requested and pages are crawled one at a time with the settle delay between them
- Pages that are still near-empty after the settle time get one more wait; a check that clears itself in a real browser ("Just a moment…", "正在进行安全检测…") is waited out for up to ~10 s; any other bot challenge is recorded as blocked, never solved. Sign-in, sign-up and account links are not followed
- URL fragments are stripped and trailing slashes normalized to avoid duplicate visits
- Private/internal addresses are blocked (SSRF protection), inside the browser too: every request Chrome makes (the page, each redirect, subresources) is paused via the CDP Fetch domain and only continued if its destination is public (no loopback, RFC 1918, link-local/cloud metadata, CGNAT or reserved address; DNS fail-closed). A page that redirects (HTTP, meta or JS) to a private address is recorded as blocked and its content discarded; the final URL and every main-frame navigation are re-checked before any text is returned

## Automation policy

Defaults: robots.txt is **not** applied (maintainer decision: OctoSense agents are personal assistants reading on one person's behalf); an operator can enable it with `OCTOS_RESPECT_ROBOTS=1`. Always applied: the honest User-Agent below, sequential pages with a settle delay, timeouts and size caps, SSRF blocking inside the browser, and no paywall/login/CAPTCHA bypass.

deep_crawl is a real browser for **reading** pages, not for getting around bot detection (OctoSense ADR 0002 §6: no disguised search):

- The browser is not disguised. There is no `navigator.webdriver` override, no `AutomationControlled` switches, no fake plugins/languages, and no spoofed desktop User-Agent. Chrome's own User-Agent is kept and `octos-research/1.0 (+https://github.com/octos-org/octos)` is appended to it.
- No CAPTCHA solving, no human-behaviour imitation, no fingerprint spoofing. A bot challenge ends the attempt for that page.
- Do not use deep_crawl to scrape search-engine results pages. Use a search provider (GDELT, Google News RSS, a self-hosted SearXNG, or a search API key) and crawl the result pages instead. (deep-search only renders a Bing results page through deep_crawl when an operator has set `OCTOS_ALLOW_SERP_SCRAPE=1`.)
- There is no flag to turn evasion back on; adding one requires a new ADR.
