//! Core tests with a scripted fetcher (no network).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::TimeZone;
use tokio::time::Instant;

use super::registry::{Engine, EngineOrigin};
use super::*;

/// What the mock does for URLs on one host.
#[derive(Clone)]
enum Behavior {
    Respond(u16, Vec<(String, String)>, String),
    Delay(Duration),
    Fail(String),
}

#[derive(Clone, Default)]
struct MockFetch {
    by_host: Arc<Mutex<BTreeMap<String, Behavior>>>,
    calls: Arc<Mutex<Vec<(Instant, HttpRequest)>>>,
}

impl MockFetch {
    fn on(&self, host: &str, b: Behavior) {
        self.by_host.lock().unwrap().insert(host.to_string(), b);
    }
    fn calls_to(&self, host: &str) -> Vec<(Instant, HttpRequest)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, r)| url::Url::parse(&r.url).unwrap().host_str() == Some(host))
            .cloned()
            .collect()
    }
}

impl Fetch for MockFetch {
    fn fetch(&self, req: HttpRequest) -> FetchFuture<'_> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((Instant::now(), req.clone()));
            let host = url::Url::parse(&req.url)
                .unwrap()
                .host_str()
                .unwrap()
                .to_string();
            let b = self.by_host.lock().unwrap().get(&host).cloned();
            match b {
                Some(Behavior::Respond(status, headers, body)) => Ok(HttpResponse {
                    status,
                    headers,
                    body,
                }),
                Some(Behavior::Delay(d)) => {
                    tokio::time::sleep(d).await;
                    Ok(HttpResponse {
                        status: 200,
                        body: r#"{"hits": []}"#.into(),
                        ..Default::default()
                    })
                }
                Some(Behavior::Fail(e)) => Err(e),
                None => Err(format!("no mock for {host}")),
            }
        })
    }
}

/// A JSON test engine on `host`: `{"hits": [{url, title, published?, lang?}]}`.
const JSON_ENGINE: &str = r#"use mod.net
use mod.std.array
use mod.std.object

fn build_request(query, opts) {
    let u = net.url({base: "https://HOST/search", query: [["q", query], ["n", opts.count]]})
    return net.request({url: u.url})
}

fn parse_response(response, opts) {
    let out = []
    for h in object.get(response.json, "hits", []) {
        array.push(out, h)
    }
    return out
}
"#;

fn test_engine(id: &str, host: &str, extra: serde_json::Value) -> Engine {
    let mut m = serde_json::json!({
        "id": id,
        "name": id,
        "categories": ["news"],
        "hosts": [host],
        "rate_limit": {"min_interval_ms": 1000},
        "docs_url": ["https://example.org/docs"],
        "license_note": "test",
        "timeout_secs": 2
    });
    for (k, v) in extra.as_object().unwrap() {
        m[k] = v.clone();
    }
    Engine::load(
        &m.to_string(),
        &JSON_ENGINE.replace("HOST", host),
        EngineOrigin::Builtin,
    )
    .unwrap()
}

fn hits(items: &[(&str, &str)]) -> String {
    let v: Vec<serde_json::Value> = items
        .iter()
        .map(|(u, t)| serde_json::json!({"url": u, "title": t}))
        .collect();
    serde_json::json!({ "hits": v }).to_string()
}

fn ok(body: String) -> Behavior {
    Behavior::Respond(200, Vec::new(), body)
}

fn search(engines: Vec<Engine>, fetch: &MockFetch, config: Config) -> Metasearch {
    let mut r = Registry::default();
    for e in engines {
        r.insert(e);
    }
    Metasearch::new(r, Arc::new(fetch.clone()), config)
}

fn request(q: &str) -> SearchRequest {
    let mut r = SearchRequest::new(q, "news");
    r.now = Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap();
    r
}

fn status_of(resp: &SearchResponse, engine: &str) -> EngineStatus {
    resp.engines
        .iter()
        .find(|r| r.engine == engine)
        .unwrap_or_else(|| panic!("{engine} not in {:?}", resp.engines))
        .status
}

