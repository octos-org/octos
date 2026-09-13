# octos-ffi

A C-ABI surface (`cdylib` + `staticlib`) for embedding octos in non-Rust hosts
— Python, Node, Go, or plain C. It reuses the same provider-construction and
agent loop the `octos` CLI uses, exposed as a small one-shot task runner plus an
optional embedder.

## Build

```bash
# Shared library (target/release/liboctos_ffi.{dylib,so} + .a static lib)
cargo build -p octos-ffi --release

# With the in-process GGUF embedder (pulls a CMake build of llama.cpp):
cargo build -p octos-ffi --release --features embed-llama          # CPU
cargo build -p octos-ffi --release --features embed-llama-metal     # Apple GPU
```

The generated C header is committed at:

```
crates/octos-ffi/include/octos.h
```

Regenerate it (after changing the surface) with
[`cbindgen`](https://github.com/mozilla/cbindgen):

```bash
cbindgen --config crates/octos-ffi/cbindgen.toml \
         --crate octos-ffi \
         --output crates/octos-ffi/include/octos.h
```

## API

| Function | Description |
|---|---|
| `OctosRuntime* octos_runtime_new(const char* config_json)` | Build a runtime; NULL on error. |
| `void octos_runtime_free(OctosRuntime*)` | Free the runtime (NULL-safe). |
| `char* octos_run_task(OctosRuntime*, const char* brief_json)` | Run one task; returns owned JSON, NULL on error. |
| `char* octos_embed(OctosRuntime*, const char* text)` | Embed text (needs `embed-llama` + a model path); NULL on error. |
| `void octos_string_free(char*)` | Free a string returned by `octos_run_task`/`octos_embed`/`octos_take_last_partial_result`. |
| `const char* octos_last_error(void)` | Thread-local last error; do NOT free; valid until the next FFI call on this thread. |
| `char* octos_take_last_partial_result(void)` | Take the last incomplete task's owned JSON once, on the same thread; NULL if absent. Free with `octos_string_free`. |
| `const char* octos_version(void)` | Static version string. |

`config_json`:

```json
{
  "provider": "openai",
  "model": "gpt-4o-mini",
  "api_key": "sk-...",            // or "api_key_env": "OPENAI_API_KEY"
  "base_url": "https://...",       // optional
  "cwd": "/path/to/workspace",     // optional; FS tools are confined here
  "allow_shell": false,            // optional; off by default
  "max_iterations": 20,            // optional
  "embedding_model_path": "/models/embed.gguf"  // optional (embed-llama)
}
```

`brief_json`: `{"prompt": "...", "max_iterations"?: N}`.
Task result: `{"output": "...", "iterations": N, "tokens": {"input", "output", ...}}`.

### Incomplete responses are failures with recoverable output

A provider `max_tokens` stop still makes `octos_run_task` return **NULL**.
`octos_last_error` holds a short, sanitized diagnostic; it never contains the
partial model body. Call `octos_take_last_partial_result` on the **same thread**
to take the actual partial `TaskResult` JSON, including accumulated token usage
and iterations. This is not a successful final answer.

The accessor transfers ownership once and returns NULL on subsequent reads.
Free its allocation, unmodified, with `octos_string_free`. Reading last-error
or version, taking the result, and successful free calls do not reset the
diagnostic. A new `octos_runtime_new`, `octos_run_task`, or `octos_embed` call
clears any untaken partial at entry, whether it later succeeds or fails. Any
new error (including a caught panic) also clears it. Untaken data is released
at thread exit; no data is shared across threads.

Partial output is the Agent's task payload: its Unicode, whitespace, and full
length are preserved by the FFI, just like successful output (normal Agent
response normalization still applies). It is **not** sanitized or shortened
by the 600-byte diagnostic cap. Hosts should not log it as error text. Native
Rust callers receive `CoreError::Incomplete { partial: TaskResult }` directly;
other error kinds retain their existing behavior.

### Safety contract

- **Handle thread-safety & lifetime.** An `OctosRuntime*` is NOT thread-safe.
  Do not call any function on a handle after `octos_runtime_free`. Do not call
  `octos_runtime_free` concurrently with — or while any other call on the same
  handle is in flight. Serialize all calls on a handle (or guard it with your
  own mutex): a concurrent run+free is a use-after-free and free+free is a
  double-free, and the library cannot prevent either across a C ABI.
- **Free from a non-async thread.** `octos_runtime_free` drops a tokio runtime;
  dropping it from inside a host's own async/tokio context fails. Call
  `octos_run_task`/`octos_embed`/`octos_runtime_free` from a plain thread (they
  block internally). The panic firewall contains such misuse (returns
  null/no-op) but the runtime cannot then clean up fully.
- **Returned strings are immutable + caller-owned.** Strings from
  `octos_run_task`/`octos_embed`/`octos_take_last_partial_result` MUST be freed, UNMODIFIED, with
  `octos_string_free` — never `free(3)`, never twice, and do not alter the bytes
  or the NUL terminator before freeing (freeing rescans for the NUL; a mutated
  terminator corrupts the allocator).
- **Panics never cross the boundary.** Every export runs inside a panic
  firewall; a panic becomes a null/error return, never an unwind into C.
- **Errors are redacted (best-effort).** `octos_last_error` strings are
  length-capped and scrubbed before being exposed: the caller's OWN configured
  key is removed by reliable **exact match** (any length, any source), while
  credential-*shaped* UNKNOWN tokens are removed by a heuristic that — being a
  heuristic — is best-effort and cannot be perfect.

### Credentials

Resolution reuses octos's `Config`. An **explicitly-passed `api_key` (or
`api_key_env`) wins**: the FFI marks the config to bypass the global
`octos auth login` AuthStore for that call, so a host that happens to be logged
in cannot silently shadow the caller's key. If you supply neither, resolution
falls back to the conventional `{PROVIDER}_API_KEY` process env var and the
AuthStore, in that order. The key is resolved exactly once and pinned so the
provider is built with that same value (no second, possibly-rotated read).

