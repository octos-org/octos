//! [`LlamaEmbedder`] — the `octos_llm::EmbeddingProvider` implementation.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use eyre::{Result, WrapErr, eyre};
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use octos_llm::EmbeddingProvider;

use crate::prompt::{batch_plan, l2_normalize, mrl_truncate, with_prompt};

/// Sequences per forward pass. Batching is the single lever that matters here:
/// a lone short text costs ~6.5 ms of which almost all is fixed per-pass
/// overhead, so folding 16 of them into one `decode` amortizes that overhead
/// instead of paying it 16 times.
const DEFAULT_MAX_BATCH: usize = 16;

/// Longest single sequence, from EmbeddingGemma's `max_seq_length`. Longer
/// inputs are truncated at tokenization rather than corrupting the batch.
const MAX_TOKENS_PER_SEQ: usize = 2048;

/// KV capacity, which llama.cpp DIVIDES by `n_seq_max`: `n_ctx_seq = n_ctx /
/// n_seq_max`. To let every sequence use the model's full 2048 tokens with 16
/// concurrent sequences, this must be `MAX_TOKENS_PER_SEQ * MAX_BATCH`. Getting
/// this wrong silently caps each sequence — llama.cpp warns
/// "n_ctx_seq (512) < n_ctx_train (2048)" and truncates.
///
/// Cheaper than it looks: the model is sliding-window (`n_swa = 512`), so only
/// the periodic global-attention layers scale their KV with `n_ctx`.
const DEFAULT_N_CTX: u32 = MAX_TOKENS_PER_SEQ as u32 * DEFAULT_MAX_BATCH as u32;

/// Tokens per `decode` call, and therefore the batch planner's budget.
///
/// Deliberately much smaller than `n_ctx`. llama.cpp builds its compute graph
/// for the WORST case (`worst-case: n_tokens = n_ubatch`), so an oversized
/// `n_ubatch` makes every decode pay for a batch it never receives — with
/// `n_ubatch = 8192` a 336-token batch cost the same as 16 separate calls and
/// batching won nothing at all. This still admits one full-length sequence or
/// a full batch of short ones.
const DEFAULT_N_BATCH: u32 = 2048;

/// llama.cpp's global backend must be initialized exactly once per process, and
/// must outlive every model and context built from it.
fn backend() -> Result<&'static LlamaBackend> {
    static BACKEND: OnceLock<std::result::Result<LlamaBackend, String>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            let mut b = LlamaBackend::init().map_err(|e| e.to_string())?;
            // llama.cpp is chatty on stderr; octos owns its own logging.
            b.void_logs();
            Ok(b)
        })
        .as_ref()
        .map_err(|e| eyre!("llama.cpp backend init failed: {e}"))
}

self_cell::self_cell!(
    /// A model plus a context borrowed from it.
    ///
    /// `LlamaContext<'a>` borrows its `LlamaModel`, so keeping both alive in one
    /// long-lived struct is self-referential. `self_cell` encodes that without
    /// `unsafe` (which the workspace denies) and without rebuilding the context
    /// per call — that would re-allocate the KV cache on every embed.
    struct ModelCtx {
        owner: LlamaModel,
        #[covariant]
        dependent: Ctx,
    }
);

type Ctx<'a> = llama_cpp_2::context::LlamaContext<'a>;

/// `LlamaContext` holds a `NonNull<llama_context>` and so is neither `Send` nor
/// `Sync`, but `EmbeddingProvider` requires both. This newtype asserts `Send`
/// only, and it is only ever stored inside a `Mutex` — which is what upgrades it
/// to `Sync` and, crucially, guarantees the access discipline the assertion
/// relies on.
///
/// SAFETY: a `llama_context` is a plain heap object with no thread-affine state
/// (no TLS, no thread-bound handles), so it may be used from any thread provided
/// it is used from only ONE at a time. `LlamaEmbedder` holds this exclusively
/// behind `Mutex<SendCtx>` and every use goes through that lock, so concurrent
/// access is impossible. This is the same discipline llama.cpp's own server
/// applies; the binding declines to assert it because it cannot see the lock.
struct SendCtx(ModelCtx);

#[allow(unsafe_code)] // FFI handle serialized by a Mutex; see SAFETY above.
unsafe impl Send for SendCtx {}

/// In-process embedding provider over any GGUF embedding model.
///
/// The context is single-threaded, so it lives behind a `Mutex` and calls
/// serialize. Tokenization needs only the model and happens outside the lock.
pub struct LlamaEmbedder {
    inner: Mutex<SendCtx>,
    native_dim: usize,
    /// MRL output dim (<= `native_dim`); truncate + renormalize when smaller.
    output_dim: usize,
    /// When true, [`EmbeddingProvider::embed`] applies the QUERY prefix; by
    /// default it applies the DOCUMENT prefix (the common indexing case).
    default_query: bool,
    max_batch: usize,
    /// Hard token cap per sequence.
    max_tokens: usize,
    /// Total tokens per batch, bounded by the context window.
    token_budget: usize,
}

impl LlamaEmbedder {
    /// Load a GGUF embedding model from a file.
    ///
    /// `n_gpu_layers` controls offload: `0` is CPU-only, a large value offloads
    /// everything. Ignored unless the crate was built with an accelerator
    /// feature (`metal` / `cuda`).
    pub fn from_model_file(path: impl AsRef<Path>, n_gpu_layers: u32) -> Result<Self> {
        let path = path.as_ref();
        let backend = backend()?;

        let model_params = LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers);
        let model = LlamaModel::load_from_file(backend, path, &model_params)
            .wrap_err_with(|| format!("loading GGUF model from {}", path.display()))?;

