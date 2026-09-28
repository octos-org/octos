//! Merge engine results into one ranked list.
//!
//! 1. Hits with the same canonical URL (tracking parameters, fragment,
//!    `www.` and scheme ignored) become one item.
//! 2. Items whose titles are near-duplicates (same story from an aggregator
//!    and from the publisher, or syndicated copies) are merged too.
//! 3. Each item scores `Σ weight(engine) / √(1 + position)` over the engines
//!    that returned it (best position per engine), times a recency factor
//!    `1 + ½·2^(−age / half_life)` when it has a date. Ties keep first-seen
//!    order, so the ranking is stable for the same inputs.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::item::SearchHit;
use crate::urls;

/// One merged, ranked result (octos.research items schema, metasearch form).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetaItem {
    /// Canonical URL.
    pub url: String,
    pub title: String,
    /// Publisher, site, venue or author.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// ISO 8601.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snippet: String,
    /// Engines that returned it, best first.
    pub engines: Vec<String>,
    pub score: f64,
    pub category: String,
    /// Publisher home page for aggregator links (used for domain filters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

impl MetaItem {
    /// As a provider hit, for callers that format or read `SearchHit`s.
    pub fn to_hit(&self) -> SearchHit {
        SearchHit {
            url: self.url.clone(),
            title: self.title.clone(),
            snippet: self.snippet.clone(),
            source: (!self.source.is_empty()).then(|| self.source.clone()),
            source_url: self.source_url.clone(),
            lang: self.lang.clone(),
            published: self.published.clone(),
            provider: super::PROVIDER_ID.to_string(),
            engines: self.engines.clone(),
            score: Some(self.score),
        }
    }
}

/// A hit with where it ranked in its engine's list.
#[derive(Debug, Clone)]
pub struct RankedHit {
    pub hit: SearchHit,
    /// Engine id (also in `hit.provider`).
    pub engine: String,
    /// 0-based position in that engine's results.
    pub position: usize,
    pub weight: f64,
}

/// Ranking knobs.
#[derive(Debug, Clone, Copy)]
pub struct RankOptions {
    pub now: DateTime<Utc>,
    /// Age at which the recency bonus halves.
    pub half_life_days: f64,
}

/// Title tokens for near-duplicate detection: lowercase words of letters
/// and digits; CJK text contributes character bigrams.
pub fn title_tokens(title: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut word = String::new();
    let mut cjk_prev: Option<char> = None;
    for ch in title.to_lowercase().chars() {
        if is_cjk(ch) {
            if !word.is_empty() {
                out.insert(std::mem::take(&mut word));
            }
            if let Some(p) = cjk_prev {
                out.insert(format!("{p}{ch}"));
            }
            cjk_prev = Some(ch);
        } else if ch.is_alphanumeric() {
            cjk_prev = None;
            word.push(ch);
        } else {
            cjk_prev = None;
            if !word.is_empty() {
                out.insert(std::mem::take(&mut word));
            }
        }
    }
    if !word.is_empty() {
        out.insert(word);
    }
    out
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

/// Whether two titles name the same story: Jaccard similarity of their
/// tokens ≥ 0.8, with at least four tokens each so short titles ("Home",
/// "Rust") never merge.
pub fn near_duplicate(a: &HashSet<String>, b: &HashSet<String>) -> bool {
    if a.len() < 4 || b.len() < 4 {
        return false;
    }
    let inter = a.intersection(b).count() as f64;
    let union = (a.len() + b.len()) as f64 - inter;
    union > 0.0 && inter / union >= 0.8
}

fn is_aggregator(url: &str) -> bool {
    urls::domain_of(url).is_some_and(|d| d == "news.google.com")
}

struct Group {
    hits: Vec<RankedHit>,
    first_seen: usize,
}

/// Merge and rank. `hits` may come from engines in any order; the result is
/// sorted by score (desc), ties by first appearance.
pub fn merge(hits: Vec<RankedHit>, category: &str, opts: RankOptions) -> Vec<MetaItem> {
    // Deterministic input order: by position, then engine id.
    let mut hits = hits;
    hits.sort_by(|a, b| a.position.cmp(&b.position).then(a.engine.cmp(&b.engine)));

    // 1. Canonical URL.
    let mut groups: Vec<Group> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for (i, h) in hits.into_iter().enumerate() {
        let key = urls::dedup_key(&h.hit.url);
        match by_key.get(&key) {
            Some(&g) => groups[g].hits.push(h),
            None => {
                by_key.insert(key, groups.len());
                groups.push(Group {
                    hits: vec![h],
                    first_seen: i,
                });
            }
        }
    }

    // 2. Near-duplicate titles (union-find over groups).
    let tokens: Vec<HashSet<String>> = groups
        .iter()
        .map(|g| title_tokens(&g.hits[0].hit.title))
        .collect();
    let mut parent: Vec<usize> = (0..groups.len()).collect();
    fn find(p: &mut [usize], x: usize) -> usize {
        let mut r = x;
        while p[r] != r {
            r = p[r];
        }
        let mut c = x;
        while p[c] != r {
            let n = p[c];
            p[c] = r;
            c = n;
        }
        r
    }
    for i in 0..groups.len() {
        for j in (i + 1)..groups.len() {
            if near_duplicate(&tokens[i], &tokens[j]) {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[rj.max(ri)] = ri.min(rj);
                }
            }
        }
    }
    let mut clusters: BTreeMap<usize, Group> = BTreeMap::new();
    for (i, g) in groups.into_iter().enumerate() {
        let root = find(&mut parent, i);
        match clusters.get_mut(&root) {
            Some(c) => {
                c.hits.extend(g.hits);
                c.first_seen = c.first_seen.min(g.first_seen);
            }
            None => {
                clusters.insert(root, g);
            }
        }
    }

    // 3. Score and pick representative fields.
    let mut items: Vec<(f64, usize, MetaItem)> = clusters
        .into_values()
        .map(|g| {
            let item = build_item(&g.hits, category, opts);
            (item.score, g.first_seen, item)
        })
        .collect();
    items.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    items.into_iter().map(|(_, _, i)| i).collect()
}

