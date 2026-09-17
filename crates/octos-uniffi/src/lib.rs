//! octos-uniffi: idiomatic Python / Swift / Kotlin bindings for embedding octos,
//! generated from ONE Rust definition by [uniffi](https://mozilla.github.io/uniffi-rs/).
//!
//! This crate is a thin, idiomatic wrapper over the **native core** exposed by
//! `octos-ffi` ([`octos_ffi::OctosRuntime`]). It adds NO logic of its own beyond
//! type marshalling — in particular, the hardened credential path (single
//! resolution + pinning + secret-scrubbing of error text) lives entirely in
//! `octos-ffi::OctosRuntime::from_config`, so it exists in exactly one place and
//! is shared by both the C-ABI and these uniffi bindings.
//!
//! The foreign surface:
//! * [`Config`] / [`Brief`] — inputs (uniffi records → dictionaries/data classes).
//! * [`TaskResult`] / [`TokenUsage`] — outputs.
//! * [`OctosError`] — a structured error enum.
//! * [`Runtime`] — an opaque object (`Arc`-shared) with `new`, `run_task`,
//!   `embed`, and the Recall memory seam `memory_upsert` / `memory_search` /
//!   `memory_load` / `memory_stats` (JSON strings in and out, same contracts as
//!   the C-ABI's `octos_memory_*` — see the `octos-ffi` README).
//!
//! Methods are synchronous: the async agent loop is driven by a `block_on`
//! inside the core, so the foreign caller sees plain blocking calls (call them
//! from a normal, non-async thread — the same contract as the C-ABI).
//!
//! ## Bindings
//!
//! Generate them from the built library (see `src/bin/uniffi-bindgen.rs`); the
//! committed Python lives in `bindings/python/`. Swift and Kotlin generate the
//! same way from the same library.
//!
//! ## Note on scratch cleanup
//!
//! Without a `data_dir`, the core keeps its episodic + Recall memory stores in
//! a tiny scratch dir under the OS temp dir. It is owned by an RAII guard in
//! the shared core (`octos-ffi`) that removes it when the last [`Runtime`] is
//! dropped — the guard is the final struct field, so it runs AFTER the stores
//! release their redb locks. Both facades share this: the C-ABI's
//! `octos_runtime_free` and a native/uniffi drop reclaim the dir identically,
//! so a long-lived Python/Swift/Kotlin host does not accumulate scratch dirs.
//! With a `data_dir` the stores are persistent and caller-owned.

use std::sync::Arc;

uniffi::setup_scaffolding!("octos");

/// Runtime configuration. Maps directly onto [`octos_ffi::RuntimeConfig`].
///
/// Supply EITHER `api_key` (a literal key) OR `api_key_env` (the name of a
/// process env var holding it). If neither is set, resolution falls back to the
/// conventional `{PROVIDER}_API_KEY` env var and then the `octos auth login`
/// store — see the credential notes on [`octos_ffi::OctosRuntime::from_config`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct Config {
    pub provider: String,
    pub model: String,
    #[uniffi(default = None)]
    pub api_key: Option<String>,
    #[uniffi(default = None)]
    pub api_key_env: Option<String>,
    #[uniffi(default = None)]
    pub base_url: Option<String>,
    /// API protocol override: `"anthropic"` / `"responses"`. Required to drive a
    /// `provider:"custom"` Anthropic-compatible endpoint (without it the factory
    /// defaults to the OpenAI protocol). Usually omitted.
    #[uniffi(default = None)]
    pub api_type: Option<String>,
    #[uniffi(default = None)]
    pub cwd: Option<String>,
    #[uniffi(default = false)]
    pub allow_shell: bool,
    #[uniffi(default = None)]
    pub max_iterations: Option<u32>,
    #[uniffi(default = None)]
    pub embedding_model_path: Option<String>,
    /// Persistent data directory for the episode + Recall memory stores. When
    /// unset they live in a scratch dir removed when the runtime is dropped.
    #[uniffi(default = None)]
    pub data_dir: Option<String>,
    /// Recall vector width (default 256; clamped to the embedder's dimension).
    #[uniffi(default = None)]
    pub recall_dimension: Option<u32>,
    /// Whether an `embed-llama` build may download the default embedding
    /// model (EmbeddingGemma-300M, 334 MB, once, into `<data_dir>/models/`)
    /// when `embedding_model_path` is unset and the file is not on disk.
    /// Default `true` (`OCTOS_NO_MODEL_DOWNLOAD=1` in the environment forces
    /// `false`). The download blocks [`Runtime::new`]; hosts that want to
    /// control it call [`embedding_model_ensure`] first. With `false` and no
    /// model the runtime is keyword-only (`embed` raises `NoEmbedder`).
    #[uniffi(default = None)]
    pub embedding_auto_download: Option<bool>,
}

