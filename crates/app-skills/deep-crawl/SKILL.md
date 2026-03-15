---
name: deep-crawl
description: Recursively crawl websites using headless Chrome. Triggers: crawl, scrape website, 爬取, crawl site, deep crawl, website content.
version: 1.0.0
author: hagency
always: true
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
- The crawler uses stealth techniques to avoid bot detection (custom user-agent, webdriver flag removal)
- Pages that appear empty or bot-blocked are retried with longer wait times
- URL fragments are stripped and trailing slashes normalized to avoid duplicate visits
- Private/internal IP addresses are blocked (SSRF protection)