#[tokio::test(start_paused = true)]
async fn should_return_results_when_one_engine_times_out_and_one_errors() {
    let fetch = MockFetch::default();
    fetch.on(
        "fast.example.org",
        ok(hits(&[
            ("https://a.org/1", "One"),
            ("https://a.org/2", "Two"),
        ])),
    );
    fetch.on("slow.example.org", Behavior::Delay(Duration::from_secs(30)));
    fetch.on(
        "down.example.org",
        Behavior::Respond(500, Vec::new(), "boom".into()),
    );
    fetch.on(
        "gone.example.org",
        Behavior::Fail("connection refused".into()),
    );
    let ms = search(
        vec![
            test_engine("fast", "fast.example.org", serde_json::json!({})),
            test_engine("slow", "slow.example.org", serde_json::json!({})),
            test_engine("down", "down.example.org", serde_json::json!({})),
            test_engine("gone", "gone.example.org", serde_json::json!({})),
        ],
        &fetch,
        Config::default(),
    );

    let t0 = Instant::now();
    let resp = ms.search(&request("q1")).await;
    assert!(
        t0.elapsed() < Duration::from_secs(3),
        "engines ran in parallel: {:?}",
        t0.elapsed()
    );
    assert_eq!(resp.items.len(), 2);
    assert_eq!(status_of(&resp, "fast"), EngineStatus::Ok);
    assert_eq!(status_of(&resp, "slow"), EngineStatus::Timeout);
    assert_eq!(status_of(&resp, "down"), EngineStatus::Error);
    assert_eq!(
        status_of(&resp, "gone"),
        EngineStatus::Error,
        "transport errors count too"
    );

    // The error suspends `down` (30 s base backoff); the timeout does not
    // suspend `slow` on its first occurrence.
    let resp = ms.search(&request("q2")).await;
    assert_eq!(status_of(&resp, "down"), EngineStatus::Suspended);
    assert_eq!(fetch.calls_to("down.example.org").len(), 1);
    assert_eq!(status_of(&resp, "slow"), EngineStatus::Timeout);

    // After the backoff it is tried again and fails again; a third
    // consecutive timeout suspends `slow` too.
    tokio::time::advance(Duration::from_secs(31)).await;
    let resp = ms.search(&request("q3")).await;
    assert_eq!(status_of(&resp, "down"), EngineStatus::Error);
    assert_eq!(status_of(&resp, "slow"), EngineStatus::Timeout);
    let resp = ms.search(&request("q3b")).await;
    assert_eq!(status_of(&resp, "slow"), EngineStatus::Suspended);
    assert_eq!(resp.items.len(), 2, "results still return");

    // The second error doubled the suspension to 60 s.
    tokio::time::advance(Duration::from_secs(45)).await;
    let resp = ms.search(&request("q4")).await;
    assert_eq!(
        status_of(&resp, "down"),
        EngineStatus::Suspended,
        "second backoff is 60 s"
    );
    assert_eq!(fetch.calls_to("down.example.org").len(), 2);
}

#[tokio::test(start_paused = true)]
async fn should_honour_retry_after_on_429() {
    let fetch = MockFetch::default();
    fetch.on(
        "busy.example.org",
        Behavior::Respond(
            429,
            vec![("retry-after".into(), "120".into())],
            "slow down".into(),
        ),
    );
    let ms = search(
        vec![test_engine(
            "busy",
            "busy.example.org",
            serde_json::json!({}),
        )],
        &fetch,
        Config::default(),
    );
    let resp = ms.search(&request("a")).await;
    let r = &resp.engines[0];
    assert_eq!(r.status, EngineStatus::Error);
    assert!(
        r.error.as_deref().unwrap().contains("retry after 120s"),
        "{r:?}"
    );

    // Longer than the 30 s error backoff: Retry-After wins.
    tokio::time::advance(Duration::from_secs(90)).await;
    let resp = ms.search(&request("b")).await;
    assert_eq!(resp.engines[0].status, EngineStatus::Suspended);
    assert_eq!(fetch.calls_to("busy.example.org").len(), 1);

    fetch.on("busy.example.org", ok(hits(&[("https://b.org/1", "Back")])));
    tokio::time::advance(Duration::from_secs(31)).await;
    let resp = ms.search(&request("c")).await;
    assert_eq!(resp.engines[0].status, EngineStatus::Ok);
    let calls = fetch.calls_to("busy.example.org");
    assert!(calls[1].0 - calls[0].0 >= Duration::from_secs(120));
}

#[tokio::test(start_paused = true)]
async fn should_space_requests_per_host_at_the_declared_rate() {
    let fetch = MockFetch::default();
    fetch.on("rl.example.org", ok(hits(&[("https://c.org/1", "C")])));
    let ms = search(
        vec![test_engine(
            "rl",
            "rl.example.org",
            serde_json::json!({"rate_limit": {"min_interval_ms": 4000}, "cache_ttl_secs": 1}),
        )],
        &fetch,
        Config::default(),
    );
    for q in ["x", "y", "z"] {
        ms.search(&request(q)).await;
    }
    let calls = fetch.calls_to("rl.example.org");
    assert_eq!(calls.len(), 3);
    for w in calls.windows(2) {
        assert!(
            w[1].0 - w[0].0 >= Duration::from_secs(4),
            "{:?}",
            w[1].0 - w[0].0
        );
    }

    // A slot that would miss the deadline is skipped, not waited for.
    let mut req = request("w");
    req.deadline = Duration::from_secs(2);
    let resp = ms.search(&req).await;
    assert_eq!(resp.engines[0].status, EngineStatus::RateLimited);
}

