# M8 Runtime Parity — Full Scope Implementation PRD

**Status**: Draft
**Owner**: ymote
**Date**: 2026-04-26
**Tracking**: GitHub umbrella issue (TBD on PR)
**Baseline**: `main` at `889e5e05`

## 1. Goal

Bring all background-work paths (`run_pipeline`, spawn-subagent, `deep_search`, `deep_crawl`, slides delivery, site delivery, mofa podcast) to first-class M8 runtime peer status — same lifecycle/observability/recovery contract as the main session actor. End state: structured per-node observability, inline progress in chat, mid-flight cancellation, restart-from-failed-node recovery, per-node cost attribution, and curated plugin output.

## 2. Background

Today's runtime gap (verified by audit `afee1b1000975fcfd` and live inspection of mini2 logs):

- `run_pipeline` workers and spawn-subagent children **lack** FileStateCache (M8.4), SubAgentOutputRouter / SubAgentSummaryGenerator (M8.7), `build_recovery_prompt` (M8.9), and per-node cost-reservation handles. They register no per-node task in `task_query_store`, so the admin dashboard never sees the substructure.
- The chat UI sees only an opaque "running run_pipeline" pill; users wait 5–15 min with zero feedback even though the backend is faithfully emitting tool_progress events (now with `tool_call_id` after `889e5e05`).
- `deep_search` returns raw Bing dumps, not synthesized answers; `deep_crawl` spawns up to 6 concurrent Chromiums per call with no resource cap and no clean SIGTERM cancellation.
- Plugin protocol is text-stderr-progress + JSON-stdout-result; no structured progress, no cost attribution, no graceful cancel contract.
- M7.9 PM supervisor (kill/steer/relaunch) is implemented but not exposed via API or UI.

The migration to `main` (yellow-tree force-push, `c8787472`) and the in-session fixes (`7d973de9` M8.6 fresh-skip, `99edfe5d` pipeline auto-send suppress, `a7b975a3` clear_spawn_only, `9687fa95` SSE tool_call_id + supervisor bridge, `889e5e05` stream_reporter parity) restore production correctness. This PRD describes the **next step**: turning the post-migration runtime into a production-grade observable system.

## 3. Architecture

### 3.1 Component map

```
┌──────────────────────────────────────────────────────────────────────┐
│  Session actor                                                       │
│  • FileStateCache, SubAgentOutputRouter, SubAgentSummaryGenerator    │
│  • CompactionRunner, M8.9 build_recovery_prompt                      │
│  • TaskSupervisor + task_query_store + ProgressReporter (B2 bridge)  │
└──────┬───────────────────────────────────────────────────────────────┘
       │
       ├─ tool: run_pipeline (sync)
       │     │
       │     ▼
       │  ┌────────────────────────────────────────────────────┐
       │  │  Pipeline executor                                  │
       │  │  Worker agent per node:                             │
       │  │  • [GAP] FileStateCache    [GAP] SubAgentOutputRouter │
       │  │  • [GAP] SummaryGenerator  [GAP] M8.9 recovery       │
       │  │  • [GAP] task_query_store  [GAP] cost reservation    │
       │  │  • plugins: deep_search, deep_crawl, ...            │
       │  └────────────────────────────────────────────────────┘
       │
       └─ tool: spawn / Deep research (subagent)
             │
             ▼
          ┌─────────────────────────────────────────────────────┐
          │  Spawn child agent (slides / site / podcast)         │
          │  • TaskSupervisor: ✅                                │
          │  • CompactionRunner: ✅                              │
          │  • [GAP] FileStateCache    [GAP] Router/Summary      │
          │  • [GAP] M8.9 recovery                               │
          │  • plugins: mofa_slides, podcast_generate, fm_tts    │
          └─────────────────────────────────────────────────────┘
```

### 3.2 Plugin protocol v2 contract (incremental)

v1 (today):
- `./plugin <tool> <stdin-args-json>` → exit code + stdout-result-json + stderr-text-lines
- Stderr lines → `ToolProgress { name, tool_id, message }` events

