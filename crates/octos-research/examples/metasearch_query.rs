//! Run one metasearch query and print the merged results as JSON.
//!
//! ```text
//! cargo run -p octos-research --features http --example metasearch_query -- <category> <lang[,lang]|-> <query...>
//! ```
//!
//! With `--features browser`, results pages that need a browser (Google)
//! load in the person's browser (see `octos_research::browser`; `OCTOS_BROWSER`
//! picks the mode).
//!
//! Uses the built-in engines with keys and settings from the environment,
//! and prints `{query, category, elapsed_ms, items: [{url, title, engines,
//! score}], engines: [reports]}`.

use std::collections::BTreeMap;

use octos_research::metasearch::{Metasearch, SearchRequest, default_fetch};

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
    let ms = Metasearch::from_env(default_fetch(), &BTreeMap::new());
    let mut req = SearchRequest::new(&query, &category);
    if langs != "-" {
        req.langs = langs.split(',').map(|l| l.trim().to_string()).collect();
    }
    // `ENGINES=google,bing` limits the run to those engines.
    if let Ok(only) = std::env::var("ENGINES") {
        req.engines = Some(only.split(',').map(|e| e.trim().to_string()).collect());
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
    #[cfg(feature = "browser")]
    octos_research::browser::close_shared().await;
}
