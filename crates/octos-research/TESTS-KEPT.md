# octos-research: tests to keep

This is the behavioral allowlist for the next test-diet slice (#3069). It is an inventory only: this change does not delete or add tests. Names below were checked on main `ed914b5fcc1e1d125df319524936cc02d1cae202` with `cargo +1.97.1 test -p octos-research -- --list` and, for feature-gated browser/impersonate tests, `cargo +1.97.1 test -p octos-research --all-features -- --list`. The default listing contained 171 tests (146 lib, 18 engine replay, 6 provider fixtures, 1 User-Agent policy); all 91 allowlist entries occur in the union of the two listings. `wake/test-lines.py --json` measured the same main at 4,748 test lines / 171 tests for this crate.

Categories: **1** cross-system chain; **2** cross-feature behavior; **3** security or untrusted-input boundary; **4** recovery, data integrity, concurrency, or wire/response protocol; **5** intentional product/design policy. A name is the exact name printed by the test binary; integration-test sections also name their target so that unqualified names are unambiguous.

## Recorded engine request → response → normalized items

Target: `tests/engine_fixtures.rs`. Each replay checks the real dispatcher, recorded request method/URL/headers, response status, and normalized items without network access.

| Test | Category | Property guarded |
|---|---:|---|
| `arxiv_build_and_parse` | 4 | arXiv Atom requests and recorded responses produce the expected scholarly articles. |
| `bing_build_and_parse` | 4 | Bing's recorded results page still yields the expected web items. |
| `bing_news_build_and_parse` | 4 | Bing News's recorded response still yields dated news articles. |
| `brave_build_and_parse` | 4 | Brave's keyed API request and recorded news response remain compatible. |
| `brave_web_build_and_parse` | 4 | Brave web's recorded results response remains compatible with its parser. |
| `duckduckgo_build_and_parse` | 4 | DuckDuckGo's recorded HTML response still produces usable result links. |
| `gdelt_build_and_parse` | 4 | GDELT ArtList requests, timestamps, languages, and items stay aligned with the fixture. |
| `github_build_and_parse` | 4 | GitHub repository-search responses retain their repository URLs and metadata. |
| `google_build_and_parse` | 4 | Google result-page exchanges, including rendered/client behavior, produce the recorded items. |
| `google_cse_build_and_parse` | 4 | Google CSE's keyed request and JSON response remain compatible. |
| `google_news_build_and_parse` | 4 | Google News RSS and publisher-link handling preserve the recorded headlines. |
| `hackernews_build_and_parse` | 4 | Hacker News/Algolia responses preserve story versus post behavior and metadata. |
| `mastodon_build_and_parse` | 4 | Mastodon timeline responses remain identifiable as social posts. |
| `openalex_build_and_parse` | 4 | OpenAlex works responses preserve scholarly result metadata. |
| `publisher_feeds_build_and_parse` | 4 | Multi-feed publisher exchanges preserve query matching and normalized publisher items. |
| `stackexchange_build_and_parse` | 4 | Stack Exchange API requests and recorded responses preserve technical result items. |
| `wikidata_build_and_parse` | 4 | Wikidata entity-search responses preserve useful general-search items. |
| `wikipedia_build_and_parse` | 4 | Wikipedia API responses preserve titles, links, and snippets. |

## Provider fixtures and output chain

Target: `tests/providers.rs` unless stated otherwise.

| Test | Category | Property guarded |
|---|---:|---|
| `should_parse_gdelt_artlist_fixture` | 4 | Real GDELT JSON keeps multilingual article metadata and drops an unsafe non-HTTP hit. |
| `should_parse_google_news_rss_fixture` | 4 | Google News RSS keeps publisher/source/date data and does not mistake a repeated headline for a snippet. |
| `should_parse_atom_feed_fixture` | 4 | Atom entries produce the same normalized article fields as RSS providers. |
| `should_reject_non_feeds` | 3 | HTML or malformed external input is rejected instead of becoming fabricated feed results. |
| `should_parse_searxng_fixture` | 4 | SearXNG JSON preserves result metadata and rejects an HTML error page. |
| `should_filter_fixture_hits_by_language` | 2 | Parsed provider hits flow through language filtering and formatted output without losing unknown-language results. |
| `should_not_contain_spoofed_browser_user_agents` (`tests/no_spoofed_user_agent.rs`) | 5 | Enforces the maintainer's honest-client policy across research and crawl sources; see [Reading pages](README.md#reading-pages) and octos#2568. |

## Grants, search scope, and result artifacts

| Test | Category | Property guarded |
|---|---:|---|
| `toolbox::tests::should_parse_grants_and_reject_bad_ones` | 3 | Untrusted grant JSON is validated rather than silently widening languages, regions, categories, or limits. |
| `toolbox::tests::should_narrow_search_calls_to_the_grant` | 3 | Caller arguments are clamped to the app's language, recency, category, result-count, and denied-domain grant. |
| `toolbox::tests::should_keep_requested_domains_inside_the_allow_list` | 3 | A caller can narrow but cannot escape the granted domain family. |
| `toolbox::tests::should_write_scoped_search_items_into_the_app_folder` | 1 | A scoped search reaches filtered, ranked results, a cited summary, and a schema-valid artifact inside the app folder. |
| `toolbox::tests::should_refuse_to_read_pages_outside_the_domain_grant` | 3 | `web_read` refuses an out-of-grant URL before reading it and returns a blocked error. |
| `toolbox::tests::should_check_the_fetched_location_not_only_the_canonical` | 3 | A redirect into a denied subdomain cannot hide behind an allowed canonical URL. |
| `toolbox::tests::should_not_overwrite_results_written_in_the_same_second` | 4 | Concurrent/same-second research runs receive distinct artifact paths instead of overwriting prior results. |

## Metasearch dispatch, recovery, and user handoff

| Test | Category | Property guarded |
|---|---:|---|
| `metasearch::tests::should_return_results_when_one_engine_times_out_and_one_errors` | 4 | Parallel engine failures do not discard healthy results, and error/timeout suspension still permits later recovery. |
| `metasearch::tests::should_honour_retry_after_on_429` | 4 | A provider's Retry-After interval overrides shorter local backoff and the engine can recover afterward. |
| `metasearch::tests::should_never_call_gdelt_more_than_once_per_five_seconds` | 4 | Concurrent searches still obey GDELT's process-wide per-host rate limit. |
| `metasearch::tests::should_reuse_fresh_responses_and_revalidate_stale_ones` | 4 | Fresh cache hits avoid network traffic while stale entries use ETag validation and preserve data on 304. |
| `metasearch::tests::should_refuse_an_engine_that_targets_an_undeclared_host` | 3 | Engine code cannot send a request outside its manifest's host allowlist. |
| `metasearch::tests::should_check_robots_txt_only_when_the_operator_turns_it_on` | 5 | Preserves the intentional opt-in robots policy and applies a disallow rule only when enabled; see [Configuration](README.md#configuration). |
| `metasearch::tests::should_merge_across_engines_and_apply_lang_since_and_domain_filters` | 2 | Multi-engine results survive canonical merging plus language, recency, and denied-domain filtering with skip reasons intact. |
| `metasearch::tests::should_not_let_a_host_setting_point_at_a_private_address` | 3 | An operator-supplied engine host cannot redirect the sandbox toward metadata/internal services. |
| `metasearch::tests::should_search_each_language_with_its_own_query` | 2 | Localized queries reach both multi-language and ordinary engines without dropping either language. |
| `metasearch::tests::should_report_failed_requests_when_a_multi_request_engine_partly_fails` | 4 | A partial multi-feed failure preserves successful items and reports the failed request count. |
| `metasearch::tests::should_drop_a_slow_engine_when_the_others_have_answered` | 4 | The soft deadline returns healthy results without waiting indefinitely for a straggler, while disabling grace still waits. |
| `metasearch::tests::should_wait_for_slow_engines_when_no_engine_has_results_yet` | 4 | The soft deadline does not abandon the only engine that might still produce results. |
| `metasearch::tests::should_hand_a_browser_challenge_to_the_person` | 1 | A real browser challenge reaches the user with its URL, is disclosed, and retries only after the fixed pause. |
| `metasearch::tests::should_ask_the_person_to_open_a_challenge_nothing_could_show` | 1 | If the host cannot display a challenge, the response still tells the user to open it rather than claiming success. |
| `metasearch::tests::should_not_disclose_browser_use_when_no_page_loaded` | 4 | A failed browser start reports an error without falsely saying that a page was loaded in a browser. |

## Page reading and browser recovery

| Test | Category | Property guarded |
|---|---:|---|
| `reader::tests::should_discard_render_that_redirects_to_a_private_ip` | 3 | Browser rendering cannot bypass SSRF protection by landing on a metadata address. |
| `reader::tests::should_check_every_reported_navigation_not_just_the_final_url` | 3 | A private intermediate navigation is rejected even when the final page is public. |
| `reader::tests::should_not_request_robots_txt_unless_enabled` | 5 | Preserves the maintainer's default-off robots policy, including fail-closed unreachable robots when enabled; see [Reading pages](README.md#reading-pages). |
| `reader::tests::should_refuse_private_urls_before_fetching` | 3 | Direct page reads reject loopback, metadata, and mapped private targets before network access. |
| `reader::tests::should_report_paywall_consent_and_error_pages_when_rendered` | 4 | Rendered fixtures retain distinct paywall, consent, login, HTTP-error, and challenge outcomes with final URLs. |
| `reader::tests::should_report_no_main_text_with_final_url_when_page_is_really_empty` | 4 | An empty JS shell becomes an honest no-main-text result rather than fabricated article content. |
| `reader::tests::should_accept_article_when_it_mentions_walls` | 4 | An article discussing walls is not misclassified and discarded as an actual access wall. |
| `reader::tests::should_report_render_timeout_when_renderer_does_not_finish` | 4 | A stuck renderer is bounded and reported as a render timeout. |
| `reader::tests::should_accept_rendered_article_when_renderer_succeeds` | 1 | The renderer-to-extraction chain returns real article text on success. |
| `reader::tests::should_read_a_blocked_page_once_in_the_browser` | 1 | Eligible 401/403/challenge reads can recover through one browser render while a 404 remains final. |
| `reader::tests::should_keep_the_block_when_the_browser_is_challenged_too_or_off` | 4 | Browser failure, timeout, or opt-out preserves the original block instead of pretending the page was read. |
| `browser::tests::should_never_use_a_browsers_own_profile` | 3 | Octos cannot open or mutate the user's real Chrome/Edge profile. |
| `browser::tests::should_leave_the_browser_out_after_it_fails_to_start` | 4 | A failed browser launch marks the capability unavailable so later work does not repeatedly rely on it. |
| `browser::tests::should_not_claim_a_challenge_is_shown_when_it_could_not_be` | 4 | A failed handoff is not recorded as shown and can be attempted again honestly. |
| `browser::tests::should_not_read_pages_that_end_on_private_addresses` | 3 | The browser's final-page gate rejects loopback/internal destinations. |

## SSRF and DNS pinning

| Test | Category | Property guarded |
|---|---:|---|
| `net::tests::should_block_private_urls_without_network` | 3 | URL and literal-address checks reject local, metadata, non-HTTP, and script targets without network access. |
| `net::tests::should_fail_closed_when_dns_answer_set_is_empty` | 3 | An empty DNS answer cannot bypass pinning and trigger an unpinned connect-time re-resolution. |
| `net::tests::should_block_answer_set_containing_private_ip` | 3 | A mixed public/private DNS answer set is rejected as a rebinding attempt. |
| `net::tests::pinned_get_runs_the_pre_check_before_any_dns` | 3 | A denied host is stopped before DNS resolution, not merely after a network request has begun. |

## Sandboxed engine and installed-engine boundaries

| Test | Category | Property guarded |
|---|---:|---|
| `metasearch::sandbox::tests::should_refuse_a_host_outside_the_manifest` | 3 | Both generated and literal requests reject host look-alikes, wrong ports/schemes, credentials, and local-file targets. |
| `metasearch::sandbox::tests::should_have_no_tool_module_and_refuse_host_headers` | 3 | Engine scripts cannot invoke host tools or override host-owned identity headers. |
| `metasearch::sandbox::tests::should_refuse_a_browser_load_the_manifest_did_not_declare` | 3 | Scripts cannot acquire browser-rendering capability unless the manifest explicitly declares it. |
| `metasearch::sandbox::tests::should_stop_runaway_scripts` | 3 | An infinite-loop engine is stopped by the bounded runtime. |
| `metasearch::sandbox::tests::should_stop_scripts_that_exhaust_heap_or_strings` | 3 | Unbounded script memory/string growth fails instead of exhausting the host. |
| `metasearch::sandbox::tests::should_enforce_host_function_call_budgets_and_bridge_bounds` | 3 | Host-function call counts and script/host JSON payload sizes remain bounded. |
| `metasearch::manifest::tests::should_reject_bad_manifests` | 3 | Untrusted manifests cannot declare private/internal hosts, unsafe documentation URLs, unsupported fields, or invalid limits. |
| `metasearch::registry::tests::should_load_only_engines_the_host_pinned` | 3 | Directory writers cannot self-pin or load a digest-mismatched engine; only an external host pin admits it. |
| `metasearch::registry::tests::should_not_let_a_directory_engine_shadow_a_builtin_by_default` | 3 | An installed engine cannot replace a built-in unless the host explicitly enables override. |
| `metasearch::impersonate::tests::should_keep_the_private_address_protection` | 3 | The alternate HTTP client preserves the same SSRF block instead of becoming a bypass path. |

## robots.txt protocol and recovery

| Test | Category | Property guarded |
|---|---:|---|
| `robots::tests::should_disallow_and_allow_by_longest_match` | 4 | Allow/disallow precedence follows longest-match semantics rather than rule order. |
| `robots::tests::should_prefer_the_group_naming_our_token` | 4 | Octos obeys its own user-agent group and crawl delay without applying it to other clients. |
| `robots::tests::should_cache_robots_per_origin_and_fetch_once` | 4 | Origin caching prevents repeated robots fetches while unreachable robots fail closed. |
| `robots::tests::should_refetch_unreachable_robots_after_ttl_but_keep_parsed` | 4 | Recovery after an outage refetches robots while valid parsed entries survive a later outage. |

## Normalization and data integrity

| Test | Category | Property guarded |
|---|---:|---|
| `metasearch::merge::tests::should_merge_near_duplicate_titles_and_prefer_the_publisher_url` | 4 | Near-duplicate provider stories merge without losing the publisher URL, including CJK titles. |
| `metasearch::merge::tests::should_take_url_and_kind_from_the_article_when_a_post_shares_its_story` | 4 | Merging a discussion post with its article preserves the article's citable URL and kind. |
| `extract::tests::should_extract_main_text_and_metadata` | 4 | Real article HTML yields body text and language/site/date/canonical metadata without navigation boilerplate. |
| `extract::tests::should_ignore_non_http_canonicals` | 3 | Untrusted HTML cannot inject a script canonical or replace the article with an unrelated host. |
| `text::tests::flagging_preserves_structure_and_is_lossless_when_cited` | 4 | Sentence flagging and citation markup do not lose or alter the underlying text. |
| `providers::tests::should_reject_a_non_http_or_malformed_searxng_base` | 3 | Operator-supplied SearXNG configuration cannot turn search into a local-file read. |
| `providers::tests::should_report_gdelt_rate_limit_text_as_error` | 4 | GDELT's text throttle response is surfaced as an error rather than an empty successful search. |
| `access::tests::should_report_consent_page_when_redirected_to_a_consent_host` | 4 | Redirects to Google/Yahoo consent hosts remain identifiable and are never clicked through. |
| `access::tests::should_report_redirect_unresolved_when_browser_stays_on_google_news` | 4 | A browser that never reaches the publisher returns redirect-unresolved instead of citing the interstitial as article content. |
| `access::tests::should_recognise_chinese_challenge_pages_and_which_clear_themselves` | 4 | Chinese WAF/slider fixtures distinguish self-clearing interstitials from challenges requiring a person. |
| `plan::tests::should_never_plan_serp_scrapers_by_default` | 5 | Enforces the intentional no-silent-SERP-scraping policy when results-page access is disabled, even if a caller lists those providers; see [Configuration](README.md#configuration). |

## Verification rule for T-0519

Delete only tests absent from this allowlist, and re-run the same `--list` command afterward. Every retained name must still be present; do not revive tests removed by earlier slices. No required-job name, `check-arm`, UPCR, production source, or README change is part of this inventory.
