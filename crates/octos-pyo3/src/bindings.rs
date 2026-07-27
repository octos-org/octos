//! The pyo3 surface, compiled only under the `python` feature (see the crate
//! root). Everything that touches pyo3 / libpython lives here so the default
//! build stays libpython-free.

use octos_ffi::{CoreError, OctosRuntime, RuntimeConfig, TaskBrief};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;

create_exception!(
    octos,
    OctosError,
    PyException,
    "Raised by every octos runtime failure (config, provider, run, embed)."
);

/// Convert a native [`CoreError`] into a Python `OctosError`.
///
/// The caller's OWN key is already exact-scrubbed inside the core. Here we
/// additionally apply octos-ffi's heuristic redactor + length cap
/// ([`octos_ffi::sanitize_error_text`]) — the SAME backstop the C-ABI runs in
/// `set_last_error` and the uniffi facade runs in its `From<CoreError>` — so a
/// secret embedded in a *provider* error body never reaches a Python caller
/// verbatim. Applied ONLY here at the facade boundary, never in the core (doing
/// so would double-apply on the C path and perturb `octos_last_error`).
///
/// A free function rather than a `From` impl: the orphan rule forbids
/// `impl From<CoreError> for PyErr` (both types are foreign to this crate).
fn to_py_err(e: CoreError) -> PyErr {
    OctosError::new_err(octos_ffi::sanitize_error_text(&e.to_string()))
}

/// Runtime configuration. Maps directly onto [`octos_ffi::RuntimeConfig`].
///
/// Supply EITHER `api_key` (a literal key) OR `api_key_env` (the name of a
/// process env var holding it). If neither is set, resolution falls back to the
/// conventional `{PROVIDER}_API_KEY` env var and then the `octos auth login`
/// store — see [`octos_ffi::OctosRuntime::from_config`].
///
/// # Secret handling
///
/// `api_key` is write-only from Python: there is deliberately NO `api_key`
/// getter, so a logger/serializer/plugin cannot read the plaintext back off a
/// `Config`. Use the boolean [`Config::api_key_is_set`] to check presence. The
/// `api_key_env` field (an env-var *name*, not a secret) stays readable.
#[pyclass]
#[derive(Debug, Clone)]
pub struct Config {
    #[pyo3(get)]
    pub provider: String,
    #[pyo3(get)]
    pub model: String,
    // NOTE: NO `#[pyo3(get)]` — the raw key must never be readable from Python.
    // Rust-visible only (used by `to_native`/`__repr__`).
    pub api_key: Option<String>,
    #[pyo3(get)]
    pub api_key_env: Option<String>,
    #[pyo3(get)]
    pub base_url: Option<String>,
    #[pyo3(get)]
    pub api_type: Option<String>,
    #[pyo3(get)]
    pub cwd: Option<String>,
    #[pyo3(get)]
    pub allow_shell: bool,
    #[pyo3(get)]
    pub max_iterations: Option<u32>,
    #[pyo3(get)]
    pub embedding_model_path: Option<String>,
}

impl Config {
    /// Map to the native core config. Plain Rust (not exposed to Python).
    fn to_native(&self) -> RuntimeConfig {
        RuntimeConfig {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            api_key_env: self.api_key_env.clone(),
            base_url: self.base_url.clone(),
            api_type: self.api_type.clone(),
            cwd: self.cwd.clone(),
            allow_shell: self.allow_shell,
            max_iterations: self.max_iterations,
            embedding_model_path: self.embedding_model_path.clone(),
        }
    }
}

#[pymethods]
impl Config {
    /// Build a runtime config.
    ///
    /// `provider` and `model` are required; everything else is optional.
    /// **`api_type`** (`"anthropic"` / `"responses"`) is included deliberately:
    /// omitting it forces a custom Anthropic-compatible endpoint onto the
    /// OpenAI protocol.
    #[new]
    #[pyo3(signature = (
        provider,
        model,
        api_key = None,
        api_key_env = None,
        base_url = None,
        api_type = None,
        cwd = None,
        allow_shell = false,
        max_iterations = None,
        embedding_model_path = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        provider: String,
        model: String,
        api_key: Option<String>,
        api_key_env: Option<String>,
        base_url: Option<String>,
        api_type: Option<String>,
        cwd: Option<String>,
        allow_shell: bool,
        max_iterations: Option<u32>,
        embedding_model_path: Option<String>,
    ) -> Self {
        Config {
            provider,
            model,
            api_key,
            api_key_env,
            base_url,
            api_type,
            cwd,
            allow_shell,
            max_iterations,
            embedding_model_path,
        }
    }

    /// Whether an `api_key` literal was supplied — a safe, boolean stand-in for
    /// the (intentionally absent) `api_key` getter, so callers can check
    /// presence without the plaintext ever being readable.
    #[getter]
    fn api_key_is_set(&self) -> bool {
        self.api_key.is_some()
    }

    fn __repr__(&self) -> String {
        format!(
            "Config(provider={:?}, model={:?}, api_key={}, api_key_env={:?}, base_url={:?}, api_type={:?}, cwd={:?}, allow_shell={}, max_iterations={:?}, embedding_model_path={:?})",
            self.provider,
            self.model,
            // Never echo the secret in a repr.
            if self.api_key.is_some() {
                "<set>"
            } else {
                "None"
            },
            self.api_key_env,
            self.base_url,
            self.api_type,
            self.cwd,
            self.allow_shell,
            self.max_iterations,
            self.embedding_model_path,
        )
    }
}

