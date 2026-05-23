//! Hybrid search index: BM25 text search + HNSW vector similarity.

use std::collections::HashMap;

use hnsw_rs::prelude::*;

/// Hybrid search index combining BM25 keyword search with HNSW vector similarity.
pub struct HybridIndex {
    /// BM25 inverted index: term -> [(doc_idx, raw_term_count)]
    inverted: HashMap<String, Vec<(usize, u32)>>,
    /// Document lengths (number of tokens per doc)
    doc_lengths: Vec<usize>,
    /// Running total of all document lengths (avoids O(n) recomputation)
    total_len: usize,
    /// Average document length
    avg_dl: f64,
    /// Episode IDs in insertion order
    ids: Vec<String>,
    /// HNSW vector index (None when no embeddings stored)
    hnsw: Option<Hnsw<'static, f32, DistCosine>>,
    /// Which docs have embeddings (by index)
    has_embedding: Vec<bool>,
    /// Expected vector dimension.
    dimension: usize,
    /// Weight for vector similarity in hybrid scoring (0.0-1.0).
    vector_weight: f32,
    /// Weight for BM25 text relevance in hybrid scoring (0.0-1.0).
    bm25_weight: f32,
}

const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

/// Default weight for vector similarity in hybrid scoring.
const DEFAULT_VECTOR_WEIGHT: f32 = 0.7;
/// Default weight for BM25 text relevance in hybrid scoring.
const DEFAULT_BM25_WEIGHT: f32 = 0.3;

/// Per-modality score breakdown returned by [`HybridIndex::search_scored`].
///
/// `combined` is the same weighted-sum score [`HybridIndex::search`] would
/// return — preserved here so the caller can keep using the existing
/// ranking semantics (operator-configured `with_weights` still controls
/// ranking). `bm25` and `vector` expose the underlying per-modality
/// similarity in `[0, 1]` so downstream gates can be modality-aware,
/// admitting a strong single-modality match (e.g. a keyword-perfect
/// episode without a stored embedding) that the combined score would
/// down-weight below the gate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HybridScore {
    /// Weighted-sum score in `[0, 1]`, matching the documented
    /// semantics of [`HybridIndex::search`].
    pub combined: f32,
    /// Normalized BM25 score in `[0, 1]`. `0.0` means the document did
    /// not match any query keyword.
    pub bm25: f32,
    /// Cosine similarity in `[0, 1]`. `0.0` means either the document
    /// has no embedding, the query has no embedding, or the embeddings
    /// are orthogonal.
    pub vector: f32,
}

impl HybridScore {
    /// Best per-modality score for this document. Useful as input to a
    /// modality-aware similarity gate: a document with a perfect BM25
    /// match (1.0) and no embedding still scores 1.0 here, instead of
    /// being capped at `bm25_weight` by the combined score.
    pub fn best_modality(&self) -> f32 {
        self.bm25.max(self.vector)
    }
}

/// HNSW index parameters.
/// max_nb_connection: max edges per node in the graph (higher = more accurate, more memory).
const HNSW_MAX_NB_CONNECTION: usize = 16;
/// HNSW capacity: pre-allocated slots for documents.
const HNSW_CAPACITY: usize = 10_000;
/// HNSW ef_construction: search width during index build (higher = slower build, better recall).
const HNSW_EF_CONSTRUCTION: usize = 200;
/// HNSW max_layer: maximum graph layers.
const HNSW_MAX_LAYER: usize = 16;

/// Per-modality candidate pool size when a `min_best_modality` floor is
/// supplied to [`HybridIndex::search_scored_filtered`].
///
/// The standard `search_scored` path uses `limit * 4` (e.g. 24 for the
/// agent's `limit=6`) which is fine when no floor is applied — the
/// caller doesn't care about candidates outside the top-N anyway. But
/// when a floor IS supplied, codex P2 round 3 flagged that capping the
/// per-modality pool to `limit * 4` reintroduces the dead band one
/// level up: a store with 25+ slightly-stronger vector-only hits AND
/// 25+ slightly-stronger BM25-only hits could push a genuinely
/// hybrid-strong (both modalities moderate, both clearing the floor)
/// candidate out of BOTH per-modality top-24 pools, so the floored
/// search never sees it.
///
/// `FLOOR_PREFILTER_POOL = HNSW_CAPACITY` (= 10_000) fully decouples
/// the prefilter pool from the injection limit: the floor sees every
/// candidate the index can hold. The cost is at most one HNSW
/// `search(..., 10_000, ...)` call per query, which is bounded by the
/// real document count (HNSW caps at HNSW_CAPACITY anyway) and only
/// runs when a caller explicitly asks for the floor.
const FLOOR_PREFILTER_POOL: usize = HNSW_CAPACITY;

/// Compute the BM25 per-modality candidate pool size for a single
/// `search_scored_filtered` call.
///
/// Pulled out as a free helper so a direct unit test can guard the
/// formula against regression (codex P3 round 5: the prior
/// large-`limit` regression test couldn't fail on a fixed-10_000
/// implementation because indexing 10_000+ docs in a unit test is
/// impractical).
///
/// Semantics:
/// - No floor: `limit * 4` matches the standard `search_scored` budget.
/// - Floor supplied: `max(FLOOR_PREFILTER_POOL, limit * 4)` — at
///   least the floor pool (so small-`limit` callers get the
///   contamination-safe pool) AND at least `limit * 4` (so
///   large-`limit` callers aren't capped at the floor pool).
fn bm25_fetch_count(limit: usize, min_best_modality: Option<f32>) -> usize {
    match min_best_modality {
        Some(_) => FLOOR_PREFILTER_POOL.max(limit * 4),
        None => limit * 4,
    }
}

