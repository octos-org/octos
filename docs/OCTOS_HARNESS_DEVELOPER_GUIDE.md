# Octos Harness Developer Guide

Date: 2026-04-19
Status: developer contract, M4.2 publication

This guide teaches you how to build a custom app that plugs into the Octos
harness without reading runtime internals. It pairs with
[OCTOS_HARNESS_DEVELOPER_INTERFACE.md](./OCTOS_HARNESS_DEVELOPER_INTERFACE.md),
which defines the product position. This document is the concrete "how to
ship" companion.

If the developer interface tells you **why** Octos has a harness, this guide
tells you **exactly what files to write, what fields to use, what fields not
to touch, and how to prove your app is harness-compliant**.

## Who Should Read This

Read this guide if you want to build any of:

- a **report generator** (produces a markdown/PDF file as the final artifact)
- an **audio workflow** (produces a rendered `.mp3`/`.wav` file)
- a **coding assistant** (produces a diff, patch, or a source tree delta)
- any other **single-artifact custom app** where the runtime must deliver a
  known output file to the user at the end of a background task

Do not read this if you are writing a first-party runtime change. Those live
under `crates/octos-agent/src/` and follow different rules.

## What You Get From The Harness

When your app declares itself via a workspace policy, the runtime gives you:

1. **Artifact-owned delivery** — the runtime delivers your declared
   `primary` artifact at the end of a successful background task. No prompt
   hacks. No `send_file` calls. No guessing what to ship.
2. **Validator-gated completion** — a task cannot report "ready" while a
   required validator is failing. If your primary file is missing or too
   small, completion is blocked.
3. **Durable lifecycle state** — the task's `lifecycle_state` field is
   stable across reload, crash, and resume. Your UI/operator surface can
   drive progress from it directly.
4. **Supervised background work** — spawn-only tools are auto-wrapped in
   `tokio::spawn` by the execution loop. You do not cooperate with the LLM
   to keep the task alive.
5. **Explicit hook points** — `before_spawn_verify`, `on_spawn_verify`,
   `on_spawn_complete`, and `on_spawn_failure` give you typed handles into
   the delivery lifecycle without patching the runtime.

If you skip the harness, you lose all of that. "Works sometimes because the
agent remembered" is not a contract.

## The Six Concepts

Every harnessed app maps onto six abstract concepts. If you can answer each
of these questions for your app, you can write the policy.

| # | Concept            | Question                                              |
|---|--------------------|--------------------------------------------------------|
| 1 | Workspace          | Where do my files live?                                |
| 2 | Artifacts          | What is my final output? What is preview? What is source? |
| 3 | Validation         | What must be true before I am considered "done"?      |
| 4 | Background tasks   | Which tools run in the background and need supervision? |
| 5 | Lifecycle hooks    | What do I run at verify/complete/failure?             |
| 6 | Operator truth     | What does the dashboard see when I fail?              |

The rest of this guide maps these concepts onto concrete files, fields, and
tests.

---

## Part 1: Stable vs Internal Contract Fields

Before you write a single policy, you need to know which fields are stable
(promised not to change without notice) and which fields are internal (may
change at any runtime revision).

### STABLE: You MAY depend on these

These fields are part of the harness developer contract. The runtime team
treats breaking these as a regression.

#### Workspace policy file (`.octos-workspace.toml`)

The file path `.octos-workspace.toml` at the workspace root is stable.

Stable top-level TOML tables:

- `[workspace]`
  - `kind` — one of `slides`, `sites`, `session`. (New kinds may be added.)
- `[version_control]`
  - `provider` — one of `git`.
  - `auto_init` — boolean.
  - `trigger` — one of `turn_end`.
  - `fail_on_error` — boolean.
- `[tracking]`
  - `ignore` — array of gitignore-style globs.
- `[validation]`
  - `on_turn_end` — array of action strings.
  - `on_source_change` — array of action strings.
  - `on_completion` — array of action strings.
