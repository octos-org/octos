# M9-γ — Server-Authoritative Projection ADR

Date: 2026-05-09
Status: PROPOSED — depends on `M9-α` (sole transport).

## Context

After `M9-α` deletes SSE, the chat flows through one WebSocket
transport. The remaining bug class — every "Q1 works, Q2 disappears"
or "phantom empty bubble after stream end" pattern — lives in the
**state model**, not the transport.

The web client's `ThreadStore` is a *parallel mutable copy* of
server state, with ten reducer entry points each doing partial
reconciliation:

```
addUserMessage           ← client-only (optimistic on send-click)
appendAssistantToken     ← server delta event
replaceAssistantText     ← server replace event
appendCompletionBubble   ← server message/persisted
appendPersistedMessage   ← server message/persisted (different shape)
stampPendingHistorySeq   ← server message/persisted (seq-only)
addToolCall              ← server tool_start
appendToolProgress       ← server tool_progress
attachAssistantFile      ← server file event
ensurePendingAssistant   ← INTERNAL safety net (creates phantoms)
```

Every entry point has its own idempotency story keyed on a different
identity (`clientMessageId`, `messageId`, `historySeq`,
`intra_thread_seq`, `threadId`, `responseToClientMessageId`). They
contradict each other under race conditions. The 2026-05-09 phantom
bubble was one such contradiction: `appendPersistedMessage` didn't
know `tryPromotePendingFromPersisted` had already finalized the
turn, so it minted a fresh empty row.

This is a *state model* defect, not a transport defect.

## Decision

Make the web client a strict **deterministic projection** of the
server's event log. The server emits `(thread_id, seq, type, payload)`
events in a strict total order (already true under M9 ledger
durability). The client maintains:

- **Committed log**: append-only `Vec<Envelope>` indexed by
  `(thread_id, seq)`. Single source of truth.
- **Projection**: pure function `(committed_log) → ChatViewModel`.
  Recomputable, deterministic, no side effects.
- **Optimistic overlay**: a separate visual layer (NOT in
  `ThreadStore`) that renders a "ghost" bubble between user-click
  and the moment the server's `seq` for that send lands. The ghost
  is purely visual — it never enters the projection, never has to
  be reconciled, never produces an orphan.

Result:
- Zero client-only mutations after send-click.
- Optimistic state is visible-only; the server-projected state is
  the truth.
- Identity collapses to `seq` (the only key the projection cares
  about).
- The "ten reducer entry points" collapse to ONE event dispatcher
  that appends to the committed log and recomputes the projection.

## Phase Plan

| Fix | Issue (TBD) | Problem | Owner | Primary Gate |
| --- | --- | --- | --- | --- |
| M9-γ-1 | #(open) | Define canonical envelope shape `(thread_id, seq, type, payload)` and the type union of `payload` (assistant_delta / assistant_persisted / tool_start / tool_progress / tool_end / file_attached / turn_completed) | Spec worker (extend `OCTOS_UI_PROTOCOL_V1_SPEC.md`) | Spec PR'd, types regenerated for both Rust + TS |
| M9-γ-2 | #(open) | Implement pure-function projection `(envelopes) → ChatViewModel` in `octos-web/src/store/projection.ts` | Web worker | Property-test: any permutation of envelopes for one turn → same final view (only seq order matters for *delta accumulation*; payload-level idempotency for everything else) |
| M9-γ-3 | #(open) | Migrate `ThreadStore` to wrap the committed-log + projection. Delete the ten reducer entry points; replace with one `ingest(envelope)` dispatcher | Web worker | All 191 vitest cases re-pass under the new model; any test that relied on directly mutating `ThreadStore` is rewritten |
| M9-γ-4 | #(open) | Move optimistic UI to a `<GhostBubble>` overlay component. The Composer renders it on send-click; it auto-removes the moment the matching `seq` lands in the projection | Web worker | Soak: ghost bubble visible <100ms after click, hidden as soon as `assistant_delta` lands; never touches `ThreadStore` |
| M9-γ-5 | #(open) | Collapse identity to server `seq`. Remove `clientMessageId` / `intra_thread_seq` / `responseToClientMessageId` from the projection model. `clientMessageId` lives ONLY on the optimistic overlay (so the ghost knows when its server reflection has arrived) | Web worker | `grep -E "responseToClientMessageId|intra_thread_seq" src/` returns 0 in projection code |
| M9-γ-6 | #(open) | Delete `MessageStore` (legacy parallel store) — projection covers all of it | Web worker | `git rm src/store/message-store.ts`; all consumers cut over to `ThreadStore.projection` |
| M9-γ-7 | #(open) | Server side: ensure every `message/persisted` carries the exact `seq` the projection needs (no late metadata-only events without payload). Eliminate the multi-emit-per-turn pattern that triggered the 2026-05-09 phantom | Server worker | A turn with N agent iterations emits exactly N+1 server events (N tool/assistant cycles + 1 turn_completed); no duplicates |

