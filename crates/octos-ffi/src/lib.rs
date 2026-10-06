//! octos-ffi: a C-ABI surface for embedding octos in non-Rust hosts.
//!
//! This crate builds a `cdylib`/`staticlib` so Python (ctypes/cffi), Node
//! (ffi-napi/koffi), Go (cgo), or plain C can drive an octos [`Agent`] without
//! linking Rust. It wraps the same provider-construction and agent loop the
//! `octos` CLI uses (see `octos-cli/src/commands/chat.rs`), trimmed to a
//! one-shot task runner plus an optional embedder, and exposes the Recall
//! memory index (`octos_memory_*`, see `docs/adr/personal-memory-tiers.md`) so
//! a host app can push its records (mail, calendar, contacts) and search them.
//!
//! # SAFETY
//!
//! The whole crate opts out of the workspace-wide `deny(unsafe_code)` with a
//! crate-level `#![allow(unsafe_code)]` because an FFI boundary is unsafe by
//! nature — it dereferences pointers it did not create and hands out owned
//! allocations across an ABI. Unlike `octos-sandbox` (two localized unsafe
//! blocks, so it uses per-fn `#[allow]`), essentially every function here is at
//! the boundary, so the allow is crate-scoped. Every unsafe operation is still
//! individually justified with a `// SAFETY:` note. The invariants the whole
//! surface upholds:
//!
//! * **Panics never cross the boundary.** Every `extern "C"` body runs inside
//!   [`std::panic::catch_unwind`]; a panic becomes a NULL/error return, never
//!   an unwind into C (which is UB).
//! * **Pointer discipline.** The runtime handle is an opaque `Box::into_raw`
//!   pointer freed only by [`octos_runtime_free`]. Every pointer argument is
//!   NULL-checked; C strings are validated as UTF-8 and rejected otherwise.
//!   Returned strings are `CString::into_raw` and must be freed ONLY by
//!   [`octos_string_free`] — freeing them with `free(3)` or double-freeing is
//!   the caller's UB, documented in `octos.h`.
//! * **Error reporting.** Failures set a thread-local last-error string,
//!   readable via [`octos_last_error`] until the next FFI call on that thread.
#![allow(unsafe_code)]
// The functions below are the C-ABI boundary: they intentionally accept raw
// pointers (the whole point of the crate) and validate them at runtime rather
// than being marked `unsafe fn` — the spec keeps them callable as plain
// `extern "C"` symbols. Silence the clippy lint that would otherwise flag every
// boundary function; each still NULL-checks before any dereference.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, NaiveDate, Utc};
use libc::c_char;
use octos_agent::{
    Agent, AgentConfig, ConversationResponse, GlobTool, GrepTool, IncompleteResponseError,
    ListDirTool, MemoryLoadTool, MemorySearchTool, ReadFileTool, ShellTool, ToolRegistry,
    WriteFileTool,
};
use octos_cli::commands::chat::create_provider_with_api_type;
use octos_cli::config::Config;
use octos_cli::embed_model;
use octos_core::{AgentId, MessageRole};
#[cfg(feature = "embed-llama")]
use octos_llm::EmbeddingProvider;
use octos_llm::LlmProvider;
use octos_memory::{
    DEFAULT_RECALL_DIMENSION, EpisodeStore, MemoryStore, RecallConfig, RecallStore, Record,
    RecordKind, SearchFilter, Trust,
};
use serde::Deserialize;
use serde_json::json;

/// Monotonic counter used to give every runtime a unique on-disk scratch dir
/// for its (minimal) episodic memory store.
static MEM_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Upper bound on records accepted by one [`OctosRuntime::memory_upsert`]
/// call; hosts page larger syncs.
pub const MAX_UPSERT_RECORDS: usize = 500;
/// Records embedded per embedder call during an upsert.
const EMBED_BATCH: usize = 16;
/// Default `limit` for [`OctosRuntime::memory_search`] (the ADR's k≈10).
const DEFAULT_SEARCH_LIMIT: usize = 10;

thread_local! {
    /// Most recent failure on this thread. Read (never freed) by
    /// [`octos_last_error`]; overwritten by the next fallible FFI call.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
    /// Task payload, not diagnostic text. Owned by this thread until taken,
    /// superseded by a fallible call/new error, or dropped at thread exit.
    static LAST_PARTIAL_RESULT: RefCell<Option<TaskResult>> = const { RefCell::new(None) };
}

/// Upper bound on a stored error string, so a runaway provider error body can't
/// balloon the thread-local buffer.
const MAX_ERROR_LEN: usize = 600;

fn set_last_error(msg: impl Into<String>) {
    LAST_PARTIAL_RESULT.with(|slot| *slot.borrow_mut() = None);
    // Redact credential-shaped tokens and cap length BEFORE storing: provider
    // error bodies can echo an auth value, and this string is handed back
    // verbatim by `octos_last_error`.
    //
    // NOTE: this redacts only what the FFI EXPOSES here. It does NOT reach
    // octos/provider `tracing` — a debug/trace subscriber may still log a
    // provider error body verbatim (a hostile endpoint could stuff the request
    // credential into it). That subscriber is the host's responsibility; see the
    // SECURITY note in README.md.
    let sanitized = sanitize_error_text(&msg.into());
    // CString rejects interior NUL; scrub so the message always stores.
    let scrubbed = sanitized.replace('\0', " ");
    let c = CString::new(scrubbed).unwrap_or_else(|_| CString::new("error").expect("no NUL"));
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(c));
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
    LAST_PARTIAL_RESULT.with(|slot| *slot.borrow_mut() = None);
}

/// Replace the caller's EXACT known secret with a placeholder — the reliable
/// scrub. Applied to error text wherever the FFI holds the key the provider was
/// configured with, at ANY length and from ANY source (env_vars-injected
/// `api_key`, `api_key_env` process var, default env var, or auth store; see
/// [`OctosRuntime`]'s `secret`). Only an empty key is skipped — an empty pattern
/// would otherwise splice the placeholder between every byte.
fn scrub_secret(mut s: String, secret: Option<&str>) -> String {
    if let Some(sec) = secret {
        if !sec.is_empty() && s.contains(sec) {
            s = s.replace(sec, "<redacted>");
        }
    }
    s
}

/// Best-effort redaction of credential-shaped substrings plus a length cap.
/// Applied to every stored error as a backstop for the env-var / auth-store key
/// paths where the plaintext isn't available for an exact scrub.
///
/// `pub` so a non-C facade (e.g. `octos-uniffi`) can apply the SAME heuristic
/// backstop + cap that the C-ABI's `set_last_error` applies. It must be invoked
/// ONLY at the outer facade boundary, never inside the core ([`OctosRuntime::
/// from_config`]/[`OctosRuntime::run_task`]/[`CoreError`] construction) — doing
/// so would double-apply on the C path and perturb `octos_last_error`'s
/// byte-for-byte output.
pub fn sanitize_error_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len().min(MAX_ERROR_LEN + 16));
    for (i, word) in input.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if looks_secretish(word) {
            out.push_str("<redacted>");
        } else {
            out.push_str(word);
        }
    }
    if out.len() > MAX_ERROR_LEN {
        let mut end = MAX_ERROR_LEN;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("…(truncated)");
    }
    out
}

/// Heuristic: does this token look like an API key / bearer token?
fn looks_secretish(word: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "sk-", "sk-ant", "AIza", "xoxb-", "xoxp-", "ghp_", "gho_", "pk-",
    ];
    // Strip surrounding quotes/punctuation before judging.
    let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
    if w.len() < 8 {
        return false;
    }
    if PREFIXES
        .iter()
        .any(|p| w.len() > p.len() + 2 && w.starts_with(p))
    {
        return true;
    }
    // Long, token-shaped run containing a digit (base64/hex-ish secrets).
    w.len() >= 24
        && w.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && w.bytes().any(|b| b.is_ascii_digit())
}

/// The canonical provider name the factory resolves the credential under.
///
/// `create_provider_with_api_type` normalizes the caller's provider spelling to
/// its canonical registry name (case-insensitive alias lookup — e.g.
/// `qwen`→`dashscope`, `OpenAI`→`openai`) and calls `get_api_key(entry.name)`.
/// The FFI MUST resolve `secret` under that SAME canonical name, or a key stored
/// under the canonical name (notably an auth-store credential, which is keyed by
/// the raw provider string) is missed and a later error goes unscrubbed. `custom`
/// stays as-is (the factory resolves it under `"custom"`); an unknown name passes
/// through (the factory then fails on it before any key is needed).
fn canonical_provider_name(provider: &str) -> String {
    if provider == "custom" {
        return "custom".to_string();
    }
    octos_llm::registry::lookup(provider)
        .map(|entry| entry.name.to_string())
        .unwrap_or_else(|| provider.to_string())
}

/// Synthetic `env_vars` key the once-resolved credential is pinned under. Since
/// `Config::api_key_env` takes precedence in `get_api_key`'s lookup, pointing it
/// at this name makes the factory read exactly the pinned value (not any real
/// process var of the same name — `env_vars` is checked before process env).
const PINNED_KEY_ENV: &str = "OCTOS_FFI_RESOLVED_KEY";

/// Pin the once-resolved effective key into `cli_cfg` so the provider factory's
/// own `get_api_key` is FULLY determined here — never a SECOND
/// AuthStore/process-env resolution that a mid-flight credential rotation could
/// make disagree with the value we captured for `secret`.
///
/// Both cases are pinned deterministically: `api_key_env` always points at the
/// synthetic [`PINNED_KEY_ENV`] and the auth store is always bypassed.
/// * `Some(key)` — served from `env_vars`, so the factory builds with exactly
///   this value.
/// * `None` — the pinned var is left ABSENT, so the factory also resolves to
///   `None` (no key at init ⇒ the factory sees no key, consistently). A key
///   added to the auth store / a real provider env var BETWEEN the single
///   resolution and provider construction is thus never picked up while
///   `secret` stays `None`; `runtime_new` fails cleanly if a key was required.
///
/// NOTE: the key is served through octos's normal `env_vars` value resolution,
/// so a literal value beginning with `keychain:` is interpreted as a keychain
/// reference (octos's secret-indirection convention), not used verbatim — see
/// the credentials note in README.md. Don't pass a raw key starting with
/// `keychain:`.
fn pin_resolved_key(cli_cfg: &mut Config, resolved: Option<&str>) {
    cli_cfg.api_key_env = Some(PINNED_KEY_ENV.to_string());
    cli_cfg.bypass_auth_store = true;
    match resolved {
        Some(key) => {
            cli_cfg
                .env_vars
                .insert(PINNED_KEY_ENV.to_string(), key.to_string());
        }
        None => {
            // Leave the pinned var absent so the factory's lookup yields None.
            cli_cfg.env_vars.remove(PINNED_KEY_ENV);
        }
    }
}