> **Do not supply a raw API key that begins with `keychain:`.** The resolved key
> is pinned into the config's `env_vars`, which octos then passes through its
> normal value resolution — so a value beginning with `keychain:` is interpreted
> as a keychain *reference* (octos's standard secret-indirection convention),
> not used verbatim. This is inherent to octos's config model, not FFI-specific.

### Security: provider error logging

`octos_last_error` is redacted, but octos and its LLM providers may also log
provider error bodies at **debug/trace** level via `tracing`. A misbehaving
"OpenAI-compatible" endpoint that echoes your request credential in a 4xx body
would then land in those logs (not in `octos_last_error`, which is redacted).
The `tracing` subscriber is the **host's** responsibility: **do not enable
debug/trace logging with untrusted providers** when embedding octos.

## Python (ctypes) example

```python
import ctypes, json

lib = ctypes.CDLL("target/release/liboctos_ffi.dylib")  # .so on Linux
lib.octos_runtime_new.restype = ctypes.c_void_p
lib.octos_runtime_new.argtypes = [ctypes.c_char_p]
lib.octos_run_task.restype = ctypes.c_void_p   # owned char* (not auto-freed)
lib.octos_run_task.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
lib.octos_string_free.argtypes = [ctypes.c_void_p]
lib.octos_runtime_free.argtypes = [ctypes.c_void_p]
lib.octos_last_error.restype = ctypes.c_char_p  # borrowed; do NOT free

cfg = json.dumps({"provider": "openai", "model": "gpt-4o-mini",
                  "api_key_env": "OPENAI_API_KEY", "cwd": "."}).encode()
rt = lib.octos_runtime_new(cfg)
if not rt:
    raise RuntimeError(lib.octos_last_error().decode())

out = lib.octos_run_task(rt, json.dumps({"prompt": "Reply with exactly OK"}).encode())
if not out:
    raise RuntimeError(lib.octos_last_error().decode())
print(json.loads(ctypes.cast(out, ctypes.c_char_p).value.decode())["output"])

lib.octos_string_free(out)     # free the owned result string
lib.octos_runtime_free(rt)     # free the runtime handle
```

> Note: `octos_run_task`/`octos_embed` are declared `restype = c_void_p` (not
> `c_char_p`) so ctypes does **not** copy-and-forget the pointer — you must read
> it (via `ctypes.cast(..., c_char_p)`) and then hand the original pointer to
> `octos_string_free`.