/// A one-shot task brief. Maps onto [`octos_ffi::TaskBrief`].
#[pyclass]
#[derive(Debug, Clone)]
pub struct Brief {
    #[pyo3(get)]
    pub prompt: String,
    #[pyo3(get)]
    pub max_iterations: Option<u32>,
}

impl Brief {
    fn to_native(&self) -> TaskBrief {
        TaskBrief {
            prompt: self.prompt.clone(),
            max_iterations: self.max_iterations,
        }
    }
}

#[pymethods]
impl Brief {
    /// Build a task brief. `prompt` is required; `max_iterations` overrides the
    /// runtime default for this task only.
    #[new]
    #[pyo3(signature = (prompt, max_iterations = None))]
    fn new(prompt: String, max_iterations: Option<u32>) -> Self {
        Brief {
            prompt,
            max_iterations,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "Brief(prompt={:?}, max_iterations={:?})",
            self.prompt, self.max_iterations
        )
    }
}

/// Token accounting for a completed [`Runtime::run_task`]. Read-only.
#[pyclass]
#[derive(Debug, Clone)]
pub struct TokenUsage {
    #[pyo3(get)]
    pub input: u64,
    #[pyo3(get)]
    pub output: u64,
    #[pyo3(get)]
    pub reasoning: u64,
    #[pyo3(get)]
    pub cache_read: u64,
    #[pyo3(get)]
    pub cache_write: u64,
}

impl TokenUsage {
    fn from_native(t: octos_ffi::TokenUsage) -> Self {
        TokenUsage {
            input: t.input,
            output: t.output,
            reasoning: t.reasoning,
            cache_read: t.cache_read,
            cache_write: t.cache_write,
        }
    }
}

#[pymethods]
impl TokenUsage {
    fn __repr__(&self) -> String {
        format!(
            "TokenUsage(input={}, output={}, reasoning={}, cache_read={}, cache_write={})",
            self.input, self.output, self.reasoning, self.cache_read, self.cache_write
        )
    }
}

/// The result of a completed [`Runtime::run_task`]. Read-only.
#[pyclass]
#[derive(Debug, Clone)]
pub struct TaskResult {
    #[pyo3(get)]
    pub output: String,
    #[pyo3(get)]
    pub iterations: u32,
    #[pyo3(get)]
    pub tokens: TokenUsage,
}

impl TaskResult {
    fn from_native(r: octos_ffi::TaskResult) -> Self {
        TaskResult {
            output: r.output,
            iterations: r.iterations,
            tokens: TokenUsage::from_native(r.tokens),
        }
    }
}

#[pymethods]
impl TaskResult {
    fn __repr__(&self) -> String {
        format!(
            "TaskResult(output={:?}, iterations={}, tokens={})",
            self.output,
            self.iterations,
            self.tokens.__repr__()
        )
    }
}

/// An embedded octos runtime — the Python counterpart of the C-ABI's opaque
/// `OctosRuntime*`.
///
/// Construct with `Runtime(config)`. Build the credential-resolved runtime once
/// and reuse it for many `run_task` / `embed` calls. The object is `Send +
/// Sync`; `run_task` and `embed` release the GIL while the internal executor
/// blocks, so other Python threads keep running (they will, however, contend on
/// the single internal executor).
///
/// # Unsupported: creation/drop inside a Rust tokio context
///
/// The runtime owns a tokio runtime inside the shared core. If you embed CPython
/// in a Rust **tokio** application and construct or drop a `Runtime` from
/// *within* an async task (e.g. Python running on `tokio::spawn`), the core
/// panics when its tokio runtime is dropped inside another tokio context, and
/// the episodic-memory scratch dir may not be cleaned up. This mirrors the
/// C-ABI's contract: build and drop the runtime from a plain, non-async thread.
/// Ordinary (non-tokio) Python threads are fine — this concerns only hosts that
/// already run a tokio reactor on the calling thread.
#[pyclass]
pub struct Runtime {
    inner: OctosRuntime,
}