/// Native runtime configuration — the single source of truth consumed by
/// [`OctosRuntime::from_config`].
///
/// The C-ABI ([`octos_runtime_new`]) parses its JSON into this, and the
/// `octos-uniffi` wrapper maps its idiomatic `Config` into it. Retains
/// `Deserialize` so the C JSON parse is byte-for-byte unchanged. Fields are
/// `pub` so the native core can be built directly (e.g. from uniffi) without
/// going through JSON.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RuntimeConfig {
    pub provider: String,
    pub model: String,
    /// Raw API key. Injected into the reused octos `Config` key resolution.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Name of a process env var holding the API key (alternative to `api_key`).
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// API protocol override: `"anthropic"` / `"responses"` (see the reused
    /// `create_provider_with_api_type`). Usually omitted. Retained for exact
    /// C-ABI parity; the uniffi surface leaves it `None`.
    #[serde(default)]
    pub api_type: Option<String>,
    /// Working directory the FS tools are confined to. Defaults to the process
    /// cwd.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Register the `shell` tool. Off by default — an embedded library should
    /// not run shell unless the host asks.
    #[serde(default)]
    pub allow_shell: bool,
    #[serde(default)]
    pub max_iterations: Option<u32>,
    /// Path to a GGUF embedding model. Enables [`OctosRuntime::embed`] (requires
    /// the `embed-llama` build feature). An explicit path is always honoured
    /// as-is; when absent, an `embed-llama` build resolves the bundled default
    /// model (EmbeddingGemma-300M Q8_0) under the data dir — see
    /// [`RuntimeConfig::embedding_auto_download`].
    #[serde(default)]
    pub embedding_model_path: Option<String>,
    /// Whether an `embed-llama` build may download the default embedding
    /// model (334 MB, once, into `<data_dir>/models/`) when
    /// `embedding_model_path` is unset and the file is not already complete.
    /// Default `true`; `OCTOS_NO_MODEL_DOWNLOAD=1` in the process environment
    /// overrides it to `false`. When the download is not allowed (or fails)
    /// the runtime starts WITHOUT an embedder: memory search is keyword-only
    /// and `embed` reports "no embedder configured". The download blocks
    /// runtime creation, so hosts that want control over timing call
    /// `octos_embedding_model_ensure` / `embedding_model_ensure` beforehand.
    /// Without a `data_dir` the model lands in the per-runtime scratch dir
    /// and is deleted with it, so set `data_dir` to keep it.
    #[serde(default)]
    pub embedding_auto_download: Option<bool>,
    /// Persistent data directory. When set, the episode store
    /// (`episodes.redb`) AND the Recall store (`recall.redb` +
    /// `recall-index/`) open under it and survive the runtime; the directory
    /// is never removed by this crate. When absent both live in a per-runtime
    /// scratch dir under the OS temp dir that is deleted on free/drop.
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Vector width kept by the Recall index (Matryoshka truncation target).
    /// Defaults to 256; clamped down to the embedder's own dimension when an
    /// embedder is loaded. Must be > 0.
    #[serde(default)]
    pub recall_dimension: Option<usize>,
}

/// The per-task brief consumed by [`OctosRuntime::run_task`]. The C-ABI
/// ([`octos_run_task`]) parses its JSON into this; retains `Deserialize` so the
/// C JSON parse is unchanged.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskBrief {
    pub prompt: String,
    #[serde(default)]
    pub max_iterations: Option<u32>,
}

/// Input of [`OctosRuntime::memory_upsert`] / `octos_memory_upsert`.
///
/// Each record is the wire form of `octos_memory::Record`: `id`, `kind`
/// (`"document"` | `"episode"`), `source`, `timestamp` (RFC3339), `title`,
/// `abstract`, optional `parent` / `body` / `fingerprint`. Other `Record`
/// fields are ignored or overwritten (`trust` → untrusted, counters reset).
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryUpsertRequest {
    pub records: Vec<Record>,
    /// One entry per record: a raw embedding (any width ≥ the recall
    /// dimension; Matryoshka-truncated by the store) or `null`. When present,
    /// no embedding is done here.
    #[serde(default)]
    pub vectors: Option<Vec<Option<Vec<f32>>>>,
    /// Embed `Record::index_text()` for records without a vector when an
    /// embedder is loaded. Default `true`.
    #[serde(default = "default_true")]
    pub embed: bool,
}

fn default_true() -> bool {
    true
}

/// Input of [`OctosRuntime::memory_search`] / `octos_memory_search`.
#[derive(Debug, Clone, Deserialize)]
pub struct MemorySearchRequest {
    pub query: String,
    /// Record kinds to keep (`"document"`, `"episode"`, `"knowledge"`); empty
    /// = all.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Sources to keep (`"mail"`, `"calendar"`, …); empty = all.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Inclusive lower bound on the record timestamp: RFC3339, or
    /// `YYYY-MM-DD` (start of that day, UTC).
    #[serde(default)]
    pub since: Option<String>,
    /// Inclusive upper bound: RFC3339, or `YYYY-MM-DD` (end of that day, UTC).
    #[serde(default)]
    pub until: Option<String>,
    /// Max hits (default 10; the store clamps to 1..=200).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Token accounting for a completed or incomplete [`OctosRuntime::run_task`].
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// The native result of [`OctosRuntime::run_task`].
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub output: String,
    pub iterations: u32,
    pub tokens: TokenUsage,
}

/// Native error for the core surface ([`OctosRuntime::from_config`],
/// [`OctosRuntime::run_task`], [`OctosRuntime::embed`]).
///
/// Every diagnostic `String` is ALREADY credential-scrubbed via [`scrub_secret`]
/// at the point it is constructed, so `Display` renders it verbatim. The C-ABI
/// then applies its own [`sanitize_error_text`] length-cap/heuristic backstop
/// (unchanged) before exposing it via [`octos_last_error`].
/// `Incomplete.partial` is task output, with the same lossless semantics as a
/// successful [`TaskResult`]; it is never rendered into the diagnostic.
#[derive(Debug)]
pub enum CoreError {
    /// Configuration / runtime-construction failure (bad config, tokio/runtime
    /// or memory-store setup).
    Config(String),
    /// Provider construction failure.
    Provider(String),
    /// Task-execution failure.
    Run(String),
    /// Embedding failure (model load, embed call, or empty result).
    Embed(String),
    /// No embedder is available: either no `embedding_model_path` was configured
    /// or the crate was built without the `embed-llama` feature.
    NoEmbedder,
    /// The provider truncated a conversational response. This is a failure,
    /// with the actual partial output and all consumed turn usage available.
    Incomplete { partial: TaskResult },
    /// Recall-memory failure (`memory_upsert`/`memory_search`/`memory_load`/
    /// `memory_stats`): a malformed request, a rejected record, a missing
    /// record ("no such record") or a store error.
    Memory(String),
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreError::Config(m)
            | CoreError::Provider(m)
            | CoreError::Run(m)
            | CoreError::Embed(m)
            | CoreError::Memory(m) => f.write_str(m),
            CoreError::NoEmbedder => f.write_str("no embedder configured"),
            CoreError::Incomplete { .. } => f.write_str(INCOMPLETE_RESPONSE_MESSAGE),
        }
    }
}

impl std::error::Error for CoreError {}

/// A fixed diagnostic shared by the C and generated binding facades. Partial
/// model output is deliberately excluded from this bounded error channel.
pub const INCOMPLETE_RESPONSE_MESSAGE: &str =
    "Model output was truncated (max_tokens); the response is incomplete";

impl From<&ConversationResponse> for TaskResult {
    fn from(response: &ConversationResponse) -> Self {
        // Preserve the existing successful iteration count: assistant history
        // rows, including tool-call carriers (the final may be separate).
        // The partial's usage already includes all earlier rounds;
        // do not add PartialTurnUsage again (it is the same accumulated total).
        let iterations = response
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .count();
        Self {
            output: response.content.clone(),
            iterations: u32::try_from(iterations).unwrap_or(u32::MAX),
            tokens: TokenUsage {
                input: u64::from(response.token_usage.input_tokens),
                output: u64::from(response.token_usage.output_tokens),
                reasoning: u64::from(response.token_usage.reasoning_tokens),
                cache_read: u64::from(response.token_usage.cache_read_tokens),
                cache_write: u64::from(response.token_usage.cache_write_tokens),
            },
        }
    }
}

/// RAII owner of the ephemeral memory scratch dir that THIS crate created under
/// the OS temp dir. Its `Drop` best-effort removes the dir (errors ignored).
///
/// It only ever owns a `mkdtemp`-style path this crate itself minted — NEVER a
/// user-supplied `cwd`/`data_dir` — so dropping it can never delete caller
/// data; a runtime opened on a caller's `data_dir` holds
/// [`ScratchDir::none`], which owns nothing. It is declared as the LAST field
/// of [`OctosRuntime`] so declaration-order field drop runs it AFTER the
/// `Arc<EpisodeStore>` and `Arc<RecallStore>`: the redb file locks release
/// first, then the dir is removed. That is exactly the ordering the old manual
/// `octos_runtime_free` cleanup guaranteed by hand (a Drop that removed first
/// would fail on Windows / lock-sensitive platforms). Holding it locally in
/// `from_config` before it is moved into the struct also cleans up if
/// construction errors out after the dir exists.
struct ScratchDir(Option<PathBuf>);

impl ScratchDir {
    fn new(path: PathBuf) -> Self {
        ScratchDir(Some(path))
    }

