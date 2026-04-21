# Octos Harness M4.1A Live Release Gate

Date: 2026-04-21
Issue: [`#474`](https://github.com/octos-org/octos/issues/474)
Milestone: `M4.1A` (Structured Progress Contract)

This document is the release gate for every M4.1A pull request. No M4.1A PR
merges to `main` until the live validation described here passes on the mini
fleet canary.

## What M4.1A promises

The M4.1A milestone replaces stderr-derived runtime truth with structured
`octos.harness.event.v1` events. Five workstreams land together:

- `#470` / `#471`: core ABI + runtime event sink → `task_status`
- `#472`: deep-search emitter
- `#473`: UI / API parent-visible progress replay
- `#474`: live release gate (this doc)
- `#475`: Python / JavaScript bridge helpers

The contract for a merged M4.1A build is:

1. A `deep_research` run emits at least one progress event per required phase.
2. Each event is persisted through `TaskStatusChanged` and exposed by
   `/api/sessions/:id/tasks` and the persistent event SSE stream.
3. The chat header reflects the latest workflow/phase/progress before the
   task completes.
4. Progress survives a session switch (the running task stays on its origin
   session and does not bleed into siblings) and a full browser reload
   (replay must rebuild the same header state from backend truth).
5. `lifecycle_state` transitions are monotonic along the canonical ladder
   `queued → running → verifying → ready` (or `failed`).

## What the gate does NOT assert

- No judgment on the quality of the research output — only on the progress
  pipeline.
- No assertion on non-deep-research workflows; the gate can be extended for
  other long-running flows later.
- No claim that Zenoh (or any other pub/sub backend) is wired up. M4.1A is
  intentionally local-transport only; Zenoh tracking lives in `#476`.

## Mandatory gate run

Supervisor runs this command against the mini fleet canary before accepting
any M4.1A PR to main:

```bash
./scripts/validate-m4-1a-live.sh \
    --base-url https://dspfac.crew.ominix.io \
    --auth-token "$OCTOS_ADMIN_TOKEN" \
    --profile dspfac \
    --output-dir /tmp/m4-1a-live-$(date -u +%Y%m%d-%H%M%S)
```

Re-run against `mini3` host too (swap `--base-url`) to prove the gate works
across two canary hosts.

### Exit codes

| Code | Meaning                                                                          |
|------|----------------------------------------------------------------------------------|
| `0`  | All assertions passed                                                            |
| `1`  | Generic Playwright / npm failure (inspect `--output-dir/playwright/`)            |
| `2`  | Missing prerequisite (jq, curl, `--base-url`, `--auth-token`)                    |
| `3`  | Structured assertion failure — see `diagnostic.json` in `--output-dir`          |
| `4`  | Timeout reached before the deep-research run reported a terminal state          |

On any non-zero exit, the supervisor MUST read `diagnostic.json` and include
the `diagnostic.kind` + `diagnostic.curl_hint` verbatim in the PR comment that
blocks merge.

### Diagnostic schema

`diagnostic.json` always has the following keys:

- `diagnostic.kind` — machine-readable failure code, e.g.
  `required_phase_missing`, `phase_sequence_not_monotonic`,
  `lifecycle_regressed`, `progress_out_of_range`, `duplicate_research_sessions`,
  `cross_session_progress_bleed`, `task_did_not_reach_terminal`,
  `sse_stream_empty`, `playwright_failed`.
- `diagnostic.base_url` — the canary being probed.
- `diagnostic.profile` — the profile id used for the run.
- `diagnostic.detail` — human-readable description of the failure, including
  the expected vs observed values.
- `diagnostic.curl_hint` — a curl command a human can run to reproduce the
  failure signal locally.
- `diagnostic.timestamp` — ISO 8601 UTC timestamp.

## Playwright spec

`e2e/tests/live-progress-gate.spec.ts` exercises the same canary URL through
the browser:

- `deep research emits live progress through every required phase` — polls
  `/api/sessions/:id/tasks` during the run and asserts the required phase
  ladder, monotonic ordering, progress range, and no duplicate tasks.
- `progress state persists across session switch and browser reload` —
  switches to a sibling session, confirms no cross-session bleed, switches
  back, reloads the browser, and asserts the task is still replayable through
  to a terminal state.
- `task API and event SSE stream expose the same phase truth` — captures
  `/api/sessions/:id/events/stream`, verifies at least one event is scoped to
  the current session, and cross-checks the API phase against the stream.

The UI indicator selectors consumed by the spec are authored in
`e2e/fixtures/m4-1a-progress-expected.json#ui_selectors`. Keep these
synchronized with `crates/octos-cli/static/app.js` (the UI-replay surface in
`#473`).

## Fixture

`e2e/fixtures/m4-1a-progress-expected.json` is the authoritative phase
ladder:

| Field                 | Meaning                                                     |
|-----------------------|-------------------------------------------------------------|
| `schema`              | `octos.harness.event.v1` (must match the runtime ABI)       |
| `workflow`            | `deep_research`                                             |
| `phase_order`         | Canonical ladder: search → fetch → synthesize → report_build → completion |
| `required_phases`     | Minimum phases a run MUST emit                              |
| `lifecycle_states`    | Ladder for the coarse public lifecycle                      |
| `terminal_states`     | `ready` and `failed`                                        |
| `active_states`       | `queued`, `running`, `verifying`                            |
| `ui_selectors`        | DOM selectors the UI replay exposes for the chat header     |
| `api_endpoints`       | Canonical routes consumed by the gate                       |
| `prompts.deep_research` | Prompt the gate submits to produce a deep-research run    |
| `limits.*`            | Run bounds (per-run timeout, polling interval, min events)  |

The fixture is the single source of truth shared by the shell script, the
Playwright spec, and the supervisor's PR reviewer. Updating any field requires
updating every consumer in the same PR.

## Running the gate locally

```bash
# 1) From the workspace root, confirm the build compiles:
cargo build --workspace

# 2) Ensure e2e dependencies are installed:
(cd e2e && npm ci)

# 3) Point the gate at a canary (mini1 example):
./scripts/validate-m4-1a-live.sh \
    --base-url https://dspfac.crew.ominix.io \
    --auth-token "$OCTOS_ADMIN_TOKEN" \
    --output-dir /tmp/m4-1a-live-mini1

# 4) Repeat for mini3:
./scripts/validate-m4-1a-live.sh \
    --base-url https://dspfac-mini3.crew.ominix.io \
    --auth-token "$OCTOS_ADMIN_TOKEN" \
    --output-dir /tmp/m4-1a-live-mini3
```

The script is idempotent: two back-to-back runs against the same canary
produce identical decisions. Each run creates its own time-stamped session
label, so repeated runs do not collide.

## Before merging an M4.1A PR

- [ ] `validate-m4-1a-live.sh` exits 0 on mini1
- [ ] `validate-m4-1a-live.sh` exits 0 on mini3
- [ ] `cargo clippy --workspace` is clean
- [ ] No new `unsafe` blocks
- [ ] PR comment links to the `diagnostic.json` (empty or `null`) captured
      during the run
- [ ] Fixture / doc / script / spec versions are pinned to the same SHA that
      shipped the runtime + UI change

## Re-entry / extension rules

- Extending the gate for a new workflow requires a new `phase_order` entry in
  the fixture plus an additional assertion block in the script.
- Adding a required invariant requires both a new `diagnostic.kind` string
  AND a matching spec assertion. Keep the two in the same PR.
- The gate MUST NOT mock the backend. If a future change makes live testing
  impossible, open a separate contract issue — do not silently relax the
  gate.
