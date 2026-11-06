//! [`LlamaEmbedder`] — the `octos_llm::EmbeddingProvider` implementation.

use std::collections::HashMap;
use std::hash::Hash;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use eyre::{Result, WrapErr, eyre};
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use octos_llm::{EmbeddingPriority, EmbeddingProvider};
use tokio::sync::Notify;

use crate::prompt::{batch_plan, l2_normalize, mrl_truncate, with_prompt};

/// Sequences per forward pass and per context. Four bounds each context's KV
/// working set; the four-context pool still admits sixteen independent calls.
const DEFAULT_MAX_BATCH: usize = 4;

/// Longest single sequence, from EmbeddingGemma's `max_seq_length`. Longer
/// inputs are truncated at tokenization rather than corrupting the batch.
const MAX_TOKENS_PER_SEQ: usize = 2048;

/// KV capacity, which llama.cpp DIVIDES by `n_seq_max`: `n_ctx_seq = n_ctx /
/// n_seq_max`. To let every sequence use the model's full 2048 tokens with four
/// concurrent sequences, this must be `MAX_TOKENS_PER_SEQ * MAX_BATCH`. Getting
/// this wrong silently caps each sequence and can truncate recall input.
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
/// batching won nothing at all. This must remain at least `MAX_TOKENS_PER_SEQ`:
/// llama.cpp asserts for encoder batches larger than `n_ubatch` instead of
/// splitting them, so a smaller graph would abort on long recall prompts.
const DEFAULT_N_BATCH: u32 = MAX_TOKENS_PER_SEQ as u32;
const _: () = assert!(DEFAULT_N_BATCH as usize >= MAX_TOKENS_PER_SEQ);

/// One waiting episode writer per embedder, in addition to active decodes.
/// More callers are refused immediately instead of accumulating unbounded
/// fire-and-forget tasks behind the bounded llama context pool.
const BACKGROUND_QUEUE_LIMIT: usize = 1;

/// Demand-allocated contexts let independent recalls decode in parallel while
/// still sharing the same model weights. Four bounds CPU and resident context
/// memory instead of scaling with the number of concurrent requests.
const CONTEXT_POOL_SIZE: usize = 4;

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

/// Process-local single-flight cache. The lock is deliberately held while the
/// loader runs on a blocking thread: it prevents two profiles from loading the
/// same 334 MiB file concurrently. Different keys are serialized too, which
/// keeps peak model-loading memory bounded; cached lookups are only a hash get.
#[derive(Clone)]
struct LoadCache<K, V> {
    entries: Arc<Mutex<HashMap<K, Arc<V>>>>,
}

impl<K, V> Default for LoadCache<K, V> {
    fn default() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<K, V> LoadCache<K, V>
where
    K: Eq + Hash + Clone,
{
    fn get_or_load(&self, key: &K, loader: impl FnOnce() -> Result<Arc<V>>) -> Result<Arc<V>> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = entries.get(key) {
            return Ok(existing.clone());
        }
        let loaded = loader()?;
        entries.insert(key.clone(), loaded.clone());
        Ok(loaded)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ModelKey {
    canonical_path: PathBuf,
    n_gpu_layers: u32,
}

fn shared_model(path: &Path, n_gpu_layers: u32) -> Result<Arc<LlamaModel>> {
    static MODELS: OnceLock<LoadCache<ModelKey, LlamaModel>> = OnceLock::new();
    let canonical_path = path
        .canonicalize()
        .wrap_err_with(|| format!("canonicalize embedding model {}", path.display()))?;
    let key = ModelKey {
        canonical_path: canonical_path.clone(),
        n_gpu_layers,
    };
    MODELS
        .get_or_init(LoadCache::default)
        .get_or_load(&key, || {
            let backend = backend()?;
            let model = LlamaModel::load_from_file(
                backend,
                &canonical_path,
                &LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers),
            )
            .wrap_err("loading GGUF model with llama.cpp")?;
            Ok(Arc::new(model))
        })
}

self_cell::self_cell!(
    /// Shared model weights plus a context borrowed from them. Every profile
    /// gets its own context, while the owner is the same process-wide Arc for
    /// an identical canonical path + GPU setting.
    struct ModelCtx {
        owner: Arc<LlamaModel>,
        #[covariant]
        dependent: Ctx,
    }
);

type Ctx<'a> = llama_cpp_2::context::LlamaContext<'a>;

impl ModelCtx {
    fn from_shared_model(model: Arc<LlamaModel>) -> Result<Self> {
        let backend = backend()?;
        let params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(DEFAULT_N_CTX))
            .with_n_batch(DEFAULT_N_BATCH)
            .with_n_ubatch(DEFAULT_N_BATCH)
            .with_n_seq_max(DEFAULT_MAX_BATCH as u32)
            .with_embeddings(true)
            .with_pooling_type(LlamaPoolingType::Mean);
        Self::try_new(model, |model| {
            model
                .new_context(backend, params)
                .wrap_err("creating llama.cpp context")
        })
    }
}

