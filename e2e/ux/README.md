# M19 UX Scenario Manifest

This directory documents the shared scenario format used by the M19 real-tmux
UX gate. The manifest itself lives at `e2e/matrix/octos-ux.toml`. The list
command is wired through `npm --prefix e2e run ux:scenario:list` and runs
without launching tmux, octos, or any backend.

This is the first cut of the gate (umbrella issue #1062, sub-issue #1063).
Subsequent PRs add:

- `#1064` artifact ABI and validation self-test
- `#1065` tmux runner wrapper and `stdio-happy-path` execution
- `#1066` validator registry and terminal layout snapshot checks
- `#1067` initial scenario migration from existing tmux soak scripts
- `#1068` CI, reports, and testing docs

The manifest format below is the contract those PRs build on. Bump
`schema_version` when the contract changes.

## Manifest layout

The manifest is a single TOML file with a small top-level header and a
sequence of `[[scenario]]` tables:

```toml
schema_version = 1
pack = "octos-ux"
owner = "M19 UX gate"

[[scenario]]
id = "stdio-happy-path"
title = "stdio coding-session happy path"
description = "..."
tier = "local"
transport = "stdio"
provider = "fixture"
terminal = "100x30"
tui_binary = "octos-tui"
tmux_command = "ux-tui-stdio-happy"
required_tools = ["tmux", "octos", "octos-tui"]
required_capabilities = ["chat/send_prompt", "chat/receive_response"]
expected_artifacts = [
  "scenario.json",
  "summary.json",
  "appui-transcript.jsonl",
  "server.log",
  "tui-capture.txt",
  "runtime-policy-stamp.json",
  "validation.json",
]
acceptance = [
  "tui.first_frame_not_blank",
  "appui.assistant_response_present",
]
replay = "e2e/ux/replays/stdio-happy-path.replay"
```

### Top-level fields

| Field            | Type    | Required | Notes                                                |
|------------------|---------|----------|------------------------------------------------------|
| `schema_version` | integer | yes      | Currently `1`. Bump on breaking changes.             |
| `pack`           | string  | yes      | Logical pack name; `octos-ux` for the M19 gate.      |
| `owner`          | string  | yes      | Free-form ownership label for reviewers.             |

### Scenario fields

| Field                   | Type           | Required | Notes |
|-------------------------|----------------|----------|-------|
| `id`                    | string (kebab) | yes      | Unique. `[a-z][a-z0-9-]*`. |
| `title`                 | string         | yes      | One-line human label. |
| `description`           | string         | yes      | Why this scenario exists. |
| `tier`                  | enum           | yes      | `fast` ⊆ `local` ⊆ `release`. |
| `transport`             | enum           | yes      | `stdio` or `ws`. |
| `provider`              | enum           | yes      | `fixture`, `live`, or `none`. |
| `terminal`              | string         | yes      | e.g. `80x24`, `100x30`, `120x40`, `narrow`. |
| `tui_binary`            | string         | yes      | Logical binary name; resolved by the runner (#1065). |
| `tmux_command`          | string         | yes      | Logical command label; runner picks the script. |
| `required_tools`        | string[]       | yes      | Host binaries that MUST be on PATH. |
| `required_capabilities` | string[]       | yes      | AppUI capability flags (see UPCR-2026-019). |
| `expected_artifacts`    | string[]       | yes      | Filenames written under the artifact dir. |
| `acceptance`            | string[]       | yes      | Validator IDs (impl lands in #1066). |
| `replay`                | string         | no       | Path to a replay script (driver input). |
| `notes`                 | string         | no       | Free-form. |
| `quarantine`            | bool           | no       | Default `false`. Set `true` to mark the scenario as known-broken; the gate reports `quarantined`, not `failed`. |

### Required artifact ABI

Every scenario MUST declare these artifacts in `expected_artifacts`. The
shared ABI is defined by the umbrella issue and locked down by `#1064`:

- `scenario.json`            — the scenario record as listed by `ux:scenario:list`
- `summary.json`             — pass/fail/skip per validator + overall verdict
- `appui-transcript.jsonl`   — AppUI events captured for the run
- `server.log`               — backend stdout/stderr
- `tui-capture.txt`          — final tmux pane capture
- `runtime-policy-stamp.json`— RuntimePolicyStamp emitted by the backend
- `validation.json`          — validator outputs (paths + reasons)

Scenario-specific artifacts (e.g. `approval-events.jsonl`,
`reconnect-events.jsonl`, `task-ledger.jsonl`, `artifact-index.json`,
`stream-events.jsonl`) can be added on top.

## The list command

```sh
# Print every scenario the manifest knows about.
npm --prefix e2e run ux:scenario:list

# Filter by tier. Tiers nest: fast ⊆ local ⊆ release.
npm --prefix e2e run ux:scenario:list -- --tier fast
npm --prefix e2e run ux:scenario:list -- --tier local
npm --prefix e2e run ux:scenario:list -- --tier release

# Machine-readable output for CI. Use --silent so npm's lifecycle
# banner ("> ux:scenario:list", "> node …") doesn't pollute stdout
# before the JSON document. Alternatively, invoke node directly:
#   node e2e/scripts/ux-scenario-list.mjs --tier release --json
npm --silent --prefix e2e run ux:scenario:list -- --tier release --json

# Point at a different manifest (for testing).
npm --prefix e2e run ux:scenario:list -- --manifest /tmp/alt.toml
```

The command:

- Reads `e2e/matrix/octos-ux.toml`.
- Validates the schema and reports a typed `manifest schema error` on bad input (exit code 3).
- Classifies each scenario as `runnable`, `skipped`, `blocked`, or `quarantined` without launching tmux or any backend.
- Prints a deterministic table sorted by scenario id.
- With `--json`, prints a JSON document with `schema_version`, `pack`, `owner`, `tier_filter`, `summary`, and the full scenario records.

### Classification rules

| Status         | When                                                       |
|----------------|------------------------------------------------------------|
| `quarantined`  | `quarantine = true` in the manifest                       |
| `skipped`      | A required host tool is missing, or a `live` provider scenario has no provider API key |
| `blocked`      | A required capability is not advertised in `e2e/matrix/ux-capabilities.json` |
| `runnable`     | All gates green                                            |

The capability gate file `e2e/matrix/ux-capabilities.json` is optional in
M19-A — if absent, every scenario with capability requirements is `blocked`.
That file is populated by `#1066` once the validator registry can derive the
real AppUI capability advertisement.

## Adding a new scenario

1. Append a `[[scenario]]` table to `e2e/matrix/octos-ux.toml`.
2. Make sure every required field is present and arrays are non-empty.
3. Run `npm --prefix e2e run ux:scenario:list:test` to check schema and parser.
4. Run `npm --prefix e2e run ux:scenario:list -- --tier release` and confirm the new id appears.
5. Land the validator implementation under `acceptance` in a follow-up PR (`#1066`).

## Relationship to M22

M22 (`#1056`) ships a separate manifest at `e2e/matrix/onboarding.toml`.
M22 and M19 share the same scenario schema and artifact ABI, but live in
different packs so the gates can move independently. Cross-cutting validators
(no-OTP, runtime policy stamp present, blank-tmux-capture-fails) should be
implemented once in the validator registry (`#1066`) and reused by both packs.