    /// No scratch dir to remove (the stores live in a caller-owned `data_dir`).
    fn none() -> Self {
        ScratchDir(None)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Opaque runtime handle.
///
/// # Thread-safety & lifetime (C-ABI contract)
///
/// An `OctosRuntime` handle is NOT thread-safe. Do not call any function on a
/// handle after [`octos_runtime_free`]. Do not call [`octos_runtime_free`]
/// concurrently with — or while any other call on the same handle is in flight.
/// Serialize all calls on a given handle (or guard it with your own mutex): a
/// concurrent run+free is a use-after-free and free+free is a double-free, and
/// the library cannot prevent either across a C ABI. Also free the handle from
/// a plain (non-async) thread — dropping the held tokio runtime from inside
/// another async/tokio context fails.
///
/// (Internally: shared tokio runtime + provider + memory; a fresh [`Agent`] is
/// built per task so a per-task `max_iterations` can be honored.)
pub struct OctosRuntime {
    tokio: tokio::runtime::Runtime,
    llm: Arc<dyn LlmProvider>,
    memory: Arc<EpisodeStore>,
    /// The Recall index (app records + episodes): `recall.redb` +
    /// `recall-index/` beside `episodes.redb`. Internally `RwLock`-guarded, so
    /// the uniffi facade may call it from several threads.
    recall: Arc<RecallStore>,
    /// The directory both stores were opened in (the caller's `data_dir` or
    /// the scratch dir). The agent's `memory_load` tool reads Knowledge pages
    /// from the markdown bank under `<mem_dir>/memory`.
    mem_dir: PathBuf,
    cwd: PathBuf,
    allow_shell: bool,
    default_max_iterations: u32,
    /// True when an embedding model is in play: the config named one, or
    /// (`embed-llama` builds) the default model was resolved and loaded. Used
    /// to distinguish "no embedder configured" from "not compiled with
    /// embed-llama"; `false` when the runtime fell back to keyword-only.
    /// `pub` so hosts (and the uniffi facade) can tell which mode they got.
    pub embedding_configured: bool,
    #[cfg(feature = "embed-llama")]
    embedder: Option<Arc<octos_embed_llama::LlamaEmbedder>>,
    /// The RESOLVED plaintext key the provider was built with — from whatever
    /// source (env_vars-injected `api_key`, `api_key_env` process var, default
    /// env var, or auth store; see `runtime_new_impl`). Retained ONLY to
    /// exact-scrub it out of provider/agent/embed error text before it reaches
    /// `octos_last_error`. (It already lives inside `llm`, so this is not new
    /// exposure.)
    secret: Option<String>,
    /// RAII owner of the memory scratch dir. MUST be the LAST field so it drops
    /// AFTER `memory` and `recall` (releasing their redb locks) on both the C
    /// free path and a native drop. See [`ScratchDir`]. `allow(dead_code)`: it
    /// exists only for its `Drop` side effect (removing the dir) and its
    /// declaration position — outside `#[cfg(test)]` nothing reads it
    /// explicitly.
    #[allow(dead_code)]
    scratch: ScratchDir,
}

impl OctosRuntime {
    /// Build a runtime from a native [`RuntimeConfig`].
    ///
    /// This is the SINGLE home of the hardened credential path — the C-ABI
    /// (`octos_runtime_new`) and the `octos-uniffi` wrapper both construct the
    /// runtime through here, so the credential logic lives in exactly one place:
    /// canonical provider-name resolution, one-shot `get_api_key` resolution,
    /// pinning that value back so the factory can never do a second
    /// (possibly-rotated) read, provider construction, tool + agent + tokio +
    /// memory setup, and retention of the resolved secret purely to scrub it out
    /// of later error text.
    pub fn from_config(cfg: RuntimeConfig) -> Result<OctosRuntime, CoreError> {
        // Reuse octos-cli's Config + provider factory (key resolution, timeout
        // overrides, api_type bypasses).
        let mut cli_cfg: Config = serde_json::from_str("{}")
            .map_err(|e| CoreError::Config(format!("internal default config: {e}")))?;
        let key_env = cfg
            .api_key_env
            .clone()
            .unwrap_or_else(|| format!("{}_API_KEY", cfg.provider.to_uppercase()));
        if let Some(key) = &cfg.api_key {
            // Inject the raw key so Config::get_api_key resolves it from env_vars,
            // and make it authoritative over any host `octos auth login` credential.
            cli_cfg.api_key_env = Some(key_env.clone());
            cli_cfg.env_vars.insert(key_env, key.clone());
            cli_cfg.bypass_auth_store = true;
        } else if cfg.api_key_env.is_some() {
            // Host explicitly named the process env var; let it win over auth store.
            cli_cfg.api_key_env = cfg.api_key_env.clone();
            cli_cfg.bypass_auth_store = true;
        }

        // SINGLE credential resolution. Resolve the effective key EXACTLY ONCE,
        // from whatever source (env_vars-injected api_key, api_key_env process
        // var, default env var, or — when not bypassed — the auth store), under
        // the CANONICAL provider name the factory uses (so an auth-store key
        // keyed by the canonical name is not missed when the caller used an
        // alias/case variant).
        let canonical = canonical_provider_name(&cfg.provider);
        let resolved_secret = cli_cfg.get_api_key(&canonical).ok();

        // Pin that single resolution back into the config so the factory's OWN
        // `get_api_key` returns this exact value from `env_vars` — never doing a
        // second AuthStore/process-env read that a mid-flight credential rotation
        // could make disagree. After this, `resolved_secret` == the key the
        // provider is built with, by construction, so `secret` mirrors it
        // deterministically. `.ok()` above (not `?`) because a genuinely missing
        // REQUIRED key is not a redaction concern and still fails at
        // `create_provider` below.
        pin_resolved_key(&mut cli_cfg, resolved_secret.as_deref());

        let llm = create_provider_with_api_type(
            &cfg.provider,
            &cli_cfg,
            Some(cfg.model.clone()),
            cfg.base_url.clone(),
            cfg.api_type.as_deref(),
        )
        .map_err(|e| {
            CoreError::Provider(scrub_secret(
                format!("failed to build provider '{}': {e}", cfg.provider),
                resolved_secret.as_deref(),
            ))
        })?;

        let tokio = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| CoreError::Config(format!("failed to build tokio runtime: {e}")))?;

        let cwd = match &cfg.cwd {
            Some(c) => PathBuf::from(c),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let default_max_iterations = cfg
            .max_iterations
            .unwrap_or_else(|| AgentConfig::default().max_iterations);
        let requested_dimension = cfg.recall_dimension.unwrap_or(DEFAULT_RECALL_DIMENSION);
        if requested_dimension == 0 {
            return Err(CoreError::Config(
                "recall_dimension must be greater than zero".to_string(),
            ));
        }

        // Where the memory stores (and the default embedding model) live. With
        // `data_dir` they are persistent and caller-owned (nothing is removed
        // on teardown). Otherwise: a unique scratch dir. Take RAII ownership of
        // the scratch dir the moment we commit to its path, so that if the
        // embedder fails to load, a store fails to open, or any later step
        // errors, the local `scratch` drops and best-effort removes the dir —
        // no mid-init leak. On success it moves into the struct's LAST field
        // and is removed only after the stores' redb locks release on
        // teardown. (`EpisodeStore::open` creates the dir before it can fail,
        // and does not hold the lock on failure.)
        let (mem_dir, scratch) = match &cfg.data_dir {
            Some(dir) => (PathBuf::from(dir), ScratchDir::none()),
            None => {
                let seq = MEM_COUNTER.fetch_add(1, Ordering::Relaxed);
                let dir = std::env::temp_dir().join(format!(
                    "octos-ffi-mem-{}-{}",
                    std::process::id(),
                    seq
                ));
                (dir.clone(), ScratchDir::new(dir))
            }
        };

        // The embedder: an explicit `embedding_model_path` is loaded as-is
        // (failure is fatal); otherwise an `embed-llama` build resolves the
        // default model under `mem_dir` (already complete → load; else fetch
        // when allowed; else keyword-only). Resolved BEFORE the stores open so
        // the Recall config can be pinned to the model that will produce its
        // vectors. `scratch` drops on the error path and removes the dir.
        #[cfg(feature = "embed-llama")]
        let resolved = resolve_embedder(&cfg, &mem_dir)?;
        let embedding_configured = {
            #[cfg(feature = "embed-llama")]
            {
                resolved.embedder.is_some()
            }
            #[cfg(not(feature = "embed-llama"))]
            {
                // Keeps `octos_embed`'s "not compiled in" diagnostic reachable
                // when a model WAS named on a feature-off build.
                cfg.embedding_model_path.is_some()
            }
        };

        // Recall index geometry: the configured width, clamped to what the
        // embedder actually produces; the embedder id pins stored vectors and
        // the persisted graph to the model that made them.
        // `(dimension, id)` of the loaded embedder, if any.
        let loaded_embedder: Option<(usize, String)> = {
            #[cfg(feature = "embed-llama")]
            {
                resolved
                    .embedder
                    .as_ref()
                    .map(|e| (e.dimension().max(1), resolved.id.clone()))
            }
            #[cfg(not(feature = "embed-llama"))]
            {
                None
            }
        };
        let (recall_dimension, embedder_id) = match loaded_embedder {
            Some((dim, id)) => (requested_dimension.min(dim), id),
            None => (requested_dimension, String::new()),
        };
        let recall_cfg = RecallConfig {
            dimension: recall_dimension,
            embedder_id,
            ..RecallConfig::default()
        };

        let memory = match tokio.block_on(EpisodeStore::open(&mem_dir)) {
            Ok(store) => Arc::new(store),
            Err(e) => {
                // `scratch` drops on this early return, removing the dir.
                return Err(CoreError::Config(format!(
                    "failed to open memory store: {e}"
                )));
            }
        };
        let recall = match RecallStore::open(&mem_dir, recall_cfg) {
            Ok(store) => Arc::new(store),
            Err(e) => {
                // `memory` drops first (releasing its lock), then `scratch`.
                return Err(CoreError::Config(format!(
                    "failed to open recall store: {e}"
                )));
            }
        };

        Ok(OctosRuntime {
            tokio,
            llm,
            memory,
            recall,
            mem_dir,
            cwd,
            allow_shell: cfg.allow_shell,
            default_max_iterations,
            embedding_configured,
            #[cfg(feature = "embed-llama")]
            embedder: resolved.embedder,
            secret: resolved_secret,
            scratch,
        })
    }

    /// Run a one-shot task, returning its native [`TaskResult`]. Builds a fresh
    /// agent (honoring a per-task `max_iterations`), blocks on the agent loop,
    /// derives the iteration count from the assistant-message count, and folds
    /// up token usage. Error text is credential-scrubbed.
    pub fn run_task(&self, brief: &TaskBrief) -> Result<TaskResult, CoreError> {
        if brief.prompt.trim().is_empty() {
            return Err(CoreError::Run("prompt is empty".to_string()));
        }
        let max_iter = brief.max_iterations.unwrap_or(self.default_max_iterations);
        let agent = self.build_agent(max_iter);
        let response = self
            .tokio
            .block_on(agent.process_message(&brief.prompt, &[], Vec::new()))
            .map_err(|e| {
                if let Some(incomplete) = e.downcast_ref::<IncompleteResponseError>() {
                    let mut partial = TaskResult::from(&incomplete.partial);
                    // MaxTokens carries the current assistant text separately
                    // from messages. Use its producer iteration instead of
                    // silently undercounting that still-consumed final round.
                    partial.iterations = partial
                        .iterations
                        .max(incomplete.partial.assistant_segments.final_iteration);
                    return CoreError::Incomplete { partial };
                }
                CoreError::Run(scrub_secret(
                    format!("agent run failed: {e}"),
                    self.secret.as_deref(),
                ))
            })?;

        Ok(TaskResult::from(&response))
    }

    /// Embed `text`, returning the raw vector. Requires the `embed-llama`
    /// feature and a configured `embedding_model_path`; otherwise
    /// [`CoreError::NoEmbedder`]. Error text is credential-scrubbed.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>, CoreError> {
        let mut vectors = self.embed_batch(&[text])?;
        Ok(vectors.swap_remove(0))
    }

    /// Whether a real embedder is loaded (feature on AND a model path given).
    fn has_embedder(&self) -> bool {
        #[cfg(feature = "embed-llama")]
        {
            self.embedder.is_some()
        }
        #[cfg(not(feature = "embed-llama"))]
        {
            false
        }
    }

    /// Embed a batch, returning exactly `texts.len()` vectors. Error text is
    /// credential-scrubbed.
    #[cfg(feature = "embed-llama")]
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, CoreError> {
        let embedder = self.embedder.as_ref().ok_or(CoreError::NoEmbedder)?;
        let vectors = self.tokio.block_on(embedder.embed(texts)).map_err(|e| {
            CoreError::Embed(scrub_secret(
                format!("embed failed: {e}"),
                self.secret.as_deref(),
            ))
        })?;
        if vectors.is_empty() {
            return Err(CoreError::Embed("embedder returned no vectors".to_string()));
        }
        if vectors.len() != texts.len() {
            return Err(CoreError::Embed(format!(
                "embedder returned {} vectors for {} texts",
                vectors.len(),
                texts.len()
            )));
        }
        Ok(vectors)
    }

    /// Embed stub when the crate is built without the `embed-llama` feature —
    /// always [`CoreError::NoEmbedder`].
    #[cfg(not(feature = "embed-llama"))]
    fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, CoreError> {
        Err(CoreError::NoEmbedder)
    }

    fn memory_err(&self, msg: impl Into<String>) -> CoreError {
        CoreError::Memory(scrub_secret(msg.into(), self.secret.as_deref()))
    }

    /// Insert or update app records in the Recall index.
    ///
    /// Input JSON: `{"records": [Record…], "vectors"?: [[f32…]|null…],
    /// "embed"?: bool}` — see [`MemoryUpsertRequest`]. Output JSON: the
    /// store's `UpsertReport` (`inserted`, `updated`, `unchanged`,
    /// `vectors_stored`) plus `"embedded"`, the number of records this call
    /// embedded itself.
    ///
    /// Rules: at most [`MAX_UPSERT_RECORDS`] records; every record needs a
    /// non-empty `id`; `kind: "knowledge"` is rejected (bank pages are not
    /// written through this seam); `trust` is forced to `untrusted` and the
    /// usage counters (`visits`, `last_visit`, `promoted`) are kernel-owned and
    /// reset (the store keeps the existing ones on re-ingest). When `vectors`
    /// is given its length must equal `records`'. When it is absent, `embed` is
    /// true (the default) and an embedder is loaded, the store is first asked
    /// which records need a vector (`RecallStore::needs_vectors`: new id,
    /// changed fingerprint / index text, or no usable stored vector) and only
    /// those records' `Record::index_text()` are embedded, in batches of 16 —
    /// re-submitting an unchanged batch embeds nothing and reports
    /// `embedded: 0`. Without an embedder the records are indexed BM25-only.
    /// The HNSW graph is persisted after the batch.
    pub fn memory_upsert(&self, json: &str) -> Result<String, CoreError> {
        let req: MemoryUpsertRequest = serde_json::from_str(json)
            .map_err(|e| self.memory_err(format!("invalid upsert json: {e}")))?;
        if req.records.len() > MAX_UPSERT_RECORDS {
            return Err(self.memory_err(format!(
                "too many records: {} > {MAX_UPSERT_RECORDS} per call",
                req.records.len()
            )));
        }
        let mut records = req.records;
        for (i, r) in records.iter_mut().enumerate() {
            if r.id.trim().is_empty() {
                return Err(self.memory_err(format!("record #{i}: id is empty")));
            }
            if r.kind == RecordKind::Knowledge {
                return Err(self.memory_err(format!(
                    "record '{}': kind \"knowledge\" is not accepted through this seam",
                    r.id
                )));
            }
            r.trust = Trust::Untrusted;
            r.visits = 0;
            r.last_visit = None;
            r.promoted = false;
        }

        let mut embedded = 0usize;
        let vectors: Vec<Option<Vec<f32>>> = match req.vectors {
            Some(v) => {
                if v.len() != records.len() {
                    return Err(self.memory_err(format!(
                        "vectors length {} does not match records length {}",
                        v.len(),
                        records.len()
                    )));
                }
                v
            }
            None if req.embed && self.has_embedder() && !records.is_empty() => {
                let needs = self
                    .recall
                    .needs_vectors(&records)
                    .map_err(|e| self.memory_err(format!("memory vector probe failed: {e}")))?;
                let (out, count) =
                    embed_flagged(&records, &needs, |texts| self.embed_batch(texts))?;
                embedded = count;
                out
            }
            None => vec![None; records.len()],
        };

        let report = self
            .recall
            .upsert(records, vectors)
            .map_err(|e| self.memory_err(format!("memory upsert failed: {e}")))?;
        self.recall
            .persist_index()
            .map_err(|e| self.memory_err(format!("memory index persist failed: {e}")))?;
        let mut out = serde_json::to_value(&report)
            .map_err(|e| self.memory_err(format!("serialize upsert report: {e}")))?;
        out["embedded"] = json!(embedded);
        Ok(out.to_string())
    }

    /// Two-stage retrieval, stage one: rank records and return their
    /// abstracts. Input JSON: `{"query": str, "kinds"?: [str], "sources"?:
    /// [str], "since"?: str, "until"?: str, "limit"?: n}` — see
    /// [`MemorySearchRequest`]. Output JSON: `{"hits": [Hit…]}` with each hit
    /// `{id, kind, source, title, abstract, score, timestamp, trust}`. The
    /// query is embedded when an embedder is loaded (hybrid ranking), else
    /// BM25 only.
    pub fn memory_search(&self, json: &str) -> Result<String, CoreError> {
        let req: MemorySearchRequest = serde_json::from_str(json)
            .map_err(|e| self.memory_err(format!("invalid search json: {e}")))?;
        let query = req.query.trim();
        if query.is_empty() {
            return Err(self.memory_err("query is empty"));
        }
        let mut kinds = Vec::with_capacity(req.kinds.len());
        for k in &req.kinds {
            kinds.push(
                RecordKind::parse(k)
                    .ok_or_else(|| self.memory_err(format!("unknown kind '{k}'")))?,
            );
        }
        let since = req
            .since
            .as_deref()
            .map(|s| parse_time_bound(s, false))
            .transpose()
            .map_err(|e| self.memory_err(format!("since: {e}")))?;
        let until = req
            .until
            .as_deref()
            .map(|s| parse_time_bound(s, true))
            .transpose()
            .map_err(|e| self.memory_err(format!("until: {e}")))?;
        let filter = SearchFilter {
            kinds,
            sources: req.sources,
            since,
            until,
            limit: req.limit.unwrap_or(DEFAULT_SEARCH_LIMIT),
        };
        let query_vector = if self.has_embedder() {
            Some(self.embed(query)?)
        } else {
            None
        };
        let hits = self
            .recall
            .search(query, query_vector.as_deref(), &filter)
            .map_err(|e| self.memory_err(format!("memory search failed: {e}")))?;
        serde_json::to_string(&json!({ "hits": hits }))
            .map_err(|e| self.memory_err(format!("serialize hits: {e}")))
    }

    /// Stage two: load one record by id, counting the visit (`touch`). Output
    /// JSON: `{"record": Record}`; error "no such record" when absent.
    pub fn memory_load(&self, id: &str) -> Result<String, CoreError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(self.memory_err("id is empty"));
        }
        let touched = self
            .recall
            .touch(id)
            .map_err(|e| self.memory_err(format!("memory touch failed: {e}")))?;
        let record = if touched {
            self.recall
                .get(id)
                .map_err(|e| self.memory_err(format!("memory load failed: {e}")))?
        } else {
            None
        };
        let record = record.ok_or_else(|| self.memory_err("no such record"))?;
        serde_json::to_string(&json!({ "record": record }))
            .map_err(|e| self.memory_err(format!("serialize record: {e}")))
    }

    /// The Recall store's `RecallStats` as JSON (`records`, `vectors_stored`,
    /// `vectors_resident`, `by_kind`, `by_source`, `dimension`, `embedder_id`,
    /// `graph_persisted`, `disk_bytes`).
    pub fn memory_stats(&self) -> Result<String, CoreError> {
        serde_json::to_string(&self.recall.stats())
            .map_err(|e| self.memory_err(format!("serialize stats: {e}")))
    }

    /// The runtime's embedder as a shared `EmbeddingProvider`, when the
    /// `embed-llama` feature is on and a model was configured; `None`
    /// otherwise (the memory tools then rank BM25-only).
    fn embedding_provider(&self) -> Option<Arc<dyn octos_llm::EmbeddingProvider>> {
        #[cfg(feature = "embed-llama")]
        {
            self.embedder
                .clone()
                .map(|e| e as Arc<dyn octos_llm::EmbeddingProvider>)
        }
        #[cfg(not(feature = "embed-llama"))]
        {
            None
        }
    }

    /// Build a fresh agent with a cwd-scoped FS toolset plus the two-stage
    /// memory retrieval tools (`memory_search` / `memory_load`) over this
    /// runtime's Recall store, and the store attached so saved episodes are
    /// mirrored into it.
    fn build_agent(&self, max_iterations: u32) -> Agent {
        let tools = self.build_tools();
        let config = AgentConfig {
            max_iterations,
            ..AgentConfig::default()
        };
        Agent::new(
            AgentId::new("ffi"),
            self.llm.clone(),
            tools,
            self.memory.clone(),
        )
        .with_recall(self.recall.clone())
        .with_config(config)
    }

    /// The FS toolset (see [`build_tools`]) plus `memory_search` and
    /// `memory_load` bound to this runtime's Recall store, embedder and
    /// markdown memory bank (`<mem_dir>/memory`).
    fn build_tools(&self) -> ToolRegistry {
        let mut registry = build_tools(&self.cwd, self.allow_shell);
        registry.register(MemorySearchTool::new(
            self.recall.clone(),
            self.embedding_provider(),
        ));
        let bank = Arc::new(MemoryStore::at_memory_dir(self.mem_dir.join("memory")));
        registry.register(MemoryLoadTool::new(self.recall.clone(), bank));
        registry
    }
}