/// `LlamaContext` holds a `NonNull<llama_context>` and so is neither `Send` nor
/// `Sync`, but `EmbeddingProvider` requires both. This newtype asserts `Send`
/// only, and it is only ever stored inside a `Mutex` — which is what upgrades it
/// to `Sync` and, crucially, guarantees the access discipline behind that
/// assertion: only the mutex holder can use the context.
///
/// SAFETY: a `llama_context` is a plain heap object with no thread-affine state
/// (no TLS, no thread-bound handles), so it may be used from any thread provided
/// it is used from only ONE at a time. Every use goes through `Mutex<SendCtx>`,
/// including decode on the blocking pool, so concurrent access is impossible.
struct SendCtx(ModelCtx);

#[allow(unsafe_code)] // FFI handle serialized by a Mutex; see SAFETY above.
unsafe impl Send for SendCtx {}

struct ContextPoolState {
    available: Vec<Arc<Mutex<SendCtx>>>,
    total: usize,
}

struct ContextPool {
    model: Arc<LlamaModel>,
    state: Mutex<ContextPoolState>,
    available_async: Notify,
    available_sync: std::sync::Condvar,
}

/// RAII checkout. Cancellation and decode errors both return the context.
struct PooledContext {
    pool: Arc<ContextPool>,
    context: Arc<Mutex<SendCtx>>,
}

impl Drop for PooledContext {
    fn drop(&mut self) {
        self.pool
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .available
            .push(self.context.clone());
        self.pool.available_async.notify_one();
        self.pool.available_sync.notify_one();
    }
}

struct ContextReservation {
    pool: Arc<ContextPool>,
    armed: bool,
}

impl Drop for ContextReservation {
    fn drop(&mut self) {
        if self.armed {
            self.pool
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .total -= 1;
            self.pool.available_async.notify_one();
            self.pool.available_sync.notify_one();
        }
    }
}

impl ContextPool {
    fn new(model: Arc<LlamaModel>) -> Arc<Self> {
        Arc::new(Self {
            model,
            state: Mutex::new(ContextPoolState {
                available: Vec::new(),
                total: 0,
            }),
            available_async: Notify::new(),
            available_sync: std::sync::Condvar::new(),
        })
    }

    fn new_context(&self) -> Result<Arc<Mutex<SendCtx>>> {
        Ok(Arc::new(Mutex::new(SendCtx(ModelCtx::from_shared_model(
            self.model.clone(),
        )?))))
    }