v2 (this work):
- Same invocation; stdout result gains optional fields.
- Stderr events MAY be JSON-structured (one event per line, validated against schema): `{"type":"progress","stage":"searching","detail":{...}}` `{"type":"cost","provider":"...","tokens_in":N,"tokens_out":N,"usd":F}`. Backwards compatible — text lines without `{` prefix become legacy `message:` events.
- New `cancel_signal: "SIGTERM"` contract: plugin must ack cleanup within 10s of SIGTERM, then exit. Host kills with SIGKILL after timeout.
- Output result JSON gains optional `summary: { kind: "...", ... }` field for AgentSummaryGenerator.

### 3.3 Data contract for per-node observability

Every supervised task (pipeline node, spawn child, plugin invocation) carries:
- `task_id` (UUID v7, supervisor-assigned)
- `parent_task_id` (the enclosing task or session)
- `tool_call_id` (the invocation that started it)
- `lifecycle_state` (`Running`, `DeliveringOutputs`, `Completed`, `Failed`)
- `runtime_state` (sub-states the supervisor tracks)
- `started_at`, `updated_at`, `completed_at`
- `output_files: Vec<PathBuf>`
- `summary: Option<TypedSummary>` (when set by SubAgentSummaryGenerator)
- `cost: Option<{ tokens_in, tokens_out, usd_used, usd_reserved }>`

The frontend renders a tree of these via the existing `tool_progress` SSE channel (now carrying `tool_call_id`). Every state transition triggers a B2 bridge event the UI consumes.

### 3.4 PM supervisor (M7.9) exposure

Already implemented:
- `TaskSupervisor::cancel(task_id)` — sets cancellation atomic, sends SIGTERM to plugin processes
- `TaskSupervisor::relaunch(task_id, opts)` — restarts a failed task
- Steering primitive (relaunch with adjusted prompt)

Needs:
- HTTP API: `POST /api/tasks/{task_id}/cancel`, `POST /api/tasks/{task_id}/restart-from-node`
- UI buttons wired to those endpoints

## 4. Tracks (parallel work units)

Four engineers / agent workers run in parallel. Each track owns a clean slice with minimal cross-track dependencies. Coordination happens via the umbrella issue and shared baseline branch.

### Track 1 (W1) — Pipeline host + frontend node tree (G1, G4)

**Branch**: `feature/m8-parity-w1-pipeline-host`

**Files**:
- `crates/octos-pipeline/src/handler.rs` — pipeline worker construction (~415 line region)
- `crates/octos-pipeline/src/executor.rs` — node execution + recovery loop
- `crates/octos-pipeline/src/lib.rs` — exports
- New: `crates/octos-pipeline/src/recovery.rs` — M8.9 recovery wrapper for nodes
- `octos-web/src/store/message-store.ts` — node tree types
- `octos-web/src/store/message-store-reducers/tool-progress-reducer.ts` — node-tree projection
- `octos-web/src/components/chat-thread.tsx` — `<NodeCard>` component
- New: `octos-web/src/components/node-card.tsx`
- New: `octos-web/src/components/cost-breakdown.tsx`

**Deliverables**:
- A1 — Pipeline workers wire `with_file_state_cache`, `with_subagent_output_router`, `with_subagent_summary_generator` from session-actor's shared instances (plumbed via `TOOL_CTX`)
- A2 — Pipeline node execution wrapped with M8.9 recovery loop: on retryable failure, build recovery prompt, retry once before bubbling up
- A3 — Each pipeline node registers a child task in `task_query_store` with `parent_task_id` = the run_pipeline tool_call_id; emits state transitions via the B2 bridge
- A4 — Per-node `CostReservationHandle` from the session's CostAccountant; commit on completion; surface in `PipelineResult.node_costs`
- G1 — Frontend `<NodeCard>` component renders the node tree under the `run_pipeline` tool-call pill. Status badge, elapsed timer, click-to-expand sub-timeline
- G4 — Frontend `<CostBreakdown>` panel: aggregate footer + sortable per-node table

**Acceptance criteria**:
- New unit test: pipeline worker has FileStateCache attached (assert via `Agent::file_state_cache()` accessor)
- New unit test: pipeline worker registers task with task_query_store on node start
- New unit test: M8.9 recovery fires on simulated retryable failure; assert agent re-engaged with build_recovery_prompt
- New unit test: per-node cost reservation handle is created and committed
- E2E test (`live-pipeline-cards.spec.ts`): trigger run_pipeline; assert N node cards appear under the bubble; assert per-node status transitions visible
- E2E test (`live-pipeline-cost.spec.ts`): trigger run_pipeline; assert cost breakdown panel renders with per-node rows

