//! Live smoke tests, one per key-less engine. Ignored by default (network):
//!
//! ```text
//! cargo test -p octos-research --features http --test engines_live -- --ignored --test-threads=1
//! ```

use std::sync::Arc;

use octos_research::metasearch::{Config, Metasearch, Registry, ReqwestFetch, SearchRequest};

async fn smoke(engine: &str, query: &str, category: &str) {
    let mut registry = Registry::default();
    registry.insert(Registry::builtin().get(engine).unwrap().clone());
    let mut config = Config::default();
    config.enabled.push(engine.to_string());
    let ms = Metasearch::new(registry, Arc::new(ReqwestFetch::new()), config);
    let mut req = SearchRequest::new(query, category);
    req.count = 5;
    let resp = ms.search(&req).await;
    let report = &resp.engines[0];
    assert!(!resp.items.is_empty(), "{engine}: {report:?}");
    for item in &resp.items {
        assert!(item.url.starts_with("http"), "{item:?}");
        assert!(!item.title.is_empty(), "{item:?}");
    }
}

macro_rules! live {
    ($name:ident, $engine:literal, $query:literal, $category:literal) => {
        #[tokio::test]
        #[ignore = "network"]
        async fn $name() {
            smoke($engine, $query, $category).await;
        }
    };
}

live!(gdelt_live, "gdelt", "climate", "news");
live!(
    wikipedia_live,
    "wikipedia",
    "Rust programming language",
    "general"
);
live!(wikidata_live, "wikidata", "Douglas Adams", "general");
live!(arxiv_live, "arxiv", "graph neural networks", "science");
live!(openalex_live, "openalex", "coral reef", "science");
live!(hackernews_live, "hackernews", "rust", "it");
live!(github_live, "github", "tokio", "it");
live!(stackexchange_live, "stackexchange", "tokio select", "it");
live!(mastodon_live, "mastodon", "rustlang", "social");
