//! octos-ffi: a C-ABI surface for embedding octos in non-Rust hosts.
//!
//! This crate builds a `cdylib`/`staticlib` so Python (ctypes/cffi), Node
//! (ffi-napi/koffi), Go (cgo), or plain C can drive an octos [`Agent`] without
//! linking Rust. It wraps the same provider-construction and agent loop the
//! `octos` CLI uses (see `octos-cli/src/commands/chat.rs`), trimmed to a
//! one-shot task runner plus an optional embedder.
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

use libc::c_char;
use octos_agent::{
    Agent, AgentConfig, GlobTool, GrepTool, ListDirTool, ReadFileTool, ShellTool, ToolRegistry,
    WriteFileTool,
};
use octos_cli::commands::chat::create_provider_with_api_type;
use octos_cli::config::Config;
use octos_core::{AgentId, MessageRole};
#[cfg(feature = "embed-llama")]
use octos_llm::EmbeddingProvider;
use octos_llm::LlmProvider;
use octos_memory::EpisodeStore;
use serde::Deserialize;
use serde_json::json;

/// Monotonic counter used to give every runtime a unique on-disk scratch dir
/// for its (minimal) episodic memory store.
static MEM_COUNTER: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Most recent failure on this thread. Read (never freed) by
    /// [`octos_last_error`]; overwritten by the next fallible FFI call.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(msg: impl Into<String>) {
    let msg = msg.into();
    // CString rejects interior NUL; scrub so the message always stores.
    let sanitized = msg.replace('\0', " ");
    let c = CString::new(sanitized).unwrap_or_else(|_| CString::new("error").expect("no NUL"));
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(c));
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// The FFI-facing config accepted by [`octos_runtime_new`] as JSON.
#[derive(Debug, Deserialize)]
struct FfiConfig {
    provider: String,
    model: String,
    /// Raw API key. Injected into the reused octos `Config` key resolution.
    #[serde(default)]
    api_key: Option<String>,
    /// Name of a process env var holding the API key (alternative to `api_key`).
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    /// API protocol override: `"anthropic"` / `"responses"` (see the reused
    /// `create_provider_with_api_type`). Usually omitted.
    #[serde(default)]
    api_type: Option<String>,
    /// Working directory the FS tools are confined to. Defaults to the process
    /// cwd.
    #[serde(default)]
    cwd: Option<String>,
    /// Register the `shell` tool. Off by default — an embedded library should
    /// not run shell unless the host asks.
    #[serde(default)]
    allow_shell: bool,
    #[serde(default)]
    max_iterations: Option<u32>,
    /// Path to a GGUF embedding model. Enables [`octos_embed`] (requires the
    /// `embed-llama` build feature).
    #[serde(default)]
    embedding_model_path: Option<String>,
}

/// The per-task brief accepted by [`octos_run_task`] as JSON.
#[derive(Debug, Deserialize)]
struct Brief {
    prompt: String,
    #[serde(default)]
    max_iterations: Option<u32>,
}

/// Opaque runtime handle. Holds the shared pieces (tokio runtime, provider,
/// memory) and rebuilds a fresh [`Agent`] per task so a per-task
/// `max_iterations` can be honored (the `Agent` config is otherwise fixed at
/// construction).
pub struct OctosRuntime {
    tokio: tokio::runtime::Runtime,
    llm: Arc<dyn LlmProvider>,
    memory: Arc<EpisodeStore>,
    cwd: PathBuf,
    allow_shell: bool,
    default_max_iterations: u32,
    /// True when the config named an embedding model, used to distinguish
    /// "no embedder configured" from "not compiled with embed-llama".
    embedding_configured: bool,
    #[cfg(feature = "embed-llama")]
    embedder: Option<Arc<octos_embed_llama::LlamaEmbedder>>,
    /// Scratch dir backing the episodic memory store; removed on free.
    mem_dir: PathBuf,
}

