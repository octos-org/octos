# octos-ffi

A C-ABI surface (`cdylib` + `staticlib`) for embedding octos in non-Rust hosts
— Python, Node, Go, or plain C. It reuses the same provider-construction and
agent loop the `octos` CLI uses, exposed as a small one-shot task runner plus an
optional embedder.

## Build

```bash
# Shared library (target/release/liboctos_ffi.{dylib,so} + .a static lib).
# The default features include `embed-llama`, the in-process GGUF embedder
# (a CMake build of llama.cpp: needs cmake + a C++ toolchain):
cargo build -p octos-ffi --release                                  # CPU
cargo build -p octos-ffi --release --features embed-llama-metal     # Apple GPU

# Pure-Rust build without the embedder (memory search is keyword-only):
cargo build -p octos-ffi --release --no-default-features
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
| `char* octos_embed(OctosRuntime*, const char* text)` | Embed text (needs `embed-llama` + a loaded model — the default one or `embedding_model_path`); NULL on error. |
| `char* octos_embedding_model_status(const char* data_dir)` | What is on disk for the default embedding model under `data_dir`; owned JSON, NULL on error. No runtime needed. |
| `char* octos_embedding_model_ensure(const char* data_dir, bool download)` | Make sure the default model is complete under `data_dir` (downloading it when `download`); owned JSON `{"path"}`, NULL on error. No runtime needed. |
| `char* octos_memory_upsert(OctosRuntime*, const char* request_json)` | Push app records into the Recall memory index; returns owned JSON report, NULL on error. |
| `char* octos_memory_search(OctosRuntime*, const char* request_json)` | Search the Recall index; returns owned JSON `{"hits": [...]}`, NULL on error. |
| `char* octos_memory_load(OctosRuntime*, const char* id)` | Load one record (counts a visit); returns owned JSON `{"record": ...}`, NULL on error. |
| `char* octos_memory_stats(OctosRuntime*)` | Recall index statistics as owned JSON; NULL on error. |
| `void octos_string_free(char*)` | Free a string returned by `octos_run_task`/`octos_embed`/`octos_memory_*`/`octos_embedding_model_*`/`octos_take_last_partial_result`. |
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
  "embedding_model_path": "/models/embed.gguf",  // optional; overrides the default model
  "embedding_auto_download": true, // optional; may the default model be fetched? (default true)
  "data_dir": "/data/octos",       // optional; persistent episode + Recall stores (+ the model)
  "recall_dimension": 256          // optional; Recall vector width (default 256)
}
```

`brief_json`: `{"prompt": "...", "max_iterations"?: N}`.
Task result: `{"output": "...", "iterations": N, "tokens": {"input", "output", ...}}`.

### The default embedding model

An `embed-llama` build (the default) embeds with **EmbeddingGemma-300M**
(`embeddinggemma-300M-Q8_0.gguf`, 768-d, Matryoshka-truncated to
`recall_dimension`). The 334 MB file is not compiled in: the runtime looks
for it at `<data_dir>/models/embeddinggemma-300M-Q8_0.gguf` and, when it is
absent or incomplete and downloads are allowed, fetches it once from the
public `ggml-org/embeddinggemma-300M-GGUF` release on Hugging Face and
verifies its SHA-256. The weights are governed by the
[Gemma Terms of Use](https://ai.google.dev/gemma/terms) (also returned as
`license_url` below) — surface that to your users where your product
requires it.

Resolution at `octos_runtime_new`, when `embedding_model_path` is unset:

1. the file is complete under the data dir → it is loaded;
2. else, if `embedding_auto_download` is not `false` and
   `OCTOS_NO_MODEL_DOWNLOAD` is not set in the environment → it is
   downloaded **synchronously, blocking `octos_runtime_new`** for the whole
   transfer, then loaded; a failed download is logged (`tracing` warn) and
   the runtime continues keyword-only;
3. else → the runtime starts **without an embedder**: memory search is
   BM25/keyword-only, `octos_embed` fails with `no embedder configured`, and
   `octos_memory_stats` reports an empty `embedder_id`.

An explicit `embedding_model_path` keeps its old meaning: that file is
loaded, nothing is downloaded, and a load failure fails `octos_runtime_new`.
The "data dir" is `data_dir` when set, otherwise the runtime's scratch dir —
which is deleted on `octos_runtime_free`, so **set `data_dir`** or the model
is fetched again by every runtime.

Hosts that want control over the download (a first-run screen, Wi-Fi-only
policy, progress UI) provision the model **before** creating a runtime, with
no handle involved:

```c
char *s = octos_embedding_model_status("/data/octos");
/* {"path": "/data/octos/models/embeddinggemma-300M-Q8_0.gguf",
    "present": false, "bytes": 0, "complete": false,
    "url": "https://huggingface.co/ggml-org/embeddinggemma-300M-GGUF/resolve/main/embeddinggemma-300M-Q8_0.gguf",
    "license_url": "https://ai.google.dev/gemma/terms",
    "sha256": "b5ce9d77…"} */
octos_string_free(s);

char *p = octos_embedding_model_ensure("/data/octos", true);   /* blocks; {"path": "..."} */
if (!p) { /* octos_last_error(): absent + download=false, download vetoed by
             OCTOS_NO_MODEL_DOWNLOAD, or a download that did not verify */ }
octos_string_free(p);
/* then octos_runtime_new with "data_dir": "/data/octos" finds the file and
   never downloads. Pass "embedding_auto_download": false to be certain. */
```