/// Per-record vectors (`None` where nothing was embedded) plus how many
/// vectors were actually produced — the result of [`embed_flagged`].
type EmbeddedVectors = (Vec<Option<Vec<f32>>>, usize);

/// Embed only the records flagged in `needs` (parallel to `records`),
/// keeping positions: the returned `vectors` has one entry per record,
/// `Some` for embedded ones and `None` for the rest. Batches of
/// [`EMBED_BATCH`]. Also returns how many vectors were produced.
fn embed_flagged<E>(
    records: &[Record],
    needs: &[bool],
    mut embed: E,
) -> Result<EmbeddedVectors, CoreError>
where
    E: FnMut(&[&str]) -> Result<Vec<Vec<f32>>, CoreError>,
{
    let targets: Vec<usize> = needs
        .iter()
        .enumerate()
        .filter_map(|(i, &needed)| needed.then_some(i))
        .collect();
    let mut out: Vec<Option<Vec<f32>>> = vec![None; records.len()];
    let mut embedded = 0usize;
    for chunk in targets.chunks(EMBED_BATCH) {
        let texts: Vec<String> = chunk.iter().map(|&i| records[i].index_text()).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let vectors = embed(&refs)?;
        embedded += vectors.len();
        for (&i, vector) in chunk.iter().zip(vectors) {
            out[i] = Some(vector);
        }
    }
    Ok((out, embedded))
}