#[pymethods]
impl Runtime {
    /// Build a runtime from a [`Config`]. Resolves and pins the credential
    /// exactly once inside the core. Raises `OctosError` on a bad config or an
    /// unknown/unbuildable provider. Does NOT touch the network.
    #[new]
    fn new(config: &Config) -> PyResult<Self> {
        let inner = OctosRuntime::from_config(config.to_native()).map_err(to_py_err)?;
        Ok(Runtime { inner })
    }

    /// Run a one-shot task and return its output + token usage. Releases the GIL
    /// around the blocking agent loop. Raises `OctosError` on failure.
    fn run_task(&self, py: Python<'_>, brief: &Brief) -> PyResult<TaskResult> {
        let native = brief.to_native();
        // Release the GIL: the core's `block_on` drives the whole agent loop
        // (network + tools) and must not hold the GIL. `OctosRuntime` is
        // Send + Sync, and neither `native` nor the result carries a GIL token.
        let result = py
            .allow_threads(|| self.inner.run_task(&native))
            .map_err(to_py_err)?;
        Ok(TaskResult::from_native(result))
    }

    /// Embed `text`, returning the raw vector. Requires the `embed-llama` build
    /// feature and an `embedding_model_path` in the [`Config`]; otherwise raises
    /// `OctosError` (NoEmbedder). Releases the GIL around the blocking call.
    fn embed(&self, py: Python<'_>, text: String) -> PyResult<Vec<f32>> {
        py.allow_threads(|| self.inner.embed(&text))
            .map_err(to_py_err)
    }

    fn __repr__(&self) -> String {
        "<octos.Runtime>".to_string()
    }
}

// Compile-time proof that the pyclass is `Send + Sync`, matching the uniffi
// facade's assertion. `#[pyclass]` already requires `Send`; this makes a
// regression a direct, readable error. It holds because
// `octos_ffi::OctosRuntime` is `Send + Sync`.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Runtime>();
};