impl OctosRuntime {
    /// Build a fresh agent with a cwd-scoped FS toolset.
    fn build_agent(&self, max_iterations: u32) -> Agent {
        let tools = build_tools(&self.cwd, self.allow_shell);
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
        .with_config(config)
    }
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

/// Collapse a `catch_unwind` result into a pointer, recording errors/panics.
fn finish_ptr<T>(result: std::thread::Result<Result<*mut T, String>>, ctx: &str) -> *mut T {
    match result {
        Ok(Ok(p)) => p,
        Ok(Err(msg)) => {
            set_last_error(msg);
            ptr::null_mut()
        }
        Err(_) => {
            set_last_error(format!("panic caught in {ctx}"));
            ptr::null_mut()
        }
    }
}

#[cfg(feature = "embed-llama")]
fn build_embedder(
    path: Option<&str>,
) -> Result<Option<Arc<octos_embed_llama::LlamaEmbedder>>, String> {
    match path {
        Some(p) => {
            // n_gpu_layers = 0 -> CPU; hosts wanting Metal/CUDA build the
            // corresponding feature which changes the linked backend.
            let embedder = octos_embed_llama::LlamaEmbedder::from_model_file(p, 0)
                .map_err(|e| format!("failed to load embedding model '{p}': {e}"))?;
            Ok(Some(Arc::new(embedder)))
        }
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// C-ABI surface
// ---------------------------------------------------------------------------

/// Create a runtime from a JSON config. Returns NULL on error (see
/// [`octos_last_error`]); the returned handle must be freed with
/// [`octos_runtime_free`].
#[unsafe(no_mangle)]
pub extern "C" fn octos_runtime_new(config_json: *const c_char) -> *mut OctosRuntime {
    clear_last_error();
    let result = catch_unwind(AssertUnwindSafe(|| runtime_new_impl(config_json)));
    finish_ptr(result, "octos_runtime_new")
}

fn runtime_new_impl(config_json: *const c_char) -> Result<*mut OctosRuntime, String> {
    // SAFETY: `cstr_to_str` NULL-checks and UTF-8-validates.
    let raw = unsafe { cstr_to_str(config_json) }.map_err(|e| format!("config_json: {e}"))?;
    let cfg: FfiConfig =
        serde_json::from_str(raw).map_err(|e| format!("invalid config_json: {e}"))?;

    // Reuse octos-cli's Config + provider factory (auth store -> env_vars ->
    // process env key resolution, timeout overrides, api_type bypasses).
    let mut cli_cfg: Config =
        serde_json::from_str("{}").map_err(|e| format!("internal default config: {e}"))?;
    let key_env = cfg
        .api_key_env
        .clone()
        .unwrap_or_else(|| format!("{}_API_KEY", cfg.provider.to_uppercase()));
    if let Some(key) = &cfg.api_key {
        // Inject the raw key so Config::get_api_key resolves it from env_vars.
        cli_cfg.api_key_env = Some(key_env.clone());
        cli_cfg.env_vars.insert(key_env, key.clone());
    } else if cfg.api_key_env.is_some() {
        // Host set the named process env var; point resolution at it.
        cli_cfg.api_key_env = cfg.api_key_env.clone();
    }

    let llm = create_provider_with_api_type(
        &cfg.provider,
        &cli_cfg,
        Some(cfg.model.clone()),
        cfg.base_url.clone(),
        cfg.api_type.as_deref(),
    )
    .map_err(|e| format!("failed to build provider '{}': {e}", cfg.provider))?;

    let tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;

    // Minimal episodic memory store in a unique scratch dir.
    let seq = MEM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mem_dir =
        std::env::temp_dir().join(format!("octos-ffi-mem-{}-{}", std::process::id(), seq));
    let memory = tokio
        .block_on(EpisodeStore::open(&mem_dir))
        .map_err(|e| format!("failed to open memory store: {e}"))?;
    let memory = Arc::new(memory);

    let cwd = match &cfg.cwd {
        Some(c) => PathBuf::from(c),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let default_max_iterations = cfg
        .max_iterations
        .unwrap_or_else(|| AgentConfig::default().max_iterations);
    let embedding_configured = cfg.embedding_model_path.is_some();

    let runtime = OctosRuntime {
        tokio,
        llm,
        memory,
        cwd,
        allow_shell: cfg.allow_shell,
        default_max_iterations,
        embedding_configured,
        #[cfg(feature = "embed-llama")]
        embedder: build_embedder(cfg.embedding_model_path.as_deref())?,
        mem_dir,
    };
    Ok(Box::into_raw(Box::new(runtime)))
}

/// Free a runtime created by [`octos_runtime_new`]. NULL is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn octos_runtime_free(runtime: *mut OctosRuntime) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if runtime.is_null() {
            return;
        }
        // SAFETY: `runtime` came from `Box::into_raw` in `octos_runtime_new`
        // and is reconstructed exactly once (caller must not double-free).
        let boxed = unsafe { Box::from_raw(runtime) };
        let mem_dir = boxed.mem_dir.clone();
        // Drop first so the redb store releases its file lock before cleanup.
        drop(boxed);
        let _ = std::fs::remove_dir_all(&mem_dir);
    }));
}

/// Run a one-shot task. `brief_json` is `{"prompt": "...", "max_iterations"?:
/// N}`. Returns owned JSON `{"output", "iterations", "tokens"}` (free with
/// [`octos_string_free`]) or NULL on error.
#[unsafe(no_mangle)]
pub extern "C" fn octos_run_task(
    runtime: *mut OctosRuntime,
    brief_json: *const c_char,
) -> *mut c_char {
    clear_last_error();
    let result = catch_unwind(AssertUnwindSafe(|| run_task_impl(runtime, brief_json)));
    finish_ptr(result, "octos_run_task")
}