    async fn acquire_async(self: &Arc<Self>) -> Result<PooledContext> {
        enum Action {
            Checkout(Arc<Mutex<SendCtx>>),
            Create,
            Wait,
        }
        loop {
            let notified = self.available_async.notified();
            let action = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(context) = state.available.pop() {
                    Action::Checkout(context)
                } else if state.total < CONTEXT_POOL_SIZE {
                    state.total += 1;
                    Action::Create
                } else {
                    Action::Wait
                }
            };
            match action {
                Action::Checkout(context) => {
                    return Ok(PooledContext {
                        pool: self.clone(),
                        context,
                    });
                }
                Action::Create => {
                    let mut reservation = ContextReservation {
                        pool: self.clone(),
                        armed: true,
                    };
                    let this = self.clone();
                    let context = tokio::task::spawn_blocking(move || this.new_context())
                        .await
                        .map_err(|error| eyre!("join llama context creator: {error}"))??;
                    reservation.armed = false;
                    return Ok(PooledContext {
                        pool: self.clone(),
                        context,
                    });
                }
                Action::Wait => notified.await,
            }
        }
    }

    fn acquire_blocking(self: &Arc<Self>) -> Result<PooledContext> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(context) = state.available.pop() {
                return Ok(PooledContext {
                    pool: self.clone(),
                    context,
                });
            }
            if state.total < CONTEXT_POOL_SIZE {
                state.total += 1;
                drop(state);
                match self.new_context() {
                    Ok(context) => {
                        return Ok(PooledContext {
                            pool: self.clone(),
                            context,
                        });
                    }
                    Err(error) => {
                        self.state.lock().unwrap_or_else(|e| e.into_inner()).total -= 1;
                        self.available_async.notify_one();
                        self.available_sync.notify_one();
                        return Err(error);
                    }
                }
            }
            state = self
                .available_sync
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct EmbedInner {
    model_path: PathBuf,
    n_gpu_layers: u32,
    pool: tokio::sync::OnceCell<Arc<ContextPool>>,
}

impl EmbedInner {
    fn eager(path: &Path, n_gpu_layers: u32) -> Result<Arc<Self>> {
        let model = shared_model(path, n_gpu_layers)?;
        let pool = ContextPool::new(model);
        // Preserve the old eager API: one context exists before the first
        // synchronous embed. Additional contexts remain demand-allocated.
        let _initial = pool.acquire_blocking()?;
        let cell = tokio::sync::OnceCell::new();
        cell.set(pool)
            .map_err(|_| eyre!("initialize eager llama context once"))?;
        Ok(Arc::new(Self {
            model_path: path.to_path_buf(),
            n_gpu_layers,
            pool: cell,
        }))
    }

    fn lazy(path: &Path, n_gpu_layers: u32) -> Arc<Self> {
        Arc::new(Self {
            model_path: path.to_path_buf(),
            n_gpu_layers,
            pool: tokio::sync::OnceCell::new(),
        })
    }

    fn is_initialized(&self) -> bool {
        self.pool.get().is_some()
    }

    async fn ensure_initialized(self: &Arc<Self>) -> Result<()> {
        if self.is_initialized() {
            return Ok(());
        }
        self.pool
            .get_or_try_init(|| async {
                let this = self.clone();
                let pool = tokio::task::spawn_blocking(move || {
                    shared_model(&this.model_path, this.n_gpu_layers).map(ContextPool::new)
                })
                .await
                .map_err(|error| eyre!("join llama model loader: {error}"))??;
                Ok::<Arc<ContextPool>, eyre::Report>(pool)
            })
            .await?;
        Ok(())
    }

    #[cfg(test)]
    fn shares_weights_with(this: &Arc<Self>, other: &Arc<Self>) -> bool {
        Arc::ptr_eq(
            &this.pool.get().expect("left initialized").model,
            &other.pool.get().expect("right initialized").model,
        )
    }
}

#[derive(Default)]
struct GateState {
    active_decodes: usize,
    foreground_waiters: usize,
    background_waiters: usize,
}

/// A bounded-context scheduler with explicit foreground priority and a bounded
/// background queue. State is protected by a short-held std mutex; only Tokio
/// notifications cross await points.
struct PriorityGate {
    state: Mutex<GateState>,
    foreground: Notify,
    background: Notify,
    background_limit: usize,
    max_decodes: usize,
}

struct WaiterGuard<'a> {
    gate: &'a PriorityGate,
    priority: EmbeddingPriority,
    armed: bool,
}

impl WaiterGuard<'_> {
    fn decrement(&mut self) {
        if !self.armed {
            return;
        }
        let wake = {
            let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
            match self.priority {
                EmbeddingPriority::Foreground => state.foreground_waiters -= 1,
                EmbeddingPriority::Background => state.background_waiters -= 1,
            }
            self.armed = false;
            if state.active_decodes < self.gate.max_decodes {
                if state.foreground_waiters > 0 {
                    Some(EmbeddingPriority::Foreground)
                } else if state.background_waiters > 0 {
                    Some(EmbeddingPriority::Background)
                } else {
                    None
                }
            } else {
                None
            }
        };
        // A permit may have woken exactly this waiter immediately before its
        // future was cancelled. Hand that wakeup to the next class instead of
        // leaving every remaining waiter asleep with free decode capacity.
        match wake {
            Some(EmbeddingPriority::Foreground) => self.gate.foreground.notify_one(),
            Some(EmbeddingPriority::Background) => self.gate.background.notify_one(),
            None => {}
        }
    }
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.decrement();
    }
}

