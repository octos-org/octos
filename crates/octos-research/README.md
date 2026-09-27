# octos-research

Research building blocks for the octos search tools (`web_search`, `deep-search`, `deep-crawl`):

- structured items (`octos.research.items.v1`);
- `lang`, `since` and domain filters;
- robots.txt handling, per-host throttling and main-text extraction;
- the provider chain;
- **metasearch**: octos's own free, key-less search.

## Metasearch

A Rust core fans a query out to small search engines written as sandboxed [OctoScript](https://github.com/OctoSense-org/Octoscript) scripts, then merges and ranks what they return. It is the first provider in the chain for every category. After it come a self-hosted SearXNG (if configured) and keyed APIs. Scraping search-results pages stays behind `OCTOS_ALLOW_SERP_SCRAPE`.

```
octos-research (Rust, trusted)                      engines/<id>/ (sandboxed OctoScript)
 ├─ dispatcher: parallel fan-out, deadline           ├─ manifest.json   id, categories, languages, hosts,
 ├─ engine suspension on errors (doubling backoff)   │                  auth, rate_limit, docs_url, license_note
 ├─ HTTP: per-host spacing, Retry-After, ETag /      ├─ engine.octoscript
 │   If-Modified-Since cache, robots.txt, octos UA   │    build_request(query, opts)  -> {url, method, headers, body}
 ├─ merge: canonical URL + near-duplicate titles     │    parse_response(response, opts) -> [item] | {items, backoff, error}
 ├─ rank: engine weight / √(1+position), recency     └─ fixtures/        recorded responses + expected items
 └─ filters: lang, since, domain allow/deny, cap
```

### The engine contract

- **Only the core does HTTP.** `build_request` describes a request and the core performs it. `parse_response` gets `response = {status, headers, json, body}`: `json` is the body decoded by the host, and `body` holds the raw text only when the response is not JSON.
- **Installed modules.** The sandbox provides exactly these, plus the frozen `mod.std.*`:
  - `net.request({url, method, headers, body})` checks a request against the hosts the manifest declares. It refuses any other host, port, scheme, credentials, and the host-owned headers (User-Agent, Authorization, cookies, key headers).
  - `net.url({base, query})` builds a percent-encoded URL on a declared host.
  - `markup.feed({lang})` parses the current response as RSS or Atom.
  - `markup.text({html})` returns the plain text of an HTML fragment.
- **No other capability.** The engines run on the bounded `octoscript-core` runtime: there is no `mod.tool`, filesystem, process, clock or network module, each method has a call budget, and instructions, heap, strings, stack and wall-clock time are bounded.
- **Keys stay in the host.** A manifest's `auth` says where the core attaches a key (header, bearer or query parameter) after `build_request` has run. The script never sees it. Engines with `needs_key` run only when the host has a key.
- **Rate limits.** Each engine's `rate_limit.min_interval_ms` is enforced per host across the whole process. When the provider signals, the core waits:
  - `Retry-After` on 429 or 503;
  - the `backoff` value an engine reads from the response, as Stack Exchange's API sends.

  A search skips an engine whose next slot would miss its deadline.
- **Discovery.** Built-in engines are compiled in. Extra engines are loaded from `OCTOS_METASEARCH_ENGINES/<id>/`, and each must be pinned by the digest `sha256(manifest.json ‖ 0x00 ‖ engine.octoscript)`, given in `pins.json` or by the host. A pinned engine replaces a built-in with the same id.

### Engines

| Engine | Category | Source | Key | Rate limit (per host) | `docs_url` |
|---|---|---|---|---|---|
| `gdelt` | news | GDELT DOC 2.0 ArtList JSON | none | 6 s | https://blog.gdeltproject.org/gdelt-doc-2-0-api-debuts/ |
| `google_news` | news | Google News search RSS (headlines) | none | 2 s, **off by default** | https://news.google.com/robots.txt |
| `wikipedia` | general | MediaWiki Action API `list=search` | none | 1 s | https://www.mediawiki.org/wiki/API:Search, https://www.mediawiki.org/wiki/API:Etiquette |
| `wikidata` | general | `wbsearchentities` | none | 1 s | https://www.wikidata.org/w/api.php?action=help&modules=wbsearchentities |
| `arxiv` | science | arXiv API (Atom) | none | 3 s | https://info.arxiv.org/help/api/user-manual.html, https://info.arxiv.org/help/api/tou.html |
| `openalex` | science | OpenAlex `/works` | optional `OPENALEX_API_KEY` | 1 s | https://help.openalex.org/api/, https://help.openalex.org/api/searching/, https://help.openalex.org/api/authentication/ |
| `hackernews` | it, news | HN Search API (Algolia) | none | 0.5 s | https://hn.algolia.com/api |
| `github` | it | REST search repositories | optional `GITHUB_TOKEN` | 6 s | https://docs.github.com/en/rest/search/search#search-repositories |
| `stackexchange` | it | API 2.3 `/search/advanced` | optional `STACKEXCHANGE_KEY` | 2 s + `backoff` | https://api.stackexchange.com/docs/advanced-search, https://api.stackexchange.com/docs/throttle |
| `mastodon` | social, news | public hashtag timeline | none | 1.5 s | https://docs.joinmastodon.org/methods/timelines/#tag, https://docs.joinmastodon.org/api/rate-limits/ |
| `brave` | general, news | Brave Search API web/news | **required** `BRAVE_API_KEY` | 1.1 s | https://api-dashboard.search.brave.com/app/documentation/web-search/query |

Notes:

- **`general` without a key is thin.** Key-less general search is Wikipedia and Wikidata only, and results say so.
- **Google News.** news.google.com/robots.txt disallows `/rss` for every user agent. The engine checks robots.txt before each request, so it ships off by default and is refused when enabled while that rule stands.
- **Mastodon.** Uses the public hashtag timeline, because full-text search needs a user token. Set another instance with `OCTOS_METASEARCH_MASTODON_INSTANCE`.
- **Small key-less quotas.** OpenAlex allows about 100 searches a day per IP without a key. Stack Exchange allows 300 requests a day.

### Clean-room method

SearXNG (AGPL-3.0) was only the conceptual model: engines as small modules, parallel dispatch, merge and rank. No SearXNG source, engine module or settings file was read, translated or copied. Each engine was written from its provider's public documentation (its manifest's `docs_url`), and its request and response shapes were checked against one recorded live response. The exceptions are Brave (needs a key) and Google News (robots.txt), whose fixtures are marked in their files. Every engine uses an official API or a published feed; no results page is scraped.

### Configuration

| Variable | Effect |
|---|---|
| `OCTOS_METASEARCH=0` | Turn the metasearch off; news falls back to direct GDELT and Google News RSS calls. |
| `OCTOS_METASEARCH_ENGINES` | Directory of extra, pinned engines. |
| `OCTOS_METASEARCH_<ENGINE>_<SETTING>` | Engine setting, e.g. `OCTOS_METASEARCH_MASTODON_INSTANCE=fosstodon.org`. |
| `<key_env>` from each manifest | Keys: `BRAVE_API_KEY`, `GITHUB_TOKEN`, `OPENALEX_API_KEY`, `STACKEXCHANGE_KEY`. Profile provider keys win. |

### Tests

```sh
cargo test -p octos-research                                  # unit, core, fixture replay, UA grep (no network)
cargo test -p octos-research --features http --test engines_live -- --ignored --test-threads=1
cargo run -p octos-research --features http --example record_engine_fixtures [engine ...]
```
