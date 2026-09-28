use std::sync::Arc;

use chrono::TimeZone;

use super::*;
use crate::metasearch::registry::{Engine, EngineOrigin};
use crate::metasearch::{Config, Fetch, FetchFuture, HttpRequest, HttpResponse, Registry};
use crate::reader::ReaderConfig;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap()
}

fn news_scope() -> Scope {
    Scope::from_grant(&json!({
        "langs": ["en", "zh"],
        "regions": ["US", "TW"],
        "domains_deny": ["tabloid.example"],
        "max_age_days": 30,
        "categories": ["news"],
        "max_results": 8
    }))
    .unwrap()
}

#[test]
fn should_parse_grants_and_reject_bad_ones() {
    let s = news_scope();
    assert_eq!(s.langs, vec!["en", "zh"]);
    for bad in [
        json!({"langs": ["english!"]}),
        json!({"regions": ["USA"]}),
        json!({"categories": ["shopping"]}),
        json!({"max_results": 0}),
        json!({"unknown": 1}),
    ] {
        assert!(Scope::from_grant(&bad).is_err(), "{bad}");
    }
}

#[test]
fn should_narrow_search_calls_to_the_grant() {
    let s = news_scope();
    let got = s
        .search_args(
            &json!({"query": "AI regulation", "query_by_lang": {"zh": "人工智能 监管"}, "since": "1y", "count": 50}),
            now(),
        )
        .unwrap();
    assert_eq!(
        got.langs,
        vec!["zh"],
        "query_by_lang languages are searched"
    );
    assert_eq!(got.category, "news");
    assert_eq!(got.count, 8, "clamped to max_results");
    assert!(got.since.unwrap().cutoff >= now() - chrono::Duration::days(30));
    assert!(got.notes.iter().any(|n| n.contains("30-day")));
    assert!(
        got.filters
            .domains_deny
            .contains(&"tabloid.example".to_string())
    );

    // No lang asked: the granted ones; no since: the recency limit applies.
    let got = s.search_args(&json!({"query": "typhoon"}), now()).unwrap();
    assert_eq!(got.langs, vec!["en", "zh"]);
    assert!(got.since.is_some());

    // Outside the grant: refused with a reason.
    for (args, why) in [
        (json!({"query": "q", "lang": "fr"}), "language"),
        (json!({"query": "q", "region": "FR"}), "region"),
        (json!({"query": "q", "category": "science"}), "category"),
        (
            json!({"query": "q", "query_by_lang": {"ja": "質問"}}),
            "language",
        ),
        (json!({"lang": "en"}), "query is required"),
    ] {
        let err = s.search_args(&args, now()).unwrap_err();
        assert!(err.contains(why), "{args}: {err}");
    }
}

#[test]
fn should_keep_requested_domains_inside_the_allow_list() {
    let s = Scope::from_grant(&json!({"domains_allow": ["example.org"]})).unwrap();
    let ok = s
        .search_args(
            &json!({"query": "q", "domains_allow": ["news.example.org"]}),
            now(),
        )
        .unwrap();
    assert_eq!(ok.filters.domains_allow, vec!["news.example.org"]);
    let dflt = s.search_args(&json!({"query": "q"}), now()).unwrap();
    assert_eq!(dflt.filters.domains_allow, vec!["example.org"]);
    let err = s
        .search_args(
            &json!({"query": "q", "domains_allow": ["other.com"]}),
            now(),
        )
        .unwrap_err();
    assert!(err.contains("outside"), "{err}");
}

#[test]
fn should_build_scoped_skill_arguments_for_research_and_crawl() {
    let s = news_scope();
    let r = s
        .research_args(
            &json!({"query": "AI regulation", "lang": "en", "depth": 9}),
            now(),
        )
        .unwrap();
    assert_eq!(r["lang"], json!(["en"]));
    assert_eq!(r["category"], "news");
    assert_eq!(r["depth"], 3);
    assert_eq!(r["output"], "items");
    assert!(r["since"].as_str().is_some());
    assert_eq!(r["domains_deny"], json!(["tabloid.example"]));

    assert!(
        s.crawl_args(&json!({"url": "https://a.org/"}))
            .unwrap_err()
            .contains("not in this app's grant")
    );
    let crawl =
        Scope::from_grant(&json!({"max_depth": 2, "max_pages": 20, "domains_deny": ["bad.org"]}))
            .unwrap();
    let c = crawl
        .crawl_args(&json!({"url": "https://docs.example.org/", "max_depth": 5, "max_pages": 500}))
        .unwrap();
    assert_eq!(c["max_depth"], 2);
    assert_eq!(c["max_pages"], 20);
    assert!(
        crawl
            .crawl_args(&json!({"url": "https://bad.org/x"}))
            .is_err()
    );
}

