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

        // HNSW capacity guard (#109)
        if self.ids.len() >= HNSW_CAPACITY {
            tracing::warn!(
                "HNSW index at capacity ({HNSW_CAPACITY}), skipping insert for {episode_id}"
            );
            return;
        }
        if self.ids.len() >= HNSW_CAPACITY * 80 / 100 {
            tracing::warn!(
                "HNSW index at {}% capacity ({}/{})",
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

        // Insert embedding into HNSW if provided and dimension matches
        let valid_emb = embedding.filter(|e| e.len() == self.dimension);
        let normalized = valid_emb.and_then(l2_normalize);
        self.has_embedding.push(normalized.is_some());
        if let Some(normalized) = normalized {
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
            .map(|(idx, score)| (self.ids[idx].clone(), score))
            .collect();

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
}
