//! [`MlxEmbedder`] — the public [`octos_llm::EmbeddingProvider`] implementation.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use eyre::Result;
use octos_llm::EmbeddingProvider;

use super::model::GemmaModel;
use super::tokenizer::GemmaTokenizer;

/// In-process EmbeddingGemma-300M embedder on Apple MLX (Metal).
///
/// `mlx-rs` `Array` is `Send` but not `Sync`, and MLX evaluation is best kept
/// single-threaded, so the model lives behind a `Mutex` — embed calls serialize
/// (each is ~5 ms). The tokenizer is `Sync` and stays outside the lock.
pub struct MlxEmbedder {
    model: Mutex<GemmaModel>,
    tokenizer: GemmaTokenizer,
    native_dim: usize,
    /// MRL output dim (<= `native_dim`); truncate + renormalize when smaller.
    output_dim: usize,
    /// When true, [`EmbeddingProvider::embed`] applies the QUERY prompt; by
    /// default it applies the DOCUMENT prompt (the common indexing case).
    default_query: bool,
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
        })
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
    /// (and renormalized) vectors of length [`Self::dimension`].
    pub fn embed_texts(&self, texts: &[&str], is_query: bool) -> Result<Vec<Vec<f32>>> {
        let model = self
            .model
            .lock()
            .map_err(|_| eyre::eyre!("embedder mutex poisoned"))?;
        let mut out = Vec::with_capacity(texts.len());
        for &t in texts {
            let ids = if is_query {
                self.tokenizer.encode_query(t)?
            } else {
                self.tokenizer.encode_document(t)?
            };
            let full = model.embed_ids(&ids)?;
            out.push(mrl_truncate(full, self.output_dim));
        }
        Ok(out)
    }
}

/// Truncate to `out` dims and renormalize (matches the Python MRL path:
/// `l2(full[:, :d])`). A no-op when `out >= len`.
fn mrl_truncate(v: Vec<f32>, out: usize) -> Vec<f32> {
    if out >= v.len() {
        return v;
    }
    let head = &v[..out];
    let norm = head.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    head.iter().map(|x| x / norm).collect()
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