/// Compute the vector per-modality candidate pool size for a single
/// `search_scored_filtered` call. Always saturated at `HNSW_CAPACITY`
/// because the HNSW index can never hold more docs than that —
/// asking for more is wasted work (codex P2 round 5).
fn vector_fetch_count(limit: usize, min_best_modality: Option<f32>) -> usize {
    bm25_fetch_count(limit, min_best_modality).min(HNSW_CAPACITY)
}

impl HybridIndex {
    /// Create a new hybrid index with the given vector dimension.
    pub fn new(dimension: usize) -> Self {
        Self {
            inverted: HashMap::new(),
            doc_lengths: Vec::new(),
            total_len: 0,
            avg_dl: 0.0,
            ids: Vec::new(),
            hnsw: None,
            has_embedding: Vec::new(),
            dimension,
            vector_weight: DEFAULT_VECTOR_WEIGHT,
            bm25_weight: DEFAULT_BM25_WEIGHT,
        }
    }

    /// Returns true if no documents have been indexed.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Set custom hybrid scoring weights. Weights should sum to 1.0.
    pub fn with_weights(mut self, vector_weight: f32, bm25_weight: f32) -> Self {
        self.vector_weight = vector_weight;
        self.bm25_weight = bm25_weight;
        self
    }

    /// Insert a document with optional embedding.
    pub fn insert(&mut self, episode_id: &str, text: &str, embedding: Option<&[f32]>) {
        // Dedup: skip if already indexed (#110)
        if self.ids.contains(&episode_id.to_string()) {
            return; // Already indexed
        }

        // HNSW capacity warnings
        let at_capacity = self.ids.len() >= HNSW_CAPACITY;
        if at_capacity {
            tracing::warn!(
                "HNSW index at capacity ({HNSW_CAPACITY}), indexing {episode_id} in BM25 only — consider rebuilding with a larger capacity"
            );
        } else if self.ids.len() >= HNSW_CAPACITY * 80 / 100 {
            tracing::warn!(
                "HNSW index at {}% capacity ({}/{}) — consider rebuilding with a larger capacity",
                self.ids.len() * 100 / HNSW_CAPACITY,
                self.ids.len(),
                HNSW_CAPACITY
            );
        }

        let doc_idx = self.ids.len();
        self.ids.push(episode_id.to_string());

        // Tokenize and build inverted index
        let tokens = tokenize(text);
        self.doc_lengths.push(tokens.len());

        // Update avg_dl incrementally (O(1) instead of O(n))
        self.total_len += tokens.len();
        self.avg_dl = self.total_len as f64 / self.doc_lengths.len() as f64;

        // Count term frequencies (store raw counts for BM25, #101)
        let mut tf_map: HashMap<&str, u32> = HashMap::new();
        for token in &tokens {
            *tf_map.entry(token.as_str()).or_default() += 1;
        }

        for (term, count) in tf_map {
            self.inverted
                .entry(term.to_string())
                .or_default()
                .push((doc_idx, count));
        }

        // Insert embedding into HNSW if provided, dimension matches, and not at capacity
        let valid_emb = embedding.filter(|e| e.len() == self.dimension);
        let normalized = valid_emb.and_then(l2_normalize);
        let can_insert_hnsw = normalized.is_some() && !at_capacity;
        self.has_embedding.push(can_insert_hnsw);
        if can_insert_hnsw {
            let normalized = normalized.unwrap();
            let hnsw = self.hnsw.get_or_insert_with(|| {
                Hnsw::new(
                    HNSW_MAX_NB_CONNECTION,
                    HNSW_CAPACITY,
                    HNSW_MAX_LAYER,
                    HNSW_EF_CONSTRUCTION,
                    DistCosine,
                )
            });
            hnsw.insert((&normalized, doc_idx));
        }
    }

    /// Tombstone an entry by clearing its ID so search skips it.
    /// Returns true if the episode was found and removed.
    pub fn remove(&mut self, episode_id: &str) -> bool {
        if let Some(pos) = self.ids.iter().position(|id| id == episode_id) {
            self.ids[pos].clear(); // tombstone — HNSW indices stay stable
            true
        } else {
            false
        }
    }

