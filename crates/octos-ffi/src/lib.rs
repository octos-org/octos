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

/// Upper bound on a stored error string, so a runaway provider error body can't
/// balloon the thread-local buffer.
const MAX_ERROR_LEN: usize = 600;

fn set_last_error(msg: impl Into<String>) {
    // Redact credential-shaped tokens and cap length BEFORE storing: provider
    // error bodies can echo an auth value, and this string is handed back
    // verbatim by `octos_last_error`.
    let sanitized = sanitize_error_text(&msg.into());
    // CString rejects interior NUL; scrub so the message always stores.
    let scrubbed = sanitized.replace('\0', " ");
    let c = CString::new(scrubbed).unwrap_or_else(|_| CString::new("error").expect("no NUL"));
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(c));
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// Replace an exact known secret value with a placeholder. Used at sites where
/// the FFI still holds the caller's plaintext key — the most reliable scrub.
fn scrub_secret(mut s: String, secret: Option<&str>) -> String {
    if let Some(sec) = secret {
        if sec.len() >= 4 && s.contains(sec) {
            s = s.replace(sec, "<redacted>");
        }
    }
    s
}

/// Best-effort redaction of credential-shaped substrings plus a length cap.
/// Applied to every stored error as a backstop for the env-var / auth-store key
/// paths where the plaintext isn't available for an exact scrub.
fn sanitize_error_text(input: &str) -> String {
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
    cwd: PathBuf,
    allow_shell: bool,
    default_max_iterations: u32,
    /// True when the config named an embedding model, used to distinguish
    /// "no embedder configured" from "not compiled with embed-llama".
    embedding_configured: bool,
    #[cfg(feature = "embed-llama")]
    embedder: Option<Arc<octos_embed_llama::LlamaEmbedder>>,
    /// The caller's plaintext key when supplied via `api_key`, retained ONLY to
    /// scrub it out of provider error text before it reaches `octos_last_error`.
    /// (It already lives inside `llm`, so this is not new exposure.)
    secret: Option<String>,
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
    let cfg: FfiConfig =
        serde_json::from_str(raw).map_err(|e| format!("invalid config_json: {e}"))?;

    // Reuse octos-cli's Config + provider factory (key resolution, timeout
    // overrides, api_type bypasses).
    let mut cli_cfg: Config =
        serde_json::from_str("{}").map_err(|e| format!("internal default config: {e}"))?;
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

    let llm = create_provider_with_api_type(
        &cfg.provider,
        &cli_cfg,
        Some(cfg.model.clone()),
        cfg.base_url.clone(),
        cfg.api_type.as_deref(),
    )
    .map_err(|e| {
        scrub_secret(
            format!("failed to build provider '{}': {e}", cfg.provider),
            cfg.api_key.as_deref(),
        )
    })?;

    let tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;

    // Build the embedder BEFORE creating the memory scratch dir so that no
    // fallible step follows dir creation (issue: a later failure would leak the
    // dir). See the cleanup on the memory-open error path below.
    #[cfg(feature = "embed-llama")]
    let embedder = build_embedder(cfg.embedding_model_path.as_deref())?;

    let cwd = match &cfg.cwd {
        Some(c) => PathBuf::from(c),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let default_max_iterations = cfg
        .max_iterations
        .unwrap_or_else(|| AgentConfig::default().max_iterations);
    let embedding_configured = cfg.embedding_model_path.is_some();

    // Minimal episodic memory store in a unique scratch dir. `EpisodeStore::open`
    // creates the dir before it can fail, so remove it on the error path (the
    // store is not constructed there, so nothing holds the redb lock).
    let seq = MEM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mem_dir =
        std::env::temp_dir().join(format!("octos-ffi-mem-{}-{}", std::process::id(), seq));
    let memory = match tokio.block_on(EpisodeStore::open(&mem_dir)) {
        Ok(store) => Arc::new(store),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&mem_dir);
            return Err(format!("failed to open memory store: {e}"));
        }
    };

    let runtime = OctosRuntime {
        tokio,
        llm,
        memory,
        cwd,
        allow_shell: cfg.allow_shell,
        default_max_iterations,
        embedding_configured,
        #[cfg(feature = "embed-llama")]
        embedder,
        secret: cfg.api_key.clone(),
        mem_dir,
    };
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
        let mem_dir = boxed.mem_dir.clone();
        // NOTE: dropping the held tokio runtime here PANICS if the host calls
        // this from inside its own async/tokio context ("Cannot drop a runtime
        // in a context where blocking is not allowed"). That panic is contained
        // by `guard` (no UB), but the drop is then incomplete and the scratch
        // dir may leak — hosts must free from a plain, non-async thread.
        //
        // Drop first so the redb store releases its file lock before cleanup.
        drop(boxed);
        let _ = std::fs::remove_dir_all(&mem_dir);
    });
}

/// Run a one-shot task. `brief_json` is `{"prompt": "...", "max_iterations"?:
/// N}`. Returns owned JSON `{"output", "iterations", "tokens"}` that the caller
/// must free, UNMODIFIED, with [`octos_string_free`] — or NULL on error.
#[unsafe(no_mangle)]
pub extern "C" fn octos_run_task(
    runtime: *mut OctosRuntime,
    brief_json: *const c_char,
) -> *mut c_char {
    guard(ptr::null_mut(), "octos_run_task", || {
        clear_last_error();
        match run_task_impl(runtime, brief_json) {
            Ok(p) => p,
            Err(msg) => {
                set_last_error(msg);
                ptr::null_mut()
            }
        }
    })
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
        .map_err(|e| scrub_secret(format!("agent run failed: {e}"), rt.secret.as_deref()))?;

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

/// Embed `text`. Returns owned JSON `{"embedding": [f32, ...]}` that the caller
/// must free, UNMODIFIED, with [`octos_string_free`] — or NULL on error.
/// Requires the `embed-llama` feature and an `embedding_model_path` in the
/// config.
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
        .map_err(|e| scrub_secret(format!("embed failed: {e}"), rt.secret.as_deref()))?;
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
/// no-op.
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

/// Return the crate version as a static NUL-terminated string (never freed).
#[unsafe(no_mangle)]
pub extern "C" fn octos_version() -> *const c_char {
    guard(ptr::null(), "octos_version", || {
        const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
        VERSION.as_ptr() as *const c_char
    })
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
}
