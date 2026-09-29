//! Run one metasearch query and print the merged results as JSON.
//!
//! ```text
//! cargo run -p octos-research --features http --example metasearch_query -- <category> <lang[,lang]|-> <query...>
//! ```
//!
//! Uses the built-in engines with keys and settings from the environment,
//! and prints `{query, category, elapsed_ms, items: [{url, title, engines,
//! score}], engines: [reports]}`.

use std::collections::BTreeMap;
use std::sync::Arc;

use octos_research::metasearch::{Metasearch, ReqwestFetch, SearchRequest};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let category = args.next().unwrap_or_else(|| "general".into());
    let langs = args.next().unwrap_or_else(|| "-".into());
    let query: Vec<String> = args.collect();
    let query = query.join(" ");
    if query.trim().is_empty() {
        eprintln!("usage: metasearch_query <category> <lang[,lang]|-> <query...>");
        std::process::exit(2);
    }
    let ms = Metasearch::from_env(Arc::new(ReqwestFetch::new()), &BTreeMap::new());
    let mut req = SearchRequest::new(&query, &category);
    if langs != "-" {
        req.langs = langs.split(',').map(|l| l.trim().to_string()).collect();
    }
    req.count = 10;
    req.limit = 30;
    let t0 = std::time::Instant::now();
    let resp = ms.search(&req).await;
    let out = serde_json::json!({
        "query": query,
        "category": category,
        "elapsed_ms": t0.elapsed().as_millis() as u64,
        "items": resp.items.iter().map(|i| serde_json::json!({
            "url": i.url, "title": i.title, "engines": i.engines, "score": i.score,
        })).collect::<Vec<_>>(),
        "engines": resp.engines,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