struct Mock;
impl Fetch for Mock {
    fn fetch(&self, _req: HttpRequest) -> FetchFuture<'_> {
        Box::pin(async {
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: json!({"hits": [
                    {"url": "https://news.example.org/a", "title": "Summit opens", "published": "2026-09-26T10:00:00Z", "lang": "en", "source": "Example News"},
                    {"url": "https://tabloid.example/b", "title": "Gossip", "lang": "en"}
                ]})
                .to_string(),
            })
        })
    }
}

fn toolbox() -> Toolbox {
    let manifest = json!({
        "id": "mock", "name": "mock", "categories": ["news"], "hosts": ["api.example.org"],
        "rate_limit": {"min_interval_ms": 10}, "docs_url": ["https://example.org/docs"], "license_note": "test"
    });
    let source = "use mod.net\nuse mod.std.array\nuse mod.std.object\n\nfn build_request(query, opts) {\nreturn net.request({url: \"https://api.example.org/s?q=\" + query})\n}\n\nfn parse_response(response, opts) {\nreturn object.get(response.json, \"hits\", [])\n}\n";
    let mut r = Registry::default();
    r.insert(Engine::load(&manifest.to_string(), source, EngineOrigin::Builtin).unwrap());
    Toolbox::new(
        Metasearch::new(r, Arc::new(Mock), Config::default()),
        Reader::new(ReaderConfig::default()),
    )
}

#[tokio::test]
async fn should_write_scoped_search_items_into_the_app_folder() {
    let dir = std::env::temp_dir().join(format!("octos-toolbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let res = toolbox()
        .search(
            &news_scope(),
            &json!({"query": "summit", "lang": "en"}),
            &dir,
            now(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.items.len(),
        1,
        "denied domain filtered: {:?}",
        res.items
    );
    let item = &res.items[0];
    assert_eq!(item.citation, Some(1));
    assert_eq!(item.engines, vec!["mock"]);
    assert_eq!(item.provider, "metasearch");
    assert!(res.items_file.starts_with(dir.join("research")));
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(&res.items_file).unwrap()).unwrap();
    assert_eq!(doc["schema"], "octos.research.items.v1");
    assert_eq!(doc["items"][0]["url"], "https://news.example.org/a");
    assert!(res.summary.starts_with("1 item(s)"), "{}", res.summary);
    assert!(res.summary.contains("[1] Summit opens"), "{}", res.summary);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn should_refuse_to_read_pages_outside_the_domain_grant() {
    let scope = Scope::from_grant(&json!({"domains_allow": ["example.org"]})).unwrap();
    let dir = std::env::temp_dir();
    let err = toolbox()
        .web_read(
            &scope,
            &json!({"url": "https://elsewhere.com/x"}),
            &dir,
            now(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("outside this app's research grant"), "{err}");
}

#[test]
fn should_check_the_fetched_location_not_only_the_canonical() {
    let scope = Scope::from_grant(&json!({
        "domains_allow": ["example.com"],
        "domains_deny": ["blog.example.com"]
    }))
    .unwrap();
    // Redirected into the denied subdomain; the page names its parent
    // domain as canonical (same host family, so the reader accepts it).
    let err = check_read_location(
        &scope,
        "https://blog.example.com/post",
        "https://example.com/post",
    )
    .unwrap_err();
    assert!(err.contains("outside this app's research grant"), "{err}");
    assert!(check_read_location(&scope, "https://example.com/a", "https://example.com/a").is_ok());
}

#[test]
fn should_not_overwrite_results_written_in_the_same_second() {
    let dir = std::env::temp_dir().join(format!("octos-toolbox-unique-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let a = unique_path(&dir, "search-q-20260927T120000", "items.json");
    std::fs::write(&a, "a").unwrap();
    let b = unique_path(&dir, "search-q-20260927T120000", "items.json");
    assert_ne!(a, b);
    assert!(
        b.ends_with("search-q-20260927T120000-2.items.json"),
        "{b:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn should_describe_the_four_toolbox_tools() {
    let specs = tool_specs();
    let names: Vec<&str> = specs.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["search", "deep_research", "web_read", "deep_crawl"]
    );
    assert_eq!(specs[3]["capability"], "crawl");
    for s in &specs {
        assert_eq!(s["input_schema"]["type"], "object");
    }
}