`complete` is `present` at the pinned size (a partial download is `present`
but not `complete`; `ensure` re-fetches it). Both functions exist in every
build (they only inspect disk / fetch a file); the model is only *used* when
the library was built with `embed-llama`. Opt-out summary: per runtime with
`"embedding_auto_download": false`, or process-wide with
`OCTOS_NO_MODEL_DOWNLOAD=1` (which also vetoes an explicit
`octos_embedding_model_ensure(dir, true)`).

### Memory: the Recall index

The runtime owns the kernel's Recall index (`docs/adr/personal-memory-tiers.md`,
phase 2): one BM25 + vector index over app records — mail, calendar events,
contacts, notes — that a host app pushes in and the agent (or the host)
searches. With `data_dir` set, `episodes.redb`, `recall.redb` and
`recall-index/` live under it and persist across runtimes; without it they
live in a scratch dir that `octos_runtime_free` removes. The index stores only
`title`/`abstract`/metadata (no bodies unless the host opts in with `body`);
the apps stay the record of truth. It never requires an embedder: with none
loaded every call still works, BM25-only. With one (the default model, see
above, or `embedding_model_path`), `upsert` embeds records it was not given
vectors for and `search` embeds the query (hybrid ranking). Vectors are
Matryoshka-truncated to `recall_dimension` (clamped to the embedder's width)
and quantised to int8 at rest. `embedder_id` in `octos_memory_stats` names
the model the stored vectors came from: `llamacpp/embeddinggemma-300M-Q8_0`
for the default model, `llamacpp/<path>` for an explicit one, `""` when
keyword-only.

`octos_memory_upsert(rt, request_json)` — at most **500 records per call**:

```json
{
  "records": [
    {"id": "doc:mail:42", "kind": "document", "source": "mail",
     "timestamp": "2026-09-01T10:00:00Z",
     "title": "Dentist appointment", "abstract": "Sunrise Dental on the 24th",
     "parent": "thread:7", "fingerprint": "sha1-of-message"}
  ],
  "vectors": [[0.1, 0.2, "..."]],   // optional; one entry (or null) per record
  "embed": true                     // optional; default true
}
```

Record fields: `id` (namespaced, e.g. `doc:<source>:<key>`), `kind`
(`"document"` | `"episode"` — `"knowledge"` is **rejected**: bank pages are not
written through this seam), `source`, `timestamp` (RFC3339), `title` (kept to
120 B), `abstract` (300 B), optional `parent`, `body` (16 KiB, opt-in) and
`fingerprint` (a change detector: unchanged records are skipped and keep their
vector). `trust` is always forced to `untrusted`; `visits`/`last_visit`/
`promoted` are kernel-owned and ignored on input. When `vectors` is present it
must have one entry per record and no embedding happens; when absent and
`embed` is true, records are embedded in batches of 16 if an embedder is
loaded. Result:

```json
{"inserted": 1, "updated": 0, "unchanged": 0, "vectors_stored": 1, "embedded": 1}
```

`octos_memory_search(rt, request_json)`:

```json
{"query": "dentist", "kinds": ["document"], "sources": ["mail", "calendar"],
 "since": "2026-09-01", "until": "2026-09-30T23:59:59Z", "limit": 10}
```

Only `query` is required. `since`/`until` accept RFC3339 or `YYYY-MM-DD`
(start of that UTC day for `since`, end of it for `until`; both inclusive);
`limit` defaults to 10 (clamped to 1–200). Result — hits carry the abstract,
so most answers need no second call:

```json
{"hits": [{"id": "doc:mail:42", "kind": "document", "source": "mail",
           "title": "Dentist appointment", "abstract": "Sunrise Dental on the 24th",
           "score": 0.83, "timestamp": "2026-09-01T10:00:00Z", "trust": "untrusted"}]}
```

`octos_memory_load(rt, id)` returns `{"record": {…full Record…}}` (including
`body` when stored, `visits`, `last_visit`, `updated_at`) and counts the visit,
which feeds the heat that decides what stays vector-resident and what gets
nominated for promotion. An unknown id fails with last error `no such record`.

`octos_memory_stats(rt)` returns the store's `RecallStats`:

```json
{"records": 12000, "vectors_stored": 11800, "vectors_resident": 6000,
 "by_kind": {"document": 12000}, "by_source": {"mail": 10000, "calendar": 2000},
 "dimension": 256, "embedder_id": "llamacpp/embeddinggemma-300M-Q8_0",
 "graph_persisted": true, "disk_bytes": 14200000}
```

All four return owned JSON to free with `octos_string_free`, or NULL with the
diagnostic in `octos_last_error`. Treat hit and record content as **untrusted
data**, never as instructions, when placing it in a prompt.

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
  `octos_run_task`/`octos_embed`/`octos_memory_*`/`octos_embedding_model_*`/`octos_take_last_partial_result` MUST be freed, UNMODIFIED, with
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