impl From<Config> for octos_ffi::RuntimeConfig {
    fn from(c: Config) -> Self {
        octos_ffi::RuntimeConfig {
            provider: c.provider,
            model: c.model,
            api_key: c.api_key,
            api_key_env: c.api_key_env,
            base_url: c.base_url,
            api_type: c.api_type,
            cwd: c.cwd,
            allow_shell: c.allow_shell,
            max_iterations: c.max_iterations,
            embedding_model_path: c.embedding_model_path,
            data_dir: c.data_dir,
            recall_dimension: c.recall_dimension.map(|d| d as usize),
            embedding_auto_download: c.embedding_auto_download,
        }
    }
}

/// What is on disk for the default embedding model under `data_dir` (the same
/// directory a [`Config::data_dir`] names), as JSON `{"path", "present",
/// "bytes", "complete", "url", "license_url", "sha256"}` — exactly the C-ABI's
/// `octos_embedding_model_status`. Needs no [`Runtime`] and never touches the
/// network; `license_url` points at the Gemma Terms of Use that apply to the
/// weights.
#[uniffi::export]
pub fn embedding_model_status(data_dir: String) -> Result<String, OctosError> {
    Ok(octos_ffi::embedding_model_status(std::path::Path::new(
        &data_dir,
    ))?)
}

/// Make sure the default embedding model is complete under `data_dir`,
/// downloading and verifying it (334 MB, once) when `download` is true, and
/// return JSON `{"path"}` — exactly the C-ABI's `octos_embedding_model_ensure`.
/// Blocks for the whole transfer, so call it from a plain thread before
/// [`Runtime::new`] when the host wants to own the timing. Raises
/// [`OctosError::Embed`] when the file is absent and `download` is false (or
/// `OCTOS_NO_MODEL_DOWNLOAD` is set), or the download fails to verify.
#[uniffi::export]
pub fn embedding_model_ensure(data_dir: String, download: bool) -> Result<String, OctosError> {
    Ok(octos_ffi::embedding_model_ensure(
        std::path::Path::new(&data_dir),
        download,
    )?)
}

/// A one-shot task brief. Maps onto [`octos_ffi::TaskBrief`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct Brief {
    pub prompt: String,
    #[uniffi(default = None)]
    pub max_iterations: Option<u32>,
}

impl From<Brief> for octos_ffi::TaskBrief {
    fn from(b: Brief) -> Self {
        octos_ffi::TaskBrief {
            prompt: b.prompt,
            max_iterations: b.max_iterations,
        }
    }
}

/// Token accounting for a completed [`Runtime::run_task`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl From<octos_ffi::TokenUsage> for TokenUsage {
    fn from(t: octos_ffi::TokenUsage) -> Self {
        TokenUsage {
            input: t.input,
            output: t.output,
            reasoning: t.reasoning,
            cache_read: t.cache_read,
            cache_write: t.cache_write,
        }
    }
}

/// The result of a completed [`Runtime::run_task`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct TaskResult {
    pub output: String,
    pub iterations: u32,
    pub tokens: TokenUsage,
}

impl From<octos_ffi::TaskResult> for TaskResult {
    fn from(r: octos_ffi::TaskResult) -> Self {
        TaskResult {
            output: r.output,
            iterations: r.iterations,
            tokens: r.tokens.into(),
        }
    }
}

