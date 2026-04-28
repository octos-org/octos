# Plugin Protocol v2

**Status**: Stable (M8 Runtime Parity)
**Audience**: Plugin authors and host implementors
**Companion code**: `crates/octos-plugin/src/protocol_v2.rs`, host parser at
`crates/octos-agent/src/plugins/tool.rs`

## 1. Goals

Protocol v1 used opaque stderr text-lines for progress and a flat JSON result
on stdout. v2 keeps the wire shape (binary, JSON-on-stdin/stdout, free-form
stderr) but layers a **structured event vocabulary** on top of stderr and an
**enriched result schema** on stdout. Hosts that only understand v1 keep
working: any stderr line that does not parse as a v2 event is treated as a
legacy text-message progress event.

The contract delivers:

1. **Structured progress events** — typed phases the UI can render as a node
   tree without string parsing.
2. **Cost attribution** — per-call provider/token/USD breakdown so the host
   ledger can attribute spend back to the specific plugin invocation.
3. **Result summary** — typed summary that the host's `SubAgentSummaryGenerator`
   can consume without re-running an LLM.
4. **Cancel-signal contract** — graceful SIGTERM with a 10-second cleanup
   budget before SIGKILL.

## 2. Wire format

### 2.1 Invocation (unchanged from v1)

```text
$ ./plugin <tool_name>          # arg = tool name
< stdin: <json args object>
> stdout: <json result object>  # exactly one JSON document
> stderr: <newline-delimited free text OR v2 events>
```

### 2.2 Stderr events (v2)

Each line on stderr is one of:

- A **v2 event**: a JSON object with a top-level `"type"` field, terminated
  by `\n`. Hosts MUST tolerate trailing whitespace and a leading BOM.
- A **legacy text line**: anything else. Hosts treat the entire line (after
  stripping the trailing `\n`) as `ToolProgress { message }`.

Events are JSON objects, never arrays or scalars. Multi-line JSON is **not**
supported — a v2 line is one event, terminated by `\n`. If a plugin needs to
emit a large blob, use `detail.body_truncated` and write the full content to
disk.

Plugins MAY mix v2 events and legacy text lines on the same stream — the
backward-compatibility shim parses each line independently. This is the
intended migration path: a plugin can emit v2 events for the phases that
matter (progress, cost) and keep ad-hoc `eprintln!` debug output as legacy
lines.

### 2.3 Event types

| `type`     | Required fields                                        | Notes                                                           |
|------------|--------------------------------------------------------|-----------------------------------------------------------------|
| `progress` | `stage` (string), `message` (string)                   | `detail` (object) and `progress` (0..1) optional                |
| `cost`    | `provider` (string), `tokens_in` (u32), `tokens_out` (u32) | `usd` (f64) and `model` (string) optional                       |
| `phase`    | `phase` (string), `message` (string)                   | High-level state transition, e.g. `searching` → `synthesizing`  |
| `artifact` | `path` (string), `kind` (string)                       | Side-effect file the plugin produced                            |
| `log`      | `level` (`debug`/`info`/`warn`/`error`), `message`     | Structured wrapper around legacy text logs                      |

Unknown `type` values are tolerated by the parser but logged as legacy text
to the user-visible progress stream.

### 2.4 Stage vocabulary (recommended)

`stage` is a stable, lowercase, snake_case string. The recommended vocabulary:

- `init`, `validating`, `searching`, `fetching`, `crawling`, `chasing`,
  `synthesizing`, `building_report`, `delivering`, `cleanup`, `complete`.

Plugins MAY introduce new stages; hosts SHOULD render unknown stages as-is.

### 2.5 Stdout result (extended)

The v1 keys remain unchanged:

```json
{
  "output": "...",
  "success": true,
  "file_modified": "...",
  "files_to_send": [...]
}
```

v2 adds:

```json
{
  "summary": {
    "kind": "deep_research",
    "headline": "...",
    "confidence": 0.78,
    "sources": [{"url": "...", "title": "...", "cited": true}],
    "extra": { ... }
  },
  "cost": {
    "tokens_in": 1024,
    "tokens_out": 856,
    "usd": 0.0034,
    "provider": "deepseek",
    "model": "deepseek-chat"
  }
}
```

`summary.kind` discriminates the variant. Reserved kinds:

- `deep_research` — used by `deep_search`. Always populated when the
  internal synthesis call succeeds.
- `crawl` — used by `deep_crawl`.
- Plugin-specific kinds may use the prefix `plugin:` (e.g.
  `plugin:mofa_slides:design_phase`).

Both `summary` and `cost` are optional; v1 plugins that do not set them
keep working. The host treats missing `cost` as zero spend (no debit
to the ledger from the plugin call alone).

