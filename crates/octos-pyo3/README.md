# octos (Python)

Native Python bindings for embedding [octos](https://github.com/octos-org/octos)
— a Rust-native agentic OS — in a Python process. Built with
[pyo3](https://pyo3.rs/) over the shared **native core** in `octos-ffi`.

This is the **native, recommended** Python binding. A second, `uniffi`-generated
Python module also exists (`crates/octos-uniffi`), but that one is primarily a
*reference/parity* artifact for the Swift and Kotlin bindings generated from the
same definition. For Python, prefer this pyo3 extension: it is a compiled
extension module (no ctypes marshalling) and shares the exact same hardened
credential path as every other binding, because that logic lives once in
`octos-ffi::OctosRuntime`.

The wheel is an **abi3** wheel (`cp39-abi3`): one wheel works on CPython 3.9+.

## Install

```bash
pip install octos            # once published
```

## Build from source

Requires a Rust toolchain and [maturin](https://www.maturin.rs/).

```bash
pip install maturin

# Build a release wheel into target/wheels/:
maturin build --release            # run from crates/octos-pyo3/

# Or build + install into the active venv for development:
maturin develop --release          # run from crates/octos-pyo3/
```

The pyo3 surface sits behind a Cargo `python` feature that is **OFF by default**,
so a plain `cargo build` / `cargo build --workspace` pulls no libpython (a
Python-less CI lane cannot break). maturin turns on `extension-module` — which
implies `python` — for the wheel; the wheel then resolves Python symbols from
the host interpreter instead of linking libpython. To run the Rust test suite,
enable the feature (it links libpython so the harness can start an interpreter):

```bash
cargo test -p octos-pyo3 --features python
```

## Example

```python
import octos

rt = octos.Runtime(octos.Config(
    provider="openai",
    model="gpt-4o-mini",
    api_key="sk-...",            # or api_key_env="OPENAI_API_KEY"
))

result = rt.run_task(octos.Brief(prompt="Reply OK"))
print(result.output)
print(result.tokens.input, result.tokens.output)
```

`Runtime`, `Config`, and `Brief` accept keyword arguments. `Config` includes
`api_type` (`"anthropic"` / `"responses"`) — set it when driving a custom
Anthropic-compatible endpoint, otherwise the factory defaults to the OpenAI
protocol. Every failure raises `octos.OctosError`:

```python
try:
    rt = octos.Runtime(octos.Config(provider="nope", model="x"))
except octos.OctosError as e:
    print("failed:", e)
```

`api_key` is **write-only**: you pass it to `Config(...)`, but it has no getter,
so it cannot be read back off a `Config` — a logger, serializer, or plugin
cannot exfiltrate the plaintext. Check presence with `Config.api_key_is_set`;
`repr(config)` masks the key as `<set>`. (`api_key_env` is an env-var *name*, not
a secret, so it stays readable.)

## Threading & tokio

`run_task` and `embed` release the GIL around the blocking core call, so other
Python threads keep running. Ordinary (non-tokio) Python threads are fully
supported.

**Unsupported: creating or dropping a `Runtime` from within a Rust tokio async
context.** If you embed CPython in a Rust **tokio** application and build or drop
a `Runtime` on a tokio task (e.g. Python running under `tokio::spawn`), the
shared core panics when its internal tokio runtime is dropped inside another
tokio context, and the episodic-memory scratch dir may not be cleaned up. Build
and drop the runtime from a plain, non-async thread — the same contract as the
octos C-ABI.

## API

| Python | Purpose |
| --- | --- |
| `Config(provider, model, api_key=None, api_key_env=None, base_url=None, api_type=None, cwd=None, allow_shell=False, max_iterations=None, embedding_model_path=None)` | Runtime configuration. `api_key` is write-only (no getter). |
| `Config.api_key_is_set -> bool` | Whether an `api_key` literal was supplied (the raw key is never readable). |
| `Brief(prompt, max_iterations=None)` | A one-shot task. |
| `Runtime(config)` | Build the runtime (resolves the credential once). |
| `Runtime.run_task(brief) -> TaskResult` | Run a task. Releases the GIL. |
| `Runtime.embed(text) -> list[float]` | Embed text (needs the `embed-llama` build feature + `embedding_model_path`). Releases the GIL. |
| `TaskResult` | `.output: str`, `.iterations: int`, `.tokens: TokenUsage` |
| `TokenUsage` | `.input`, `.output`, `.reasoning`, `.cache_read`, `.cache_write` |
| `OctosError` | Raised by every failure. |

### Embeddings

`Runtime.embed` returns a real vector only when the extension is built with the
`embed-llama` feature (an in-process GGUF embedder via llama.cpp) and `Config`
sets `embedding_model_path`. Otherwise it raises `OctosError` ("no embedder
configured"). Build it with:

```bash
maturin build --release --features octos-pyo3/embed-llama
```

## Security note

Error messages surfaced to Python are credential-scrubbed: the caller's own key
is exact-scrubbed inside the core, and this binding additionally applies a
heuristic redactor + length cap to any secret-shaped token echoed in a provider
error body. As with the C-ABI, a `tracing` subscriber you install in-process is
still your responsibility — it may log a provider error body verbatim.