/// Structured error surfaced to the foreign side. Each fallible message string
/// is ALREADY credential-scrubbed by the core before it reaches here.
#[derive(Debug, uniffi::Error)]
pub enum OctosError {
    /// Configuration / runtime-construction failure.
    Config { msg: String },
    /// Provider construction failure.
    Provider { msg: String },
    /// Task-execution failure.
    Run { msg: String },
    /// Embedding failure.
    Embed { msg: String },
    /// No embedder is available (no model path configured, or built without the
    /// `embed-llama` feature).
    NoEmbedder,
    /// Provider output was truncated. This remains a failure; partial output
    /// and consumed usage are available separately from the short diagnostic.
    Incomplete { partial: TaskResult },
    /// Recall-memory failure (`memory_*`): malformed request, rejected record,
    /// "no such record", or a store error. Appended after `Incomplete` to keep
    /// existing variant ordinals stable.
    Memory { msg: String },
}

impl std::fmt::Display for OctosError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OctosError::Config { msg }
            | OctosError::Provider { msg }
            | OctosError::Run { msg }
            | OctosError::Embed { msg }
            | OctosError::Memory { msg } => f.write_str(msg),
            OctosError::NoEmbedder => f.write_str("no embedder configured"),
            OctosError::Incomplete { .. } => f.write_str(octos_ffi::INCOMPLETE_RESPONSE_MESSAGE),
        }
    }
}

impl std::error::Error for OctosError {}

impl From<octos_ffi::CoreError> for OctosError {
    fn from(e: octos_ffi::CoreError) -> Self {
        use octos_ffi::CoreError;
        // The caller's OWN key is already exact-scrubbed inside the core. Here we
        // additionally apply octos-ffi's heuristic redactor + length cap — the
        // SAME backstop the C-ABI runs in `set_last_error` — so a secret embedded
        // in a *provider* error body does not reach uniffi callers verbatim.
        // Applied ONLY at this facade boundary (never in the core), so the C
        // path's byte-for-byte `octos_last_error` output is unaffected.
        let redact = octos_ffi::sanitize_error_text;
        match e {
            CoreError::Config(msg) => OctosError::Config { msg: redact(&msg) },
            CoreError::Provider(msg) => OctosError::Provider { msg: redact(&msg) },
            CoreError::Run(msg) => OctosError::Run { msg: redact(&msg) },
            CoreError::Embed(msg) => OctosError::Embed { msg: redact(&msg) },
            CoreError::NoEmbedder => OctosError::NoEmbedder,
            CoreError::Incomplete { partial } => OctosError::Incomplete {
                partial: partial.into(),
            },
            CoreError::Memory(msg) => OctosError::Memory { msg: redact(&msg) },
        }
    }
}

/// An embedded octos runtime — the idiomatic counterpart of the C-ABI's opaque
/// `OctosRuntime*`. Shared as `Arc<Runtime>`; construct with [`Runtime::new`].
///
/// Unlike the raw C handle (which the caller must manually free and never share
/// across threads), this object is reference-counted and `Send + Sync`, so the
/// foreign side may hold and call it from multiple threads. `run_task` and
/// `embed` each build/drive their own work against shared, internally-synchronized
/// state, so concurrent calls are memory-safe (they will, however, contend on
/// the single internal executor).
#[derive(uniffi::Object)]
pub struct Runtime {
    inner: octos_ffi::OctosRuntime,
}

#[uniffi::export]
impl Runtime {
    /// Build a runtime from a [`Config`]. Resolves and pins the credential
    /// exactly once inside the core (see [`octos_ffi::OctosRuntime::from_config`]).
    #[uniffi::constructor]
    pub fn new(config: Config) -> Result<Arc<Self>, OctosError> {
        let inner = octos_ffi::OctosRuntime::from_config(config.into())?;
        Ok(Arc::new(Runtime { inner }))
    }

    /// Run a one-shot task and return its output + token usage.
    pub fn run_task(&self, brief: Brief) -> Result<TaskResult, OctosError> {
        let native_brief: octos_ffi::TaskBrief = brief.into();
        let result = self.inner.run_task(&native_brief)?;
        Ok(result.into())
    }