- `[artifacts]`
  - named keys like `primary`, `entrypoint`, `deck`, `previews`,
    `primary_audio` mapping to a glob string.
  - `primary` is the canonical single-artifact entry. Always declare it.
- `[spawn_tasks.<tool_name>]`
  - `artifact` — string, name of a declared artifact.
  - `artifacts` — array of strings, names of declared artifacts. Prefer this
    over singular `artifact` when the tool produces multiple artifacts.
  - `on_verify` — array of action strings.
  - `on_deliver` — array of action strings.
  - `on_failure` — array of action strings.

#### Stable action strings

Validator and hook action strings documented here are stable. You may
declare them in `on_verify`, `on_turn_end`, `on_source_change`,
`on_completion`, `on_deliver`, `on_failure`:

- `file_exists:<glob>` — at least one file matches the glob pattern.
- `file_size_min:<glob>:<bytes>` — matching files are at least N bytes.
- `file_exists:$<artifact>` — resolve artifact by declared name.
- `file_size_min:$<artifact>:<bytes>` — size check by declared name.
- `notify_user:<message>` — log a notification (informational).

`$artifact`, `$primary`, `$deck`, `$primary_audio`, and any other
`$<name>` substitution refers to the artifact with that key in the
`[artifacts]` table. This is how you write tool-agnostic policies.

#### Task lifecycle states

The `lifecycle_state` field on a task (exposed in
`/api/sessions/:id/tasks`) is stable with these values:

- `queued` — the task was registered but has not started execution yet.
- `running` — the child worker is actively executing.
- `verifying` — execution finished; outputs are being resolved, validated,
  or delivered.
- `ready` — terminal success; outputs are ready for the user-facing surface.
- `failed` — execution or verification failed.

Clients must drive progress UI from `lifecycle_state` and must not invent
new states. If you need a finer-grained view (e.g. distinguishing
"resolving outputs" from "verifying outputs"), use the
`runtime_state` field and treat it as informational only.

#### Hook events

The following hook event names are stable. You may register hooks against
them via the runtime hook config:

- `before_tool_call`
- `after_tool_call`
- `before_llm_call`
- `after_llm_call`
- `on_resume`
- `on_turn_end`
- `before_spawn_verify` — blocking pre-delivery hook for successful child
  sessions.
- `on_spawn_verify` — observer after BeforeSpawnVerify resolves.
- `on_spawn_complete` — observer after the task is marked Completed.
- `on_spawn_failure` — observer after the task is marked Failed.

#### `before_spawn_verify` semantics