    /// Add an embedding to an existing document (by episode_id).
    /// Returns false if the episode_id is not found or dimension mismatches.
    pub fn add_embedding(&mut self, episode_id: &str, embedding: &[f32]) -> bool {
        if embedding.len() != self.dimension {
            return false;
        }

        let Some(doc_idx) = self.ids.iter().position(|id| id == episode_id) else {
            return false;
        };

        if self.has_embedding[doc_idx] {
            return true; // already has one
        }

        let Some(normalized) = l2_normalize(embedding) else {
            return false; // zero vector cannot be indexed
        };

        // Enforce HNSW_CAPACITY here too (parity with `insert`).
        // Without this, `add_embedding` could grow the HNSW past its
        // nominal capacity (the underlying `hnsw_rs` treats
        // max_elements as a reservation hint, not a hard cap), which
        // codex P2 round 6 flagged as a regression vector for the new
        // `vector_fetch_count.min(HNSW_CAPACITY)` saturation: if HNSW
        // contained more than HNSW_CAPACITY points, the saturated
        // fetch would silently omit them. The strict cap restores the
        // invariant `hnsw.get_nb_point() <= HNSW_CAPACITY` so the
        // saturation rule is correct.
        let hnsw_full = self
            .hnsw
            .as_ref()
            .map(|h| h.get_nb_point() >= HNSW_CAPACITY)
            .unwrap_or(false);
        if hnsw_full {
            tracing::warn!(
                "HNSW index at capacity ({HNSW_CAPACITY}), skipping vector insert for {episode_id} (BM25 retained)"
            );
            // Leave `has_embedding[doc_idx]` as false so BM25-only
            // recall still works and a future rebuild can pick this
            // doc up.
            return false;
        }

        self.has_embedding[doc_idx] = true;
        let hnsw = self.hnsw.get_or_insert_with(|| {
            Hnsw::new(
                HNSW_MAX_NB_CONNECTION,
                HNSW_CAPACITY,
                HNSW_MAX_LAYER,
                HNSW_EF_CONSTRUCTION,
                DistCosine,
            )
        });
        hnsw.insert((&normalized, doc_idx));
        true
    }

    /// Search the index, returning (episode_id, score) pairs sorted by descending score.
    pub fn search(
        &self,
        query_text: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
    ) -> Vec<(String, f32)> {
        if self.ids.is_empty() {
            return Vec::new();
        }

        let fetch_count = limit * 4;

        // BM25 scores
        let bm25_scores = self.bm25_score(query_text, fetch_count);

        // Vector scores (skip if dimension mismatches)
        let valid_query_emb = query_embedding
            .filter(|e| e.len() == self.dimension)
            .and_then(l2_normalize);
        let vector_scores: HashMap<usize, f32> = match (&valid_query_emb, &self.hnsw) {
            (Some(normalized), Some(hnsw)) => {
                let neighbors = hnsw.search(normalized, fetch_count, 30);
                let mut scores: HashMap<usize, f32> = HashMap::new();
                for n in neighbors {
                    // DistCosine returns 1 - cos_sim, so similarity = 1 - distance
                    let sim: f32 = 1.0 - n.distance;
                    scores.insert(n.d_id, sim.max(0.0));
                }
                scores
            }
            _ => HashMap::new(),
        };

        // Merge scores
        let has_vectors = !vector_scores.is_empty();
        let mut combined: HashMap<usize, f32> = HashMap::new();

        if has_vectors {
            for (&idx, &score) in &vector_scores {
                *combined.entry(idx).or_default() += self.vector_weight * score;
            }
            for (&idx, &score) in &bm25_scores {
                *combined.entry(idx).or_default() += self.bm25_weight * score;
            }
        } else {
            // BM25 only
            for (&idx, &score) in &bm25_scores {
                combined.insert(idx, score);
            }
        }

        let mut results: Vec<(String, f32)> = combined
            .into_iter()
            .filter(|(idx, _)| !self.ids[*idx].is_empty()) // skip tombstoned
            .map(|(idx, score)| (self.ids[idx].clone(), score))
            .collect();

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        results
    }

    /// Search the index and return per-modality score breakdowns.
    ///
    /// Like [`Self::search`] but exposes the underlying BM25 and vector
    /// similarity scores separately so downstream gates can be
    /// modality-aware. This lets callers admit a strong single-modality
    /// match (e.g. an older episode without a stored embedding, scoring
    /// 1.0 on BM25 but 0.0 on vector similarity) that would otherwise be
    /// down-weighted by the configured `bm25_weight` / `vector_weight`
    /// — useful for the agent loop's "Relevant Past Experiences"
    /// similarity gate that must not strand keyword-perfect matches
    /// (NEW-06 follow-up).
    ///
    /// Ranking still uses the same configured weighted sum as
    /// [`Self::search`] — this method only adds the per-modality
    /// breakdown so callers can gate independently from ranking.
    pub fn search_scored(
        &self,
        query_text: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
    ) -> Vec<(String, HybridScore)> {
        self.search_scored_filtered(query_text, query_embedding, limit, None)
    }