/// Minimal FS toolset, every tool confined to `cwd` via the default
/// `FilesystemScope::Workspace`. `shell` is added only when explicitly allowed
/// (and defaults to `SafePolicy`, which denies `rm -rf /`, `dd`, fork bombs…).
fn build_tools(cwd: &Path, allow_shell: bool) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(ReadFileTool::new(cwd));
    registry.register(WriteFileTool::new(cwd));
    registry.register(ListDirTool::new(cwd));
    registry.register(GlobTool::new(cwd));
    registry.register(GrepTool::new(cwd));
    if allow_shell {
        registry.register(ShellTool::new(cwd));
    }
    registry
}

/// Parse a search time bound: RFC3339 (any offset, normalised to UTC) or a
/// bare `YYYY-MM-DD`, which means the start of that UTC day for a lower bound
/// and the end of it (`23:59:59.999`) for an upper bound.
fn parse_time_bound(s: &str, end_of_day: bool) -> Result<DateTime<Utc>, String> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| format!("'{s}' is neither RFC3339 nor YYYY-MM-DD"))?;
    let time = if end_of_day {
        date.and_hms_milli_opt(23, 59, 59, 999)
    } else {
        date.and_hms_opt(0, 0, 0)
    };
    time.map(|t| t.and_utc())
        .ok_or_else(|| format!("'{s}' is not a valid date"))
}

/// Convert a C string pointer to `&str`, rejecting NULL and non-UTF-8.
///
/// # Safety
/// `ptr` must be NULL or point to a valid NUL-terminated C string that stays
/// alive for the duration of the returned borrow.
unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err("null pointer argument".to_string());
    }
    // SAFETY: non-null verified above; the caller guarantees NUL-termination
    // and that the buffer outlives this borrow.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| "argument is not valid UTF-8".to_string())
}

/// Move an owned `String` across the ABI as a `CString` the caller frees via
/// [`octos_string_free`].
fn to_owned_ptr(s: String) -> Result<*mut c_char, String> {
    CString::new(s)
        .map(CString::into_raw)
        .map_err(|_| "output contained an interior NUL byte".to_string())
}

/// Panic firewall around an FFI body.
///
/// Runs `body` inside [`catch_unwind`]; a panic never crosses the C ABI (which
/// would be UB). On a caught panic it returns `default` and records a generic
/// last-error, and it LEAKS the panic payload via [`std::mem::forget`] instead
/// of dropping it — a panicking `Drop` on the payload could otherwise unwind
/// past the boundary. The whole body (including `clear_last_error`, the impl
/// call, error conversion, and `set_last_error`) must run through here, so those
/// are placed inside the closure at each call site. The best-effort last-error
/// write is itself firewalled and its payload leaked too.
fn guard<T>(default: T, ctx: &'static str, body: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(payload) => {
            std::mem::forget(payload);
            if let Err(p) = catch_unwind(AssertUnwindSafe(|| {
                set_last_error(format!("panic caught in {ctx}"))
            })) {
                std::mem::forget(p);
            }
            default
        }
    }
}

#[cfg(feature = "embed-llama")]
fn build_embedder(path: &Path) -> Result<Arc<octos_embed_llama::LlamaEmbedder>, CoreError> {
    // n_gpu_layers = 0 -> CPU; hosts wanting Metal/CUDA build the
    // corresponding feature which changes the linked backend.
    let embedder = octos_embed_llama::LlamaEmbedder::from_model_file(path, 0).map_err(|e| {
        CoreError::Embed(format!(
            "failed to load embedding model '{}': {e}",
            path.display()
        ))
    })?;
    Ok(Arc::new(embedder))
}

/// The embedder an `embed-llama` runtime ended up with (see
/// [`resolve_embedder`]).
#[cfg(feature = "embed-llama")]
struct ResolvedEmbedder {
    /// `None` = keyword-only (no path configured and no default model).
    embedder: Option<Arc<octos_embed_llama::LlamaEmbedder>>,
    /// The `RecallConfig::embedder_id` that pins stored vectors to the model:
    /// `llamacpp/<path>` for an explicit path,
    /// [`embed_model::DEFAULT_MODEL_ID`] for the default model, empty when
    /// there is no embedder.
    id: String,
}

/// Pick and load the runtime's embedder. An explicit `embedding_model_path`
/// is loaded verbatim and a load failure is fatal (unchanged semantics).
/// Otherwise the default model is resolved under `data_dir`
/// ([`resolve_default_model`]); when it is unavailable the runtime runs
/// keyword-only rather than failing, so a host that is offline on first
/// launch still gets a working (BM25) memory. A default model that IS on
/// disk but fails to load is reported, like an explicit one.
#[cfg(feature = "embed-llama")]
fn resolve_embedder(cfg: &RuntimeConfig, data_dir: &Path) -> Result<ResolvedEmbedder, CoreError> {
    if let Some(path) = cfg.embedding_model_path.as_deref() {
        return Ok(ResolvedEmbedder {
            embedder: Some(build_embedder(Path::new(path))?),
            id: format!("llamacpp/{path}"),
        });
    }
    match resolve_default_model(data_dir, cfg.embedding_auto_download) {
        Some(path) => Ok(ResolvedEmbedder {
            embedder: Some(build_embedder(&path)?),
            id: embed_model::DEFAULT_MODEL_ID.to_string(),
        }),
        None => Ok(ResolvedEmbedder {
            embedder: None,
            id: String::new(),
        }),
    }
}

/// Where the default model is for `data_dir`, fetching it first when it is
/// absent/incomplete and `embed_model::downloads_allowed(auto_download)`.
/// `None` means "run keyword-only": downloads are disabled, or the download
/// failed (logged at `warn`; a host wanting the error itself calls
/// [`embedding_model_ensure`] before building the runtime).
#[cfg(feature = "embed-llama")]
fn resolve_default_model(data_dir: &Path, auto_download: Option<bool>) -> Option<PathBuf> {
    let status = embed_model::model_status(data_dir);
    if status.complete {
        return Some(status.path);
    }
    if !embed_model::downloads_allowed(auto_download) {
        tracing::warn!(
            path = %status.path.display(),
            "default embedding model is not present and automatic download is disabled; \
             memory search runs keyword-only"
        );
        return None;
    }
    match embed_model::ensure_default_model(data_dir, true) {
        Ok(path) => Some(path),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "default embedding model download failed; memory search runs keyword-only"
            );
            None
        }
    }
}

/// Reject an empty data dir before it silently resolves to `models/…` under
/// the process cwd.
fn checked_data_dir(data_dir: &Path) -> Result<&Path, CoreError> {
    if data_dir.as_os_str().is_empty() {
        return Err(CoreError::Embed("data_dir is empty".to_string()));
    }
    Ok(data_dir)
}

/// What is on disk for the default embedding model under `data_dir`, as the
/// JSON `{"path", "present", "bytes", "complete", "url", "license_url",
/// "sha256"}` the C-ABI and uniffi facades return (`octos_embedding_model_status`
/// / `embedding_model_status`). `complete` is `present` AND the pinned size;
/// `url` is where [`embedding_model_ensure`] fetches from and `license_url`
/// the terms that apply to the weights (Gemma Terms of Use). Needs no
/// runtime and never touches the network. Available in every build; the model
/// is only USED by `embed-llama` builds.
pub fn embedding_model_status(data_dir: &Path) -> Result<String, CoreError> {
    let status = embed_model::model_status(checked_data_dir(data_dir)?);
    let out = json!({
        "path": status.path.to_string_lossy(),
        "present": status.present,
        "bytes": status.bytes,
        "complete": status.complete,
        "url": embed_model::DEFAULT_MODEL_URL,
        "license_url": embed_model::DEFAULT_MODEL_LICENSE_URL,
        "sha256": embed_model::DEFAULT_MODEL_SHA256,
    });
    Ok(out.to_string())
}

/// Make sure the default embedding model is complete under `data_dir`,
/// downloading (and SHA-256-verifying) it when `download` is true, and
/// return JSON `{"path"}`. Blocks for the whole 334 MB transfer; call it from
/// a plain thread BEFORE building a runtime so the runtime constructor finds
/// the file and does not download itself. Fails with [`CoreError::Embed`] when
/// the file is absent and `download` is false — or `OCTOS_NO_MODEL_DOWNLOAD`
/// is set in the environment, which vetoes even an explicit `true` — or the
/// download does not verify (the partial file is discarded).
pub fn embedding_model_ensure(data_dir: &Path, download: bool) -> Result<String, CoreError> {
    let data_dir = checked_data_dir(data_dir)?;
    let allowed = embed_model::downloads_allowed(Some(download));
    let path = embed_model::ensure_default_model(data_dir, allowed)
        .map_err(|e| CoreError::Embed(format!("{e:#}")))?;
    Ok(json!({ "path": path.to_string_lossy() }).to_string())
}

// ---------------------------------------------------------------------------
// C-ABI surface
// ---------------------------------------------------------------------------

/// Create a runtime from a JSON config. Returns NULL on error (see
/// [`octos_last_error`]); the returned handle must be freed with
/// [`octos_runtime_free`].
#[unsafe(no_mangle)]
pub extern "C" fn octos_runtime_new(config_json: *const c_char) -> *mut OctosRuntime {
    // Entire body — clear/set last-error and the build — runs inside the panic
    // firewall so nothing can unwind across the C ABI.
    guard(ptr::null_mut(), "octos_runtime_new", || {
        clear_last_error();
        match runtime_new_impl(config_json) {
            Ok(p) => p,
            Err(msg) => {
                set_last_error(msg);
                ptr::null_mut()
            }
        }
    })
}

fn runtime_new_impl(config_json: *const c_char) -> Result<*mut OctosRuntime, String> {
    // SAFETY: `cstr_to_str` NULL-checks and UTF-8-validates.
    let raw = unsafe { cstr_to_str(config_json) }.map_err(|e| format!("config_json: {e}"))?;
    let cfg: RuntimeConfig =
        serde_json::from_str(raw).map_err(|e| format!("invalid config_json: {e}"))?;

    // All construction (incl. the hardened credential path) lives in the native
    // core; the C-ABI only marshals JSON in and a boxed handle out. `Display` on
    // `CoreError` renders the already-scrubbed message verbatim, so the stored
    // last-error is unchanged.
    let runtime = OctosRuntime::from_config(cfg).map_err(|e| e.to_string())?;
    Ok(Box::into_raw(Box::new(runtime)))
}