The blocking pre-delivery hook contract (see
[developer interface](./OCTOS_HARNESS_DEVELOPER_INTERFACE.md#beforespawnverify-semantics)):

- **allow** — return success with no body: runtime keeps its selected
  `output_files`.
- **modify** — return a JSON string array, or `{"output_files":[...]}`
  with absolute or workspace-relative paths.
- **deny** — exit non-zero: runtime marks the task as a terminal failure.
- **hook error** — log the error; runtime continues with its own selection.

Implementations must be idempotent. A hook that runs twice must produce the
same final `output_files` for the same inputs.

#### Task API response (`/api/sessions/:id/tasks`)

Stable top-level task fields:

- `id`
- `tool_name`
- `status` (one of `spawned`, `running`, `completed`, `failed`)
- `lifecycle_state` (see above)
- `started_at`, `updated_at`, `completed_at`
- `output_files` (array of absolute paths)
- `error` (string, present on failure)

### INTERNAL: You MAY NOT depend on these

These fields exist today in the runtime. They are **not** part of the
developer contract. Do not build your app around them. They may change,
move, rename, or disappear without notice.

- `runtime_state` internal values like `executing_tool`, `resolving_outputs`,
  `cleaning_up`. Read-only for diagnostics. Not a UI driver.
- `runtime_detail` free-form string. Diagnostic only.
- `task_ledger_path` — internal persistence detail.
- `child_session_key` — internal supervisor bookkeeping.
- `child_join_state`, `child_terminal_state`, `child_failure_action` —
  supervisor-facing, not developer-facing.
- `TaskSupervisor`, `WorkspacePolicy`, `BackgroundTask` Rust types — these
  are internal runtime types. You interact with the harness through the
  `.octos-workspace.toml` file, the manifest file, hook event names, and
  the stable task API. Do not depend on the Rust types directly from a
  third-party crate.
- `crates/octos-agent/src/workspace_contract.rs` implementation — the
  contract enforcement algorithm is internal. Your policy declares what
  must be true; the runtime decides how to check it.
- Any field prefixed with `internal_` or `_` anywhere in the task JSON is
  runtime-private.
- The exact file format of `~/.octos/` (profile, auth, episode, session
  storage). Config hot-reload and format migrations are runtime concerns.
- The `progress event sink` (`OCTOS_EVENT_SINK` and
  `octos.harness.event.v1`) is **forthcoming in M4.1A**. Until M4.1A
  lands, treat progress events as best-effort stderr. Structured progress
  event schemas are promised to land, but the exact transport URI shape is
  not yet stable.

### Deprecated but still accepted

- `artifact` (singular) on a spawn task — use `artifacts` (array) instead.
  Singular `artifact` remains accepted for backward compatibility.
- `on_complete` — use `on_deliver` instead. Legacy name still accepted.

If you are starting a new policy today, use only the non-deprecated field
names. Migrations are our problem if we ever rename them.

---

## Part 2: Your First Harnessed App

This part walks you through building a minimal harnessed app: a report
generator that writes a markdown file and has the runtime deliver it.

### Step 1: Choose a workspace kind

Your app runs inside a workspace directory. For single-artifact apps
without source tracking requirements, use `session`:

```toml
[workspace]
kind = "session"
```

`session` is the right choice when:
- the workspace is ephemeral per conversation
- you do not need a `git init` in the workspace
- your app produces a single deliverable

Use `slides` or `sites` when you own a specific project-kind workflow that
matches those templates. Most custom apps will be `session`.

### Step 2: Declare your workspace policy

Write `.octos-workspace.toml` in your workspace root:

```toml
[workspace]
kind = "session"

[version_control]
provider = "git"
auto_init = false
trigger = "turn_end"
fail_on_error = false

[tracking]
ignore = ["tmp/**", ".DS_Store"]

[artifacts]
primary = "reports/*.md"

[spawn_tasks.generate_report]
artifact = "primary"
on_verify = [
    "file_exists:$artifact",
    "file_size_min:$artifact:256",
]
on_failure = ["notify_user:Report generation failed"]
```

What each piece does:

- `[artifacts].primary = "reports/*.md"` tells the runtime: the final
  output is any markdown file under `reports/`.
- `[spawn_tasks.generate_report]` binds the spawn-only tool named
  `generate_report` to the `primary` artifact.
- `on_verify` runs after the child session finishes but before the task is
  marked ready. The runtime refuses to deliver if validators fail.
- `on_failure` actions run if anything in the lifecycle fails.

### Step 3: Declare your tool in a plugin manifest

Write `manifest.json`:

```json
{
  "name": "harness-starter-report",
  "version": "1.0.0",
  "author": "you",
  "description": "Generate a markdown report from a prompt.",
  "timeout_secs": 60,
  "tools": [
    {
      "name": "generate_report",
      "description": "Generate a markdown report. Writes to reports/<slug>.md.",
      "spawn_only": true,
      "input_schema": {
        "type": "object",
        "properties": {
          "topic": {"type": "string", "description": "Report topic"}
        },
        "required": ["topic"]
      }
    }
  ]
}
```

Key fields:

- `name` — plugin identifier (kebab-case).
- `tools[].spawn_only: true` — tells the runtime to intercept this tool
  call and wrap it in `tokio::spawn`. No LLM cooperation needed.
- `tools[].name` — must match the spawn_tasks key in your workspace
  policy.

### Step 4: Implement the tool binary

The plugin binary protocol is: `./skill_binary <tool_name>` with JSON on
stdin, JSON on stdout:

```rust
// src/main.rs — minimal shape
use std::io::Read;
use serde_json::{json, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tool_name = args.get(1).map(String::as_str).unwrap_or("");
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).expect("stdin");
    let input: Value = serde_json::from_str(&raw).expect("json");

    match tool_name {
        "generate_report" => generate(&input),
        other => fail(&format!("unknown tool '{other}'")),
    }
}

fn generate(input: &Value) {
    let topic = input["topic"].as_str().unwrap_or("untitled");
    let slug = slugify(topic);
    let path = format!("reports/{slug}.md");
    std::fs::create_dir_all("reports").expect("mkdir");
    std::fs::write(&path, format!("# {topic}\n\nReport body goes here.\n"))
        .expect("write");
    println!("{}", json!({
        "success": true,
        "output": format!("Wrote {path}"),
        "files_to_send": [path]
    }));
}

fn fail(msg: &str) {
    println!("{}", json!({"success": false, "output": msg}));
    std::process::exit(1);
}

fn slugify(s: &str) -> String {
    s.chars().filter_map(|c| match c {
        'A'..='Z' | 'a'..='z' | '0'..='9' => Some(c.to_ascii_lowercase()),
        ' ' | '-' | '_' => Some('-'),
        _ => None,
    }).collect()
}
```

Notes:

- The binary's output must be a single JSON object on stdout. Log to stderr
  if you want diagnostics.
- `files_to_send` is a hint to the runtime, but the workspace policy is
  the source of truth. The runtime will resolve `primary` from your policy
  even if the hint is missing or stale.
- Exit code semantics: non-zero means failure. The runtime records
  `on_failure` actions and marks the task `failed`.

### Step 5: Prove it with a smoke test

Every starter in this guide has a `cargo test` smoke test that asserts:

1. the plugin manifest parses via `octos_plugin::PluginManifest::from_file`.
2. the workspace policy parses via `toml::from_str::<WorkspacePolicy>()`.
3. at least one declared artifact is produced under a fake run.
4. `lifecycle_state` transitions `Queued → Running → Verifying → Ready` via
   a `BackgroundTask` fixture.

Use the starter crates in `crates/app-skills/harness-starter-*` as a
copyable template. Each smoke test runs under `cargo test -p <crate>` with
no API keys, no network, and no external dependencies.

---

## Part 3: Artifact Roles

Artifacts are named in the `[artifacts]` table of the workspace policy.
Roles are conventional: the runtime uses the name `primary` as the
canonical delivery path, and other names as supporting metadata.

### Required role: `primary`

Every harnessed app **must** declare a `primary` artifact. The runtime
treats this as the single canonical deliverable. If your app ships
multiple files, choose the one a user would reasonably call "the output"
and mark it `primary`.

Examples from first-party apps:

- slides: `primary = "output/deck.pptx"`
- sites: `primary = "dist/index.html"`
- audio: `primary_audio = "*.mp3"` (and also `primary = "..."` if a
  specific file is canonical)

### Conventional roles

Use these conventional names to keep policies readable across apps:

- `primary` — the main output (required).
- `preview` or `previews` — a user-visible preview of the primary output
  (slide thumbnails, site screenshots, first page of a PDF).
- `source` — the authored input before rendering. You may track and version
  sources without shipping them.
- `entrypoint` — for sites, the HTML entrypoint.
- `deck` — for slides, the pptx path (usually equal to `primary`).
- `primary_audio` / `podcast_audio` — for audio apps.

### Roles you invent

You may invent new artifact names. The runtime does not enforce a fixed
vocabulary. But:

- A third-party tool that reads your workspace cannot find a role it does
  not know about.
- The dashboard and operator surface only auto-display `primary`.

If you name something `weekly_summary_appendix_b`, make sure your own
validators reference it (`file_exists:$weekly_summary_appendix_b`). Do not
rely on the dashboard to surface arbitrary custom names.

### Glob semantics

Artifact patterns are gitignore-compatible globs, evaluated relative to the
workspace root:

- `reports/*.md` — any `.md` file directly under `reports/`.
- `output/**/slide-*.png` — any png starting with `slide-` anywhere under
  `output/`.
- `*.mp3` — any mp3 at the workspace root.

The runtime prefers files modified after the task started. If no such
files exist, it falls back to any existing match. If no files match at all,
the validator fails and the task does not reach `ready`.

---

## Part 4: Validators

Validators are the "what must be true before I ship" contract. They run at
three lifecycle points.

### `on_turn_end` — cheap, every turn

Runs after every agent turn (target < 100ms total). Use this for tier-1
checks: source file existence, manifest integrity, obvious corruption.

Example:
```toml
[validation]
on_turn_end = [
    "file_exists:memory.md",
    "file_exists:changelog.md",
]
```

Do **not** put expensive checks here. Every turn runs these.

### `on_source_change` — medium, source-guarded

Runs when a source file is modified. Use for tier-2 checks: preview render,
incremental build verification (target 1-5s).

Example:
```toml
[validation]
on_source_change = [
    "file_exists:build/preview.png",
]
```

The runtime currently triggers `on_source_change` at the same boundary as
`on_turn_end` when source files changed; treat it as "medium-cost checks
only when inputs change".

### `on_completion` — expensive, terminal

Runs when the app claims it is done, before marking the task `ready`.
Target 10-30s for expensive checks: Playwright smoke, full test suite, PDF
render validation.

Example:
```toml
[validation]
on_completion = [
    "file_exists:output/deck.pptx",
    "file_exists:output/**/slide-*.png",
]
```

If any `on_completion` action fails, the runtime refuses to mark the task
`ready`. This is the hard gate.

### Per-spawn-task validators

The `[spawn_tasks.<tool>].on_verify` list is the **pre-delivery** validator
set for a specific background tool. Prefer this over `on_completion` when
validation is tool-specific.

Example:
```toml
[spawn_tasks.render_audio]
artifact = "primary_audio"
on_verify = [
    "file_exists:$artifact",
    "file_size_min:$artifact:4096",
]
```

`$artifact` resolves to the artifact named in `artifact` or any entry in
`artifacts`. For multi-artifact tasks, `$<name>` resolves each named entry
independently.

### Internal-only validator fields

Do **not** depend on:

- the exact stdout/stderr format of validator action evaluation
- the internal `ActionContext` struct
- the return order of globbed files
- whether validators run serially or in parallel (currently serial; may
  change)

Your policy declares **what** must be true. The runtime decides **how** to
check it.

---

## Part 5: Background Task Contracts

Background tasks are declared via spawn-only tools. Every spawn-only tool
should have a matching `[spawn_tasks.<tool_name>]` entry in your
workspace policy. That entry is your contract for "when this tool finishes,
here is the artifact to ship and here are the checks to run".

### Declaration

```toml
[spawn_tasks.podcast_generate]
artifacts = ["podcast_audio"]        # prefer plural for new policies
on_verify = [
    "file_exists:$podcast_audio",
    "file_size_min:$podcast_audio:4096",
]
on_deliver = []
on_failure = ["notify_user:Podcast generation failed"]
```

### Lifecycle (what the runtime does)

1. User prompt triggers the LLM to call `podcast_generate`.
2. Runtime sees `spawn_only: true` in the manifest, wraps in
   `tokio::spawn`, and returns a task id immediately. `lifecycle_state` is
   `queued`, then `running`.
3. Child session executes. stdout produces a JSON result. Files land in the
   workspace.
4. Runtime reaches `verifying`. It resolves artifacts from the policy,
   applies `BeforeSpawnVerify` hook if registered, runs `on_verify`
   actions.
5. If `on_verify` passes, runtime runs `on_deliver` actions. Default
   delivery is "mark the resolved files as deliverable to the parent
   session". Explicit `on_deliver` entries (e.g. notify, custom hooks) run
   after that.
6. If all pass, runtime marks `lifecycle_state = ready` and sends the
   artifacts back to the parent conversation.
7. If any step fails, runtime runs `on_failure`, marks `failed`.

### Resume behavior

Tasks are durable. If the runtime restarts while a task is `running`:

- the `task_ledger_path` (internal) replays the task state
- the external `lifecycle_state` resumes where it left off
- `on_resume` hooks fire so observers can react

You do not need to implement resume logic in your tool. You do need to
make sure your tool's outputs are deterministic enough that re-running it
from scratch produces the same artifact paths.

### When to NOT use spawn-only

Use a synchronous tool (`spawn_only: false` or absent) when:

- the tool finishes in under 5 seconds
- the result fits in a single LLM reply
- the LLM needs the output to make the next decision

Use spawn-only when:

- the tool takes > 10 seconds
- the result is a file the user wants to download
- you want `lifecycle_state` tracking

---

## Part 6: Hooks

Hooks are shell commands the runtime runs at lifecycle events. They are
configured outside the workspace policy, in the runtime hook config (see
`octos-agent/src/hooks.rs` for the wiring; the stable surface is the hook
event names, not the Rust types).

### Useful events for custom apps

- `before_spawn_verify` — your chance to replace or reject the runtime's
  selected `output_files` before delivery. Use this when your artifact
  path is computed at runtime (not statically declarable in the policy).
- `on_spawn_verify` — observer after verification resolves.
- `on_spawn_complete` — observer after the task is marked `Completed`.
  Great for custom delivery (email attachment, webhook, analytics).
- `on_spawn_failure` — observer after the task is marked `Failed`. Log,
  alert, or kick off a compensating workflow.

### Hook protocol

The runtime invokes your hook as an argv array (no shell interpretation)
with a JSON payload on stdin. Exit code semantics:

- `0` — allow (for before-hooks) or acknowledge.
- `1` — deny (for before-hooks) or report handled failure.
- `2+` — hook error; runtime logs and continues with its own decision.

Stdout is captured. For `before_spawn_verify`, the runtime interprets
stdout as either:

- a JSON array of strings: `["path/one", "path/two"]`
- a JSON object: `{"output_files": ["path/one", "path/two"]}`

### Environment sanitization

The runtime strips a fixed list of sensitive env vars from your hook
process (`BLOCKED_ENV_VARS`). Do not expect `LD_PRELOAD`, `DYLD_*`,
`NODE_OPTIONS`, etc. to be present. If your hook needs credentials, declare
them explicitly in the hook config's env allowlist.

### Circuit breaker

If your hook fails 3 times consecutively, it is auto-disabled for the
session. Make hooks robust and fast. Target < 1 second.

---

## Part 7: Progress Events (forthcoming)

The `OCTOS_EVENT_SINK` transport and the `octos.harness.event.v1` structured
progress schema are defined in
[OCTOS_HARNESS_M4_WORKSTREAMS_2026-04-21.md](./OCTOS_HARNESS_M4_WORKSTREAMS_2026-04-21.md)
under M4.1A. They are **not** yet implemented as of this guide's
publication.

Until M4.1A lands:

- emit human-readable progress to **stderr**. It is captured as diagnostic
  output.
- do **not** try to bridge into parent task status from your tool; that
  bridge does not exist yet.
- when M4.1A ships, this section will point to the emitter helpers and
  the structured event shape.

The structured event shape (to be honored once available) is:

```json
{
  "schema": "octos.harness.event.v1",
  "kind": "progress",
  "session_id": "...",
  "task_id": "...",
  "workflow": "your_app",
  "phase": "rendering",
  "message": "Rendered 3/5 pages",
  "progress": 0.6
}
```

---

## Part 8: Delivery Expectations

This section is the contract between your app and "what ends up in front of
the user".

### What the runtime guarantees

When `lifecycle_state` reaches `ready`:

1. The file at `[artifacts].primary` exists and passed `on_verify`.
2. The file is attached to the parent session's reply (the exact transport
   depends on the channel — chat UI, Telegram bot, etc.).
3. The task API returns the absolute path in `output_files[0]`.
4. The operator dashboard (forthcoming, M4.5) will show the artifact as
   delivered.

### What the runtime does not do

- Rename or transform your artifact file.
- Compress, zip, or re-encode audio.
- Check content quality (we check existence and minimum size, not
  semantics).
- Guarantee delivery to every channel. Some channels have file size limits.
  If your artifact is > 50 MB, expect delivery failures in some channels.

### When delivery fails

If the runtime cannot deliver the artifact (channel rejection, file too
large, filesystem error), the task still reaches `ready` because the
artifact was produced and validated. The **delivery** failure is separate
from task success. Check the operator dashboard for per-channel delivery
status.

---

## Part 9: Starter Apps

Four starter apps live under `crates/app-skills/`. Copy the one closest to
your use case and adapt.

### `harness-starter-generic`

A minimal single-artifact app. Useful as a reference for the smallest legal
harnessed skill. Declares:

- one `primary` artifact
- one `on_verify` file-exists check
- no hooks

### `harness-starter-report`

A markdown report generator. Demonstrates:

- `primary = "reports/*.md"`
- size-based validator (`file_size_min`)
- `on_failure` notification

### `harness-starter-audio`

An audio file generator (synthesizes a tiny WAV via a pure-Rust helper, no
external synth). Demonstrates:

- `primary_audio = "*.wav"`
- multi-validator contract (existence + minimum size)
- `on_failure` notification

### `harness-starter-coding`

A coding-assistant-style starter that produces a diff artifact. Demonstrates:

- `primary = "patches/*.diff"`
- validator using `file_size_min` for non-empty patches
- `preview` artifact (the changed file list)

Each starter crate has:

```
crates/app-skills/harness-starter-<name>/
├── Cargo.toml
├── manifest.json
├── SKILL.md
├── workspace-policy.toml    # example policy; copy to workspace root
└── src/
    ├── lib.rs               # tool logic + smoke tests
    └── main.rs              # thin CLI binary
```

Run the smoke test:

```bash
cargo test -p harness-starter-report
cargo test -p harness-starter-audio
cargo test -p harness-starter-coding
cargo test -p harness-starter-generic
```

No API keys, no network, no external deps.

---

## Part 10: Checklist For Shipping A Custom App

Before you ship, confirm each of the following:

- [ ] Workspace policy file `.octos-workspace.toml` exists at the
  workspace root.
- [ ] `[workspace].kind` is one of `slides`, `sites`, `session`.
- [ ] `[artifacts].primary` is declared as a glob pattern.
- [ ] Every spawn-only tool in your manifest has a matching
  `[spawn_tasks.<tool_name>]` block.
- [ ] Every `[spawn_tasks.*]` block declares either `artifact` or
  `artifacts`.
- [ ] Each spawn-task contract has at least one `on_verify` entry.
- [ ] `on_failure` includes a `notify_user` action with an actionable
  message.
- [ ] Your tool binary writes its output to a path matching the artifact
  glob.
- [ ] Your tool binary exits non-zero on any internal failure.
- [ ] Your manifest sets `spawn_only: true` on background tools.
- [ ] You have a smoke test asserting: manifest parses, policy parses,
  artifact is produced, `lifecycle_state` transitions are sensible.
- [ ] You do not depend on any field listed as **INTERNAL** above.
- [ ] Your `SKILL.md` has a front-matter block with `name`,
  `description`, `version`, and `author` — per the app-skill dev guide.

If all of those are true, your app is harness-compliant.

---

## Part 11: Debugging

### "Task is stuck in verifying"

The most common cause: a validator never passes. Check the task API for
`error` and inspect the logs for `file_exists check failed` or
`file_size_min check failed`. Confirm:

- your artifact glob matches a real file path relative to the workspace
  root
- the file was written **after** the task started (runtime prefers
  recently-modified files)
- the file size exceeds the `file_size_min` threshold

### "Task is ready but no artifact was delivered"

Check:

- did the channel accept the file? (some channels have size limits)
- is the file path in `output_files[0]`?
- did your `on_deliver` hook fail? (`on_deliver` failures mark the task
  as failed; the operator dashboard shows the hook error)

### "Tool runs but lifecycle_state never changes"

Check:

- did you set `spawn_only: true` in the manifest?
- is the tool name in the manifest matching the `[spawn_tasks.<name>]`
  key exactly?
- is your binary actually writing JSON on stdout? (stderr-only output is
  treated as no result)

### "Policy file is not loaded"

The runtime reads `.octos-workspace.toml` from the workspace root, which
for a `session` kind is the active conversation's data directory. Confirm
the file is present with `ls -la .octos-workspace.toml` from the workspace
root. If you are running via `octos chat`, the workspace root is the
current working directory.

### "Changes to policy do not take effect"

Policy changes take effect for new spawn-task runs. In-flight tasks use
the policy snapshot from when they started. Restart the runtime to pick
up changes across all sessions.

---

## Part 12: What Comes Next

This guide captures M4.2's contract. Future workstreams tighten the
surface further:

- **M4.1A** (`#464`) — structured progress event ABI and the
  `OCTOS_EVENT_SINK` transport. Once landed, this guide gains a full
  Part 7 with emitter examples.
- **M4.3** (`#466`) — typed validator runner with per-validator status,
  duration, reason, and replayable evidence path. Today's validators are
  pass/fail; M4.3 adds a richer result shape.
- **M4.4** (`#467`) — third-party skill compatibility gate. Once landed,
  every starter in this guide will also be run through a live install/run/
  reload/remove cycle as part of CI.
- **M4.5** (`#468`) — operator dashboard. Once landed, this guide gains a
  screenshot-annotated section on what the operator sees.
- **M4.6** (`#469`) — explicit schema versioning. All stable fields
  listed in Part 1 will carry an explicit `schema_version`, and this
  guide will document the compatibility promise and deprecation process.

Until those land, the contract is **exactly** the set of stable fields
in Part 1 and the starter apps in Part 9. If you stay inside that box,
your app is portable across future runtime revisions.

---

## Cross References

- [Octos Harness Developer Interface](./OCTOS_HARNESS_DEVELOPER_INTERFACE.md)
  — product position and conceptual overview.
- [Octos Harness M4 Workstreams](./OCTOS_HARNESS_M4_WORKSTREAMS_2026-04-21.md)
  — roadmap that this guide implements for M4.2.
- [App Skill Dev Guide (English)](./app-skill-dev-guide.md) — general app
  skill plugin authoring guide (non-harnessed).
- [App Skill Dev Guide (Chinese)](./app-skill-dev-guide-zh.md) — Chinese
  translation of the app skill dev guide.
- [Architecture](./ARCHITECTURE.md) — full crate layout and flow diagrams.

## Stability Promise

The stable fields enumerated in Part 1 are promised not to break across
runtime revisions until M4.6 introduces explicit schema versions. At that
point, this guide will document the compatibility and deprecation rules.

Until M4.6, if you build against only the stable fields here, your custom
app should continue to work across every Octos runtime release.

If you need to depend on something in Part 1's **INTERNAL** list, open an
issue on the repo asking for that field to be promoted to stable. Do not
silently build against internal types; the runtime team will rename them
without notice.
