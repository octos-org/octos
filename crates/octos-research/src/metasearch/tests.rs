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
                        body: "[]".into(),
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