struct PriorityPermit {
    gate: Arc<PriorityGate>,
}

impl Drop for PriorityPermit {
    fn drop(&mut self) {
        let wake_foreground = {
            let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
            state.active_decodes -= 1;
            state.foreground_waiters > 0
        };
        if wake_foreground {
            self.gate.foreground.notify_one();
        } else {
            self.gate.background.notify_one();
        }
    }
}

impl PriorityGate {
    fn new(background_limit: usize, max_decodes: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState::default()),
            foreground: Notify::new(),
            background: Notify::new(),
            background_limit,
            max_decodes: max_decodes.max(1),
        })
    }

    async fn acquire(self: &Arc<Self>, priority: EmbeddingPriority) -> Result<PriorityPermit> {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let can_decode = state.active_decodes < self.max_decodes
                && (priority == EmbeddingPriority::Foreground || state.foreground_waiters == 0);
            if can_decode {
                state.active_decodes += 1;
                return Ok(PriorityPermit { gate: self.clone() });
            }
            if priority == EmbeddingPriority::Background
                && state.background_waiters >= self.background_limit
            {
                // T-0317: typed shed marker so callers can distinguish
                // queue-full backpressure (retryable) from a real embedding
                // failure (keyword-only fallback). Message unchanged.
                return Err(eyre::Report::new(octos_llm::EmbedQueueFull {
                    detail: "embedding background queue is full; deferring episode embedding"
                        .to_string(),
                }));
            }
            match priority {
                EmbeddingPriority::Foreground => state.foreground_waiters += 1,
                EmbeddingPriority::Background => state.background_waiters += 1,
            }
        }
        let mut waiter = WaiterGuard {
            gate: self,
            priority,
            armed: true,
        };
        loop {
            let notified = match priority {
                EmbeddingPriority::Foreground => self.foreground.notified(),
                EmbeddingPriority::Background => self.background.notified(),
            };
            {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let can_decode = state.active_decodes < self.max_decodes
                    && (priority == EmbeddingPriority::Foreground || state.foreground_waiters == 0);
                if can_decode {
                    state.active_decodes += 1;
                    match priority {
                        EmbeddingPriority::Foreground => state.foreground_waiters -= 1,
                        EmbeddingPriority::Background => state.background_waiters -= 1,
                    }
                    waiter.armed = false;
                    return Ok(PriorityPermit { gate: self.clone() });
                }
            }
            notified.await;
        }
    }

    #[cfg(test)]
    async fn wait_for_background_waiters(&self, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if self.state.lock().unwrap().background_waiters >= expected {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background waiter registered");
    }

    #[cfg(test)]
    async fn wait_for_foreground_waiters(&self, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if self.state.lock().unwrap().foreground_waiters >= expected {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("foreground waiter registered");
    }
}

/// In-process llama.cpp embedder. Construction can be eager (legacy API) or
/// lazy; async embedding always performs model loading and decode on Tokio's
/// blocking pool.
pub struct LlamaEmbedder {
    inner: Arc<EmbedInner>,
    gate: Arc<PriorityGate>,
    native_dim: usize,
    output_dim: usize,
    default_query: bool,
    max_batch: usize,
    max_tokens: usize,
    token_budget: usize,
}

#[cfg(test)]
static LAST_DECODE_THREAD: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

#[cfg(test)]
fn record_decode_thread() {
    *LAST_DECODE_THREAD.lock().unwrap() = Some(std::thread::current().id());
}

#[cfg(test)]
fn last_decode_thread() -> std::thread::ThreadId {
    LAST_DECODE_THREAD.lock().unwrap().expect("decode ran")
}

impl LlamaEmbedder {
    /// Load model weights eagerly. Prefer [`Self::from_model_file_lazy`] for
    /// long-lived profiles that should stay small until memory recall is used.
    pub fn from_model_file(path: impl AsRef<Path>, n_gpu_layers: u32) -> Result<Self> {
        let path = path.as_ref();
        let model = shared_model(path, n_gpu_layers)?;
        let native_dim = usize::try_from(model.n_embd())
            .map_err(|_| eyre!("model reported a negative embedding dimension"))?;
        let inner = EmbedInner::eager(path, n_gpu_layers)?;
        Ok(Self::new(inner, native_dim))
    }

    /// Construct without touching the GGUF file or allocating a llama context.
    /// `native_dim` must describe the configured model; the bundled
    /// EmbeddingGemma model is 768-dimensional. The first async embed performs
    /// the actual load and validates the embedding length at decode time.
    pub fn from_model_file_lazy(
        path: impl AsRef<Path>,
        n_gpu_layers: u32,
        native_dim: usize,
    ) -> Self {
        Self::new(EmbedInner::lazy(path.as_ref(), n_gpu_layers), native_dim)
    }

    fn new(inner: Arc<EmbedInner>, native_dim: usize) -> Self {
        Self {
            inner,
            gate: PriorityGate::new(BACKGROUND_QUEUE_LIMIT, CONTEXT_POOL_SIZE),
            native_dim,
            output_dim: native_dim,
            default_query: false,
            max_batch: DEFAULT_MAX_BATCH,
            max_tokens: MAX_TOKENS_PER_SEQ,
            token_budget: DEFAULT_N_BATCH as usize,
        }
    }

    /// Whether model weights and the context have actually been materialized.
    pub fn is_initialized(&self) -> bool {
        self.inner.is_initialized()
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

    /// Synchronous API retained for existing callers and model-backed tests.
    /// Lazy embedders must use the async trait so initialization and decode can
    /// be offloaded; calling this before initialization returns a clear error.
    pub fn embed_texts(&self, texts: &[&str], is_query: bool) -> Result<Vec<Vec<f32>>> {
        let pool = self
            .inner
            .pool
            .get()
            .ok_or_else(|| eyre!("lazy llama embedder is not initialized; use async embed"))?;
        let checkout = pool.acquire_blocking()?;
        decode_texts_blocking(
            &checkout,
            texts,
            is_query,
            self.max_batch,
            self.max_tokens,
            self.token_budget,
            self.output_dim,
        )
    }

    async fn embed_prioritized(
        &self,
        texts: &[&str],
        priority: EmbeddingPriority,
    ) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Admit before lazy initialization too: otherwise the first model
        // load is an unbounded queue of background tasks outside the gate.
        let _permit = self.gate.acquire(priority).await?;
        self.inner.ensure_initialized().await?;
        let pool = self.inner.pool.get().expect("initialized above").clone();
        let checkout = pool.acquire_async().await?;
        let owned: Vec<String> = texts.iter().map(|text| (*text).to_owned()).collect();
        let max_batch = self.max_batch;
        let max_tokens = self.max_tokens;
        let token_budget = self.token_budget;
        let output_dim = self.output_dim;
        let is_query = self.default_query;
        tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            decode_texts_blocking(
                &checkout,
                &refs,
                is_query,
                max_batch,
                max_tokens,
                token_budget,
                output_dim,
            )
        })
        .await
        .map_err(|error| eyre!("join llama decode worker: {error}"))?
    }

    #[cfg(test)]
    fn shares_weights_with(&self, other: &Self) -> bool {
        EmbedInner::shares_weights_with(&self.inner, &other.inner)
    }
}