/// Free a runtime created by [`octos_runtime_new`]. NULL is a no-op.
///
/// Call from a plain (non-async) thread and never concurrently with another
/// call on the same handle (see [`OctosRuntime`]).
#[unsafe(no_mangle)]
pub extern "C" fn octos_runtime_free(runtime: *mut OctosRuntime) {
    guard((), "octos_runtime_free", || {
        if runtime.is_null() {
            return;
        }
        // SAFETY: `runtime` came from `Box::into_raw` in `octos_runtime_new`
        // and is reconstructed exactly once (caller must not double-free).
        let boxed = unsafe { Box::from_raw(runtime) };
        // Dropping the box drops OctosRuntime's fields in declaration order: the
        // `Arc<EpisodeStore>` (releasing the redb file lock) BEFORE the trailing
        // `ScratchDir`, whose Drop removes the temp dir — the same order the old
        // manual cleanup guaranteed by hand, now enforced by field order + RAII.
        //
        // NOTE: dropping the held tokio runtime here PANICS if the host calls
        // this from inside its own async/tokio context ("Cannot drop a runtime
        // in a context where blocking is not allowed"). That panic is contained
        // by `guard` (no UB), but the drop is then incomplete and the scratch
        // dir may leak — hosts must free from a plain, non-async thread.
        drop(boxed);
    });
}

/// Run a one-shot task. `brief_json` is `{"prompt": "...", "max_iterations"?:
/// N}`. Returns owned JSON `{"output", "iterations", "tokens"}` that the caller
/// must free, UNMODIFIED, with [`octos_string_free`] — or NULL on error.
/// On an incomplete response, NULL still means failure; retrieve the partial
/// output separately with [`octos_take_last_partial_result`] on this thread.
#[unsafe(no_mangle)]
pub extern "C" fn octos_run_task(
    runtime: *mut OctosRuntime,
    brief_json: *const c_char,
) -> *mut c_char {
    guard(ptr::null_mut(), "octos_run_task", || {
        clear_last_error();
        match run_task_impl(runtime, brief_json) {
            Ok(p) => p,
            Err(error) => {
                set_last_error(error.to_string());
                if let CoreError::Incomplete { partial } = error {
                    LAST_PARTIAL_RESULT.with(|slot| *slot.borrow_mut() = Some(partial));
                }
                ptr::null_mut()
            }
        }
    })
}

fn run_task_impl(
    runtime: *mut OctosRuntime,
    brief_json: *const c_char,
) -> Result<*mut c_char, CoreError> {
    // SAFETY: NULL-checked; must be a live handle from `octos_runtime_new`.
    let rt = unsafe { runtime.as_ref() }
        .ok_or_else(|| CoreError::Run("runtime pointer is null".to_string()))?;
    // SAFETY: `cstr_to_str` NULL-checks and UTF-8-validates.
    let raw = unsafe { cstr_to_str(brief_json) }.map_err(CoreError::Run)?;
    let brief: TaskBrief = serde_json::from_str(raw)
        .map_err(|e| CoreError::Run(format!("invalid brief_json: {e}")))?;
    // Preserve the exact C-ABI message for an empty prompt (the native
    // `run_task` also guards this, with its own message, for the uniffi path).
    if brief.prompt.trim().is_empty() {
        return Err(CoreError::Run("brief_json.prompt is empty".to_string()));
    }

    // Run through the native core, then serialize its `TaskResult` into the
    // exact JSON shape/order the C-ABI has always emitted.
    let result = rt.run_task(&brief)?;
    to_owned_ptr(task_result_json(&result)).map_err(CoreError::Run)
}

fn task_result_json(result: &TaskResult) -> String {
    json!({
        "output": result.output,
        "iterations": result.iterations,
        "tokens": {
            "input": result.tokens.input,
            "output": result.tokens.output,
            "reasoning": result.tokens.reasoning,
            "cache_read": result.tokens.cache_read,
            "cache_write": result.tokens.cache_write,
        }
    })
    .to_string()
}

/// Embed `text`. Returns owned JSON `{"embedding": [f32, ...]}` that the caller
/// must free, UNMODIFIED, with [`octos_string_free`] — or NULL on error.
/// Requires the `embed-llama` feature and a loaded model: the config's
/// `embedding_model_path`, or the default model resolved at
/// [`octos_runtime_new`] (see [`octos_embedding_model_ensure`]). Without one
/// the last error is "no embedder configured".
#[unsafe(no_mangle)]
pub extern "C" fn octos_embed(runtime: *mut OctosRuntime, text: *const c_char) -> *mut c_char {
    guard(ptr::null_mut(), "octos_embed", || {
        clear_last_error();
        match embed_entry(runtime, text) {
            Ok(p) => p,
            Err(msg) => {
                set_last_error(msg);
                ptr::null_mut()
            }
        }
    })
}

fn embed_entry(runtime: *mut OctosRuntime, text: *const c_char) -> Result<*mut c_char, String> {
    // SAFETY: NULL-checked; must be a live handle from `octos_runtime_new`.
    let rt = unsafe { runtime.as_ref() }.ok_or_else(|| "runtime pointer is null".to_string())?;
    // SAFETY: `cstr_to_str` NULL-checks and UTF-8-validates.
    let text = unsafe { cstr_to_str(text) }?;
    // Preserve the exact C-ABI distinction: "no embedder configured" when the
    // config named no model path, vs "embedding support not compiled in…" when a
    // model was named but the feature is off (the latter is decided in
    // `embed_serialize`, whose feature split predates the native core).
    if !rt.embedding_configured {
        return Err("no embedder configured".to_string());
    }
    embed_serialize(rt, text)
}

/// Serialize an embedding produced by the native [`OctosRuntime::embed`] core.
/// Split by feature so the "not compiled in" C-ABI message is preserved exactly.
#[cfg(feature = "embed-llama")]
fn embed_serialize(rt: &OctosRuntime, text: &str) -> Result<*mut c_char, String> {
    let vector = rt.embed(text).map_err(|e| e.to_string())?;
    let serialized = serde_json::to_string(&json!({ "embedding": vector }))
        .map_err(|e| format!("serialize embedding: {e}"))?;
    to_owned_ptr(serialized)
}

#[cfg(not(feature = "embed-llama"))]
fn embed_serialize(_rt: &OctosRuntime, _text: &str) -> Result<*mut c_char, String> {
    Err(
        "embedding support not compiled in (rebuild octos-ffi with --features embed-llama)"
            .to_string(),
    )
}

/// Shared body of the `octos_embedding_model_*` entry points: panic firewall,
/// last-error reset, `data_dir` NULL/UTF-8 check, then `body` producing the
/// owned JSON string. No runtime handle is involved.
fn model_ffi(
    data_dir: *const c_char,
    ctx: &'static str,
    body: impl FnOnce(&Path) -> Result<String, CoreError>,
) -> *mut c_char {
    guard(ptr::null_mut(), ctx, || {
        clear_last_error();
        // SAFETY: `cstr_to_str` NULL-checks and UTF-8-validates.
        let result = unsafe { cstr_to_str(data_dir) }
            .map_err(|e| format!("data_dir: {e}"))
            .and_then(|dir| body(Path::new(dir)).map_err(|e| e.to_string()))
            .and_then(to_owned_ptr);
        match result {
            Ok(p) => p,
            Err(msg) => {
                set_last_error(msg);
                ptr::null_mut()
            }
        }
    })
}

/// Report what is on disk for the default embedding model (EmbeddingGemma-300M
/// Q8_0) under `data_dir` — the same directory a runtime's `data_dir` config
/// names. Needs NO runtime handle and never touches the network. Returns owned
/// JSON `{"path", "present", "bytes", "complete", "url", "license_url",
/// "sha256"}` that the caller must free, UNMODIFIED, with [`octos_string_free`]
/// — or NULL on error. `complete` means present at the pinned size (a partial
/// download is `present` but not `complete`); `url` is the public release the
/// file is fetched from and `license_url` the terms that apply to the weights
/// (Gemma Terms of Use).
#[unsafe(no_mangle)]
pub extern "C" fn octos_embedding_model_status(data_dir: *const c_char) -> *mut c_char {
    model_ffi(
        data_dir,
        "octos_embedding_model_status",
        embedding_model_status,
    )
}

/// Make sure the default embedding model is complete under `data_dir`,
/// downloading and SHA-256-verifying it (334 MB, once) when `download` is
/// true. Needs NO runtime handle, so a host can provision the model — on its
/// own schedule, from a plain thread — BEFORE [`octos_runtime_new`], which
/// otherwise blocks on the same download when `embedding_auto_download` is
/// not `false`. Returns owned JSON `{"path"}` that the caller must free,
/// UNMODIFIED, with [`octos_string_free`] — or NULL on error: the file is
/// absent and `download` is false (or `OCTOS_NO_MODEL_DOWNLOAD` is set in the
/// environment, which vetoes even an explicit `true`), or the download failed
/// or did not verify (the partial file is discarded).
#[unsafe(no_mangle)]
pub extern "C" fn octos_embedding_model_ensure(
    data_dir: *const c_char,
    download: bool,
) -> *mut c_char {
    model_ffi(data_dir, "octos_embedding_model_ensure", |dir| {
        embedding_model_ensure(dir, download)
    })
}

/// Shared body of the `octos_memory_*` entry points: panic firewall, last-error
/// reset, runtime NULL check, then `body` producing the owned JSON string.
fn memory_ffi(
    runtime: *mut OctosRuntime,
    ctx: &'static str,
    body: impl FnOnce(&OctosRuntime) -> Result<String, String>,
) -> *mut c_char {
    guard(ptr::null_mut(), ctx, || {
        clear_last_error();
        // SAFETY: NULL-checked; must be a live handle from `octos_runtime_new`.
        let result = unsafe { runtime.as_ref() }
            .ok_or_else(|| "runtime pointer is null".to_string())
            .and_then(body)
            .and_then(to_owned_ptr);
        match result {
            Ok(p) => p,
            Err(msg) => {
                set_last_error(msg);
                ptr::null_mut()
            }
        }
    })
}

/// Push app records into the Recall memory index. `request_json` is
/// `{"records": [Record…], "vectors"?: [[f32…]|null…], "embed"?: bool}` where a
/// Record is `{"id", "kind": "document"|"episode", "source", "timestamp":
/// RFC3339, "title", "abstract", "parent"?, "body"?, "fingerprint"?}`. At most
/// 500 records per call; `kind: "knowledge"` is rejected; `trust` is forced to
/// untrusted. Without `vectors`, records are embedded here when an embedder is
/// loaded (else indexed BM25-only). Returns owned JSON `{"inserted",
/// "updated", "unchanged", "vectors_stored", "embedded"}` that the caller must
/// free, UNMODIFIED, with [`octos_string_free`] — or NULL on error.
#[unsafe(no_mangle)]
pub extern "C" fn octos_memory_upsert(
    runtime: *mut OctosRuntime,
    request_json: *const c_char,
) -> *mut c_char {
    memory_ffi(runtime, "octos_memory_upsert", |rt| {
        // SAFETY: `cstr_to_str` NULL-checks and UTF-8-validates.
        let raw = unsafe { cstr_to_str(request_json) }.map_err(|e| format!("request_json: {e}"))?;
        rt.memory_upsert(raw).map_err(|e| e.to_string())
    })
}