    /// Like [`Self::search_scored`] but applies a `min_best_modality`
    /// floor on every candidate's [`HybridScore::best_modality`] BEFORE
    /// the `combined`-rank truncation to `limit`.
    ///
    /// Without this pre-truncation floor, a memory store containing
    /// `limit` or more sub-threshold vector-only matches will crowd out
    /// a high-`best_modality` low-`combined` candidate (e.g. a
    /// keyword-perfect older episode where `combined = bm25_weight *
    /// 1.0 ≈ 0.30` falls below a mid-tier `combined = vector_weight *
    /// 0.54 ≈ 0.378`). Codex P2 (PR #1195 review round 2) flagged that
    /// the agent's previous over-fetch-by-4× workaround was just a
    /// numeric band-aid: any store large enough to hold 4× sub-threshold
    /// vector hits would still strand the BM25 winner.
    ///
    /// The floor is applied INSIDE the index where the full per-modality
    /// breakdown is available for ALL candidates that contributed in
    /// either modality (not just the top-`limit`-by-combined). This
    /// guarantees that if ANY candidate in the index clears the floor,
    /// it survives into the returned set (subject to `limit`).
    ///
    /// `min_best_modality == None` matches [`Self::search_scored`]
    /// semantics exactly.
    pub fn search_scored_filtered(
        &self,
        query_text: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
        min_best_modality: Option<f32>,
    ) -> Vec<(String, HybridScore)> {
        if self.ids.is_empty() {
            return Vec::new();
        }

        // When a floor is supplied, decouple the per-modality
        // candidate pool from `limit` so the floor isn't restricted
        // to the top-`limit*4` of either modality. Codex P2 round 3
        // flagged that the standard `limit * 4` pool can still hide a
        // hybrid-strong candidate (moderate-but-floor-passing in both
        // BM25 and vector) outside both per-modality top-N pools.
        // See `FLOOR_PREFILTER_POOL` for the structural rationale.
        //
        // Per-modality candidate pool sizes — see free helpers above
        // for the per-channel rationale (BM25 may need >10_000 for
        // large-`limit` floored exports; HNSW is always saturated at
        // HNSW_CAPACITY because it cannot hold more docs).
        let bm25_pool = bm25_fetch_count(limit, min_best_modality);
        let vector_pool = vector_fetch_count(limit, min_best_modality);

        let bm25_scores = self.bm25_score(query_text, bm25_pool);

        let valid_query_emb = query_embedding
            .filter(|e| e.len() == self.dimension)
            .and_then(l2_normalize);
        let vector_scores: HashMap<usize, f32> = match (&valid_query_emb, &self.hnsw) {
            (Some(normalized), Some(hnsw)) => {
                let neighbors = hnsw.search(normalized, vector_pool, 30);
                let mut scores: HashMap<usize, f32> = HashMap::new();
                for n in neighbors {
                    let sim: f32 = 1.0 - n.distance;
                    scores.insert(n.d_id, sim.max(0.0));
                }
                scores
            }
            _ => HashMap::new(),
        };

        let has_vectors = !vector_scores.is_empty();
        // Collect every doc that contributed in either modality.
        let mut docs: std::collections::HashSet<usize> = std::collections::HashSet::new();
        docs.extend(bm25_scores.keys().copied());
        docs.extend(vector_scores.keys().copied());

        let mut results: Vec<(String, HybridScore)> = docs
            .into_iter()
            .filter(|idx| !self.ids[*idx].is_empty()) // skip tombstoned
            .map(|idx| {
                let bm25 = bm25_scores.get(&idx).copied().unwrap_or(0.0);
                let vector = vector_scores.get(&idx).copied().unwrap_or(0.0);
                // The "combined" score below preserves the documented
                // weighted-sum ranking semantics from `search()` so
                // operator-configured `with_weights` still controls
                // ranking. BM25-only candidates are skipped from the
                // combined score when no vector results exist so the
                // BM25-only branch (`has_vectors == false`) matches
                // `search()` exactly.
                let combined = if has_vectors {
                    self.vector_weight * vector + self.bm25_weight * bm25
                } else {
                    bm25
                };
                (
                    self.ids[idx].clone(),
                    HybridScore {
                        combined,
                        bm25,
                        vector,
                    },
                )
            })
            // Modality-aware floor — applied BEFORE truncation so a
            // high-`best_modality` low-`combined` candidate isn't
            // crowded out by sub-threshold combined-rank hits.
            .filter(|(_, score)| match min_best_modality {
                Some(floor) => score.best_modality() >= floor,
                None => true,
            })
            .collect();

        results.sort_by(|a, b| {
            b.1.combined
                .partial_cmp(&a.1.combined)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        results
    }

    /// Compute BM25 scores for a query, returning top candidates.
    fn bm25_score(&self, query: &str, limit: usize) -> HashMap<usize, f32> {
        let query_tokens = tokenize(query);
        let n = self.ids.len() as f64;
        let mut scores: HashMap<usize, f64> = HashMap::new();

        for token in &query_tokens {
            if let Some(postings) = self.inverted.get(token.as_str()) {
                let df = postings.len() as f64;
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

                for &(doc_idx, raw_tf) in postings {
                    let dl = self.doc_lengths[doc_idx] as f64;
                    let tf_d = raw_tf as f64;
                    let numerator = tf_d * (BM25_K1 + 1.0);
                    let denominator = tf_d + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / self.avg_dl);
                    *scores.entry(doc_idx).or_default() += idf * numerator / denominator;
                }
            }
        }

        // Normalize to [0, 1]. Use epsilon to avoid amplifying noise from near-zero scores.
        let max_score = scores.values().copied().fold(0.0f64, f64::max);
        if max_score < 1e-10 {
            return HashMap::new();
        }

        let mut result: Vec<(usize, f64)> = scores.into_iter().collect();
        result.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        result.truncate(limit);

        result
            .into_iter()
            .map(|(idx, score)| (idx, (score / max_score) as f32))
            .collect()
    }
}

/// Tokenize text: lowercase, split on non-alphanumeric, filter tokens < 2 chars.
fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() >= 2)
        .map(String::from)
        .collect()
}

