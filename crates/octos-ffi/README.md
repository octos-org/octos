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
| `void octos_string_free(char*)` | Free a string returned by `octos_run_task`/`octos_embed`. |
| `const char* octos_last_error(void)` | Thread-local last error; do NOT free; valid until the next FFI call on this thread. |
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

### Safety contract

- The runtime handle is opaque; free it exactly once with `octos_runtime_free`.
- Strings from `octos_run_task`/`octos_embed` are owned by the caller and must
  be freed with `octos_string_free` — never `free(3)`, never twice.
- All pointer arguments are NULL-checked and panics never cross the boundary.

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
