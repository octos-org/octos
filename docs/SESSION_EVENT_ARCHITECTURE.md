# Session Event Architecture

## Problem

Before this change, background task completion in web chat was assembled from three different sources:

- `/sessions/:id/tasks`
- `/sessions/:id/messages`
- `/sessions/:id/files`

That made terminal delivery inherently racy:

1. a background task finished on the server
2. task state flipped to `completed`
3. the foreground `/chat` SSE stream was already closed
4. the browser learned about the final assistant message and media later by polling

The visible failure mode was a gap where:

- the banner no longer showed a running task
- the server had already persisted the MP3
- the browser still showed no final audio bubble

## Invariant

For background assistant results, the session actor is the single writer of truth:

1. persist the terminal assistant message into session history
2. obtain its committed history sequence
3. emit a committed session-result event derived from that durable write
4. let web stores project from that committed event

This means the browser no longer has to infer completion from a disappearing task plus a later poll.

## Phase 1 Implementation

This repository now implements a first event-ledger step:

- `SessionHandle::add_message_with_seq()` and `SessionManager::add_message_with_seq()` return the committed message sequence
- `session_actor` uses that sequence to attach `_session_result` metadata to background assistant notifications after the history write succeeds
- `api_channel` exposes `GET /sessions/:id/events/stream`
- `api_channel` broadcasts:
  - `task_status`
  - `session_result`
- `octos-web` `task-watcher` opens a dedicated background session stream for watched sessions and applies committed `session_result` events through `MessageStore.appendHistoryMessages()`

## Phase 2 — Sticky thread_id and committed_seq (M8.10)

Subsequent work hardened the contract so that the browser can replay deterministically:

- **Persistent thread_id** (#628) — every session has a stable `thread_id` carried alongside its `key` and persisted in JSONL meta.
- **thread_id on every SSE event** (#629) — `token`, `tool_progress`, `task_status`, and `session_result` events all carry the thread_id; the `done` event additionally carries `committed_seq` so a client knows the exact sequence number of the durable terminal write.
- **Sticky on api_channel** (#635) — once an api_channel SSE connection has emitted any event for a thread, that thread_id stays bound to the connection. The thread_id is bound **before** the first emission (#637) so very fast first-token paths still see it.
- **Replay-harness fixtures** (#656) — `crates/octos-agent/tests/` holds JSONL fixtures that exercise thread_id binding correctness; the harness replays the fixture and asserts every event in the stream carries the expected thread_id and committed_seq.
- **e2e progress-gate** (#655) — `e2e/tests/live-progress-gate.spec.ts` exercises the background-task UX end-to-end, including tool-retry collapse and thread interleave (#630).

## Why This Is Better

This change moves completion truth from:

- “task disappeared, now poll until the message maybe appears”

to:

- “session actor committed message seq N, and the browser received that committed result event”

The committed result event is authoritative because it is emitted only after durable session history persistence.

## Current Boundaries

This is not yet the full end-state event ledger.

What is solved now:

- background media results no longer depend purely on delayed `/messages` polling
- a watched session can receive terminal task status and the committed result after the foreground `/chat` stream has closed
- the browser applies committed results through the same authoritative history merge path used by REST history sync

What is still future work:

- replace the separate `/tasks`, `/messages`, and `/files` polling model with a single resumable `/events?after_seq=` feed
- make `TaskStore`, `MessageStore`, and `FileStore` pure projections of one ordered session event log
- unify non-media background report previews with the same committed result path
- make the event stream topic-aware for all session surfaces, not just default web chat

## Design Rule Going Forward

Background work is not complete when a worker exits.

Background work is complete when the session actor has durably committed a terminal session result and emitted the corresponding committed event.