fn contribution(h: &RankedHit) -> f64 {
    h.weight / (1.0 + h.position as f64).sqrt()
}

fn build_item(hits: &[RankedHit], category: &str, opts: RankOptions) -> MetaItem {
    // Best contribution per engine.
    let mut per_engine: BTreeMap<&str, f64> = BTreeMap::new();
    for h in hits {
        let c = contribution(h);
        let e = per_engine.entry(h.engine.as_str()).or_insert(0.0);
        if c > *e {
            *e = c;
        }
    }
    let mut engines: Vec<(&str, f64)> = per_engine.into_iter().collect();
    engines.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(b.0))
    });
    let base: f64 = engines.iter().map(|(_, c)| c).sum();

    // Representative: best-contributing hit, preferring a publisher URL
    // over an aggregator redirect.
    let rep = hits
        .iter()
        .max_by(|a, b| {
            let key = |h: &RankedHit| (!is_aggregator(&h.hit.url), contribution(h));
            let (ka, kb) = (key(a), key(b));
            ka.0.cmp(&kb.0)
                .then(ka.1.partial_cmp(&kb.1).unwrap_or(std::cmp::Ordering::Equal))
                // max_by keeps the last maximum; prefer the earlier hit.
                .then(std::cmp::Ordering::Greater)
        })
        .expect("group has hits");
    let first = |f: fn(&SearchHit) -> Option<&String>| -> Option<String> {
        f(&rep.hit)
            .or_else(|| hits.iter().find_map(|h| f(&h.hit)))
            .filter(|s| !s.is_empty())
            .cloned()
    };
    let published = first(|h| h.published.as_ref());
    let recency = published
        .as_deref()
        .and_then(crate::date::parse_timestamp)
        .map(|(dt, _)| {
            let age_days = ((opts.now - dt).num_seconds().max(0) as f64) / 86_400.0;
            1.0 + 0.5 * 0.5f64.powf(age_days / opts.half_life_days.max(0.01))
        })
        .unwrap_or(1.0);
    let snippet = hits
        .iter()
        .map(|h| h.hit.snippet.as_str())
        .max_by_key(|s| s.chars().count())
        .unwrap_or("")
        .to_string();
    let score = (base * recency * 10_000.0).round() / 10_000.0;
    MetaItem {
        url: urls::canonicalize(&rep.hit.url),
        title: rep.hit.title.clone(),
        source: first(|h| h.source.as_ref()).unwrap_or_default(),
        lang: first(|h| h.lang.as_ref()),
        published,
        snippet,
        engines: engines.iter().map(|(e, _)| e.to_string()).collect(),
        score,
        category: category.to_string(),
        source_url: first(|h| h.source_url.as_ref()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn opts() -> RankOptions {
        RankOptions {
            now: Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap(),
            half_life_days: 3.0,
        }
    }

    fn hit(engine: &str, pos: usize, url: &str, title: &str) -> RankedHit {
        RankedHit {
            hit: SearchHit {
                url: url.into(),
                title: title.into(),
                provider: engine.into(),
                ..Default::default()
            },
            engine: engine.into(),
            position: pos,
            weight: 1.0,
        }
    }

    #[test]
    fn should_merge_the_same_canonical_url_across_engines() {
        let items = merge(
            vec![
                hit(
                    "gdelt",
                    0,
                    "https://www.example.com/a?utm_source=x#top",
                    "Story A",
                ),
                hit(
                    "hackernews",
                    3,
                    "http://example.com/a",
                    "Story A (discussion)",
                ),
                hit("gdelt", 1, "https://example.com/b", "Story B"),
            ],
            "news",
            opts(),
        );
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].engines, vec!["gdelt", "hackernews"]);
        assert_eq!(items[0].url, "https://www.example.com/a");
        assert!(items[0].score > items[1].score, "agreement ranks higher");
    }

    #[test]
    fn should_merge_near_duplicate_titles_and_prefer_the_publisher_url() {
        let items = merge(
            vec![
                hit(
                    "google_news",
                    0,
                    "https://news.google.com/rss/articles/CBMi123",
                    "Typhoon makes landfall in southern China, thousands evacuated",
                ),
                hit(
                    "gdelt",
                    2,
                    "https://www.publisher.cn/world/typhoon",
                    "Typhoon makes landfall in southern China; thousands evacuated",
                ),
                hit(
                    "gdelt",
                    3,
                    "https://other.org/x",
                    "Typhoon season outlook for 2027 released",
                ),
            ],
            "news",
            opts(),
        );
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].url, "https://www.publisher.cn/world/typhoon");
        assert_eq!(items[0].engines.len(), 2);

        // CJK near-duplicates merge on character bigrams.
        let a = title_tokens("台风登陆广东 数千人紧急转移");
        let b = title_tokens("台风登陆广东，数千人紧急转移");
        assert!(near_duplicate(&a, &b));
        assert!(!near_duplicate(
            &title_tokens("Rust"),
            &title_tokens("Rust")
        ));
    }

    #[test]
    fn should_carry_engines_and_score_into_hits() {
        let items = merge(
            vec![
                hit("gdelt", 0, "https://a.org/x", "Story"),
                hit("hackernews", 1, "https://a.org/x", "Story"),
            ],
            "news",
            opts(),
        );
        let h = items[0].to_hit();
        assert_eq!(h.provider, "metasearch");
        assert_eq!(h.engines, vec!["gdelt", "hackernews"]);
        assert_eq!(h.score, Some(items[0].score));
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["engines"], serde_json::json!(["gdelt", "hackernews"]));
        let plain = serde_json::to_value(SearchHit::default()).unwrap();
        assert!(plain.get("engines").is_none() && plain.get("score").is_none());
        let out = crate::providers::format_hits("q", &[h]);
        assert!(out.contains("via metasearch: gdelt, hackernews"), "{out}");
    }

    #[test]
    fn should_rank_stably_and_favour_recent_items() {
        let mut old = hit(
            "gdelt",
            0,
            "https://a.org/old",
            "Old story about the summit talks",
        );
        old.hit.published = Some("2026-08-01T00:00:00Z".into());
        let mut new = hit(
            "gdelt",
            1,
            "https://b.org/new",
            "New report on the climate summit",
        );
        new.hit.published = Some("2026-09-27T06:00:00Z".into());
        let tie1 = hit("wikipedia", 0, "https://c.org/1", "Alpha");
        let tie2 = hit("wikidata", 0, "https://d.org/2", "Beta");
        let input = vec![old, new, tie1, tie2];
        let a = merge(input.clone(), "news", opts());
        let mut reversed = input;
        reversed.reverse();
        let b = merge(reversed, "news", opts());
        assert_eq!(a, b, "input order does not change the ranking");
        let pos = |url: &str| a.iter().position(|i| i.url == url).unwrap();
        assert!(
            pos("https://b.org/new") < pos("https://a.org/old"),
            "{a:#?}"
        );
        assert!(
            pos("https://d.org/2") < pos("https://c.org/1"),
            "ties break by engine id order"
        );
    }
}