/// Search the Recall memory index. `request_json` is `{"query": "...",
/// "kinds"?: [..], "sources"?: [..], "since"?: RFC3339|YYYY-MM-DD, "until"?:
/// RFC3339|YYYY-MM-DD, "limit"?: N}`. Returns owned JSON `{"hits": [{"id",
/// "kind", "source", "title", "abstract", "score", "timestamp", "trust"}…]}`
/// that the caller must free, UNMODIFIED, with [`octos_string_free`] — or NULL
/// on error. The query is embedded when an embedder is loaded; BM25 otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn octos_memory_search(
    runtime: *mut OctosRuntime,
    request_json: *const c_char,
) -> *mut c_char {
    memory_ffi(runtime, "octos_memory_search", |rt| {
        // SAFETY: `cstr_to_str` NULL-checks and UTF-8-validates.
        let raw = unsafe { cstr_to_str(request_json) }.map_err(|e| format!("request_json: {e}"))?;
        rt.memory_search(raw).map_err(|e| e.to_string())
    })
}

/// Load one Recall record by `id`, counting the visit. Returns owned JSON
/// `{"record": Record}` that the caller must free, UNMODIFIED, with
/// [`octos_string_free`] — or NULL on error (last error "no such record" when
/// the id is unknown).
#[unsafe(no_mangle)]
pub extern "C" fn octos_memory_load(runtime: *mut OctosRuntime, id: *const c_char) -> *mut c_char {
    memory_ffi(runtime, "octos_memory_load", |rt| {
        // SAFETY: `cstr_to_str` NULL-checks and UTF-8-validates.
        let id = unsafe { cstr_to_str(id) }.map_err(|e| format!("id: {e}"))?;
        rt.memory_load(id).map_err(|e| e.to_string())
    })
}

/// Recall index statistics. Returns owned JSON `{"records", "vectors_stored",
/// "vectors_resident", "by_kind", "by_source", "dimension", "embedder_id",
/// "graph_persisted", "disk_bytes"}` that the caller must free, UNMODIFIED,
/// with [`octos_string_free`] — or NULL on error.
#[unsafe(no_mangle)]
pub extern "C" fn octos_memory_stats(runtime: *mut OctosRuntime) -> *mut c_char {
    memory_ffi(runtime, "octos_memory_stats", |rt| {
        rt.memory_stats().map_err(|e| e.to_string())
    })
}

/// Free a string returned by [`octos_run_task`], [`octos_embed`], an
/// `octos_memory_*` or `octos_embedding_model_*` function, or
/// [`octos_take_last_partial_result`]. NULL is a no-op.
///
/// The string is owned by the caller and MUST be freed here, UNMODIFIED — do
/// not alter its bytes or NUL terminator before freeing. (This reclaims via
/// `CString::from_raw`, which rescans for the NUL; a mutated terminator
/// miscomputes the length and corrupts the allocator.) Never call `free(3)` on
/// these; never double-free.
#[unsafe(no_mangle)]
pub extern "C" fn octos_string_free(s: *mut c_char) {
    guard((), "octos_string_free", || {
        if !s.is_null() {
            // SAFETY: `s` came from `CString::into_raw` in this crate and is
            // reclaimed exactly once.
            drop(unsafe { CString::from_raw(s) });
        }
    });
}

/// Return the thread-local last-error string, or NULL if none. Do NOT free it;
/// valid only until the next FFI call on this thread.
#[unsafe(no_mangle)]
pub extern "C" fn octos_last_error() -> *const c_char {
    guard(ptr::null(), "octos_last_error", || {
        LAST_ERROR.with(|slot| match &*slot.borrow() {
            Some(c) => c.as_ptr(),
            None => ptr::null(),
        })
    })
}

/// Take this thread's last incomplete task result as owned JSON with the same
/// shape as [`octos_run_task`]. Returns NULL if absent or already taken. This
/// does not change the error diagnostic and does NOT turn the task into success.
///
/// Consume once on the SAME thread, before another `octos_runtime_new`,
/// `octos_run_task`, `octos_embed`, `octos_memory_*` or
/// `octos_embedding_model_*` call (success or failure clears it).
/// Any new error, including a caught panic, also clears it. Error/version
/// inspection and successful free calls leave it available. The caller must
/// free the returned allocation, UNMODIFIED, with [`octos_string_free`].
/// The payload is lossless model output, not a sanitized/600-byte diagnostic.
#[unsafe(no_mangle)]
pub extern "C" fn octos_take_last_partial_result() -> *mut c_char {
    guard(ptr::null_mut(), "octos_take_last_partial_result", || {
        let partial = LAST_PARTIAL_RESULT.with(|slot| slot.borrow_mut().take());
        let Some(partial) = partial else {
            return ptr::null_mut();
        };
        match to_owned_ptr(task_result_json(&partial)) {
            Ok(pointer) => pointer,
            Err(error) => {
                set_last_error(error);
                ptr::null_mut()
            }
        }
    })
}

/// Return the crate version as a static NUL-terminated string (never freed).
#[unsafe(no_mangle)]
pub extern "C" fn octos_version() -> *const c_char {
    guard(ptr::null(), "octos_version", || {
        const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
        VERSION.as_ptr() as *const c_char
    })
}

#[cfg(test)]
impl OctosRuntime {
    /// Test-only view of the RAII-owned scratch dir path (the field is private
    /// and has no runtime accessor).
    fn scratch_dir_for_test(&self) -> PathBuf {
        self.scratch
            .0
            .clone()
            .expect("scratch dir present while runtime is alive")
    }
}

#[cfg(test)]
mod incomplete_tests;

#[cfg(test)]
mod memory_tests;

