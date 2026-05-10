# M9-α — Sole Transport ADR (Delete SSE)

Date: 2026-05-09
Branch (target): `coding-green-m9-local-20260428` and successor.
Status: PROPOSED — addendum to the M9 milestone after the
2026-05-09 phantom-bubble + base_domain WS-misroute incidents.

## Context

The chat surface ships with two parallel transports for assistant
turns:

1. Legacy SSE foreground path (`/api/chat`, `/api/sessions/*/stream`),
   ingested in `octos-web` by `src/runtime/sse-bridge.ts`.
2. M9 UI Protocol v1 WebSocket (`/api/ui-protocol/ws`), ingested in
   `octos-web` by `src/runtime/ui-protocol-bridge.ts` +
   `ui-protocol-event-router.ts`.

Both deliver assistant streaming, tool_progress, and lifecycle events
for the SAME turn. Every reducer entry point in `ThreadStore` and
`MessageStore` has to dedupe events from the other transport. The
overlap zone is where every UX bug surfaced in 2026-04 / 2026-05
lives:

- `#649` thread-binding regression chain (sticky-map drift across
  rotations)
- `#664` / `#680` / `#740` rapid-fire / overflow-stress recurring
  failures
- `#761` server live-pub fixup (3 codex BLOCKs)
- `#73` C-2 wiring fixup (5 codex BLOCKs)
- `2026-05-09` phantom-bubble bug — multi-iteration agent loops emit
  duplicate `assistant`-role `message/persisted` events; the second
  arrived empty under the M10 metadata-only wire shape and rendered
  as a phantom timestamp-only bubble. Fix shipped as PR `octos-web#92`
  but is a defensive guard, not the root.
- `2026-05-09` base_domain WS misroute — old fleet binary on
  mini2/mini3 omitted `/api/status.base_domain`; web bundle
  fell back to a wrong WS host; chat appeared to "stream then
  drop" with `realContent=0`. Manifest of the dual-transport
  surface area exposed.

The M9 plan was to *replace* SSE with WS. It landed *alongside*. Every
reducer is now expensively reconciling identical events from two
transports.

## Decision

Make `/api/ui-protocol/ws` the **sole** chat transport. Delete the
legacy SSE foreground path entirely. No flag, no fallback, no
"coexistence" period beyond the phased migration in this ADR.

`/api/chat` REST endpoint REMAINS for health checks and one-shot
diagnostic curl probes (return shape: synchronous JSON answer for
non-streaming queries). It does NOT participate in the streaming
chat lifecycle. Browser client never calls it.

## Phase Plan

| Fix | Issue (TBD) | Problem | Owner | Primary Gate |
| --- | --- | --- | --- | --- |
| M9-α-1 | #(open) | Audit every SSE call site (web + server + e2e); produce delete-list | Explore agent → human review | Audit doc lists every file:line; no surprises |
| M9-α-2 | #(open) | Migrate background `tool_progress` events from SSE to WS UI Protocol | Runtime worker | Soak: deep_research delivers progress over WS only; no SSE bytes on the wire |
| M9-α-3 | #(open) | Migrate session lifecycle events (open/close/title/result) from SSE to WS | Runtime worker | Soak: full session sees no SSE frames |
| M9-α-4 | #(open) | Migrate status / heartbeat / progress-gate events to WS | Runtime worker | Heartbeat + progress events go through one WS stream |
| M9-α-5 | #(open) | Delete `octos-web` SSE bridge (`sse-bridge.ts`, `task-watcher.ts` SSE paths, `runtime-provider.tsx` SSE wiring) | Web worker | `git grep EventSource` returns 0; `sse-bridge` modules deleted |
| M9-α-6 | #(open) | Delete server SSE routes + handlers (anything emitting `text/event-stream`) | Server worker | `git grep text/event-stream` returns 0; route table has no SSE |
| M9-α-7 | #(open) | Update e2e harness to drop SSE-specific selectors / waits (`isSpawnAckOnly` SSE chrome, etc.) | E2E worker | Soak full pass; no `EventSource` polyfill needed in playwright |
| M9-α-8 | #(open) | Fleet redeploy: build + deploy WS-only binary to mini1/2/3/5 (mini4 separate) | Deploy worker | All minis serve `/api/status` + `/api/ui-protocol/ws`; SSE 404 |

Phases are sequential within (α-2 → α-3 → α-4) and (α-5 + α-6 must
be done atomically — server and web bundle versions must move
together). α-1 is the first prerequisite.

## What `α` does NOT solve

α removes the transport-level race. It does NOT remove:

- Optimistic-vs-persisted reconciliation race (client creates
  bubble on send-click; server eventually echoes a
  `message/persisted`; reconcile by `messageId` / `historySeq` is
  still the same race, just on one wire).
- Identity proliferation (`clientMessageId` / `messageId` /
  `historySeq` / `intra_thread_seq` / `threadId` /
  `responseToClientMessageId` — every reducer still has to know
  which to dedupe by).
- Mutable client state (`ThreadStore` is still a parallel mutable
  copy of server state; orphan-bucket creation,
  `ensurePendingAssistant`, optimistic placeholder all introduce
  client-only mutations that drift).

These three remaining defects are addressed by `M9-γ` (Server
Projection ADR — `M9-GAMMA-SERVER-PROJECTION-ADR.md`). `α` is the
foundation `γ` projects from.

## Acceptance Gate

After all M9-α fixes land:

1. `git grep -E "EventSource|text/event-stream|/api/sessions/.*stream"`
   in both repos returns ZERO non-test results.
2. `octos-web` ships exactly one ingest module (`ui-protocol-bridge.ts`)
   and one event router (`ui-protocol-event-router.ts`).
3. The full 9-scenario soak gate (overflow-stress + thread-interleave +
   marathon-thirty-messages) passes 9/9 on mini1, mini2, mini3 with
   `PHANTOM_BUBBLE` assertion enabled.
4. A new `live-stream-recovery.spec.ts` scenario passes:
   - Send a long-running deep_research turn.
   - Mid-stream, kill the WS at the browser layer (`page.evaluate(() => ws.close())`).
   - Assert: bubble pauses, browser reconnects within 3s, stream resumes from last-seen seq, no duplicate bubble created.

## Order of operations (vs. M9-γ)

`α` ships first. `γ` builds on top and is independently scoped in
`M9-GAMMA-SERVER-PROJECTION-ADR.md`. `α` alone resolves ~60% of the
chat bug class. `α + γ` resolves the remainder by construction.