## What γ Eliminates

| Bug class | Survives α alone | Survives α + γ |
| --- | --- | --- |
| Transport race (WS vs SSE) | NO | NO |
| Optimistic/persisted reconcile | YES | NO |
| Identity proliferation | YES | NO |
| Termination invariant violations (phantom bubble) | YES | NO |
| Sticky-map drift | partially | NO |
| Late spawn_only orphan | partially | NO |
| Browser refresh shows different state than server | YES | NO |

## Optimistic UI under γ

Composer flow:
1. User types + clicks Send.
2. Composer renders a `<GhostBubble text="…" />` immediately. Ghost has its own React state, not ThreadStore.
3. POST WS frame `chat/send` with `client_message_id`.
4. Server assigns server-time `seq`, emits `assistant_delta` events.
5. Projection ingests deltas → renders the *real* assistant bubble.
6. Ghost component watches the projection; the moment it sees a server bubble whose `client_message_id` matches its own, it unmounts itself.
7. If the server-side send fails (404, 500, no event ever arrives), Ghost shows an inline error + retry; nothing dirty in ThreadStore to clean up.

This eliminates the entire class of "Q1 works, Q2 disappears" because the disappear path doesn't exist — there is no client-only state to lose.

## Soak Gates Specific to γ

After M9-γ ships:

1. **Existing soak**: 9-scenario gate (overflow-stress + thread-interleave + marathon-thirty-messages) passes 9/9 with `PHANTOM_BUBBLE` assertion + a new `OPTIMISTIC_LEAK` assertion that fails if any client-only message survives in `ThreadStore.projection` after a turn finalizes.

2. **New `live-projection-determinism.spec.ts`**: send a complex marathon, capture the server's event log via WS frame inspection, replay the SAME log into a fresh client, assert the final ChatViewModel is byte-identical. Property test for projection determinism.

3. **New `live-refresh-equality.spec.ts`**: send N turns, hard refresh the browser, assert the rendered chat is byte-identical to pre-refresh state. Catches any client-only state that would otherwise be lost.

4. **Overnight soak**: 8-hour run alternating between fast Q's, deep_research bursts, and forced WS reconnects. Pass criterion: 0 phantom bubbles, 0 optimistic-leak violations, 0 thread-binding misroutes across the run.

## Total scope

- α + γ together: ~5–6 weeks.
- Net delete: thousands of LOC of reducer-reconciliation logic.
- Net add: ~1 small projection module + 1 ghost-overlay component.
- Bug class eliminated: every chat-reconciliation race we've patched in 2026-04 and 2026-05.

## Migration safety

Each `M9-γ-N` lands behind a feature flag (`chat_projection_v1`)
that defaults OFF on production until the full chain is green on the
soak gate for 7 consecutive days. Once green, flag flips to ON,
the legacy reducers + `MessageStore` get deleted in a single PR
called "remove legacy chat state model".
