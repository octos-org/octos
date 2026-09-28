//! Record fixtures for the built-in metasearch engines.
//!
//! ```text
//! cargo run -p octos-research --features http --example record_engine_fixtures [engine ...]
//! ```
//!
//! For each case below this runs the real engine once (at each provider's
//! published rate) and writes `engines/<id>/fixtures/<case>.json` (request,
//! the URL the engine built, expected items) and `<case>.body` (the
//! response). Cases marked `File` replay a stored body instead of calling the
//! provider: Brave needs a key, and news.google.com/robots.txt disallows the
//! RSS feed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use octos_research::metasearch::{
    Config, Fetch, FetchFuture, HttpRequest, HttpResponse, Metasearch, Registry, ReqwestFetch,
    SearchRequest,
};

enum Source {
    Live,
    /// Replay this file (relative to the crate root).
    File(&'static str),
}

struct Case {
    engine: &'static str,
    name: &'static str,
    query: &'static str,
    langs: &'static [&'static str],
    since: Option<&'static str>,
    category: &'static str,
    source: Source,
}

const CASES: &[Case] = &[
    Case {
        engine: "gdelt",
        name: "news_en_zh",
        query: "climate summit",
        langs: &["en", "zh"],
        since: Some("7d"),
        category: "news",
        source: Source::File("engines/gdelt/fixtures/news_en_zh.body"),
    },
    Case {
        engine: "google_news",
        name: "news_en",
        query: "climate summit",
        langs: &["en"],
        since: Some("7d"),
        category: "news",
        source: Source::Live,
    },
    Case {
        engine: "google_news",
        name: "news_zh",
        query: "台风",
        langs: &["zh-CN"],
        since: Some("7d"),
        category: "news",
        source: Source::Live,
    },
    Case {
        engine: "wikipedia",
        name: "general_en",
        query: "Rust programming language",
        langs: &["en"],
        since: None,
        category: "general",
        source: Source::Live,
    },
    Case {
        engine: "wikipedia",
        name: "general_zh",
        query: "台风",
        langs: &["zh"],
        since: None,
        category: "general",
        source: Source::Live,
    },
    Case {
        engine: "wikidata",
        name: "general_en",
        query: "Douglas Adams",
        langs: &["en"],
        since: None,
        category: "general",
        source: Source::Live,
    },
    Case {
        engine: "arxiv",
        name: "science",
        query: "retrieval augmented generation",
        langs: &[],
        since: None,
        category: "science",
        source: Source::Live,
    },
    Case {
        engine: "openalex",
        name: "science_since",
        query: "coral reef bleaching",
        langs: &[],
        since: Some("2025-01-01"),
        category: "science",
        source: Source::Live,
    },
    Case {
        engine: "hackernews",
        name: "news_since",
        query: "rust",
        langs: &["en"],
        since: Some("7d"),
        category: "news",
        source: Source::Live,
    },
    Case {
        engine: "hackernews",
        name: "it",
        query: "tokio async runtime",
        langs: &[],
        since: None,
        category: "it",
        source: Source::Live,
    },
    Case {
        engine: "github",
        name: "it",
        query: "metasearch engine rust",
        langs: &[],
        since: None,
        category: "it",
        source: Source::Live,
    },
    Case {
        engine: "stackexchange",
        name: "it",
        query: "tokio select timeout",
        langs: &[],
        since: None,
        category: "it",
        source: Source::Live,
    },
    Case {
        engine: "mastodon",
        name: "social",
        query: "rustlang",
        langs: &[],
        since: None,
        category: "social",
        source: Source::Live,
    },
    Case {
        engine: "publisher_feeds",
        name: "news_en_zh",
        query: "AI",
        langs: &["en", "zh"],
        since: None,
        category: "news",
        source: Source::Live,
    },
    Case {
        engine: "brave",
        name: "general_synthetic",
        query: "octos agent",
        langs: &["en"],
        since: None,
        category: "general",
        source: Source::File("engines/brave/fixtures/general_synthetic.body"),
    },
];

/// Records every request and response of one search.
#[derive(Clone)]
struct Recorder {
    live: Arc<ReqwestFetch>,
    replay: Option<String>,
    seen: Arc<Mutex<Vec<(HttpRequest, HttpResponse)>>>,
}

