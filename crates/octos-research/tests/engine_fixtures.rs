//! Replays every recorded engine fixture (`engines/<id>/fixtures/*.json`):
//! `build_request` must produce the recorded URL and headers, and
//! `parse_response` must turn the recorded body into the recorded items.
//! No network.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use octos_research::metasearch::{
    Config, EngineStatus, Fetch, FetchFuture, HttpRequest, HttpResponse, Metasearch, Registry,
    SearchRequest,
};
use serde_json::Value;

struct Replay {
    body: String,
    seen: Mutex<Vec<HttpRequest>>,
}

impl Fetch for Replay {
    fn fetch(&self, req: HttpRequest) -> FetchFuture<'_> {
        Box::pin(async move {
            // robots.txt (Google News asks for it): not found = no rules.
            if req.url.ends_with("/robots.txt") {
                return Ok(HttpResponse {
                    status: 404,
                    ..Default::default()
                });
            }
            self.seen.lock().unwrap().push(req);
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: self.body.clone(),
            })
        })
    }
}

fn engines_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("engines")
}

fn cases(engine: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(engines_dir().join(engine).join("fixtures"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.retain(|p| p.extension().is_some_and(|e| e == "json"));
    v.sort();
    v
}

async fn replay(engine: &str, case: &Path) {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(case).unwrap()).unwrap();
    let body =
        std::fs::read_to_string(case.with_file_name(doc["body_file"].as_str().unwrap())).unwrap();
    let fetch = Arc::new(Replay {
        body,
        seen: Mutex::new(Vec::new()),
    });
    let mut registry = Registry::default();
    registry.insert(Registry::builtin().get(engine).unwrap().clone());
    let mut config = Config::default();
    config.enabled.push(engine.to_string());
    config.keys.insert("brave".into(), "fixture-key".into());
    let ms = Metasearch::new(registry, fetch.clone(), config);

    let r = &doc["request"];
    let now = chrono::DateTime::parse_from_rfc3339(r["now"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut req = SearchRequest::new(
        r["query"].as_str().unwrap(),
        r["category"].as_str().unwrap(),
    );
    req.now = now;
    req.count = r["count"].as_u64().unwrap() as usize;
    req.langs = r["langs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_string())
        .collect();
    req.since = r["since"]
        .as_str()
        .map(|s| octos_research::date::Since::parse(s, now).unwrap());

    let resp = ms.search(&req).await;
    let name = case.display();
    assert_eq!(resp.engines.len(), 1, "{name}: {:?}", resp.engines);
    assert_eq!(
        resp.engines[0].status,
        EngineStatus::Ok,
        "{name}: {:?}",
        resp.engines[0]
    );

    // build_request
    let seen = fetch.seen.lock().unwrap();
    let sent = &seen[0];
    assert_eq!(
        sent.method,
        doc["http"]["method"].as_str().unwrap(),
        "{name}"
    );
    let expected_url = doc["http"]["url"].as_str().unwrap();
    assert_eq!(sent.url, expected_url, "{name}: request URL");
    for (k, v) in doc["http"]["headers"].as_object().unwrap() {
        assert!(
            sent.headers
                .contains(&(k.clone(), v.as_str().unwrap().to_string())),
            "{name}: header {k} missing from {:?}",
            sent.headers
        );
    }
    assert!(
        sent.headers
            .contains(&("user-agent".into(), octos_research::USER_AGENT.into())),
        "{name}: identifiable User-Agent"
    );

    // parse_response (+ normalization and merge)
    let got: Vec<Value> = resp
        .items
        .iter()
        .map(|i| {
            serde_json::json!({
                "url": i.url,
                "title": i.title,
                "source": i.source,
                "lang": i.lang,
                "published": i.published,
            })
        })
        .collect();
    assert_eq!(Value::Array(got), doc["expect"], "{name}: items");
}

async fn replay_engine(id: &str) {
    let cs = cases(id);
    assert!(!cs.is_empty(), "engine {id} has no fixtures");
    for case in cs {
        replay(id, &case).await;
    }
}

macro_rules! fixture_tests {
    ($($name:ident => $id:literal),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                replay_engine($id).await;
            }
        )*

        #[test]
        fn should_have_a_fixture_test_for_every_builtin_engine() {
            let covered = [$($id),*];
            for e in Registry::builtin().engines() {
                assert!(covered.contains(&e.id()), "no fixture test for {}", e.id());
            }
        }
    };
}

fixture_tests! {
    arxiv_build_and_parse => "arxiv",
    brave_build_and_parse => "brave",
    gdelt_build_and_parse => "gdelt",
    github_build_and_parse => "github",
    google_news_build_and_parse => "google_news",
    hackernews_build_and_parse => "hackernews",
    mastodon_build_and_parse => "mastodon",
    openalex_build_and_parse => "openalex",
    stackexchange_build_and_parse => "stackexchange",
    wikidata_build_and_parse => "wikidata",
    wikipedia_build_and_parse => "wikipedia",
}

#[test]
fn should_document_every_builtin_engine() {
    for e in Registry::builtin().engines() {
        let m = &e.manifest;
        assert!(!m.docs_url.is_empty(), "{}", m.id);
        assert!(!m.license_note.is_empty(), "{}", m.id);
        assert!(
            m.rate_limit.source.is_some(),
            "{}: say where the rate limit comes from",
            m.id
        );
        assert!(
            e.source.contains("Written from") || m.id == "google_news",
            "{}: name the documentation in the script header",
            m.id
        );
    }
}