#[cfg(test)]
mod model_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp dir for a hermetic auth store (no process-env, no subprocess).
    fn tmp_auth_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "octos-ffi-auth-{}-{}",
            std::process::id(),
            MEM_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cred(token: &str, provider: &str) -> octos_cli::auth::AuthCredential {
        octos_cli::auth::AuthCredential {
            access_token: token.to_string(),
            refresh_token: None,
            expires_at: None,
            provider: provider.to_string(),
            auth_method: "paste_token".to_string(),
        }
    }

    #[test]
    fn owned_string_round_trips_through_free() {
        let ptr = to_owned_ptr("hello ffi".to_string()).expect("no interior NUL");
        assert!(!ptr.is_null());
        // SAFETY: freshly minted, non-null CString pointer.
        let read = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap();
        assert_eq!(read, "hello ffi");
        // Must not double-free / leak.
        octos_string_free(ptr);
    }

    #[test]
    fn last_error_set_get_clear() {
        clear_last_error();
        assert!(octos_last_error().is_null());
        set_last_error("boom");
        let p = octos_last_error();
        assert!(!p.is_null());
        // SAFETY: non-null, points at the thread-local CString.
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "boom");
        clear_last_error();
        assert!(octos_last_error().is_null());
    }

    #[test]
    fn cstr_rejects_null() {
        // SAFETY: passing NULL is explicitly handled.
        let err = unsafe { cstr_to_str(ptr::null()) }.unwrap_err();
        assert!(err.contains("null"));
    }

    #[test]
    fn guard_contains_panic_and_returns_default() {
        clear_last_error();
        // A panicking body must NOT unwind past `guard`: it returns `default`
        // (here a null pointer) and records a generic error. The default panic
        // hook still prints "boom" to stderr — expected; the test PASSING is the
        // proof that no abort/unwind escaped the firewall.
        let out: *mut u8 = guard(ptr::null_mut(), "unit_panic", || panic!("boom"));
        assert!(out.is_null());
        let p = octos_last_error();
        assert!(!p.is_null());
        // SAFETY: non-null thread-local error string.
        let msg = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert!(msg.contains("panic caught in unit_panic"), "got: {msg}");
    }

    #[test]
    fn sanitizer_redacts_secret_shaped_tokens_and_caps_length() {
        let e = sanitize_error_text("auth failed with key sk-abc123DEF456ghijkLMNOP789 trailing");
        assert!(e.contains("<redacted>"), "got: {e}");
        assert!(!e.contains("sk-abc123DEF456ghijkLMNOP789"));
        assert!(e.contains("auth failed"));

        // Length cap keeps a runaway provider body bounded.
        let long = "z".repeat(4000);
        let capped = sanitize_error_text(&long);
        assert!(capped.len() <= MAX_ERROR_LEN + 16, "len {}", capped.len());
    }

    #[test]
    fn scrub_secret_removes_exact_known_key() {
        let key = "topsecretkeyvalue";
        let e = scrub_secret(format!("provider rejected {key} verbatim"), Some(key));
        assert!(e.contains("<redacted>"));
        assert!(!e.contains(key));
    }

    #[test]
    fn scrub_secret_redacts_short_exact_key() {
        // The exact-match scrub must fire regardless of key length — a 3-char
        // key is unusual but must still be removed (the length guard is only for
        // the UNKNOWN-token heuristic, not the exact scrub).
        let e = scrub_secret("401 unauthorized for abc here".to_string(), Some("abc"));
        assert!(e.contains("<redacted>"), "got: {e}");
        assert!(!e.contains("abc"));
    }

    #[test]
    fn canonical_provider_name_matches_factory() {
        // The factory resolves the credential under the canonical registry name;
        // `secret` must resolve under the SAME name. Case + real alias + custom +
        // unknown-passthrough.
        assert_eq!(canonical_provider_name("qwen"), "dashscope"); // alias
        assert_eq!(canonical_provider_name("OpenAI"), "openai"); // case
        assert_eq!(canonical_provider_name("custom"), "custom"); // special-cased
        assert_eq!(
            canonical_provider_name("totally-unknown-xyz"),
            "totally-unknown-xyz"
        ); // passthrough
    }

    #[test]
    fn resolved_secret_uses_canonical_name_so_authstore_key_is_found_and_scrubbed() {
        // Hermetic (temp auth store; no process-env mutation, no subprocess).
        // A credential stored under the CANONICAL name `dashscope` must be found
        // when the caller configures the ALIAS `qwen` — which is exactly what
        // resolving under `canonical_provider_name(cfg.provider)` achieves — and
        // the resolved token must then be exact-scrubbed out of an error string.
        let dir = tmp_auth_dir();
        let token = "canon-token-abc123XYZ";
        {
            let mut store = octos_cli::auth::AuthStore::at(&dir).unwrap();
            store.set("dashscope", cred(token, "dashscope")).unwrap();
        }

        let store = octos_cli::auth::AuthStore::at(&dir).unwrap();
        // The alias spelling misses the canonically-keyed credential...
        assert!(store.get("qwen").is_none());
        // ...but the canonical name the FFI now resolves under hits it.
        let canonical = canonical_provider_name("qwen");
        let found = store
            .get(&canonical)
            .expect("credential under canonical name");
        assert_eq!(found.access_token, token);

        // And the resolved token is scrubbed from error text.
        let scrubbed = scrub_secret(
            format!("401 unauthorized: {token}"),
            Some(&found.access_token),
        );
        assert!(scrubbed.contains("<redacted>"));
        assert!(!scrubbed.contains(token));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn factory_consumes_the_single_resolved_pinned_key() {
        // The single-resolution invariant: a key resolved ONCE (here sourced
        // from a temp auth store, keyed by the canonical `dashscope`) and pinned
        // via `pin_resolved_key` is what the factory's OWN `get_api_key` returns
        // and what it builds the provider with — no second AuthStore/env read.
        let dir = tmp_auth_dir();
        let token = "authstore-only-tok-42abc";
        {
            let mut store = octos_cli::auth::AuthStore::at(&dir).unwrap();
            store.set("dashscope", cred(token, "dashscope")).unwrap();
        }

        // Resolve ONCE (simulating runtime_new_impl's single read).
        let resolved = octos_cli::auth::AuthStore::at(&dir)
            .unwrap()
            .get(&canonical_provider_name("qwen"))
            .map(|c| c.access_token.clone());
        assert_eq!(resolved.as_deref(), Some(token));

        // Pin it exactly as runtime_new_impl does, then the factory consumes it.
        let mut cli_cfg: Config = serde_json::from_str("{}").unwrap();
        pin_resolved_key(&mut cli_cfg, resolved.as_deref());

        // The factory calls get_api_key(canonical); it must return the pinned
        // value deterministically (from env_vars, auth store bypassed).
        let canonical = canonical_provider_name("qwen"); // "dashscope"
        assert_eq!(cli_cfg.get_api_key(&canonical).ok().as_deref(), Some(token));

        // And a provider builds from that config (consumes the pinned key).
        let provider = create_provider_with_api_type(
            "qwen",
            &cli_cfg,
            Some("qwen-max".to_string()),
            None,
            None,
        );
        assert!(
            provider.is_ok(),
            "provider build failed: {:?}",
            provider.err()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_secret_mirrors_the_single_resolved_key() {
        // End-to-end through runtime_new_impl: rt.secret equals the exact key the
        // provider was built with (single resolution → pinned → mirrored).
        let cfg = CString::new(
            r#"{"provider":"openai","model":"gpt-4o-mini","api_key":"pinme-key-123",
                "embedding_auto_download":false}"#,
        )
        .unwrap();
        let raw = runtime_new_impl(cfg.as_ptr()).expect("runtime built");
        // SAFETY: non-null handle just built by `runtime_new_impl`.
        let rt = unsafe { &*raw };
        assert_eq!(rt.secret.as_deref(), Some("pinme-key-123"));
        octos_runtime_free(raw);
    }

    #[test]
    fn native_drop_removes_scratch_dir() {
        // A native `OctosRuntime` (the uniffi path) must clean up its scratch dir
        // when dropped — RAII via the trailing `ScratchDir` field.
        let rt = OctosRuntime::from_config(RuntimeConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            api_key: Some("dummy-key".to_string()),
            embedding_auto_download: Some(false),
            ..RuntimeConfig::default()
        })
        .expect("runtime built");
        let dir = rt.scratch_dir_for_test();
        assert!(
            dir.exists(),
            "scratch dir should exist while alive: {dir:?}"
        );
        drop(rt);
        assert!(
            !dir.exists(),
            "scratch dir must be removed after native drop: {dir:?}"
        );
    }

    #[test]
    fn c_free_removes_scratch_dir() {
        // The C free path must still remove the scratch dir now that the manual
        // `remove_dir_all` is gone (drop order: store lock releases, then
        // `ScratchDir` removes the dir).
        let cfg = CString::new(
            r#"{"provider":"openai","model":"gpt-4o-mini","api_key":"dummy-key",
                "embedding_auto_download":false}"#,
        )
        .unwrap();
        let raw = runtime_new_impl(cfg.as_ptr()).expect("runtime built");
        // SAFETY: non-null handle just built by `runtime_new_impl`.
        let dir = unsafe { &*raw }.scratch_dir_for_test();
        assert!(
            dir.exists(),
            "scratch dir should exist while alive: {dir:?}"
        );
        octos_runtime_free(raw);
        assert!(
            !dir.exists(),
            "scratch dir must be removed after octos_runtime_free: {dir:?}"
        );
    }

    /// Unique, caller-owned data dir (the runtime never removes it; the test
    /// does).
    fn tmp_data_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "octos-ffi-data-{}-{}",
            std::process::id(),
            MEM_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn native_runtime(data_dir: Option<&Path>) -> OctosRuntime {
        OctosRuntime::from_config(RuntimeConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            api_key: Some("dummy-key".to_string()),
            data_dir: data_dir.map(|d| d.to_string_lossy().into_owned()),
            embedding_auto_download: Some(false),
            ..RuntimeConfig::default()
        })
        .expect("runtime built")
    }

    fn upsert_batch() -> serde_json::Value {
        json!([
            {"id": "doc:mail:42", "kind": "document", "source": "mail",
             "timestamp": "2026-09-01T10:00:00Z", "title": "Dentist appointment",
             "abstract": "Sunrise Dental on the 24th", "fingerprint": "h42"},
            {"id": "episode:sess-1:7", "kind": "episode", "source": "episodes",
             "timestamp": "2026-09-02T10:00:00Z", "title": "Fixed the build",
             "abstract": "Bumped rustls and re-ran CI.", "fingerprint": "e7"}
        ])
    }

    #[test]
    fn memory_upsert_resubmitting_unchanged_batch_embeds_nothing() {
        // Re-submitting an identical batch must be a no-op for the embedder:
        // the store reports every record unchanged and `embedded` is 0.
        let rt = native_runtime(None);
        let req = json!({ "records": upsert_batch() }).to_string();

        let first: serde_json::Value =
            serde_json::from_str(&rt.memory_upsert(&req).expect("first upsert")).unwrap();
        assert_eq!(first["inserted"], 2);
        assert_eq!(first["unchanged"], 0);
        assert_eq!(first["embedded"], 0, "no embedder loaded");

        let second: serde_json::Value =
            serde_json::from_str(&rt.memory_upsert(&req).expect("second upsert")).unwrap();
        assert_eq!(second["inserted"], 0);
        assert_eq!(second["updated"], 0);
        assert_eq!(second["unchanged"], 2);
        assert_eq!(second["embedded"], 0);
    }

    #[test]
    fn embed_flagged_only_embeds_records_the_store_flags() {
        // Store the batch WITH vectors, then probe: nothing needs a vector, so
        // the embedder must not be called at all. Change one fingerprint and
        // only that record is embedded, at its own position.
        let rt = native_runtime(None);
        let dim = rt.recall.dimension();
        let vectors: Vec<Vec<f32>> = vec![vec![0.5; dim], vec![0.25; dim]];
        let req = json!({ "records": upsert_batch(), "vectors": vectors }).to_string();
        let report: serde_json::Value =
            serde_json::from_str(&rt.memory_upsert(&req).expect("upsert with vectors")).unwrap();
        assert_eq!(report["vectors_stored"], 2);

        let mut records: Vec<Record> = serde_json::from_value(upsert_batch()).unwrap();
        let needs = rt.recall.needs_vectors(&records).expect("probe");
        assert_eq!(
            needs,
            vec![false, false],
            "unchanged records keep their vectors"
        );
        let (out, embedded) = embed_flagged(&records, &needs, |texts| {
            panic!("embedder called for {} unchanged records", texts.len())
        })
        .expect("nothing to embed");
        assert_eq!(embedded, 0);
        assert_eq!(out, vec![None, None]);

        records[1].fingerprint = "e7-v2".to_string();
        let needs = rt.recall.needs_vectors(&records).expect("probe");
        assert_eq!(needs, vec![false, true]);
        let mut calls = 0usize;
        let (out, embedded) = embed_flagged(&records, &needs, |texts| {
            calls += 1;
            assert_eq!(texts.len(), 1);
            assert!(texts[0].contains("Fixed the build"), "got {:?}", texts[0]);
            Ok(vec![vec![1.0; dim]])
        })
        .expect("one record embedded");
        assert_eq!(calls, 1);
        assert_eq!(embedded, 1);
        assert!(out[0].is_none(), "unchanged record keeps `None`");
        assert_eq!(out[1].as_deref(), Some(&vec![1.0f32; dim][..]));
    }

    #[test]
    fn build_agent_exposes_memory_tools_over_the_runtime_recall_store() {
        // An embedded agent must see the runtime's Recall store: the two-stage
        // retrieval tools are registered next to the FS toolset, with a
        // persistent `data_dir` and with the scratch dir alike.
        let data_dir = tmp_data_dir();
        let rt = native_runtime(Some(&data_dir));
        assert_eq!(rt.mem_dir, data_dir);
        let agent = rt.build_agent(3);
        let registry = agent.tool_registry();
        assert!(
            registry.get("memory_search").is_some(),
            "memory_search missing"
        );
        assert!(registry.get("memory_load").is_some(), "memory_load missing");
        assert!(
            registry.get("read_file").is_some(),
            "FS toolset still present"
        );
        assert!(
            registry.get("shell").is_none(),
            "shell stays opt-in (allow_shell defaults to false)"
        );
        drop(rt);
        assert!(data_dir.exists(), "caller-owned data_dir is never removed");
        let _ = std::fs::remove_dir_all(&data_dir);

        let scratch = native_runtime(None);
        let registry = scratch.build_agent(1);
        assert!(registry.tool_registry().get("memory_search").is_some());
        assert!(registry.tool_registry().get("memory_load").is_some());
    }

    #[test]
    fn pin_none_makes_factory_resolution_none_deterministically() {
        // resolved == None must be pinned too: the factory's get_api_key resolves
        // to None as well, and a key added AFTER the single resolution (simulated
        // by a real provider key already sitting in env_vars) is NOT picked up —
        // no mid-init race where the factory sees a key while rt.secret is None.
        let mut cli_cfg: Config = serde_json::from_str("{}").unwrap();
        cli_cfg
            .env_vars
            .insert("OPENAI_API_KEY".to_string(), "late-added-key".to_string());

        pin_resolved_key(&mut cli_cfg, None);

        // api_key_env now points at the (absent) pinned var and the auth store is
        // bypassed, so the natural OPENAI_API_KEY entry is ignored -> None.
        assert!(cli_cfg.get_api_key("openai").is_err());
    }
}