#[tokio::test(start_paused = true)]
async fn should_never_call_gdelt_more_than_once_per_five_seconds() {
    let fetch = MockFetch::default();
    fetch.on("api.gdeltproject.org", ok(r#"{"articles":[]}"#.into()));
    let mut r = Registry::builtin();
    let gdelt = r.get("gdelt").unwrap().clone();
    r = Registry::default();
    r.insert(gdelt);
    let ms = Metasearch::new(r, Arc::new(fetch.clone()), Config::default());
    // Concurrent searches with different queries (no cache hits).
    let reqs: Vec<SearchRequest> = (0..4).map(|i| request(&format!("topic {i}"))).collect();
    futures::future::join_all(reqs.iter().map(|q| ms.search(q))).await;
    let calls = fetch.calls_to("api.gdeltproject.org");
    assert_eq!(calls.len(), 4);
    let mut times: Vec<Instant> = calls.iter().map(|(t, _)| *t).collect();
    times.sort();
    for w in times.windows(2) {
        assert!(w[1] - w[0] >= Duration::from_secs(5), "{:?}", w[1] - w[0]);
    }
}

#[tokio::test(start_paused = true)]
async fn should_reuse_fresh_responses_and_revalidate_stale_ones() {
    let fetch = MockFetch::default();
    fetch.on(
        "etag.example.org",
        Behavior::Respond(
            200,
            vec![("etag".into(), "\"v1\"".into())],
            hits(&[("https://e.org/1", "E")]),
        ),
    );
    let ms = search(
        vec![test_engine(
            "etag",
            "etag.example.org",
            serde_json::json!({"cache_ttl_secs": 60}),
        )],
        &fetch,
        Config::default(),
    );
    ms.search(&request("same")).await;
    let resp = ms.search(&request("same")).await;
    assert!(resp.engines[0].cached);
    assert_eq!(
        fetch.calls_to("etag.example.org").len(),
        1,
        "fresh hit: no request"
    );

    tokio::time::advance(Duration::from_secs(61)).await;
    fetch.on(
        "etag.example.org",
        Behavior::Respond(304, Vec::new(), String::new()),
    );
    let resp = ms.search(&request("same")).await;
    assert!(resp.engines[0].cached);
    assert_eq!(resp.items.len(), 1, "304 reuses the stored body");
    let calls = fetch.calls_to("etag.example.org");
    assert_eq!(calls.len(), 2);
    assert!(
        calls[1]
            .1
            .headers
            .contains(&("if-none-match".into(), "\"v1\"".into()))
    );
}

#[tokio::test(start_paused = true)]
async fn should_refuse_an_engine_that_targets_an_undeclared_host() {
    let fetch = MockFetch::default();
    fetch.on("evil.example.com", ok(hits(&[("https://x.org/", "X")])));
    let manifest = test_engine("sneaky", "api.example.org", serde_json::json!({})).manifest;
    let engine = Engine::load(
        &serde_json::to_string(&manifest).unwrap(),
        &JSON_ENGINE.replace("HOST", "evil.example.com"),
        EngineOrigin::Builtin,
    )
    .unwrap();
    let ms = search(vec![engine], &fetch, Config::default());
    let resp = ms.search(&request("q")).await;
    let r = &resp.engines[0];
    assert_eq!(r.status, EngineStatus::Error);
    assert!(
        r.error
            .as_deref()
            .unwrap()
            .contains("not declared in the engine manifest"),
        "{r:?}"
    );
    assert!(
        fetch.calls_to("evil.example.com").is_empty(),
        "no request left the sandbox"
    );
}

#[tokio::test(start_paused = true)]
async fn should_attach_keys_in_the_host_and_skip_keyed_engines_without_one() {
    let fetch = MockFetch::default();
    fetch.on("keyed.example.org", ok(hits(&[("https://k.org/1", "K")])));
    let keyed = || {
        test_engine(
            "keyed",
            "keyed.example.org",
            serde_json::json!({"needs_key": true, "auth": {"header": {"name": "X-Token"}}}),
        )
    };
    let ms = search(vec![keyed()], &fetch, Config::default());
    assert!(
        ms.search(&request("q")).await.engines.is_empty(),
        "no key: not planned"
    );

    let mut config = Config::default();
    config.keys.insert("keyed".into(), "s3cret".into());
    let ms = search(vec![keyed()], &fetch, config);
    let resp = ms.search(&request("q")).await;
    assert_eq!(resp.engines[0].status, EngineStatus::Ok);
    let (_, req) = &fetch.calls_to("keyed.example.org")[0];
    assert!(req.headers.contains(&("x-token".into(), "s3cret".into())));
    assert!(
        req.headers
            .contains(&("user-agent".into(), crate::USER_AGENT.into()))
    );
}

#[tokio::test(start_paused = true)]
async fn should_check_robots_txt_only_when_the_operator_turns_it_on() {
    let fetch = MockFetch::default();
    fetch.on(
        "feed.example.org",
        Behavior::Respond(200, Vec::new(), "User-agent: *\nDisallow: /search\n".into()),
    );
    let engine = || {
        test_engine(
            "feed",
            "feed.example.org",
            serde_json::json!({"robots": true}),
        )
    };

    // Default: robots.txt is not fetched or applied.
    let ms = search(vec![engine()], &fetch, Config::default());
    let resp = ms.search(&request("q")).await;
    assert_ne!(resp.engines[0].status, EngineStatus::Robots);
    let calls = fetch.calls_to("feed.example.org");
    assert!(calls.iter().all(|(_, r)| !r.url.ends_with("/robots.txt")));

    // Operator opt-in: the disallow rule wins.
    let fetch = MockFetch::default();
    fetch.on(
        "feed.example.org",
        Behavior::Respond(200, Vec::new(), "User-agent: *\nDisallow: /search\n".into()),
    );
    let config = Config {
        respect_robots: true,
        ..Config::default()
    };
    let ms = search(vec![engine()], &fetch, config);
    let resp = ms.search(&request("q")).await;
    assert_eq!(resp.engines[0].status, EngineStatus::Robots);
    let calls = fetch.calls_to("feed.example.org");
    assert_eq!(calls.len(), 1);
    assert!(calls[0].1.url.ends_with("/robots.txt"));
}

#[tokio::test(start_paused = true)]
async fn should_merge_across_engines_and_apply_lang_since_and_domain_filters() {
    let fetch = MockFetch::default();
    let a = serde_json::json!({"hits": [
        {"url": "https://www.news.example/story?utm_source=a", "title": "Summit opens", "published": "2026-09-27T08:00:00Z", "lang": "en"},
        {"url": "https://old.example/x", "title": "Old", "published": "2026-01-01", "lang": "en"},
        {"url": "https://blocked.example/y", "title": "Blocked", "lang": "en"},
        {"url": "https://fr.example/z", "title": "Sommet", "lang": "fr"}
    ]});
    let b = serde_json::json!({"hits": [
        {"url": "http://news.example/story", "title": "Summit opens (discussion)", "published": 1790496000}
    ]});
    fetch.on("a.example.org", ok(a.to_string()));
    fetch.on("b.example.org", ok(b.to_string()));
    let ms = search(
        vec![
            test_engine("ea", "a.example.org", serde_json::json!({})),
            test_engine("eb", "b.example.org", serde_json::json!({})),
        ],
        &fetch,
        Config::default(),
    );
    let mut req = request("summit");
    req.langs = vec!["en".into()];
    req.since = Some(crate::date::Since::parse("7d", req.now).unwrap());
    req.filters.domains_deny = vec!["blocked.example".into()];
    let resp = ms.search(&req).await;
    assert_eq!(resp.items.len(), 1, "{:#?}", resp.items);
    let item = &resp.items[0];
    assert_eq!(item.url, "https://www.news.example/story");
    assert_eq!(item.engines, vec!["ea", "eb"]);
    assert_eq!(item.published.as_deref(), Some("2026-09-27T08:00:00Z"));
    let reasons: BTreeMap<String, String> = resp
        .skipped
        .iter()
        .map(|s| (s.url.clone(), s.reason.clone()))
        .collect();
    assert_eq!(reasons["https://old.example/x"], "older_than_since");
    assert_eq!(reasons["https://blocked.example/y"], "domain_deny");
    assert_eq!(reasons["https://fr.example/z"], "lang");
}

#[tokio::test(start_paused = true)]
async fn should_note_that_keyless_general_search_is_thin() {
    let fetch = MockFetch::default();
    let ms = Metasearch::new(
        Registry::builtin(),
        Arc::new(fetch.clone()),
        Config::default(),
    );
    let mut req = request("rust");
    req.category = "general".into();
    req.engines = Some(Vec::new());
    let resp = ms.search(&req).await;
    assert!(resp.note.as_deref().unwrap().contains("BRAVE_API_KEY"));

    let mut config = Config::default();
    config.keys.insert("brave".into(), "k".into());
    let ms = Metasearch::new(Registry::builtin(), Arc::new(fetch), config);
    assert!(ms.search(&req).await.note.is_none());
}

#[test]
fn should_read_keys_settings_and_contact_from_the_environment() {
    let env: BTreeMap<&str, &str> = BTreeMap::from([
        ("BRAVE_API_KEY", " bk "),
        ("GITHUB_TOKEN", ""),
        ("OCTOS_METASEARCH_MASTODON_INSTANCE", "fosstodon.org"),
        ("OCTOS_RESEARCH_CONTACT", "ops@example.org"),
    ]);
    let lookup = |k: &str| env.get(k).map(|v| v.to_string());
    let extra = BTreeMap::from([("stackexchange".to_string(), "sekey".to_string())]);
    let c = Config::from_env(&Registry::builtin(), lookup, &extra);
    assert_eq!(c.keys.get("brave").map(String::as_str), Some("bk"));
    assert!(!c.keys.contains_key("github"), "empty values are ignored");
    assert_eq!(
        c.keys.get("stackexchange").map(String::as_str),
        Some("sekey")
    );
    assert_eq!(c.settings["mastodon"]["instance"], "fosstodon.org");
    assert_eq!(c.contact.as_deref(), Some("ops@example.org"));
    assert!(enabled(|_| None));
    assert!(!enabled(|_| Some("0".into())));
}

#[tokio::test(start_paused = true)]
async fn should_not_let_a_host_setting_point_at_a_private_address() {
    let fetch = MockFetch::default();
    fetch.on("169.254.169.254", ok(hits(&[("https://x.org/", "X")])));
    fetch.on("public.example.org", ok(hits(&[("https://y.org/", "Y")])));
    let source = r#"use mod.net

fn build_request(query, opts) {
    return net.request({url: "https://" + opts.settings.instance + "/search?q=" + query})
}

fn parse_response(response, opts) {
    return response.json.hits
}
"#;
    let manifest = serde_json::json!({
        "id": "inst",
        "name": "inst",
        "categories": ["news"],
        "hosts": [],
        "rate_limit": {"min_interval_ms": 1000},
        "docs_url": ["https://example.org/docs"],
        "license_note": "test",
        "settings": {"instance": {"description": "host", "default": "public.example.org", "host": true}}
    });
    let engine = || Engine::load(&manifest.to_string(), source, EngineOrigin::Builtin).unwrap();

    let mut config = Config::default();
    config
        .settings
        .entry("inst".into())
        .or_default()
        .insert("instance".into(), "169.254.169.254".into());
    let ms = search(vec![engine()], &fetch, config);
    let resp = ms.search(&request("q")).await;
    let r = &resp.engines[0];
    assert_eq!(r.status, EngineStatus::Error, "{r:?}");
    assert!(
        r.error.as_deref().unwrap().contains("not declared"),
        "{r:?}"
    );
    assert!(fetch.calls_to("169.254.169.254").is_empty());

    // The default public instance works.
    let ms = search(vec![engine()], &fetch, Config::default());
    assert_eq!(
        ms.search(&request("q")).await.engines[0].status,
        EngineStatus::Ok
    );
}

#[tokio::test(start_paused = true)]
async fn should_search_each_language_with_its_own_query() {
    let fetch = MockFetch::default();
    fetch.on("multi.example.org", ok(hits(&[("https://m.org/1", "M")])));
    fetch.on("single.example.org", ok(hits(&[("https://s.org/1", "S")])));
    let ms = search(
        vec![
            test_engine(
                "multi",
                "multi.example.org",
                serde_json::json!({"multi_language": true}),
            ),
            test_engine("single", "single.example.org", serde_json::json!({})),
        ],
        &fetch,
        Config::default(),
    );
    let mut req = request("AI regulation");
    req.langs = vec!["en".into(), "zh-CN".into()];
    req.query_by_lang
        .insert("zh".into(), "人工智能 监管".into());
    assert_eq!(req.query_for(Some("zh-CN")), "人工智能 监管");
    assert_eq!(req.query_for(Some("en")), "AI regulation");
    assert_eq!(req.query_for(None), "AI regulation");
    ms.search(&req).await;

    let q = |host: &str| -> Vec<String> {
        let mut v: Vec<String> = fetch
            .calls_to(host)
            .iter()
            .map(|(_, r)| {
                url::Url::parse(&r.url)
                    .unwrap()
                    .query_pairs()
                    .find(|(k, _)| k == "q")
                    .unwrap()
                    .1
                    .into_owned()
            })
            .collect();
        v.sort();
        v
    };
    // The multi-language engine is split because the queries differ.
    assert_eq!(
        q("multi.example.org"),
        vec!["AI regulation", "人工智能 监管"]
    );
    assert_eq!(
        q("single.example.org"),
        vec!["AI regulation", "人工智能 监管"]
    );
}

#[tokio::test(start_paused = true)]
async fn should_report_failed_requests_when_a_multi_request_engine_partly_fails() {
    let source = r#"use mod.net
use mod.std.array
use mod.std.object

fn build_request(query, opts) {
    return [net.request({url: "https://feed-a.example.org/rss"}), net.request({url: "https://feed-b.example.org/rss"})]
}

fn parse_response(response, opts) {
    let out = []
    for h in object.get(response.json, "hits", []) {
        array.push(out, h)
    }
    return out
}
"#;
    let manifest = serde_json::json!({
        "id": "feeds",
        "name": "feeds",
        "categories": ["news"],
        "hosts": ["feed-a.example.org", "feed-b.example.org"],
        "rate_limit": {"min_interval_ms": 1000},
        "docs_url": ["https://example.org/docs"],
        "license_note": "test",
        "timeout_secs": 2,
        "max_requests": 2
    });
    let engine = Engine::load(&manifest.to_string(), source, EngineOrigin::Builtin).unwrap();
    let fetch = MockFetch::default();
    fetch.on(
        "feed-a.example.org",
        ok(hits(&[("https://a.org/1", "One")])),
    );
    fetch.on(
        "feed-b.example.org",
        Behavior::Respond(500, Vec::new(), "boom".into()),
    );
    let ms = search(vec![engine], &fetch, Config::default());
    let resp = ms.search(&request("q")).await;
    let report = resp.engines.iter().find(|r| r.engine == "feeds").unwrap();
    assert_eq!(report.status, EngineStatus::Ok, "one feed still answered");
    assert_eq!(resp.items.len(), 1);
    let err = report.error.as_deref().unwrap_or_default();
    assert!(err.starts_with("1 of 2 requests failed"), "{err}");
}

/// A Mastodon-like engine (posts, high weight so it would win on score) and
/// a news engine: in `news` every article ranks before any post; posts are
/// kept (signal) and marked, and an item's own `kind` wins.
#[tokio::test(start_paused = true)]
async fn should_rank_posts_after_articles_when_category_is_news() {
    let fetch = MockFetch::default();
    fetch.on(
        "social.example.org",
        ok(hits(&[
            (
                "https://social.example/@a/1",
                "GTA 2 runs on my old Nvidia card",
            ),
            ("https://social.example/@b/2", "Nvidia earnings thread"),
        ])),
    );
    let news = serde_json::json!({"hits": [
        {"url": "https://paper.example/nvidia-results", "title": "Nvidia beats estimates"},
        {"url": "https://forum.example/t/9", "title": "Discussion: results", "kind": "post"},
        {"url": "https://wire.example/nvidia-guidance", "title": "Nvidia raises guidance"}
    ]});
    fetch.on("news.example.org", ok(news.to_string()));
    let engines = || {
        vec![
            test_engine(
                "socialish",
                "social.example.org",
                serde_json::json!({"kind": "post", "weight": 5.0, "categories": ["news", "social"]}),
            ),
            test_engine(
                "newsish",
                "news.example.org",
                serde_json::json!({"categories": ["news", "social"]}),
            ),
        ]
    };
    let ms = search(engines(), &fetch, Config::default());
    let resp = ms.search(&request("nvidia earnings")).await;
    let kinds: Vec<(&str, ItemKind)> = resp
        .items
        .iter()
        .map(|i| (i.url.as_str(), i.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("https://paper.example/nvidia-results", ItemKind::Article),
            ("https://wire.example/nvidia-guidance", ItemKind::Article),
            ("https://social.example/@a/1", ItemKind::Post),
            ("https://social.example/@b/2", ItemKind::Post),
            ("https://forum.example/t/9", ItemKind::Post),
        ],
        "{:#?}",
        resp.items
    );
    assert!(resp.hits().iter().filter(|h| h.kind.is_post()).count() == 3);

    // Outside news, posts are ranked by score like anything else.
    let ms = search(engines(), &fetch, Config::default());
    let mut req = request("nvidia earnings");
    req.category = "social".into();
    let resp = ms.search(&req).await;
    assert_eq!(resp.items[0].url, "https://social.example/@a/1");
    assert_eq!(resp.items[0].kind, ItemKind::Post);
}

#[test]
fn should_parse_engine_kind_and_default_to_article() {
    let post = test_engine("p", "p.example.org", serde_json::json!({"kind": "post"}));
    assert_eq!(post.manifest.kind, ItemKind::Post);
    let plain = test_engine("a", "a.example.org", serde_json::json!({}));
    assert_eq!(plain.manifest.kind, ItemKind::Article);
    assert_eq!(
        Registry::builtin().get("mastodon").unwrap().manifest.kind,
        ItemKind::Post,
        "Mastodon results are posts"
    );
    assert_eq!(
        Registry::builtin().get("hackernews").unwrap().manifest.kind,
        ItemKind::Article,
        "HN stories link to articles; text posts mark themselves"
    );
}

/// An engine on `host` that answers after `delay` with `items`.
fn delayed(fetch: &MockFetch, host: &str, delay: Duration) {
    fetch.on(host, Behavior::Delay(delay));
}

#[tokio::test(start_paused = true)]
async fn should_drop_a_slow_engine_when_the_others_have_answered() {
    let fetch = MockFetch::default();
    for (host, url) in [
        ("a.example.org", "https://a.org/1"),
        ("b.example.org", "https://b.org/1"),
        ("c.example.org", "https://c.org/1"),
    ] {
        fetch.on(host, ok(hits(&[(url, "Story")])));
    }
    // GDELT-like: answers only after 9 s, well inside its own timeout.
    delayed(&fetch, "slow.example.org", Duration::from_secs(9));
    let ms = search(
        vec![
            test_engine("a", "a.example.org", serde_json::json!({})),
            test_engine("b", "b.example.org", serde_json::json!({})),
            test_engine("c", "c.example.org", serde_json::json!({})),
            test_engine(
                "slow",
                "slow.example.org",
                serde_json::json!({"timeout_secs": 15}),
            ),
        ],
        &fetch,
        Config::default(),
    );
    let t0 = Instant::now();
    let resp = ms.search(&request("q")).await;
    let took = t0.elapsed();
    assert!(
        took >= DEFAULT_STRAGGLER_GRACE && took < DEFAULT_STRAGGLER_GRACE + Duration::from_secs(1),
        "{took:?}"
    );
    assert_eq!(resp.items.len(), 3);
    let slow = resp.engines.iter().find(|r| r.engine == "slow").unwrap();
    assert_eq!(slow.status, EngineStatus::Timeout);
    assert!(
        slow.error.as_deref().unwrap().contains("soft deadline"),
        "{slow:?}"
    );
    // Reports keep the plan order.
    let order: Vec<&str> = resp.engines.iter().map(|r| r.engine.as_str()).collect();
    assert_eq!(order, ["a", "b", "c", "slow"]);

    // Without a grace the search waits for it.
    let mut req = request("q2");
    req.straggler_grace = None;
    let t0 = Instant::now();
    let resp = ms.search(&req).await;
    assert!(t0.elapsed() >= Duration::from_secs(9));
    assert_eq!(status_of(&resp, "slow"), EngineStatus::Empty);
}

#[tokio::test(start_paused = true)]
async fn should_wait_for_slow_engines_when_no_engine_has_results_yet() {
    let fetch = MockFetch::default();
    fetch.on("a.example.org", ok(hits(&[])));
    fetch.on("b.example.org", ok(hits(&[])));
    fetch.on("c.example.org", ok(hits(&[])));
    delayed(&fetch, "slow.example.org", Duration::from_secs(4));
    let ms = search(
        vec![
            test_engine("a", "a.example.org", serde_json::json!({})),
            test_engine("b", "b.example.org", serde_json::json!({})),
            test_engine("c", "c.example.org", serde_json::json!({})),
            test_engine(
                "slow",
                "slow.example.org",
                serde_json::json!({"timeout_secs": 10}),
            ),
        ],
        &fetch,
        Config::default(),
    );
    let t0 = Instant::now();
    let resp = ms.search(&request("q")).await;
    assert!(t0.elapsed() >= Duration::from_secs(4), "{:?}", t0.elapsed());
    assert_eq!(status_of(&resp, "slow"), EngineStatus::Empty);
}

#[tokio::test(start_paused = true)]
async fn should_give_up_on_gdelt_after_its_manifest_timeout_when_it_answers_slowly() {
    // GDELT answers a throttled IP's requests with a 429 only after ~10 s.
    let gdelt = Registry::builtin().get("gdelt").unwrap().clone();
    assert!(
        (4..=5).contains(&gdelt.manifest.timeout_secs),
        "{}",
        gdelt.manifest.timeout_secs
    );
    let fetch = MockFetch::default();
    delayed(&fetch, "api.gdeltproject.org", Duration::from_secs(10));
    let mut r = Registry::default();
    r.insert(gdelt);
    let ms = Metasearch::new(r, Arc::new(fetch.clone()), Config::default());
    let t0 = Instant::now();
    let resp = ms.search(&request("climate")).await;
    assert!(t0.elapsed() <= Duration::from_secs(5), "{:?}", t0.elapsed());
    assert_eq!(resp.engines[0].status, EngineStatus::Timeout);
    assert_eq!(
        resp.engines[0].error.as_deref(),
        Some("no response within 5s")
    );
}

#[tokio::test(start_paused = true)]
async fn should_suspend_longer_when_an_engine_keeps_timing_out() {
    let fetch = MockFetch::default();
    delayed(&fetch, "slow.example.org", Duration::from_secs(30));
    let ms = search(
        vec![test_engine(
            "slow",
            "slow.example.org",
            serde_json::json!({}),
        )],
        &fetch,
        Config::default(),
    );
    let run = |q: &'static str| {
        let ms = ms.clone();
        async move { status_of(&ms.search(&request(q)).await, "slow") }
    };
    for q in ["q1", "q2", "q3"] {
        assert_eq!(run(q).await, EngineStatus::Timeout);
    }
    assert_eq!(run("q4").await, EngineStatus::Suspended, "30 s after 3");
    tokio::time::advance(Duration::from_secs(31)).await;
    for q in ["q5", "q6", "q7"] {
        assert_eq!(run(q).await, EngineStatus::Timeout);
    }
    tokio::time::advance(Duration::from_secs(45)).await;
    assert_eq!(
        run("q8").await,
        EngineStatus::Suspended,
        "the second run of timeouts suspends for 60 s"
    );
}

#[tokio::test(start_paused = true)]
async fn should_skip_hits_that_do_not_match_the_query_when_the_engine_lists_feeds() {
    let fetch = MockFetch::default();
    fetch.on(
        "feeds.example.org",
        ok(hits(&[
            (
                "https://f24.example/video/terrorist-act",
                "Locals near UK airbase react to 'terrorist act' arrests",
            ),
            (
                "https://npr.example/ai-schools",
                "Welcome to the 'Wild West' of AI in schools",
            ),
            (
                "https://cna.example/eu-ai-act",
                "EU delays parts of its AI Act for high-risk systems",
            ),
        ])),
    );
    fetch.on(
        "search.example.org",
        ok(hits(&[(
            "https://other.example/eu",
            "Searched results are not re-checked",
        )])),
    );
    let ms = search(
        vec![
            test_engine(
                "feeds",
                "feeds.example.org",
                serde_json::json!({"query_match": true}),
            ),
            test_engine("searcher", "search.example.org", serde_json::json!({})),
        ],
        &fetch,
        Config::default(),
    );
    let resp = ms.search(&request("EU AI Act")).await;
    let urls: Vec<&str> = resp.items.iter().map(|i| i.url.as_str()).collect();
    assert!(urls.contains(&"https://cna.example/eu-ai-act"), "{urls:?}");
    assert!(urls.contains(&"https://other.example/eu"), "{urls:?}");
    assert_eq!(urls.len(), 2, "{urls:?}");
    let feeds = resp.engines.iter().find(|r| r.engine == "feeds").unwrap();
    assert_eq!(feeds.hits, 1);
    let skipped: Vec<(&str, &str)> = resp
        .skipped
        .iter()
        .map(|s| (s.url.as_str(), s.reason.as_str()))
        .collect();
    assert!(
        skipped.contains(&("https://f24.example/video/terrorist-act", "query_mismatch")),
        "{skipped:?}"
    );
    assert!(
        skipped.contains(&("https://npr.example/ai-schools", "query_mismatch")),
        "{skipped:?}"
    );
    assert!(
        Registry::builtin()
            .get("publisher_feeds")
            .unwrap()
            .manifest
            .query_match
    );
}

/// A mock that can also render (a host with a browser).
#[derive(Clone, Default)]
struct RenderingFetch(MockFetch);

impl Fetch for RenderingFetch {
    fn fetch(&self, req: HttpRequest) -> FetchFuture<'_> {
        self.0.fetch(req)
    }

    fn render(&self, req: HttpRequest) -> FetchFuture<'_> {
        self.0.fetch(req)
    }

    fn can_render(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn should_leave_out_browser_engines_where_the_host_has_no_browser() {
    let fetch = MockFetch::default();
    fetch.on("a.example.org", ok(hits(&[("https://a.org/1", "Story")])));
    fetch.on("g.example.org", ok(hits(&[("https://g.org/1", "Story")])));
    let engines = || {
        vec![
            test_engine("a", "a.example.org", serde_json::json!({})),
            test_engine(
                "g",
                "g.example.org",
                serde_json::json!({"results_page": true, "renders": true}),
            ),
        ]
    };
    // No browser: not called, not reported, no backoff.
    let resp = search(engines(), &fetch, Config::default())
        .search(&request("q"))
        .await;
    let ids: Vec<&str> = resp.engines.iter().map(|r| r.engine.as_str()).collect();
    assert_eq!(ids, ["a"]);
    assert!(fetch.calls_to("g.example.org").is_empty());

    // With a browser it runs.
    let mut r = Registry::default();
    for e in engines() {
        r.insert(e);
    }
    let ms = Metasearch::new(
        r,
        Arc::new(RenderingFetch(fetch.clone())),
        Config::default(),
    );
    assert_eq!(
        status_of(&ms.search(&request("q2")).await, "g"),
        EngineStatus::Ok
    );
}

#[test]
fn should_let_only_results_page_engines_declare_renders() {
    let load = |extra: serde_json::Value| {
        let mut m = serde_json::json!({
            "id": "g", "name": "g", "categories": ["general"], "hosts": ["g.example.org"],
            "rate_limit": {"min_interval_ms": 1000}, "docs_url": ["https://example.org/docs"],
            "license_note": "test"
        });
        for (k, v) in extra.as_object().unwrap() {
            m[k] = v.clone();
        }
        Engine::load(&m.to_string(), JSON_ENGINE, EngineOrigin::Builtin)
    };
    assert!(
        load(serde_json::json!({"renders": true}))
            .unwrap_err()
            .contains("needs `results_page`")
    );
    assert!(load(serde_json::json!({"results_page": true, "renders": true})).is_ok());
}
