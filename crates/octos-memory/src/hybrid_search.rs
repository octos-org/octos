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
        if self.ids.is_empty() {
            return Vec::new();
        }

        let fetch_count = limit * 4;

        let bm25_scores = self.bm25_score(query_text, fetch_count);

        let valid_query_emb = query_embedding
            .filter(|e| e.len() == self.dimension)
            .and_then(l2_normalize);
        let vector_scores: HashMap<usize, f32> = match (&valid_query_emb, &self.hnsw) {
            (Some(normalized), Some(hnsw)) => {
                let neighbors = hnsw.search(normalized, fetch_count, 30);
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
    fn search_scored_exposes_per_modality_breakdown() {
        // The agent loop's similarity gate (MIN_EPISODE_SIMILARITY = 0.35)
        // must still accept genuinely relevant single-modality matches
        // (e.g. older episodes without embeddings, where BM25 is the
        // only signal). The new `search_scored` exposes both modalities
        // so the agent can gate on `best_modality()` instead of the
        // combined weighted-sum score that down-weights BM25-only hits
        // to `bm25_weight` (0.3 with default weights) when any vector
        // result exists.
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
        assert!(
            ep1.best_modality() >= 0.35,
            "ep1.best_modality() should clear the agent's 0.35 gate (got {})",
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
}