fn run_task_impl(
    runtime: *mut OctosRuntime,
    brief_json: *const c_char,
) -> Result<*mut c_char, String> {
    // SAFETY: NULL-checked; must be a live handle from `octos_runtime_new`.
    let rt = unsafe { runtime.as_ref() }.ok_or_else(|| "runtime pointer is null".to_string())?;
    // SAFETY: `cstr_to_str` NULL-checks and UTF-8-validates.
    let raw = unsafe { cstr_to_str(brief_json) }?;
    let brief: Brief = serde_json::from_str(raw).map_err(|e| format!("invalid brief_json: {e}"))?;
    if brief.prompt.trim().is_empty() {
        return Err("brief_json.prompt is empty".to_string());
    }
    let max_iter = brief.max_iterations.unwrap_or(rt.default_max_iterations);

    let agent = rt.build_agent(max_iter);
    let response = rt
        .tokio
        .block_on(agent.process_message(&brief.prompt, &[], Vec::new()))
        .map_err(|e| format!("agent run failed: {e}"))?;

    // `ConversationResponse` carries no explicit loop-iteration count, so
    // derive one: each LLM round contributes exactly one assistant message.
    let iterations = response
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .count();
    let out = json!({
        "output": response.content,
        "iterations": iterations,
        "tokens": {
            "input": response.token_usage.input_tokens,
            "output": response.token_usage.output_tokens,
            "reasoning": response.token_usage.reasoning_tokens,
            "cache_read": response.token_usage.cache_read_tokens,
            "cache_write": response.token_usage.cache_write_tokens,
        }
    });
    let serialized = serde_json::to_string(&out).map_err(|e| format!("serialize output: {e}"))?;
    to_owned_ptr(serialized)
}

/// Embed `text`. Returns owned JSON `{"embedding": [f32, ...]}` (free with
/// [`octos_string_free`]) or NULL on error. Requires the `embed-llama` feature
/// and an `embedding_model_path` in the config.
#[unsafe(no_mangle)]
pub extern "C" fn octos_embed(runtime: *mut OctosRuntime, text: *const c_char) -> *mut c_char {
    clear_last_error();
    let result = catch_unwind(AssertUnwindSafe(|| embed_entry(runtime, text)));
    finish_ptr(result, "octos_embed")
}

fn embed_entry(runtime: *mut OctosRuntime, text: *const c_char) -> Result<*mut c_char, String> {
    // SAFETY: NULL-checked; must be a live handle from `octos_runtime_new`.
    let rt = unsafe { runtime.as_ref() }.ok_or_else(|| "runtime pointer is null".to_string())?;
    // SAFETY: `cstr_to_str` NULL-checks and UTF-8-validates.
    let text = unsafe { cstr_to_str(text) }?;
    if !rt.embedding_configured {
        return Err("no embedder configured".to_string());
    }
    embed_impl(rt, text)
}

#[cfg(feature = "embed-llama")]
fn embed_impl(rt: &OctosRuntime, text: &str) -> Result<*mut c_char, String> {
    let embedder = rt
        .embedder
        .as_ref()
        .ok_or_else(|| "no embedder configured".to_string())?;
    let mut vectors = rt
        .tokio
        .block_on(embedder.embed(&[text]))
        .map_err(|e| format!("embed failed: {e}"))?;
    let first = if vectors.is_empty() {
        return Err("embedder returned no vectors".to_string());
    } else {
        vectors.swap_remove(0)
    };
    let serialized = serde_json::to_string(&json!({ "embedding": first }))
        .map_err(|e| format!("serialize embedding: {e}"))?;
    to_owned_ptr(serialized)
}

#[cfg(not(feature = "embed-llama"))]
fn embed_impl(_rt: &OctosRuntime, _text: &str) -> Result<*mut c_char, String> {
    Err(
        "embedding support not compiled in (rebuild octos-ffi with --features embed-llama)"
            .to_string(),
    )
}

/// Free a string returned by [`octos_run_task`] / [`octos_embed`]. NULL is a
/// no-op. Never call `free(3)` on these; never double-free.
#[unsafe(no_mangle)]
pub extern "C" fn octos_string_free(s: *mut c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !s.is_null() {
            // SAFETY: `s` came from `CString::into_raw` in this crate and is
            // reclaimed exactly once.
            drop(unsafe { CString::from_raw(s) });
        }
    }));
}

/// Return the thread-local last-error string, or NULL if none. Do NOT free it;
/// valid only until the next FFI call on this thread.
#[unsafe(no_mangle)]
pub extern "C" fn octos_last_error() -> *const c_char {
    catch_unwind(|| {
        LAST_ERROR.with(|slot| match &*slot.borrow() {
            Some(c) => c.as_ptr(),
            None => ptr::null(),
        })
    })
    .unwrap_or(ptr::null())
}

/// Return the crate version as a static NUL-terminated string (never freed).
#[unsafe(no_mangle)]
pub extern "C" fn octos_version() -> *const c_char {
    catch_unwind(|| {
        const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
        VERSION.as_ptr() as *const c_char
    })
    .unwrap_or(ptr::null())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
