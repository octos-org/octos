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
//! * [`Runtime`] — an opaque object (`Arc`-shared) with `new`, `run_task`, `embed`.
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
//! The core keeps a tiny episodic-memory scratch dir under the OS temp dir. It
//! is owned by an RAII guard in the shared core (`octos-ffi`) that removes it
//! when the last [`Runtime`] is dropped — the guard is the final struct field,
//! so it runs AFTER the episodic store releases its redb lock. Both facades
//! share this: the C-ABI's `octos_runtime_free` and a native/uniffi drop reclaim
//! the dir identically, so a long-lived Python/Swift/Kotlin host does not
//! accumulate scratch dirs.

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
        }
    }
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
}

impl std::fmt::Display for OctosError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OctosError::Config { msg }
            | OctosError::Provider { msg }
            | OctosError::Run { msg }
            | OctosError::Embed { msg } => f.write_str(msg),
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
}

// Compile-time proof that the uniffi Object is `Send + Sync` — required for a
// handle shared as `Arc<Runtime>` across foreign threads. It holds because
// `octos_ffi::OctosRuntime` is `Send + Sync` (a tokio runtime, `Arc<dyn
// LlmProvider>` whose trait is `Send + Sync`, `Arc<EpisodeStore>`, and plain
// data). `#[derive(uniffi::Object)]` also requires this; the assertion just
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
