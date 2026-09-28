//! Provider parsing from recorded fixtures (no network).

use octos_research::filter::Filters;
use octos_research::item::SearchHit;
use octos_research::providers::{format_hits, parse_feed, parse_gdelt, parse_searxng};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn should_parse_gdelt_artlist_fixture() {
    let hits = parse_gdelt(&fixture("gdelt_artlist.json")).unwrap();
    assert_eq!(hits.len(), 3, "non-http URL dropped: {hits:?}");
    let first = &hits[0];
    assert_eq!(
        first.url,
        "https://elpais.com/clima/2026-09-26/cumbre-acuerdo.html"
    );
    assert_eq!(
        first.title,
        "La cumbre del clima cierra con un acuerdo preliminar"
    );
    assert_eq!(first.source.as_deref(), Some("elpais.com"));
    assert_eq!(first.lang.as_deref(), Some("es"));
    assert_eq!(first.published.as_deref(), Some("2026-09-26T10:15:00Z"));
    assert_eq!(first.provider, "gdelt");
    assert_eq!(hits[1].lang.as_deref(), Some("fr"));
}

#[test]
fn should_parse_google_news_rss_fixture() {
    let hits = parse_feed(&fixture("google_news_search.xml"), "google_news_rss", None).unwrap();
    assert_eq!(hits.len(), 2);
    let h = &hits[0];
    assert_eq!(
        h.title,
        "Turkey launches AI initiative ahead of COP31 climate summit"
    );
    assert!(h.url.starts_with("https://news.google.com/rss/articles/"));
    assert_eq!(h.source.as_deref(), Some("Reuters"));
    assert_eq!(h.source_url.as_deref(), Some("https://www.reuters.com"));
    assert_eq!(h.lang.as_deref(), Some("en-US"), "channel <language>");
    assert_eq!(h.published.as_deref(), Some("2026-09-25T14:05:00Z"));
    assert!(
        h.snippet.is_empty(),
        "headline-only description dropped: {:?}",
        h.snippet
    );
    assert_eq!(hits[1].title, "Climate Summit 2026");
}

#[test]
fn should_parse_atom_feed_fixture() {
    let hits = parse_feed(&fixture("atom_feed.xml"), "feed", Some("de")).unwrap();
    assert_eq!(hits.len(), 1);
    let h = &hits[0];
    assert_eq!(h.url, "https://nachrichten.example/2026/09/26/klimagipfel");
    assert_eq!(h.published.as_deref(), Some("2026-09-26T04:00:00Z"));
    assert_eq!(h.lang.as_deref(), Some("de"));
    assert_eq!(
        h.snippet,
        "Die Delegierten haben sich in der Nacht geeinigt."
    );
}

#[test]
fn should_reject_non_feeds() {
    assert!(parse_feed("<html><body>hi</body></html>", "feed", None).is_err());
}

#[test]
fn should_parse_searxng_fixture() {
    let hits = parse_searxng(&fixture("searxng_news.json")).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].url,
        "https://apnews.com/article/climate-summit-deal-2026"
    );
    assert_eq!(hits[0].published.as_deref(), Some("2026-09-25T21:40:00Z"));
    assert!(hits[0].snippet.starts_with("Negotiators"));
    assert_eq!(hits[0].provider, "searxng");
    assert_eq!(hits[1].published, None);
    assert!(parse_searxng("<html>format not enabled</html>").is_err());
}

#[test]
fn should_filter_fixture_hits_by_language() {
    let mut hits: Vec<SearchHit> = parse_gdelt(&fixture("gdelt_artlist.json")).unwrap();
    hits.extend(parse_searxng(&fixture("searxng_news.json")).unwrap());
    let f = Filters::new(vec!["es".into(), "en".into()], None, vec![], vec![], None).unwrap();
    let (kept, skipped) = f.apply(hits);
    assert!(kept.iter().all(|h| h.lang.as_deref() != Some("fr")));
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].reason, "lang");
    // Unknown-language SearXNG hits are kept.
    assert!(kept.iter().any(|h| h.provider == "searxng"));
    assert!(format_hits("q", &kept).contains("via gdelt"));
}