/// L2-normalize a vector (required for cosine distance).
/// Returns `None` for zero/near-zero vectors that cannot be meaningfully normalized.
fn l2_normalize(v: &[f32]) -> Option<Vec<f32>> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm < f32::EPSILON {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize() {
        let tokens = tokenize("Hello, World! This is a test-123.");
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"world".to_string()));
        assert!(tokens.contains(&"test".to_string()));
        assert!(tokens.contains(&"123".to_string()));
        assert!(tokens.contains(&"this".to_string()));
        assert!(tokens.contains(&"is".to_string()));
        // Single-char "a" should be filtered out
        assert!(!tokens.contains(&"a".to_string()));
    }

    #[test]
    fn test_l2_normalize() {
        let v = vec![3.0, 4.0];
        let n = l2_normalize(&v).unwrap();
        assert!((n[0] - 0.6).abs() < 1e-6);
        assert!((n[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_l2_normalize_zero() {
        let v = vec![0.0, 0.0];
        assert!(l2_normalize(&v).is_none());
    }

    #[test]
    fn test_zero_vector_not_indexed() {
        let mut index = HybridIndex::new(2);
        index.insert("ep1", "hello world", Some(&[0.0, 0.0]));
        // Zero vector should not be marked as having an embedding
        assert!(!index.has_embedding[0]);
        // add_embedding with zero vector should return false
        index.insert("ep2", "test doc", None);
        assert!(!index.add_embedding("ep2", &[0.0, 0.0]));
    }

    #[test]
    fn test_bm25_only_ranking() {
        let mut index = HybridIndex::new(4);
        index.insert("ep1", "rust ownership borrow checker memory safety", None);
        index.insert("ep2", "python web framework django flask", None);
        index.insert("ep3", "rust async tokio runtime concurrency", None);

        let results = index.search("rust memory ownership", None, 3);
        assert!(!results.is_empty());
        // ep1 should rank highest (most query term overlap)
        assert_eq!(results[0].0, "ep1");
    }

    #[test]
    fn test_hybrid_ranking() {
        let mut index = HybridIndex::new(4);

        // ep1: textually relevant, embedding far from query
        index.insert(
            "ep1",
            "rust ownership borrow checker",
            Some(&[1.0, 0.0, 0.0, 0.0]),
        );
        // ep2: textually less relevant, embedding close to query
        index.insert(
            "ep2",
            "python programming language",
            Some(&[0.0, 1.0, 0.0, 0.0]),
        );
        // ep3: moderately relevant both ways
        index.insert("ep3", "rust async programming", Some(&[0.1, 0.9, 0.0, 0.0]));

        // Query embedding is close to ep2/ep3
        let results = index.search("rust programming", Some(&[0.0, 1.0, 0.0, 0.0]), 3);
        assert_eq!(results.len(), 3);
        // With default weights (0.7 vector + 0.3 bm25), ep3 should rank well (has "rust" + close embedding)
    }

    #[test]
    fn test_empty_index() {
        let index = HybridIndex::new(4);
        let results = index.search("anything", None, 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_insert_without_embedding() {
        let mut index = HybridIndex::new(4);
        index.insert("ep1", "hello world", None);
        index.insert("ep2", "hello rust", Some(&[1.0, 0.0, 0.0, 0.0]));

        // Should still work with BM25 for ep1
        let results = index.search("hello", None, 2);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn add_embedding_basic_contract() {
        // Basic happy-path contract for `add_embedding` (NOT the
        // capacity-branch regression — see
        // `add_embedding_returns_false_when_hnsw_at_capacity` for
        // that).
        let mut index = HybridIndex::new(4);
        index.insert("ep1", "alpha beta", None);
        // BM25-only initially.
        assert!(!index.has_embedding[0]);

        // add_embedding succeeds and flips has_embedding to true.
        assert!(index.add_embedding("ep1", &[1.0, 0.0, 0.0, 0.0]));
        assert!(index.has_embedding[0]);

        // Idempotent: already has one.
        assert!(index.add_embedding("ep1", &[0.0, 1.0, 0.0, 0.0]));
        assert!(index.has_embedding[0]);

        // Non-existent episode -> false.
        assert!(!index.add_embedding("nonexistent", &[1.0, 0.0, 0.0, 0.0]));
    }

    #[test]
    fn add_embedding_returns_false_when_hnsw_at_capacity() {
        // Regression for codex P3 round 7: the previous capacity test
        // never made `hnsw_full == true`. This test fills the HNSW
        // index to `HNSW_CAPACITY` via `add_embedding` and then
        // verifies the guard fires:
        // - `add_embedding` returns false past capacity
        // - `has_embedding[doc_idx]` stays false (BM25-only retained)
        // - The HNSW point count does NOT exceed `HNSW_CAPACITY`
        //   (so `vector_fetch_count.min(HNSW_CAPACITY)` saturation is
        //    correct)
        //
        // Cost: 10_000 inserts + 10_001-th add_embedding. Sub-second
        // in release/debug, well within unit-test budget.
        let mut index = HybridIndex::new(4);
        // Fill `HNSW_CAPACITY` BM25-only docs and attach an embedding
        // to each. After this loop, hnsw.get_nb_point() == HNSW_CAPACITY.
        for i in 0..HNSW_CAPACITY {
            let id = format!("ep-{i}");
            index.insert(&id, "text", None);
            assert!(
                index.add_embedding(&id, &[1.0, 0.0, 0.0, 0.0]),
                "doc #{i} below capacity should accept embedding"
            );
        }
        assert_eq!(
            index.hnsw.as_ref().map(|h| h.get_nb_point()),
            Some(HNSW_CAPACITY),
            "HNSW should be filled exactly to capacity by the loop"
        );

        // One more doc + add_embedding past capacity — guard must
        // fire.
        let overflow_id = "ep-overflow";
        index.insert(overflow_id, "text", None);
        let overflow_idx = index
            .ids
            .iter()
            .position(|id| id == overflow_id)
            .expect("overflow doc must be in ids");

        let result = index.add_embedding(overflow_id, &[0.5, 0.5, 0.0, 0.0]);
        assert!(
            !result,
            "add_embedding past capacity must return false (got true)"
        );
        assert!(
            !index.has_embedding[overflow_idx],
            "has_embedding for the overflow doc must remain false so it's still BM25-only retrievable"
        );
        assert_eq!(
            index.hnsw.as_ref().map(|h| h.get_nb_point()),
            Some(HNSW_CAPACITY),
            "HNSW point count must NOT grow past capacity — saturation invariant for vector_fetch_count"
        );
    }

    #[test]
    fn search_scored_exposes_per_modality_breakdown() {
        // The agent loop's similarity gate (MIN_EPISODE_SIMILARITY,
        // currently 0.55) must still accept genuinely relevant
        // single-modality matches (e.g. older episodes without
        // embeddings, where BM25 is the only signal). The new
        // `search_scored` exposes both modalities so the agent can gate
        // on `best_modality()` instead of the combined weighted-sum
        // score that down-weights BM25-only hits to `bm25_weight` (0.3
        // with default weights) when any vector result exists.
        let mut index = HybridIndex::new(4);
        // ep1: keyword-perfect match, NO embedding (old episode).
        index.insert("ep1", "rust ownership borrow checker", None);
        // ep2: irrelevant text, vector close to query.
        index.insert("ep2", "python web flask", Some(&[1.0, 0.0, 0.0, 0.0]));

        let query_emb: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
        let results = index.search_scored("rust ownership", Some(&query_emb), 5);
        let ep1 = results
            .iter()
            .find(|(id, _)| id == "ep1")
            .map(|(_, s)| *s)
            .expect("ep1 should appear in results");
        let ep2 = results
            .iter()
            .find(|(id, _)| id == "ep2")
            .map(|(_, s)| *s)
            .expect("ep2 should appear in results");

        // ep1: BM25-perfect (max-normalized = 1.0), no embedding.
        assert!(
            ep1.bm25 >= 0.99,
            "ep1 should have a near-1.0 BM25 score (got {})",
            ep1.bm25
        );
        assert_eq!(ep1.vector, 0.0, "ep1 has no embedding");
        // Combined still preserves documented weighted-sum semantics:
        // ep1's combined = bm25_weight * 1.0 = ~0.3 with default weights.
        assert!(
            (ep1.combined - 0.3).abs() < 0.05,
            "ep1.combined should equal bm25_weight * 1.0 (got {})",
            ep1.combined
        );
        // best_modality() is the modality-aware floor — the agent gate
        // uses this so BM25-only matches survive the threshold check.
        // A keyword-perfect match (BM25 ~1.0) clears any reasonable gate
        // (current threshold is 0.55, originally 0.35); the test asserts
        // it clears 0.55 so this remains valid after future tightening
        // without spurious failures.
        assert!(
            ep1.best_modality() >= 0.55,
            "ep1.best_modality() should clear the agent's 0.55 gate (got {})",
            ep1.best_modality()
        );

        // ep2: vector-perfect, no keyword overlap.
        assert_eq!(ep2.bm25, 0.0, "ep2 has no keyword match");
        assert!(
            ep2.vector >= 0.99,
            "ep2 should have a near-1.0 vector score (got {})",
            ep2.vector
        );
    }

    #[test]
    fn search_scored_combined_matches_search_for_ranking_parity() {
        // `search_scored` must produce the same combined score that
        // `search` would, so callers that switch don't get different
        // rankings.
        let mut index = HybridIndex::new(4);
        index.insert(
            "ep1",
            "rust ownership borrow checker",
            Some(&[1.0, 0.0, 0.0, 0.0]),
        );
        index.insert("ep2", "python web flask", Some(&[0.0, 1.0, 0.0, 0.0]));
        index.insert("ep3", "rust async programming", Some(&[0.5, 0.5, 0.0, 0.0]));

        let q = "rust programming";
        let q_emb: [f32; 4] = [0.7, 0.3, 0.0, 0.0];
        let plain = index.search(q, Some(&q_emb), 5);
        let scored = index.search_scored(q, Some(&q_emb), 5);

        assert_eq!(plain.len(), scored.len());
        for (a, b) in plain.iter().zip(scored.iter()) {
            assert_eq!(a.0, b.0, "ranking order must match");
            assert!(
                (a.1 - b.1.combined).abs() < 1e-5,
                "combined score must match plain score for {}: search={} search_scored.combined={}",
                a.0,
                a.1,
                b.1.combined
            );
        }
    }

    #[test]
    fn search_scored_filtered_pool_decoupled_from_limit_when_floor_supplied() {
        // Regression for codex P2 round 3: when a floor is supplied,
        // the per-modality candidate pool must NOT be capped at
        // `limit * 4`. Otherwise a hybrid-strong candidate (moderate
        // in BOTH BM25 and vector, clearing the floor on at least one)
        // can sit outside both per-modality top-N pools and be
        // invisible to the floor.
        //
        // The test must fail against the OLD (`limit * 4`) path —
        // codex P3 round 4 flagged that the previous test fixture
        // (10 docs / limit=20 / pool=80) couldn't distinguish the old
        // and new paths because the old pool already included every
        // doc.
        //
        // Construct 50 docs, all matching a high-frequency keyword
        // ("alpha") with decreasing term frequency so per-modality
        // BM25 rank is well-defined. Doc #25 ALSO matches a unique
        // keyword ("ferrocene"); the query targets the unique
        // keyword. With `limit=2`:
        //   - OLD path: per-modality pool = limit * 4 = 8. Doc #25 is
        //     at BM25 rank ~26 (because alpha-saturated docs #0..#24
        //     all out-rank it on the alpha term, even though doc #25
        //     is the ONLY ferrocene match). So the old path's pool
        //     does NOT contain doc #25 — invisible to the floor.
        //   - NEW path: per-modality pool = FLOOR_PREFILTER_POOL =
        //     10_000. Doc #25 is in the pool, floor=0.0 admits it,
        //     and it surfaces as the top result.
        let mut index = HybridIndex::new(4);
        for i in 0..50 {
            // Most docs match "alpha" many times. Doc #25 ALSO matches
            // the unique "ferrocene" keyword. By assigning decreasing
            // alpha repetitions, BM25 ranks docs in a known order on
            // the alpha term so doc #25 is firmly outside the top 8.
            let alpha_count = 50 - i;
            let mut text = "alpha ".repeat(alpha_count);
            if i == 25 {
                text.push_str("ferrocene ");
            }
            // Different vector orientation per doc so per-modality
            // vector rank is well-defined too — doc #25 also sits
            // outside the top 8 by vector rank.
            let cos = 1.0 - (i as f32) * 0.01;
            let orth = (1.0 - cos * cos).sqrt();
            let emb = [cos, orth, 0.0, 0.0];
            index.insert(&format!("ep-{i:02}"), text.trim(), Some(&emb));
        }

        // Query targets the unique ferrocene match.
        let q = "ferrocene";
        let q_emb: [f32; 4] = [0.5, 0.866, 0.0, 0.0]; // arbitrary direction

        // With the NEW path: per-modality BM25 pool is
        // FLOOR_PREFILTER_POOL=10_000, large enough to include doc
        // #25, so the floor admits it.
        let filtered = index.search_scored_filtered(q, Some(&q_emb), 2, Some(0.0));
        assert!(
            filtered.iter().any(|(id, _)| id == "ep-25"),
            "doc #25 (the unique ferrocene match) must reach the floored search even though \
             alpha-saturated docs crowd it out of the per-modality top-8 BM25 pool that the \
             old `limit * 4` path would have used. Result IDs: {:?}",
            filtered
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        );

        // Sanity: with NO floor and limit=2, the standard path returns
        // at most 2 results.
        let unfiltered = index.search_scored_filtered(q, Some(&q_emb), 2, None);
        assert!(
            unfiltered.len() <= 2,
            "standard limit=2 path returns at most 2 results"
        );
    }

    #[test]
    fn bm25_fetch_count_formula_guards_floor_and_large_limit() {
        // Direct unit test for the per-modality pool formula. This is
        // the regression guard codex P3 round 5 asked for: tested via
        // the helper function so we don't need to index 10_000+ docs
        // to detect a regression to fixed `FLOOR_PREFILTER_POOL`.

        // No floor: pool is `limit * 4`, matching the standard
        // `search_scored` budget.
        assert_eq!(bm25_fetch_count(6, None), 24);
        assert_eq!(bm25_fetch_count(100, None), 400);
        // A huge no-floor limit still gets `limit * 4` (no floor cap).
        assert_eq!(bm25_fetch_count(100_000, None), 400_000);

        // Small `limit` with floor: pool floors at FLOOR_PREFILTER_POOL
        // so the contamination-safe BM25-only recall guarantee holds
        // even for tiny limits (codex round 3).
        assert_eq!(bm25_fetch_count(6, Some(0.55)), FLOOR_PREFILTER_POOL);
        assert_eq!(bm25_fetch_count(1, Some(0.0)), FLOOR_PREFILTER_POOL);
        // Right at the crossover: limit*4 = FLOOR_PREFILTER_POOL.
        assert_eq!(
            bm25_fetch_count(FLOOR_PREFILTER_POOL / 4, Some(0.55)),
            FLOOR_PREFILTER_POOL
        );

        // Large `limit` with floor: pool expands with `limit * 4` so
        // export jobs aren't capped at FLOOR_PREFILTER_POOL (codex
        // round 4). This is the regression the previous test fixture
        // couldn't reach because it only indexed 31 docs.
        assert_eq!(bm25_fetch_count(3_000, Some(0.0)), 12_000);
        assert_eq!(bm25_fetch_count(100_000, Some(0.0)), 400_000);
    }

    #[test]
    fn vector_fetch_count_saturates_at_hnsw_capacity() {
        // Regression for codex P2 round 5: vector-side fetch must not
        // exceed `HNSW_CAPACITY` since the HNSW index can never hold
        // more docs than that — asking for more is wasted work.

        // Small limit, with or without floor: vector pool == bm25
        // pool because both are below HNSW_CAPACITY.
        assert_eq!(vector_fetch_count(6, None), 24);
        assert_eq!(vector_fetch_count(6, Some(0.55)), HNSW_CAPACITY);

        // Large limit with floor: BM25 pool expands to limit*4, but
        // vector pool MUST saturate at HNSW_CAPACITY.
        assert_eq!(bm25_fetch_count(100_000, Some(0.0)), 400_000);
        assert_eq!(vector_fetch_count(100_000, Some(0.0)), HNSW_CAPACITY);
        assert_eq!(vector_fetch_count(50_000, Some(0.0)), HNSW_CAPACITY);

        // Large limit without floor: same saturation rule.
        assert_eq!(vector_fetch_count(100_000, None), HNSW_CAPACITY);
    }

    #[test]
    fn search_scored_filtered_none_floor_matches_unfiltered() {
        // `search_scored_filtered` with `min_best_modality == None` must
        // be a no-op wrapper around `search_scored` so callers can
        // adopt the floor API without changing existing behavior.
        let mut index = HybridIndex::new(4);
        index.insert("ep1", "alpha beta", Some(&[1.0, 0.0, 0.0, 0.0]));
        index.insert("ep2", "gamma delta", Some(&[0.0, 1.0, 0.0, 0.0]));
        index.insert("ep3", "alpha gamma", Some(&[0.7, 0.3, 0.0, 0.0]));

        let q = "alpha";
        let q_emb: [f32; 4] = [0.8, 0.2, 0.0, 0.0];
        let plain = index.search_scored(q, Some(&q_emb), 5);
        let unfiltered = index.search_scored_filtered(q, Some(&q_emb), 5, None);

        assert_eq!(
            plain.len(),
            unfiltered.len(),
            "no-op floor must preserve length"
        );
        for (a, b) in plain.iter().zip(unfiltered.iter()) {
            assert_eq!(a.0, b.0, "no-op floor must preserve order");
            assert!((a.1.combined - b.1.combined).abs() < 1e-5);
        }
    }

    #[test]
    fn search_scored_filtered_admits_bm25_only_winner_across_large_noise() {
        // Regression for codex P2 round-2 (the storage-layer fix):
        // simulate a memory store where the BM25-perfect older episode
        // is buried under many sub-threshold vector-only candidates.
        // With combined-rank truncation BEFORE filtering, the BM25
        // winner would be stranded. The floor applied inside the index
        // (before truncation) guarantees it survives — this is the
        // contamination-safe BM25-only recall guarantee.
        //
        // Construct 12 vector-only docs with vector ~0.54 each
        // (sub-threshold for the 0.55 gate). Then one keyword-perfect
        // BM25-only doc with no embedding. Default weights: combined
        // for vector docs ~ 0.7 * 0.54 = 0.378; combined for BM25
        // winner = bm25_weight * 1.0 = 0.30. With `limit=6` and
        // floor=0.55, the result MUST contain the BM25 winner and
        // NONE of the vector noise.
        let mut index = HybridIndex::new(4);
        // Vector-only sub-threshold noise. Each gets a slightly
        // different embedding so they're all valid candidates but
        // all score ~0.54 against the query.
        let query_emb: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
        for i in 0..12 {
            // Pick an embedding whose cosine-with-query is ~0.54: at
            // angle theta with cos(theta) = 0.54, vector (0.54, sqrt(1-0.54^2), 0, 0).
            let cosine_target: f32 = 0.54;
            let orth = (1.0 - cosine_target * cosine_target).sqrt();
            let emb = [cosine_target, orth, 0.0, 0.0];
            index.insert(
                &format!("vec-noise-{i}"),
                &format!("noise topic {i}"),
                Some(&emb),
            );
        }
        // BM25-perfect older episode, no embedding.
        index.insert("bm25-winner", "ferrocene rustacean ownership", None);

        // Query keywords match only "bm25-winner"; query embedding is
        // close to the vector-noise embeddings.
        let q = "ferrocene rustacean ownership";

        // Without the floor: the 12 vector-noise docs at combined~0.378
        // each rank above bm25-winner at combined=0.30 (when has_vectors=true,
        // bm25-winner gets combined = bm25_weight * bm25 = 0.3 * 1.0 = 0.3),
        // so a limit-6 unfiltered call returns only vector noise.
        let unfiltered = index.search_scored_filtered(q, Some(&query_emb), 6, None);
        assert_eq!(
            unfiltered.len(),
            6,
            "without floor, top-6 fills with vector noise"
        );
        let any_winner = unfiltered.iter().any(|(id, _)| id == "bm25-winner");
        assert!(
            !any_winner,
            "without floor, the BM25 winner is crowded out of top-6 by vector noise — \
             this is the dead band codex P2 round 2 flagged"
        );

        // With the floor: vector-noise docs (best_modality ≈ 0.54) are
        // dropped INSIDE the index before truncation, so bm25-winner
        // (best_modality = 1.0) survives.
        let filtered = index.search_scored_filtered(q, Some(&query_emb), 6, Some(0.55));
        let winner = filtered.iter().find(|(id, _)| id == "bm25-winner").expect(
            "BM25 winner must survive the index-level floor regardless of how much vector \
                 noise sits ahead of it in combined-rank order",
        );
        assert!(
            winner.1.bm25 >= 0.99,
            "BM25 winner should have a near-1.0 BM25 score (got {})",
            winner.1.bm25
        );
        assert_eq!(winner.1.vector, 0.0, "BM25 winner has no embedding");
        for (id, score) in &filtered {
            assert!(
                score.best_modality() >= 0.55,
                "every returned candidate must clear the floor (got {} for {})",
                score.best_modality(),
                id
            );
        }
    }
}