    /// Embed `text`, returning the raw vector. Requires the `embed-llama`
    /// feature and an `embedding_model_path` in the [`Config`]; otherwise
    /// [`OctosError::NoEmbedder`].
    pub fn embed(&self, text: String) -> Result<Vec<f32>, OctosError> {
        Ok(self.inner.embed(&text)?)
    }

    /// Push app records into the Recall memory index. `json` is
    /// `{"records": [Record…], "vectors"?: [[f32…]|null…], "embed"?: bool}`;
    /// returns `{"inserted", "updated", "unchanged", "vectors_stored",
    /// "embedded"}`. At most 500 records per call; `kind: "knowledge"` is
    /// rejected; `trust` is forced to untrusted. See
    /// [`octos_ffi::OctosRuntime::memory_upsert`].
    pub fn memory_upsert(&self, json: String) -> Result<String, OctosError> {
        Ok(self.inner.memory_upsert(&json)?)
    }

    /// Search the Recall index. `json` is `{"query", "kinds"?, "sources"?,
    /// "since"?, "until"?, "limit"?}`; returns `{"hits": [Hit…]}`. See
    /// [`octos_ffi::OctosRuntime::memory_search`].
    pub fn memory_search(&self, json: String) -> Result<String, OctosError> {
        Ok(self.inner.memory_search(&json)?)
    }

    /// Load one Recall record by id (counting the visit). Returns
    /// `{"record": Record}`; [`OctosError::Memory`] "no such record" when the
    /// id is unknown.
    pub fn memory_load(&self, id: String) -> Result<String, OctosError> {
        Ok(self.inner.memory_load(&id)?)
    }

    /// Recall index statistics as JSON (`RecallStats`).
    pub fn memory_stats(&self) -> Result<String, OctosError> {
        Ok(self.inner.memory_stats()?)
    }
}