### Track 2 (W2) — Spawn host + workflows + frontend cancel/restart (G2, G3) + API

**Branch**: `feature/m8-parity-w2-spawn-host`

**Files**:
- `crates/octos-agent/src/tools/spawn.rs` — spawn child Agent::new builder chain (~1898 line region)
- `crates/octos-cli/src/workflows/slides_delivery.rs` — per-phase validator wiring
- `crates/octos-cli/src/workflows/site_delivery.rs` — same
- `crates/octos-cli/src/workflows/research_podcast.rs` — same
- `crates/octos-cli/src/api/handlers.rs` — new endpoints
- `crates/octos-cli/src/api/router.rs` — route registration
- `octos-web/src/components/node-card.tsx` (collaborate with W1) — cancel + restart buttons
- `octos-web/src/api/types.ts` — new POST request types

**Deliverables**:
- B1 — Spawn child agent wires `with_file_state_cache(parent.file_state_cache.clone())`, `with_subagent_output_router(parent.router.clone())`, `with_subagent_summary_generator(...)` — all inherited from parent session
- B2 — Spawn task wraps `run_task` with M8.9 recovery loop (mirror of session_actor's pattern)
- E — slides/site/podcast workflows surface per-phase validator output to assistant message ("✓ design phase: validators passed (5/5)")
- API1 — `POST /api/tasks/{task_id}/cancel` → forwards to `TaskSupervisor::cancel(task_id)`. Returns 200 on accept, 404 if task unknown, 409 if already terminal.
- API2 — `POST /api/tasks/{task_id}/restart-from-node` body `{node_id?: string}` → forwards to `TaskSupervisor::relaunch(task_id, RelaunchOpts { from_node: node_id })`. Returns 200 with new task_id, 404/409 like cancel.
- G2 — Frontend cancel button on each running NodeCard. Confirmation modal. Optimistic state → final state from supervisor SSE event.
- G3 — Frontend restart-from-node action on failed NodeCard. Modal explains scope ("upstream cached outputs reused"). Action triggers API2 with the failed node_id.

**Acceptance criteria**:
- New unit test: spawn child has FileStateCache + Router + Summary attached
- New unit test: spawn task M8.9 recovery on simulated failure
- New API integration test: POST /api/tasks/{id}/cancel cancels a running task; verify lifecycle transitions to Cancelled
- New API integration test: POST /api/tasks/{id}/restart-from-node restarts; verify upstream output files preserved
- E2E test (`live-cancel.spec.ts`): trigger long-running pipeline, click cancel, verify task transitions to Cancelled within 15s
- E2E test (`live-restart.spec.ts`): trigger pipeline that fails partway (use a fault injection prompt), click restart-from-node, verify only failed node + downstream re-run

### Track 3 (W3) — deep_search/deep_crawl plugin upgrade + protocol v2

**Branch**: `feature/m8-parity-w3-search-crawl`

**Files**:
- `crates/octos-plugin/src/protocol_v2.rs` (new) — v2 spec types
- `crates/octos-plugin/src/lifecycle.rs` — backward compat shim
- `crates/octos-plugin/docs/protocol-v2.md` (new) — protocol spec doc
- `crates/octos-agent/src/plugins/tool.rs` — host-side v2 progress parser (in addition to existing text-line path)
- `crates/app-skills/deep-search/src/main.rs` (and supporting modules)
- `crates/app-skills/deep-crawl/src/main.rs` (and supporting modules)

**Deliverables**:
- F1 — Plugin protocol v2 spec doc + Rust types for: structured progress events, cost-attribution events, output summary, cancel-signal contract
- F2 — Backward-compat shim in `octos-plugin/src/lifecycle.rs`: parser tries JSON-event first, falls back to legacy text-message; v1 plugins keep working unchanged
- C1 — `deep_search`: replace raw Bing search dump with synthesized multi-source answer. Internal LLM call (model from manifest config) consumes search results + crawl excerpts and emits a structured report: `{summary, sources[], confidence}`. The `_report.md` becomes a real synthesized document, not a search result dump.
- C2 — `deep_search`: emit structured progress events (`{stage, detail}`) per protocol v2
- C3 — `deep_search`: SIGTERM handler; cleanup browsers and temp files within 10s; exit cleanly
- C4 — `deep_search`: cost attribution from internal LLM/API calls reported in result JSON
- D1 — `deep_crawl`: max-concurrent-browsers config (default 3); SIGTERM handling; browser pool cleanup on cancel; structured progress events
- D2 — `deep_crawl`: protocol v2 adoption with structured events

**Acceptance criteria**:
- Plugin protocol v2 unit tests covering: v2 plugin emits structured event → host parses correctly; v1 plugin emits text → host falls back to legacy parser
- `deep_search` integration test: run a query, assert returned `_report.md` contains synthesized prose (not just raw search snippets); assert report has source citations
- `deep_search` cancel test: spawn deep_search, send SIGTERM, assert process exits within 12s and no browser zombies
- `deep_crawl` resource test: spawn deep_crawl, assert at most 3 concurrent Chromium processes (down from 6+)
- `deep_crawl` cancel test: same as deep_search
- E2E test (`live-deep-search-quality.spec.ts`): user prompt → deep research → assert delivered report contains synthesized analysis (presence of paragraph structure, source citations) not just bulleted search results

### Track 4 (W4) — Other plugin v2 adoption + integration tests + docs

**Branch**: `feature/m8-parity-w4-plugins-tests`

**Files**:
- `crates/platform-skills/voice/...` (mofa_slides handler if separate)
- `crates/app-skills/podcast_generate/...`
- `crates/platform-skills/voice/...` (fm_tts)
- `e2e/tests/*` — new integration specs
- `docs/m8-runtime-parity-prd.md` — this doc
- `docs/m8-runtime-contract.md` (new) — contract reference for future contributors
- `docs/m8-runtime-migration-runbook.md` (new) — rollout/rollback steps

**Deliverables**:
- mofa_slides v2 adoption: structured progress + cancel + cost attribution
- podcast_generate v2 adoption: same
- fm_tts v2 adoption: same
- Cross-cutting integration tests:
  - `live-pipeline-end-to-end.spec.ts`: full deep research run, assert all M8 features wire up (cards visible, costs tracked, no orphan tool_progress events, cancel works mid-flight, restart-from-node works on failure injection)
  - `live-spawn-end-to-end.spec.ts`: trigger slides_delivery; assert per-phase progress, cancel works, recovery on simulated failure
  - `live-cost-tracking.spec.ts`: trigger multiple pipelines; assert cost breakdown reflects each
- M8 runtime contract reference doc (`docs/m8-runtime-contract.md`)
- Migration runbook (`docs/m8-runtime-migration-runbook.md`)

**Acceptance criteria**:
- All 3 plugins emit valid v2 events (validated by protocol parser unit tests)
- All 3 plugins respond to SIGTERM within 10s
- Integration test suite passes against fleet (mini1/2/4 — never mini5)
- Docs reviewed and merged

## 5. Cross-track integration

### 5.1 Shared baseline

All 4 tracks branch from `main@889e5e05`. Tracks rebase on `main` weekly to pick up integration commits.

### 5.2 Hand-off points

- W1.A1 wires `SubAgentOutputRouter` into pipeline workers — but the router type lives in `octos-agent`. W1 must NOT change router internals; only attach via builder.
- W2.B1 same constraint for spawn-subagent.
- W3.F1+F2 lands first (~day 1). Other plugin tracks (W3.C2/D2 and W4) consume v2 protocol after F2 lands.
- W2.API1+API2 lands by day 3 so W2.G2/G3 can wire up; if API delays, W2 mocks and proceeds with UI-only.
- W1.A3 (task_query_store wiring) lands by day 4 so W1.G1 frontend has tree data to render.
- W1.A4 (cost reservation) lands by day 4 so W1.G4 has cost data to render.

### 5.3 Merge order

When all tracks complete, merge in this order to minimize conflicts:
1. W3 (plugin protocol v2 + deep_search/deep_crawl) — minimal host-side changes
2. W2 (spawn host + API + workflows) — backend
3. W1 (pipeline host + frontend G1/G4) — backend + frontend  
4. W4 (other plugin v2 + integration tests + docs)

Each merge gets a fleet redeploy + smoke test before the next merges.

## 6. Testing strategy

### 6.1 Unit tests (per track)

Each commit must include unit tests for the changed surface. Baseline coverage targets:
- Pipeline host: per-feature wiring tests (FileStateCache attached, supervisor registered, etc.)
- Spawn host: same
- Plugins: protocol parser tests, output schema tests, cancel-signal tests
- Frontend: reducer tests, component snapshot tests

### 6.2 Integration tests (cross-track)

Live against `mini2` (`https://dspfac.bot.ominix.io`). Token: `octos-admin-2026`. NEVER `mini5`.

Test matrix:

| Test | Track owner | Validates |
|---|---|---|
| `live-pipeline-cards.spec.ts` | W1 | G1 per-node card rendering, B2 progress bridge |
| `live-pipeline-cost.spec.ts` | W1 | G4 cost panel, A4 per-node cost reservation |
| `live-cancel.spec.ts` | W2 | M7.9 cancel path end-to-end |
| `live-restart.spec.ts` | W2 | M7.9 relaunch path end-to-end |
| `live-deep-search-quality.spec.ts` | W3 | C1 output curation |
| `live-deep-crawl-resource.spec.ts` | W3 | D1 resource cap |
| `live-pipeline-end-to-end.spec.ts` | W4 | full integration of everything |
| `live-spawn-end-to-end.spec.ts` | W4 | slides/site/podcast end-to-end |
| `live-cost-tracking.spec.ts` | W4 | aggregate cost across multiple runs |

### 6.3 Manual QA checklist

Before merge to main, manual verification on https://dspfac.bot.ominix.io/chat:

- [ ] Trigger deep research — see node tree fill in
- [ ] Cancel mid-flight — task transitions to Cancelled within 15s
- [ ] Trigger pipeline with fault injection — verify failure surfaces with reason
- [ ] Click restart-from-node — verify only downstream re-runs
- [ ] Open cost breakdown — verify per-node values
- [ ] Trigger slides delivery — verify per-phase status
- [ ] Trigger podcast — verify cancellation works (no orphan ffmpeg/python processes)
- [ ] Verify deep_search returns synthesized prose, not raw Bing dump

### 6.4 CI gates

All track PRs must pass:
- `cargo build --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace` (sharded per #590)
- `pnpm typecheck` + `pnpm build` (octos-web)
- Track's own integration tests pass against staging mini2

## 7. Rollout plan

Per-track:
1. Open PR; CI green
2. Self-merge once another agent or operator has reviewed
3. Deploy to mini1 first as canary
4. Smoke test (manual QA item from §6.3)
5. Deploy to mini2/3/4

After all 4 tracks merge:
1. Deploy consolidated build to fleet
2. Run full §6.2 test matrix
3. Update memory: `reference_minis_ssh.md` notes new contract
4. Close umbrella issue

## 8. Rollback plan

If any stage breaks production:
1. Identify the offending merge from `git log main`
2. `git revert <commit>` (avoid `--no-edit`; write rollback rationale)
3. Push to main (admin bypass), redeploy fleet
4. Open follow-up issue describing what went wrong + fix plan

The migration force-push of `c8787472` is preserved in `release/coding-yellow` and `release/coding-purple` — full rollback to pre-migration is still possible by force-push, but should not be needed.

## 9. Out of scope

- Re-architecting plugin invocation to be in-process (still binary protocol)
- Replacing TaskSupervisor with an external scheduler (Kubernetes-style)
- Changing octos-web from React to anything else
- Migrating session storage from JSONL to a database
- Adding new features beyond the M8 contract (M9 family is separate scope)

## 10. Glossary

- **M8.4** — FileStateCache: per-actor cache of file-state claims for resume continuity
- **M8.7** — SubAgentOutputRouter + AgentSummaryGenerator: deterministic output routing + typed summaries for sub-agents
- **M8.9** — Runtime failure recovery: on retryable failure, agent re-engages LLM with `build_recovery_prompt`
- **F-003** — Cost reservation handle (Review A): per-task USD budget reservation
- **F-005** — Workspace contract enforcement (Review A): declared validators run against task outputs
- **M7.9** — PM supervisor: kill/steer/relaunch primitives on the supervisor
- **B1+B2** (this session, `9687fa95`+`889e5e05`) — SSE tool_call_id + supervisor → ToolProgress bridge