## 3. Cancellation

### 3.1 Host responsibilities

- The host sends `SIGTERM` to the plugin's direct process (and on Unix, the
  whole process group via `kill -SIGTERM -- -<pgid>`) when the user clicks
  cancel or the supervisor decides to abort the task.
- The host waits **10 seconds** for the plugin to exit cleanly. If the plugin
  has not exited by the deadline, the host sends `SIGKILL` to the process
  group (`kill -KILL -- -<pgid>` on Unix; `taskkill /F /T /PID` on Windows).
- During the 10-second window the host MUST continue draining stdout/stderr
  so the plugin's final `summary` and any farewell `progress` events are
  captured.

### 3.2 Plugin responsibilities

On receipt of `SIGTERM`, plugins SHOULD:

1. Stop scheduling new work (e.g. stop launching new browsers, stop new
   HTTP requests).
2. Cancel in-flight async work via `tokio::select!` against a
   shutdown signal.
3. Tear down owned resources: kill child Chromium processes, close
   temp files, flush partial output to disk.
4. Emit one final `progress` event with `stage = "cleanup"` (best effort).
5. Exit with status code 130 (128 + SIGTERM=2) within the 10-second
   budget.

Plugins that need >10s of cleanup MUST split it: do the urgent cleanup
(kill children, release locks) inside the budget, schedule slow cleanup
(metric flushing, telemetry uploads) on a detached background process,
and exit.

### 3.3 Reference implementation

```rust
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
let mut sigterm = signal(SignalKind::terminate())?;

tokio::spawn(async move {
    sigterm.recv().await;
    let _ = shutdown_tx.send(true);
});

// Inside the workload:
tokio::select! {
    biased;
    _ = shutdown_rx.changed() => {
        cleanup().await;
        // Emit final progress event
        eprintln!(r#"{{"type":"progress","stage":"cleanup","message":"received SIGTERM, exiting"}}"#);
        std::process::exit(130);
    }
    result = real_work() => {
        // ...
    }
}
```

## 4. Backward compatibility

The host parses stderr line-by-line. For each line:

1. Trim trailing `\r\n`/`\n` and any leading BOM.
2. If the trimmed line is empty, ignore it.
3. If the line starts with `{` or `[`, attempt `serde_json::from_str::<ProtocolV2Event>(line)`.
4. On success → dispatch as a structured event.
5. On failure (or non-JSON line) → emit a legacy
   `ToolProgress { message: line }` event.

This means a plugin can be upgraded incrementally — add v2 events for the
hot paths and leave existing `eprintln!` calls in place. Hosts that don't
understand v2 (e.g. third-party tools shelling out) see the JSON as opaque
text, which is forward-compatible.

## 5. Versioning

The `schema_version` of structured events is implicit (the v2 protocol).
If a future v3 changes the event shape it MUST add a `schema_version`
field to v2 events first (with a default of 2) and ship that change at
least one release before v3 events appear in the wild.

## 6. Examples

### 6.1 Progress + cost from a synthesizing plugin

```text
{"type":"progress","stage":"searching","message":"round 1/3","progress":0.10}
{"type":"progress","stage":"fetching","message":"30 pages in parallel","progress":0.45}
{"type":"cost","provider":"deepseek","model":"deepseek-chat","tokens_in":1024,"tokens_out":856,"usd":0.0034}
{"type":"progress","stage":"synthesizing","message":"calling LLM","progress":0.75}
{"type":"progress","stage":"complete","message":"all done","progress":1.0}
```

### 6.2 Mixed v2 + legacy

```text
{"type":"progress","stage":"init","message":"booting headless chrome"}
[chrome] launched on port 9223       <-- legacy text, becomes ToolProgress { message }
{"type":"progress","stage":"crawling","message":"page 5/10","progress":0.5}
```

### 6.3 Final result with summary

```json
{
  "output": "Deep Research: foo\n\n## Synthesis\n\n...",
  "success": true,
  "file_modified": "/tmp/research/foo/_report.md",
  "files_to_send": ["/tmp/research/foo/_report.md"],
  "summary": {
    "kind": "deep_research",
    "headline": "Survey of 5 sources answering 'foo'",
    "confidence": 0.82,
    "sources": [
      {"url": "https://a.example/1", "title": "...", "cited": true},
      {"url": "https://b.example/2", "title": "...", "cited": true}
    ]
  },
  "cost": {
    "provider": "deepseek",
    "model": "deepseek-chat",
    "tokens_in": 4321,
    "tokens_out": 1234,
    "usd": 0.0086
  }
}
```
