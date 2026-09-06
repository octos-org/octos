# octos-uniffi

Idiomatic **Python / Swift / Kotlin** bindings for embedding octos, generated
from a single Rust definition by [uniffi](https://mozilla.github.io/uniffi-rs/)
(v0.29).

It is a thin wrapper over the **native core** in
[`octos-ffi`](../octos-ffi) (`octos_ffi::OctosRuntime`). All the real work —
provider construction, the agent loop, the embedder, and the **hardened
credential path** (single key resolution + pinning + secret-scrubbing of error
text) — lives in that one core, shared with the C-ABI. This crate only adds
idiomatic type marshalling, so there is exactly one place to audit for
credential handling.

## Surface

| Rust | Foreign |
|---|---|
| `Config` (record) | dict / data class of provider, model, key, cwd, … |
| `Brief` (record) | `{ prompt, max_iterations? }` |
| `TaskResult` / `TokenUsage` (records) | outputs |
| `OctosError` (error enum) | exception (`Config`/`Provider`/`Run`/`Embed`/`NoEmbedder`/`Incomplete`) |
| `Runtime` (object) | `new(config)`, `run_task(brief)`, `embed(text)` |

Methods are **synchronous**: the async agent loop is driven by a `block_on`
inside the core, so callers see plain blocking calls — call them from a normal
(non-async) thread. `Runtime` is `Send + Sync` and reference-counted, so it may
be shared across threads (concurrent calls contend on one internal executor).

## Build

```bash
cargo build -p octos-uniffi                      # cdylib + staticlib + rlib
cargo build -p octos-uniffi --features embed-llama  # + in-process GGUF embedder
```

## Generating bindings

Bindings are generated from the **built library** (proc-macro metadata, no
UDL) via the in-crate `uniffi-bindgen` binary — so the exact uniffi version the
crate compiled against is the one that generates:

```bash
cargo build -p octos-uniffi
cargo run -p octos-uniffi --bin uniffi-bindgen -- generate \
    --library target/debug/liboctos_uniffi.dylib \   # .so on Linux
    --language python \
    --out-dir crates/octos-uniffi/bindings/python

# Deterministic post-gen tidy so `git diff --check` stays clean (the generator
# emits some trailing whitespace + a trailing blank line):
perl -i -pe  's/[ \t]+$//'  crates/octos-uniffi/bindings/python/octos.py
perl -i -0pe 's/\n+\z/\n/'  crates/octos-uniffi/bindings/python/octos.py
```

Swift and Kotlin generate identically — swap `--language swift` (emits
`octos.swift` + a `.modulemap`) or `--language kotlin` (emits `octos.kt`). The
committed Python bindings live in [`bindings/python/octos.py`](bindings/python/).

## Python example

Put `octos.py` on the `PYTHONPATH` and the compiled library
(`liboctos_uniffi.dylib`/`.so`) where it can be loaded (uniffi looks it up by
name), then:

```python
from octos import Runtime, Config, Brief

rt = Runtime(Config(provider="openai", model="gpt-4o-mini", api_key="sk-..."))
print(rt.run_task(Brief(prompt="Reply OK")).output)
```

Optional `Config` fields default sensibly (`api_key_env`, `base_url`,
`api_type`, `cwd`, `allow_shell=False`, `max_iterations`,
`embedding_model_path`), so only `provider` and `model` (plus a credential) are
required. Set `api_type="anthropic"` (or `"responses"`) to drive a
`provider="custom"` Anthropic-compatible endpoint onto the right protocol.

Errors surface as an `OctosError` exception; a failed provider build or run
carries a scrubbed message, and `embed` without an embedder raises
`OctosError.NoEmbedder`.

A provider `max_tokens` stop raises `OctosError.Incomplete`, **not** a successful
`TaskResult`. Its `partial` field contains the actual output, accumulated token
usage, and iterations. The diagnostic string is fixed and short; the partial
is lossless task payload and is not passed through error redaction or the
600-byte diagnostic cap. Treat it as unfinished output, not as a final answer.

```python
from octos import OctosError

try:
    result = rt.run_task(Brief(prompt="Explain the design"))
except OctosError.Incomplete as error:
    unfinished_output = error.partial.output
    consumed_tokens = error.partial.tokens
    # Display/store as incomplete; do not report a successful final answer.
```

The error variant is appended, preserving existing variant ordinals. Regenerate
bindings together with the library to consume the new structured error; the
committed Python binding is generated from this version's library metadata.

Offline ABI regression (real C exports and generated Python, localhost fixture
with a fake key only; both incomplete and successful controls):

```bash
cargo build -p octos-ffi -p octos-uniffi
python3 crates/octos-uniffi/tests/incomplete_bindings.py --library-dir target/debug
```

## Credentials & safety

Credential resolution, pinning, and error-text scrubbing are inherited verbatim
from `octos-ffi` — see that crate's README for the full contract (an explicitly
supplied `api_key`/`api_key_env` wins over the `octos auth login` store; the key
is resolved once and pinned; do not pass a raw key beginning with `keychain:`).
As there, `tracing` debug/trace logging of provider error bodies is the host's
responsibility.

> Scratch cleanup: the core keeps a small episodic-memory scratch dir under the
> OS temp dir. It is owned by an RAII guard in the shared `octos-ffi` core that
> removes it when the last runtime is dropped (the guard is the final struct
> field, so it runs after the episodic store releases its redb lock). Both the
> C-ABI's `octos_runtime_free` and a native/uniffi drop reclaim it identically,
> so a long-lived host does not accumulate scratch dirs.