fn decode_texts_blocking(
    checkout: &PooledContext,
    texts: &[&str],
    is_query: bool,
    max_batch: usize,
    max_tokens: usize,
    token_budget: usize,
    output_dim: usize,
) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    #[cfg(test)]
    record_decode_thread();

    let mut guard = checkout
        .context
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
        t.truncate(max_tokens);
        if t.is_empty() {
            return Err(eyre!("text tokenized to zero tokens"));
        }
        tokens.push(t);
    }
    let lens: Vec<usize> = tokens.iter().map(Vec::len).collect();

    let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
    for group in batch_plan(&lens, max_batch, token_budget) {
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
                out[idx] = mrl_truncate(v, output_dim);
            }
            Ok(())
        })?;
    }

    Ok(out)
}

#[async_trait]
impl EmbeddingProvider for LlamaEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_prioritized(texts, EmbeddingPriority::Foreground)
            .await
    }

    async fn embed_with_priority(
        &self,
        texts: &[&str],
        priority: EmbeddingPriority,
    ) -> Result<Vec<Vec<f32>>> {
        self.embed_prioritized(texts, priority).await
    }

    fn dimension(&self) -> usize {
        self.output_dim
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn should_load_a_cached_model_only_once_and_share_its_arc() {
        let cache = LoadCache::<String, usize>::default();
        let loads = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let cache = cache.clone();
            let loads = loads.clone();
            workers.push(std::thread::spawn(move || {
                cache.get_or_load(&"same-model".to_owned(), || {
                    loads.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(25));
                    Ok(Arc::new(42usize))
                })
            }));
        }
        let values: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap().unwrap())
            .collect();
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(values.iter().all(|value| Arc::ptr_eq(value, &values[0])));
    }

    #[tokio::test]
    async fn should_prioritize_foreground_and_bound_background_waiters() {
        let gate = PriorityGate::new(1, 1);
        let first = gate.acquire(EmbeddingPriority::Foreground).await.unwrap();

        let background = tokio::spawn({
            let gate = gate.clone();
            async move { gate.acquire(EmbeddingPriority::Background).await }
        });
        gate.wait_for_background_waiters(1).await;
        let full = gate.acquire(EmbeddingPriority::Background).await;
        assert!(
            full.is_err(),
            "a second queued background call must be refused"
        );

        let foreground = tokio::spawn({
            let gate = gate.clone();
            async move { gate.acquire(EmbeddingPriority::Foreground).await }
        });
        gate.wait_for_foreground_waiters(1).await;
        drop(first);

        let foreground_permit = tokio::time::timeout(Duration::from_secs(1), foreground)
            .await
            .expect("foreground should acquire first")
            .unwrap()
            .unwrap();
        assert!(!background.is_finished(), "background must yield to recall");
        drop(foreground_permit);
        tokio::time::timeout(Duration::from_secs(1), background)
            .await
            .expect("background should acquire after recall")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn should_wake_remaining_waiters_when_a_notified_waiter_is_cancelled() {
        let gate = PriorityGate::new(1, 1);
        {
            let mut state = gate.state.lock().unwrap();
            // The last active decode released capacity to a foreground waiter.
            // A background request is still asleep behind it.
            state.active_decodes = 0;
            state.foreground_waiters = 1;
            state.background_waiters = 1;
        }
        let background_notified = gate.background.notified();
        let mut cancelled = WaiterGuard {
            gate: &gate,
            priority: EmbeddingPriority::Foreground,
            armed: true,
        };
        cancelled.decrement();
        tokio::time::timeout(Duration::from_millis(50), background_notified)
            .await
            .expect("cancelling the woken foreground must wake the next class");
        let state = gate.state.lock().unwrap();
        assert_eq!(state.foreground_waiters, 0);
        assert_eq!(state.background_waiters, 1);
    }

    #[tokio::test]
    #[ignore = "needs the embed-llama feature + a cached GGUF"]
    async fn should_lazy_load_share_weights_and_decode_off_the_async_worker() {
        let path = std::env::var_os("OCTOS_EMBED_GGUF")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").expect("HOME or OCTOS_EMBED_GGUF");
                std::path::PathBuf::from(home).join(".octos/models/embeddinggemma-300M-Q8_0.gguf")
            });
        let first = LlamaEmbedder::from_model_file_lazy(&path, 0, 768);
        assert!(!first.is_initialized(), "constructor must not load weights");
        let async_worker = std::thread::current().id();
        first.embed(&["hello world"]).await.unwrap();
        assert!(first.is_initialized());
        assert_ne!(
            async_worker,
            last_decode_thread(),
            "llama decode must run on a blocking worker"
        );

        let second = LlamaEmbedder::from_model_file_lazy(&path, 0, 768);
        second.embed(&["hello again"]).await.unwrap();
        assert!(first.shares_weights_with(&second));
    }
}