/// The `octos` Python module. The `#[pymodule]` name and the `[lib] name` are
/// both `octos`, so `import octos` loads this extension.
#[pymodule]
fn octos(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Config>()?;
    m.add_class::<Brief>()?;
    m.add_class::<TokenUsage>()?;
    m.add_class::<TaskResult>()?;
    m.add_class::<Runtime>()?;
    m.add("OctosError", m.py().get_type::<OctosError>())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensure a Python interpreter is initialized for the standalone test binary
    /// (the wheel is loaded BY Python and needs none of this). Idempotent.
    fn init_py() {
        pyo3::prepare_freethreaded_python();
    }

    fn sample_config() -> Config {
        Config::new(
            "openai".to_string(),
            "gpt-4o-mini".to_string(),
            Some("sk-pyo3-test-dummy".to_string()),
            None,
            None,
            None,
            Some(".".to_string()),
            false,
            Some(3),
            None,
        )
    }

    #[test]
    fn config_maps_to_runtime_config() {
        let native = sample_config().to_native();
        assert_eq!(native.provider, "openai");
        assert_eq!(native.model, "gpt-4o-mini");
        assert_eq!(native.api_key.as_deref(), Some("sk-pyo3-test-dummy"));
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
        // reach the factory — assert it survives Config -> RuntimeConfig.
        let cfg = Config::new(
            "custom".to_string(),
            "some-model".to_string(),
            Some("sk-x".to_string()),
            None,
            Some("https://example.test/v1".to_string()),
            Some("anthropic".to_string()),
            None,
            false,
            None,
            None,
        );
        let native = cfg.to_native();
        assert_eq!(native.provider, "custom");
        assert_eq!(native.api_type.as_deref(), Some("anthropic"));
        assert_eq!(native.base_url.as_deref(), Some("https://example.test/v1"));
    }

    #[test]
    fn brief_maps_to_task_brief() {
        let brief = Brief::new("hello".to_string(), Some(7));
        let native = brief.to_native();
        assert_eq!(native.prompt, "hello");
        assert_eq!(native.max_iterations, Some(7));
    }

    #[test]
    fn native_results_map_to_pyclasses() {
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
        let mapped = TaskResult::from_native(native);
        assert_eq!(mapped.output, "done");
        assert_eq!(mapped.iterations, 2);
        assert_eq!(mapped.tokens.input, 10);
        assert_eq!(mapped.tokens.output, 20);
        assert_eq!(mapped.tokens.reasoning, 3);
        assert_eq!(mapped.tokens.cache_read, 4);
        assert_eq!(mapped.tokens.cache_write, 5);
    }

    #[test]
    fn config_does_not_expose_raw_api_key() {
        // P1 regression guard: the raw api_key must NOT be readable from Python
        // via any obvious path (attribute, repr, str). A logger/serializer that
        // walks a Config must not be able to exfiltrate the plaintext key.
        init_py();
        let secret = "sk-super-secret-DO-NOT-LEAK-1234567890";
        Python::with_gil(|py| {
            let cfg = Config::new(
                "openai".to_string(),
                "gpt-4o-mini".to_string(),
                Some(secret.to_string()),
                None,
                None,
                None,
                None,
                false,
                None,
                None,
            );
            let obj = Py::new(py, cfg).expect("wrap Config in a Python object");
            let bound = obj.bind(py);

            // No `api_key` attribute exists (the getter was removed).
            assert!(
                bound.getattr("api_key").is_err(),
                "api_key must not be attribute-accessible"
            );
            // The boolean presence accessor works and is true.
            let is_set: bool = bound
                .getattr("api_key_is_set")
                .expect("api_key_is_set getter present")
                .extract()
                .expect("bool");
            assert!(is_set, "api_key_is_set should be true when a key was given");
            // repr masks the key.
            let repr: String = bound.repr().expect("repr").extract().expect("str");
            assert!(!repr.contains(secret), "repr leaked the key: {repr}");
            assert!(
                repr.contains("<set>"),
                "repr should mark the key <set>: {repr}"
            );
            // str() falls back to repr -> also masked.
            let s: String = bound.str().expect("str").extract().expect("str");
            assert!(!s.contains(secret), "str leaked the key: {s}");
            // api_key_env (an env-var NAME, not a secret) remains readable — but
            // it was never set here, so it reads as None.
            let env_attr = bound.getattr("api_key_env").expect("api_key_env getter");
            assert!(env_attr.is_none(), "api_key_env should be None here");
        });
    }

    #[test]
    fn runtime_new_rejects_unknown_provider_with_octos_error() {
        // Hermetic: provider construction fails offline (no network) before any
        // scratch dir is created, so this exercises the error path cleanly.
        init_py();
        Python::with_gil(|py| {
            let cfg = Config::new(
                "totally-not-a-real-provider".to_string(),
                "x".to_string(),
                Some("sk-x".to_string()),
                None,
                None,
                None,
                Some(".".to_string()),
                false,
                Some(3),
                None,
            );
            let err = Runtime::new(&cfg)
                .err()
                .expect("unknown provider must raise");
            assert!(
                err.is_instance_of::<OctosError>(py),
                "expected OctosError, got: {err}"
            );
        });
    }

    #[test]
    fn to_py_err_redacts_secret_shaped_tokens() {
        // A provider error body can echo a credential the core did not know to
        // exact-scrub. `to_py_err` must apply octos-ffi's heuristic redactor so
        // it never reaches a Python caller verbatim.
        init_py();
        let leaked = "sk-abc123DEF456ghijkLMNOP789";
        let err = to_py_err(CoreError::Provider(format!(
            "upstream 401: token {leaked} rejected"
        )));
        Python::with_gil(|py| {
            assert!(
                err.is_instance_of::<OctosError>(py),
                "expected OctosError, got: {err}"
            );
            let msg = err.value(py).to_string();
            assert!(msg.contains("<redacted>"), "not redacted: {msg}");
            assert!(!msg.contains(leaked), "leaked verbatim: {msg}");
        });
    }

    #[test]
    fn no_embedder_maps_to_octos_error() {
        // Building a runtime succeeds offline; embed without a model reports
        // NoEmbedder (feature-off is always NoEmbedder; feature-on finds no
        // loaded embedder). Its message maps through `to_py_err`.
        let err = to_py_err(CoreError::NoEmbedder);
        init_py();
        Python::with_gil(|py| {
            assert!(err.is_instance_of::<OctosError>(py));
            let msg = err.value(py).to_string();
            assert_eq!(msg, "no embedder configured");
        });
    }

    /// Real end-to-end run. Ignored: needs a live provider + network. Configure
    /// via env `OCTOS_PYO3_TEST_KEY_ENV` (default `OPENAI_API_KEY`). Run with:
    ///   cargo test -p octos-pyo3 --features python -- --ignored real_run_task
    #[test]
    #[ignore = "needs a real API key + network"]
    fn real_run_task_returns_output() {
        init_py();
        let key_env = std::env::var("OCTOS_PYO3_TEST_KEY_ENV")
            .unwrap_or_else(|_| "OPENAI_API_KEY".to_string());
        let cfg = Config::new(
            "openai".to_string(),
            "gpt-4o-mini".to_string(),
            None,
            Some(key_env),
            None,
            None,
            Some(".".to_string()),
            false,
            Some(3),
            None,
        );
        let rt = Runtime::new(&cfg).expect("runtime built");
        Python::with_gil(|py| {
            let brief = Brief::new("Reply with exactly OK".to_string(), Some(3));
            let result = rt.run_task(py, &brief).expect("run_task succeeded");
            assert!(result.output.contains("OK"), "got: {}", result.output);
        });
    }
}