impl Fetch for Recorder {
    fn fetch(&self, req: HttpRequest) -> FetchFuture<'_> {
        Box::pin(async move {
            let resp = match &self.replay {
                Some(body) => HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: body.clone(),
                },
                None => self.live.fetch(req.clone()).await?,
            };
            self.seen.lock().unwrap().push((req, resp.clone()));
            Ok(resp)
        })
    }
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let only: Vec<String> = std::env::args().skip(1).collect();
    for case in CASES {
        if !only.is_empty() && !only.iter().any(|o| o == case.engine) {
            continue;
        }
        let replay = match case.source {
            Source::Live => None,
            Source::File(p) => {
                Some(std::fs::read_to_string(crate_dir().join(p)).expect("replay file"))
            }
        };
        let rec = Recorder {
            live: Arc::new(ReqwestFetch::new()),
            replay,
            seen: Arc::default(),
        };
        let mut registry = Registry::default();
        registry.insert(
            Registry::builtin()
                .get(case.engine)
                .expect("engine")
                .clone(),
        );
        let mut config = Config::default();
        config.enabled.push(case.engine.to_string());
        if case.engine == "brave" {
            config.keys.insert("brave".into(), "fixture-key".into());
        }
        let ms = Metasearch::new(registry, Arc::new(rec.clone()), config);

        let now = Utc::now()
            .with_timezone(&Utc)
            .format("%Y-%m-%dT%H:00:00Z")
            .to_string();
        let now = chrono::DateTime::parse_from_rfc3339(&now)
            .unwrap()
            .with_timezone(&Utc);
        let mut req = SearchRequest::new(case.query, case.category);
        req.now = now;
        req.count = 5;
        req.langs = case.langs.iter().map(|l| l.to_string()).collect();
        req.since = case
            .since
            .map(|s| octos_research::date::Since::parse(s, now).unwrap());
        let resp = ms.search(&req).await;
        let report = &resp.engines[0];
        let mut seen = rec.seen.lock().unwrap().clone();
        seen.sort_by(|a, b| a.0.url.cmp(&b.0.url));
        let Some((http, body)) = seen.first().cloned() else {
            eprintln!("{}/{}: no request made: {report:?}", case.engine, case.name);
            continue;
        };
        if body.status != 200 || resp.items.is_empty() {
            eprintln!("{}/{}: not recorded ({report:?})", case.engine, case.name);
            continue;
        }
        let dir = crate_dir()
            .join("engines")
            .join(case.engine)
            .join("fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        let body_file = format!("{}.body", case.name);
        if seen.len() == 1
            && !matches!(case.source, Source::File(p) if Path::new(p).ends_with(&body_file))
        {
            std::fs::write(dir.join(&body_file), &body.body).unwrap();
        }
        // Engines that build several requests (one per feed) record each.
        let exchanges: Vec<serde_json::Value> = if seen.len() > 1 {
            seen.iter()
                .enumerate()
                .map(|(i, (rq, rs))| {
                    let file = format!("{}.{i}.body", case.name);
                    std::fs::write(dir.join(&file), &rs.body).unwrap();
                    serde_json::json!({"method": rq.method, "url": rq.url, "status": rs.status, "body_file": file})
                })
                .collect()
        } else {
            Vec::new()
        };
        let expect: Vec<serde_json::Value> = resp
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
        let headers: BTreeMap<String, String> = http
            .headers
            .iter()
            .filter(|(k, _)| k != "user-agent" && !k.starts_with("x-subscription"))
            .cloned()
            .collect();
        let doc = serde_json::json!({
            "recorded": match case.source {
                Source::Live => format!("live, {}", Utc::now().format("%Y-%m-%d")),
                Source::File(p) => format!("replayed body from {p}"),
            },
            "request": {
                "query": case.query,
                "langs": case.langs,
                "since": case.since,
                "category": case.category,
                "count": 5,
                "now": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            },
            "http": { "method": http.method, "url": http.url, "headers": headers },
            "body_file": body_file,
            "expect": expect,
        });
        let mut doc = doc;
        if !exchanges.is_empty() {
            doc["exchanges"] = serde_json::json!(exchanges);
            doc.as_object_mut().unwrap().remove("body_file");
        }
        std::fs::write(
            dir.join(format!("{}.json", case.name)),
            serde_json::to_string_pretty(&doc).unwrap() + "\n",
        )
        .unwrap();
        println!("{}/{}: {} items", case.engine, case.name, resp.items.len());
        // Each case uses a fresh metasearch, so space them by hand.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
