//! [`MlxEmbedder`] — the public [`octos_llm::EmbeddingProvider`] implementation.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use eyre::Result;
use octos_llm::EmbeddingProvider;

use crate::plan::{batch_plan, mrl_truncate};

use super::model::GemmaModel;
use super::tokenizer::GemmaTokenizer;

/// Sequences per forward pass. Batching amortizes MLX's per-op dispatch cost
/// (24 blocks × ~15 ops each), which dominates the runtime at these sequence
/// lengths — the marginal cost of a second sequence is far below a second
/// forward pass. Capped so a batch of long episodes cannot balloon the
/// intermediate `[B, L, 4*hidden]` activations.
const DEFAULT_MAX_BATCH: usize = 16;

/// In-process EmbeddingGemma-300M embedder on Apple MLX (Metal).
///
/// `mlx-rs` `Array` is `Send` but not `Sync`, and MLX evaluation is best kept
/// single-threaded, so the model lives behind a `Mutex` — forward passes
/// serialize. Tokenization is `Sync` and happens outside the lock, and a
/// batched call takes ONE lock for the whole batch rather than one per text.
///
/// Cost of a short sequence on Metal, warm, `--release` (a debug build is ~3x
/// slower — do not quote dev-profile timings):
///
/// * batch=1  — ~7 ms per embedding
/// * batch=16 — ~1.2 ms per embedding
///
/// That 6x gap is the point: at batch=1 most of the wall clock is per-op
/// dispatch for ~360 kernel launches (24 blocks x ~15 ops), not arithmetic.
/// The model is small enough that a single sequence never fills the GPU, so
/// batching is what turns latency-bound work into throughput-bound work.
pub struct MlxEmbedder {
    model: Mutex<GemmaModel>,
    tokenizer: GemmaTokenizer,
    native_dim: usize,
    /// MRL output dim (<= `native_dim`); truncate + renormalize when smaller.
    output_dim: usize,
    /// When true, [`EmbeddingProvider::embed`] applies the QUERY prompt; by
    /// default it applies the DOCUMENT prompt (the common indexing case).
    default_query: bool,
    /// Max sequences per forward pass.
    max_batch: usize,
}

impl MlxEmbedder {
    /// Load the model, tokenizer and SentenceTransformers head from a local
    /// directory (the HuggingFace snapshot of
    /// `mlx-community/embeddinggemma-300m-8bit`): `config.json`,
    /// `model.safetensors`, `tokenizer.json`.
    pub fn from_model_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let model = GemmaModel::from_dir(dir)?;
        let native_dim = model.dim();
        let tokenizer = GemmaTokenizer::from_dir(dir)?;
        Ok(Self {
            model: Mutex::new(model),
            tokenizer,
            native_dim,
            output_dim: native_dim,
            default_query: false,
            max_batch: DEFAULT_MAX_BATCH,
        })
    }

    /// Override the number of sequences per forward pass (clamped to >= 1).
    pub fn with_max_batch(mut self, max_batch: usize) -> Self {
        self.max_batch = max_batch.max(1);
        self
    }

    /// Make [`EmbeddingProvider::embed`] treat inputs as queries (default: docs).
    pub fn with_query_default(mut self, query: bool) -> Self {
        self.default_query = query;
        self
    }

    /// Matryoshka (MRL) output dim. Truncates the 768-d embedding to `dim` and
    /// renormalizes. Clamped to `[1, native_dim]`. Typical values: 512/256/128.
    pub fn with_output_dim(mut self, dim: usize) -> Self {
        self.output_dim = dim.clamp(1, self.native_dim);
        self
    }

    pub fn native_dim(&self) -> usize {
        self.native_dim
    }

    /// The active output dimension (== [`EmbeddingProvider::dimension`]).
    pub fn output_dim(&self) -> usize {
        self.output_dim
    }

    /// Embed a batch with an explicit role (`is_query`), returning MRL-truncated
    /// (and renormalized) vectors of length [`Self::dimension`], in input order.
    ///
    /// Texts are tokenized outside the model lock, grouped into length-sorted
    /// batches of at most `max_batch`, and run one forward pass per batch.
    /// Padding is masked out of both attention and mean-pooling, so a text's
    /// embedding does not depend on what it was batched with.
    pub fn embed_texts(&self, texts: &[&str], is_query: bool) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Tokenize first: it needs no lock, and the lengths drive the batching.
        let ids: Vec<Vec<i32>> = texts
            .iter()
            .map(|&t| {
                if is_query {
                    self.tokenizer.encode_query(t)
                } else {
                    self.tokenizer.encode_document(t)
                }
            })
            .collect::<Result<_>>()?;
        let lens: Vec<usize> = ids.iter().map(Vec::len).collect();

        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        let model = self
            .model
            .lock()
            .map_err(|_| eyre::eyre!("embedder mutex poisoned"))?;
        for group in batch_plan(&lens, self.max_batch) {
            let batch: Vec<Vec<i32>> = group.iter().map(|&i| ids[i].clone()).collect();
            let embedded = model.embed_batch(&batch)?;
            for (&i, v) in group.iter().zip(embedded) {
                out[i] = mrl_truncate(v, self.output_dim);
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl EmbeddingProvider for MlxEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_texts(texts, self.default_query)
    }

    fn dimension(&self) -> usize {
        self.output_dim
    }
}
