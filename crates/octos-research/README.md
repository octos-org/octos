# octos-research

Research building blocks for the octos search tools (`web_search`, `deep-search`, `deep-crawl`):

- structured items (`octos.research.items.v1`);
- `lang`, `since` and domain filters;
- robots.txt handling, per-host throttling and main-text extraction;
- the provider chain;
- **metasearch**: octos's own free, key-less search.

## Metasearch

A Rust core fans a query out to small search engines written as sandboxed [OctoScript](https://github.com/OctoSense-org/Octoscript) scripts, then merges and ranks what they return. It is the first provider in the chain for every category. After it come a self-hosted SearXNG (if configured) and keyed APIs. Results-page search (DuckDuckGo, Bing via Chrome) follows as the last tier for general web results; it is on unless `OCTOS_ALLOW_SERP_SCRAPE=0`.

```
octos-research (Rust, trusted)                      engines/<id>/ (sandboxed OctoScript)
 ├─ dispatcher: parallel fan-out, deadline,         ├─ manifest.json   id, categories, languages, hosts,
 │   soft deadline for stragglers                   │
 ├─ engine suspension on errors (doubling backoff)   │                  auth, rate_limit, docs_url, license_note
 ├─ HTTP: per-host spacing, Retry-After, ETag /      ├─ engine.octoscript
 │   If-Modified-Since cache, octos UA   │    build_request(query, opts)  -> request | [request] (≤ max_requests)
 ├─ merge: canonical URL + near-duplicate titles     │    parse_response(response, opts) -> [item] | {items, backoff, error}
 ├─ rank: engine weight / √(1+position), recency     └─ fixtures/        recorded responses + expected items
 └─ filters: lang, since, domain allow/deny, cap
```

### The engine contract

- **Only the core does HTTP.** `build_request` describes a request and the core performs it. With the bundled fetcher, every request goes through the crate's one SSRF implementation (`net::pinned_client`): public hosts only, DNS resolved fail-closed and pinned, no redirects. Manifests may not declare IP literals, `localhost`, private addresses or internal suffixes (`.local`, `.internal`, …), and the same check applies to host settings such as the Mastodon instance. `parse_response` gets `response = {status, headers, json, body}`: `json` is the body decoded by the host, and `body` holds the raw text only when the response is not JSON.
- **Installed modules.** The sandbox provides exactly these, plus the frozen `mod.std.*`:
  - `net.request({url, method, headers, body})` checks a request against the hosts the manifest declares. It refuses any other host, port, scheme, credentials, and the host-owned headers (User-Agent, Authorization, cookies, key headers).
  - `net.url({base, query})` builds a percent-encoded URL on a declared host.
  - `markup.feed({lang})` parses the current response as RSS or Atom.
  - `markup.text({html})` returns the plain text of an HTML fragment.
  - `markup.matches({query, text})` returns `{matched}`: whether a headline is about the query (see [Publisher feeds](#engines) below).
- **No other capability.** The engines run on the bounded `octoscript-core` runtime: there is no `mod.tool`, filesystem, process, clock or network module, each method has a call budget, and instructions, heap, strings, stack and wall-clock time are bounded.
- **Keys stay in the host.** A manifest's `auth` says where the core attaches a key (header, bearer or query parameter) after `build_request` has run. The script never sees it. Engines with `needs_key` run only when the host has a key.
- **Rate limits.** Each engine's `rate_limit.min_interval_ms` is enforced per host across the whole process. When the provider signals, the core waits:
  - `Retry-After` on 429 or 503;
  - the `backoff` value an engine reads from the response, as Stack Exchange's API sends.

  A search skips an engine whose next slot would miss its deadline.
- **Timeouts and slow engines.** Each request is bounded by its manifest's `timeout_secs` (GDELT: 5 s, because it answers a throttled client's request with a 429 only after about 10 s). Once one engine has answered with results and at most a quarter of the calls (at least one) are still running, those get `SearchRequest::straggler_grace` (default 2 s) more; then they are dropped and reported as `timeout` with the reason "dropped (soft deadline)". A dropped call counts as a timeout for the engine's health: three in a row suspend it for 30 s, doubling each time it happens again before the engine answers. An error suspends it at once (30 s, doubling, or `Retry-After` when longer).
- **Discovery.** Built-in engines are compiled in. Extra engines are loaded from `OCTOS_METASEARCH_ENGINES/<id>/`, and each must be pinned by the digest `sha256(manifest.json ‖ 0x00 ‖ engine.octoscript)`. Pins come only from the host: a file named by `OCTOS_METASEARCH_PINS`, which must live outside the engine directory, so write access to that directory is not enough to add or change an engine. A directory engine may not replace a built-in with the same id unless `OCTOS_METASEARCH_ALLOW_OVERRIDE=1`.

### Engines

| Engine | Category | Source | Key | Rate limit (per host) | `docs_url` |
|---|---|---|---|---|---|
| `gdelt` | news | GDELT DOC 2.0 ArtList JSON | none | 6 s | https://blog.gdeltproject.org/gdelt-doc-2-0-api-debuts/ |
| `google_news` | news | Google News search RSS (headlines only) | none | 2 s | https://news.google.com/rss |
| `wikipedia` | general | MediaWiki Action API `list=search` | none | 1 s | https://www.mediawiki.org/wiki/API:Search, https://www.mediawiki.org/wiki/API:Etiquette |
| `wikidata` | general | `wbsearchentities` | none | 1 s | https://www.wikidata.org/w/api.php?action=help&modules=wbsearchentities |
| `arxiv` | science | arXiv API (Atom) | none | 3 s | https://info.arxiv.org/help/api/user-manual.html, https://info.arxiv.org/help/api/tou.html |
| `openalex` | science | OpenAlex `/works` | optional `OPENALEX_API_KEY` | 1 s | https://help.openalex.org/api/, https://help.openalex.org/api/searching/, https://help.openalex.org/api/authentication/ |
| `hackernews` | it, news | HN Search API (Algolia) | none | 0.5 s | https://hn.algolia.com/api |
| `github` | it | REST search repositories | optional `GITHUB_TOKEN` | 6 s | https://docs.github.com/en/rest/search/search#search-repositories |
| `stackexchange` | it | API 2.3 `/search/advanced` | optional `STACKEXCHANGE_KEY` | 2 s + `backoff` | https://api.stackexchange.com/docs/advanced-search, https://api.stackexchange.com/docs/throttle |
| `mastodon` | social, news | public hashtag timeline | none | 1.5 s | https://docs.joinmastodon.org/methods/timelines/#tag, https://docs.joinmastodon.org/api/rate-limits/ |
| `publisher_feeds` | news | publishers' own RSS feeds (en: NPR, France 24, CNA; zh: RFI 中文, 自由亚洲电台, 中央社, 端傳媒), filtered by the query | none | 1 s, 15 min cache | each publisher's feed and terms page (see the manifest) |
| `brave` | general, news | Brave Search API web/news | **required** `BRAVE_API_KEY` | 1.1 s | https://api-dashboard.search.brave.com/app/documentation/web-search/query |

Notes:

- **`general` without a key is thin.** Key-less general search is Wikipedia and Wikidata only, and results say so.
- **Google News.** Headlines, publisher and date only; article redirect links are cited, never fetched. Google doesn't document the feed, and its text limits it to personal, non-commercial feed-reader use, which is how an octos agent acting for one person uses it.
- **Mastodon.** Uses the public hashtag timeline, because full-text search needs a user token. Set another instance with `OCTOS_METASEARCH_MASTODON_INSTANCE`. Its results are posts (see [Articles and posts](#articles-and-posts)).
- **Publisher feeds.** Feeds can't be searched, so the engine reads the feeds for the requested languages (one request per feed, each cached 15 minutes) and keeps the entries whose headline or summary is about the query: the query as a phrase, or every significant term of it. Stop-words ("the", "of", "news", "的", "最新"…) and one-letter terms are not significant, and some terms are never enough, so "EU AI Act" does not match a "terrorist act" or "AI in schools". Words match whole words ("ai" is not in "said"; a plural "s" is allowed); CJK terms match anywhere in the CJK text, ignoring punctuation between characters. The manifest's `query_match: true` makes the core apply the same test again and report anything else as skipped (`query_mismatch`). Headline, source, date and link only. Publishers whose terms forbid AI or automated use (BBC, The Guardian, Al Jazeera, DW, NYT 中文网) are not included.
- **Small key-less quotas.** OpenAlex allows about 100 searches a day per IP without a key. Stack Exchange allows 300 requests a day.

### Articles and posts

Every result has a `kind`: `article` (a news story, page, paper or repository that can be read and cited) or `post` (a social post, or a discussion thread with no linked article). JSON omits `kind` for articles, so an absent `kind` means `article`.

- **Mastodon** results are posts (its manifest sets `"kind": "post"`).
- **Hacker News** stories are articles, because the item's URL is the submitted link, which is what gets read. Text posts such as Ask HN link only to their thread and are posts.
- An engine sets its default in the manifest (`kind`), and an item may override it with its own `kind`.
- When a post and an article are merged as the same story, the item keeps the article's URL and kind.

In category `news`, posts rank after every article. They stay in the results as signal (what people are saying, or that a story is spreading), but they are not reports, so a caller that reads sources as evidence should skip `kind: post` or treat it as discussion. Other categories (`social`, `it`, …) rank posts by score like anything else. The `kind` field reaches `SearchHit`, `MetaItem` and the `octos.research.items.v1` items.

## Reading pages

The shared reader (`reader::Reader`, used by `deep-search`, the built-in `deep_search` tool and the toolbox's `web_read`) reads a page over plain HTTP first and asks a browser renderer when that finds no article (as with Google News links, which reach the publisher only through a script) or when plain HTTP was blocked (a bot challenge, 401 or 403): a real browser, and especially a phone's WebView, is often let through where a plain client is not (`ReaderConfig::render_blocked`, on by default; OctoSense ADR 0002 §6 as amended). A renderer that meets a check which clears itself ("Just a moment…", "正在进行安全检测…", `access::is_interstitial`) waits for it. Nothing is solved or clicked: a challenge that asks a person, or one the browser meets too, is final.

### Failure reasons

Every failed read is a `ReadError`: `{reason, detail, final_url}`. It displays as `<reason>: <detail> (final URL: <url>)`, and `SkippedUrl` keeps the reason and final URL.

| `reason` | Meaning |
|---|---|
| `redirect_unresolved` | A Google News link never reached the publisher: no renderer is configured, or the browser stayed on news.google.com. |
| `consent_page` | A cookie or privacy consent wall, including a redirect to consent.google.com or guce.yahoo.com, a consent dialog that extraction took for the article (most of the text is consent wording, with at least three of the dialog's own phrases, such as "Strictly necessary", "Always active" or "Alle akzeptieren", not in quotes), or an embedded player's consent prompt ("To display this content from YouTube, you must enable advertisement tracking…") with no article text. It is not clicked through. |
| `stub_page` | The page loaded, but its text is only a stub: boilerplate with less than about one sentence of article text (CJK counted three per character), a video playlist (running times and titles, no paragraph), or a video page (`og:type` video, or a `/video/` path) with only its caption. |
| `paywall` | Subscriber-only content, from schema.org `isAccessibleForFree: false` or the page's subscribe prompt. It is not bypassed. |
| `login_wall` | The content is shown only to signed-in users. |
| `bot_challenge` | An anti-bot check: Cloudflare, DataDome, HUMAN/PerimeterX, or Google's unusual-traffic page. It is not bypassed. |
| `render_failed` / `render_timeout` | The browser renderer failed, or did not finish within `ReaderConfig::render_timeout` (60 s). |
| `no_main_text` | The page loaded, was none of the above, and had no extractable article. |
| `blocked` | Refused by octos: SSRF protection (a private or internal address, before fetching or anywhere in the browser's navigation) or the caller's scope. |
| `http_<status>` | The publisher answered with an error status, or the rendered page is an error page that states one (`403 Forbidden`, `Access denied`) when the renderer reports no status. |
| `robots`, `robots_unreachable` | robots.txt refused the page (only when `OCTOS_RESPECT_ROBOTS=1`). |
| `fetch_error`, `unsupported_content_type` | Network error, or the page is not HTML, XML or text. |

The detection is deliberately conservative (`access::diagnose`), so an article that only talks about cookies, paywalls or "Just a moment..." screens is never reported as a wall:

- **Always applied:** URL rules (consent hosts, Google's `/sorry/` page, a Google News link the browser never left) and markers that only challenge pages carry (`_cf_chl_opt`, `captcha-delivery.com`, `px-captcha`, or a challenge title).
- **Only when the page had no main text and little visible text:** the phrase rules (challenge, consent, paywall and login wording) and error-page titles.
- **Consent dialog taken for the article:** caught only when the extracted text is short, uses consent-dialog wording ("store and/or access information on a device", "our partners", …), and the page shows the dialog's buttons.

**Renderers** should return `Ok(Rendered)` with the page the browser ended on, even when it is a challenge or an error page, and set `Rendered::status` when they know it. The reader then classifies the page and reports its final URL. An `Err(message)` is classified by `ReadError::from_render_error`: a message that starts with a reason code (`bot_challenge: …`) keeps that code.

### Clean-room method

SearXNG (AGPL-3.0) was the conceptual model: engines as small modules, parallel dispatch, merge and rank. No SearXNG source, engine module or settings file was read, translated or copied. Each API engine was written from its provider's public documentation (its manifest's `docs_url`), and its request and response shapes were checked against one recorded live response. The exceptions are Brave (needs a key) and GDELT (it answered HTTP 429 while recording), whose fixtures are synthetic and marked as such in their files.

The results-page engines (`results_page`: DuckDuckGo, Bing, Bing News, Brave web, Google) read search engines' own pages, as SearXNG does; the maintainer decided that octos searches the way SearXNG does, with no person in the loop. Their parsers were written from the pages as observed, with recorded or reconstructed fixtures. Google answers plain clients with an "unusual traffic" page. How SearXNG still gets results was learned black-box, from outside: its requests through a logging proxy on a test instance (endpoint, parameters, headers and their order) and its installed HTTP client library (curl_cffi, a browser-fingerprint client). Those observed facts are reproduced by the `legacy_mobile` client (`metasearch::impersonate`, feature `impersonate`, built on the `wreq` crate); no SearXNG code was read. A challenge page is never solved: the engine is suspended and the other engines answer.

### Configuration

| Variable | Effect |
|---|---|
| `OCTOS_RESPECT_ROBOTS=1` | Operator opt-in: check robots.txt for engines whose manifest sets `robots` (off by default; octos agents act for one person). |
| `OCTOS_METASEARCH=0` | Turn the metasearch off; news falls back to direct GDELT and Google News RSS calls. |
| `OCTOS_ALLOW_SERP_SCRAPE=0` | Turn the results-page engines off (on by default). |
| `OCTOS_BROWSER` | `off` (default), `auto`, `window` or `headless`: load pages of engines that render (`google_cse`) in the octos browser profile (`octos_research::browser`). Any other value, `1` and `true` included, means `off`, so a typo never opens windows. |
| `OCTOS_METASEARCH_ENGINES` | Directory of extra engines. |
| `OCTOS_METASEARCH_PINS` | Pins file for those engines (`{"id": "sha256:…"}`), kept outside the engine directory. |
| `OCTOS_METASEARCH_ALLOW_OVERRIDE=1` | Let a pinned directory engine replace a built-in with the same id. |
| `OCTOS_METASEARCH_<ENGINE>_<SETTING>` | Engine setting, e.g. `OCTOS_METASEARCH_MASTODON_INSTANCE=fosstodon.org`. |
| `<key_env>` from each manifest | Keys: `BRAVE_API_KEY`, `GITHUB_TOKEN`, `OPENALEX_API_KEY`, `STACKEXCHANGE_KEY`. Profile provider keys win. |

### Tests

```sh
cargo test -p octos-research                                  # unit, core, fixture replay, UA grep (no network)
cargo test -p octos-research --features http --test engines_live -- --ignored --test-threads=1
cargo run -p octos-research --features http --example record_engine_fixtures [engine ...]
```