        let native_dim = usize::try_from(model.n_embd())
            .map_err(|_| eyre!("model reported a negative embedding dimension"))?;

        // `n_seq_max` MUST cover every sequence id used in a batch: it defaults
        // to 1, and a `seq_id >= n_seq_max` makes llama_decode reject the whole
        // batch with a bare -1 (which llama-cpp-2 reports, misleadingly, as
        // "n_tokens == 0"). `n_batch`/`n_ubatch` must hold a full batch in one
        // pass, else pooled embeddings would be split across ubatches.
        // `pooling = Mean` matches EmbeddingGemma's Pooling module.
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(DEFAULT_N_CTX))
            .with_n_batch(DEFAULT_N_BATCH)
            .with_n_ubatch(DEFAULT_N_BATCH)
            .with_n_seq_max(DEFAULT_MAX_BATCH as u32)
            .with_embeddings(true)
            .with_pooling_type(LlamaPoolingType::Mean);

        let inner = ModelCtx::try_new(model, |model| {
            model
                .new_context(backend, ctx_params)
                .wrap_err("creating llama.cpp context")
        })?;

        Ok(Self {
            inner: Mutex::new(SendCtx(inner)),
            native_dim,
            output_dim: native_dim,
            default_query: false,
            max_batch: DEFAULT_MAX_BATCH,
            max_tokens: MAX_TOKENS_PER_SEQ,
            token_budget: DEFAULT_N_BATCH as usize,
        })
    }

    /// Make [`EmbeddingProvider::embed`] treat inputs as queries (default: docs).
    pub fn with_query_default(mut self, query: bool) -> Self {
        self.default_query = query;
        self
    }

    /// Matryoshka (MRL) output dim. Clamped to `[1, native_dim]`.
    pub fn with_output_dim(mut self, dim: usize) -> Self {
        self.output_dim = dim.clamp(1, self.native_dim);
        self
    }

    /// Override sequences per forward pass.
    ///
    /// Clamped to `[1, DEFAULT_MAX_BATCH]`: the context was created with
    /// `n_seq_max = DEFAULT_MAX_BATCH`, and asking for more would produce
    /// sequence ids llama.cpp rejects at decode time.
    pub fn with_max_batch(mut self, max_batch: usize) -> Self {
        self.max_batch = max_batch.clamp(1, DEFAULT_MAX_BATCH);
        self
    }

    pub fn native_dim(&self) -> usize {
        self.native_dim
    }

    /// The active output dimension (== [`EmbeddingProvider::dimension`]).
    pub fn output_dim(&self) -> usize {
        self.output_dim
    }

    /// Embed a batch with an explicit role, returning unit-norm vectors of
    /// length [`Self::output_dim`], in input order.
    pub fn embed_texts(&self, texts: &[&str], is_query: bool) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut guard = self
            .inner
            .lock()
            .map_err(|_| eyre!("llama embedder mutex poisoned"))?;
        let inner = &mut guard.0;

        // Tokenize with the task prefix applied, truncating over-long inputs so
        // one oversized episode cannot blow past the context window.
        let mut tokens = Vec::with_capacity(texts.len());
        for &text in texts {
            let prompted = with_prompt(text, is_query);
            let mut t = inner
                .borrow_owner()
                .str_to_token(&prompted, AddBos::Always)
                .map_err(|e| eyre!("tokenizing: {e}"))?;
            t.truncate(self.max_tokens);
            if t.is_empty() {
                return Err(eyre!("text tokenized to zero tokens"));
            }
            tokens.push(t);
        }
        let lens: Vec<usize> = tokens.iter().map(Vec::len).collect();

        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        for group in batch_plan(&lens, self.max_batch, self.token_budget) {
            let total: usize = group.iter().map(|&i| lens[i]).sum();
            let mut batch = LlamaBatch::new(total, group.len() as i32);

            for (seq_id, &idx) in group.iter().enumerate() {
                batch
                    .add_sequence(&tokens[idx], seq_id as i32, false)
                    .map_err(|e| eyre!("building batch: {e}"))?;
            }

            inner.with_dependent_mut(|_model, ctx| -> Result<()> {
                // Pooled embeddings are read per sequence after the decode, so
                // the KV cache must not carry state across batches.
                ctx.clear_kv_cache();
                ctx.decode(&mut batch).map_err(|e| eyre!("decode: {e}"))?;

                for (seq_id, &idx) in group.iter().enumerate() {
                    let embedding = ctx
                        .embeddings_seq_ith(seq_id as i32)
                        .map_err(|e| eyre!("reading embedding for seq {seq_id}: {e}"))?;
                    let mut v = embedding.to_vec();
                    // llama.cpp mean-pooling does not normalize; EmbeddingGemma's
                    // pipeline ends in Normalize, so do it here.
                    l2_normalize(&mut v);
                    out[idx] = mrl_truncate(v, self.output_dim);
                }
                Ok(())
            })?;
        }

        Ok(out)
    }
}

#[async_trait]
impl EmbeddingProvider for LlamaEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_texts(texts, self.default_query)
    }

    fn dimension(&self) -> usize {
        self.output_dim
    }
}