// Compile-time proof that the uniffi Object is `Send + Sync` — required for a
// handle shared as `Arc<Runtime>` across foreign threads. It holds because
// `octos_ffi::OctosRuntime` is `Send + Sync` (a tokio runtime, `Arc<dyn
// LlmProvider>` whose trait is `Send + Sync`, `Arc<EpisodeStore>`, the
// `RwLock`-guarded `Arc<RecallStore>`, and plain data).
// `#[derive(uniffi::Object)]` also requires this; the assertion just
// gives a direct, readable error if the core ever regresses.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Runtime>();
};

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> Config {
        Config {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            api_key: Some("sk-uniffi-test-dummy".to_string()),
            api_key_env: None,
            base_url: None,
            api_type: None,
            cwd: Some(".".to_string()),
            allow_shell: false,
            max_iterations: Some(3),
            embedding_model_path: None,
            data_dir: None,
            recall_dimension: None,
            // Never fetch the default model in tests.
            embedding_auto_download: Some(false),
        }
    }

    #[test]
    fn config_maps_to_runtime_config() {
        let cfg = sample_config();
        let native: octos_ffi::RuntimeConfig = cfg.into();
        assert_eq!(native.provider, "openai");
        assert_eq!(native.model, "gpt-4o-mini");
        assert_eq!(native.api_key.as_deref(), Some("sk-uniffi-test-dummy"));
        assert_eq!(native.cwd.as_deref(), Some("."));
        assert!(!native.allow_shell);
        assert_eq!(native.max_iterations, Some(3));
        assert_eq!(native.embedding_model_path, None);
        // Unset api_type maps through as None.
        assert_eq!(native.api_type, None);
        assert_eq!(native.data_dir, None);
        assert_eq!(native.recall_dimension, None);
        assert_eq!(native.embedding_auto_download, Some(false));
    }

    #[test]
    fn embedding_model_status_reports_absent_model_for_empty_dir() {
        let dir = tempfile_dir("status");
        let status: serde_json::Value = serde_json::from_str(
            &embedding_model_status(dir.to_string_lossy().into_owned()).expect("status ok"),
        )
        .unwrap();
        assert_eq!(status["present"], false);
        assert_eq!(status["complete"], false);
        assert_eq!(status["bytes"], 0);
        assert!(status["path"].as_str().unwrap().ends_with(".gguf"));
        assert!(status["url"].as_str().unwrap().starts_with("https://"));
        assert!(
            status["license_url"]
                .as_str()
                .unwrap()
                .starts_with("https://")
        );
        assert_eq!(status["sha256"].as_str().unwrap().len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn embedding_model_ensure_without_download_raises_embed_error() {
        let dir = tempfile_dir("ensure");
        match embedding_model_ensure(dir.to_string_lossy().into_owned(), false) {
            Err(OctosError::Embed { msg }) => {
                assert!(msg.contains("automatic download is disabled"), "got: {msg}");
            }
            other => panic!("expected Embed error, got {other:?}"),
        }
        assert!(!dir.join("models").exists(), "nothing downloaded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_without_model_and_download_opted_out_is_keyword_only() {
        let dir = tempfile_dir("runtime");
        let rt = Runtime::new(Config {
            data_dir: Some(dir.to_string_lossy().into_owned()),
            embedding_auto_download: Some(false),
            ..sample_config()
        })
        .unwrap_or_else(|e| panic!("build failed: {e}"));
        assert!(!rt.inner.embedding_configured);
        assert!(matches!(
            rt.embed("hello".to_string()),
            Err(OctosError::NoEmbedder)
        ));
        rt.memory_upsert(
            r#"{"records":[{"id":"doc:mail:1","kind":"document","source":"mail",
                "timestamp":"2026-09-01T10:00:00Z","title":"Dentist appointment",
                "abstract":"Sunrise Dental on the 24th"}]}"#
                .to_string(),
        )
        .expect("upsert ok");
        let hits: serde_json::Value = serde_json::from_str(
            &rt.memory_search(r#"{"query":"dentist"}"#.to_string())
                .expect("keyword-only search works"),
        )
        .unwrap();
        assert_eq!(hits["hits"][0]["id"], "doc:mail:1");
        let stats: serde_json::Value = serde_json::from_str(&rt.memory_stats().unwrap()).unwrap();
        assert_eq!(stats["embedder_id"], "");
        assert!(!dir.join("models").exists(), "no download attempted");
        drop(rt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unique caller-owned dir under the OS temp dir (removed by the test).
    fn tempfile_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "octos-uniffi-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_maps_memory_fields_through() {
        let cfg = Config {
            data_dir: Some("/tmp/octos-data".to_string()),
            recall_dimension: Some(128),
            ..sample_config()
        };
        let native: octos_ffi::RuntimeConfig = cfg.into();
        assert_eq!(native.data_dir.as_deref(), Some("/tmp/octos-data"));
        assert_eq!(native.recall_dimension, Some(128));
    }

    #[test]
    fn memory_error_maps_and_redacts() {
        use octos_ffi::CoreError;
        let err: OctosError = CoreError::Memory("no such record".into()).into();
        assert!(matches!(&err, OctosError::Memory { msg } if msg == "no such record"));
        assert_eq!(err.to_string(), "no such record");
        let leaked = "sk-abc123DEF456ghijkLMNOP789";
        let err: OctosError = CoreError::Memory(format!("store said {leaked}")).into();
        let OctosError::Memory { msg } = err else {
            panic!("expected Memory");
        };
        assert!(
            msg.contains("<redacted>") && !msg.contains(leaked),
            "got: {msg}"
        );
    }

    #[test]
    fn memory_round_trip_bm25_only_through_uniffi_surface() {
        // No embedder: the Recall index is BM25-only and still fully usable.
        let rt = Runtime::new(sample_config()).unwrap_or_else(|e| panic!("build failed: {e}"));
        let report: serde_json::Value = serde_json::from_str(
            &rt.memory_upsert(
                r#"{"records":[{"id":"doc:mail:1","kind":"document","source":"mail",
                    "timestamp":"2026-09-01T10:00:00Z","title":"Dentist appointment",
                    "abstract":"Sunrise Dental on the 24th"}]}"#
                    .to_string(),
            )
            .expect("upsert ok"),
        )
        .unwrap();
        assert_eq!(report["inserted"], 1);
        assert_eq!(report["embedded"], 0);

        let hits: serde_json::Value = serde_json::from_str(
            &rt.memory_search(r#"{"query":"dentist","sources":["mail"]}"#.to_string())
                .expect("search ok"),
        )
        .unwrap();
        assert_eq!(hits["hits"][0]["id"], "doc:mail:1");
        assert_eq!(hits["hits"][0]["trust"], "untrusted");

        let loaded: serde_json::Value =
            serde_json::from_str(&rt.memory_load("doc:mail:1".to_string()).expect("load ok"))
                .unwrap();
        assert_eq!(loaded["record"]["visits"], 1);

        let stats: serde_json::Value =
            serde_json::from_str(&rt.memory_stats().expect("stats ok")).unwrap();
        assert_eq!(stats["records"], 1);

        match rt.memory_load("doc:mail:none".to_string()) {
            Err(OctosError::Memory { msg }) => assert_eq!(msg, "no such record"),
            other => panic!("expected Memory error, got {other:?}"),
        }
    }

    #[test]
    fn config_maps_api_type_through() {
        // A custom Anthropic-compatible endpoint needs the api_type override to
        // reach the factory — assert it survives the Config -> RuntimeConfig map.
        let cfg = Config {
            provider: "custom".to_string(),
            api_type: Some("anthropic".to_string()),
            ..sample_config()
        };
        let native: octos_ffi::RuntimeConfig = cfg.into();
        assert_eq!(native.provider, "custom");
        assert_eq!(native.api_type.as_deref(), Some("anthropic"));
    }

    #[test]
    fn brief_maps_to_task_brief() {
        let brief = Brief {
            prompt: "hello".to_string(),
            max_iterations: Some(7),
        };
        let native: octos_ffi::TaskBrief = brief.into();
        assert_eq!(native.prompt, "hello");
        assert_eq!(native.max_iterations, Some(7));
    }

    #[test]
    fn core_error_variants_map_to_octos_error() {
        use octos_ffi::CoreError;
        assert!(matches!(
            OctosError::from(CoreError::Config("c".into())),
            OctosError::Config { msg } if msg == "c"
        ));
        assert!(matches!(
            OctosError::from(CoreError::Provider("p".into())),
            OctosError::Provider { msg } if msg == "p"
        ));
        assert!(matches!(
            OctosError::from(CoreError::Run("r".into())),
            OctosError::Run { msg } if msg == "r"
        ));
        assert!(matches!(
            OctosError::from(CoreError::Embed("e".into())),
            OctosError::Embed { msg } if msg == "e"
        ));
        assert!(matches!(
            OctosError::from(CoreError::NoEmbedder),
            OctosError::NoEmbedder
        ));
        // Display renders the scrubbed message / the fixed NoEmbedder text.
        assert_eq!(
            OctosError::Provider { msg: "boom".into() }.to_string(),
            "boom"
        );
        assert_eq!(OctosError::NoEmbedder.to_string(), "no embedder configured");
    }

    #[test]
    fn octos_error_from_core_error_redacts_secret_shaped_tokens() {
        use octos_ffi::CoreError;
        // A provider error body can echo a credential the core did not know to
        // exact-scrub. The From<CoreError> conversion must apply octos-ffi's
        // heuristic redactor so it never reaches a uniffi caller verbatim.
        let leaked = "sk-abc123DEF456ghijkLMNOP789";
        let err: OctosError =
            CoreError::Provider(format!("upstream 401: token {leaked} rejected")).into();
        match err {
            OctosError::Provider { msg } => {
                assert!(msg.contains("<redacted>"), "not redacted: {msg}");
                assert!(!msg.contains(leaked), "leaked verbatim: {msg}");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    #[test]
    fn native_results_map_to_uniffi_records() {
        let native = octos_ffi::TaskResult {
            output: "done".to_string(),
            iterations: 2,
            tokens: octos_ffi::TokenUsage {
                input: 10,
                output: 20,
                reasoning: 3,
                cache_read: 4,
                cache_write: 5,
            },
        };
        let mapped: TaskResult = native.into();
        assert_eq!(mapped.output, "done");
        assert_eq!(mapped.iterations, 2);
        assert_eq!(mapped.tokens.input, 10);
        assert_eq!(mapped.tokens.output, 20);
        assert_eq!(mapped.tokens.reasoning, 3);
        assert_eq!(mapped.tokens.cache_read, 4);
        assert_eq!(mapped.tokens.cache_write, 5);
    }

    #[test]
    fn incomplete_error_preserves_payload_without_sanitizing_it_as_a_diagnostic() {
        let output = format!("  模型 partial\n{}", "actual output ".repeat(100));
        let error = OctosError::from(octos_ffi::CoreError::Incomplete {
            partial: octos_ffi::TaskResult {
                output: output.clone(),
                iterations: 2,
                tokens: octos_ffi::TokenUsage {
                    input: 18,
                    output: 8,
                    reasoning: 6,
                    cache_read: 7,
                    cache_write: 5,
                },
            },
        });
        assert!(!error.to_string().contains("模型"));
        let OctosError::Incomplete { partial } = error else {
            panic!("incomplete output must remain a structured error");
        };
        assert_eq!(partial.output, output);
        assert_eq!(partial.iterations, 2);
        assert_eq!(partial.tokens.input, 18);
        assert_eq!(partial.tokens.output, 8);
        assert_eq!(partial.tokens.reasoning, 6);
        assert_eq!(partial.tokens.cache_read, 7);
        assert_eq!(partial.tokens.cache_write, 5);
    }

    #[test]
    fn runtime_new_rejects_unknown_provider() {
        // Hermetic: provider construction fails offline (no network), before any
        // scratch dir is created, so this exercises the error mapping cleanly.
        let cfg = Config {
            provider: "totally-not-a-real-provider".to_string(),
            model: "x".to_string(),
            ..sample_config()
        };
        // Avoid `expect_err` (it needs `Debug` on the `Arc<Runtime>` Ok value,
        // which the opaque core deliberately does not implement).
        match Runtime::new(cfg) {
            Ok(_) => panic!("unknown provider must fail"),
            Err(OctosError::Provider { msg }) => {
                assert!(msg.contains("unknown provider"), "got: {msg}");
            }
            Err(other) => panic!("expected Provider error, got {other:?}"),
        }
    }

    #[test]
    fn runtime_builds_then_embed_reports_no_embedder() {
        // A valid provider config builds offline (no network until run_task).
        let rt =
            Runtime::new(sample_config()).unwrap_or_else(|e| panic!("runtime build failed: {e}"));

        // No embedding_model_path was configured, so embed reports NoEmbedder in
        // BOTH builds: feature-off is always NoEmbedder; feature-on finds no
        // loaded embedder. (No network is touched.)
        let err = rt
            .embed("hello".to_string())
            .expect_err("embed must fail without an embedder");
        assert!(
            matches!(err, OctosError::NoEmbedder),
            "expected NoEmbedder, got {err:?}"
        );
    }

    /// Real end-to-end run. Ignored: needs a live provider + network. Configure
    /// via env `OCTOS_UNIFFI_TEST_KEY_ENV` (default `OPENAI_API_KEY`). Run with:
    ///   cargo test -p octos-uniffi -- --ignored real_run_task
    #[test]
    #[ignore = "needs a real API key + network"]
    fn real_run_task_returns_output() {
        let key_env = std::env::var("OCTOS_UNIFFI_TEST_KEY_ENV")
            .unwrap_or_else(|_| "OPENAI_API_KEY".to_string());
        let cfg = Config {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            api_key: None,
            api_key_env: Some(key_env),
            base_url: None,
            api_type: None,
            cwd: Some(".".to_string()),
            allow_shell: false,
            max_iterations: Some(3),
            embedding_model_path: None,
            data_dir: None,
            recall_dimension: None,
            embedding_auto_download: Some(false),
        };
        let rt = Runtime::new(cfg).expect("runtime built");
        let result = rt
            .run_task(Brief {
                prompt: "Reply with exactly OK".to_string(),
                max_iterations: Some(3),
            })
            .expect("run_task succeeded");
        assert!(result.output.contains("OK"), "got: {}", result.output);
    }
}
