# Octos UI Protocol Change Request: `context/compaction_started` Notification

## Header

- Request id: `UPCR-2026-026`
- Title: Add `context/compaction_started` notification for in-progress compaction UX
- Author: compaction-UX slice (memory/default-on follow-up)
- Date: 2026-07-08
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Sibling UPCRs: `UPCR-2026-022` (context lifecycle: `context/compaction_completed`,
  `context/normalization_reported`)

## Summary

Adds one additive JSON-RPC notification, `context/compaction_started`, emitted
immediately BEFORE a server-owned context-manager compaction pass. Today
clients learn about compaction only after the fact (`context/compaction_completed`),
so a UI cannot render an in-progress state ("Compacting conversation…" with a
progress/fullness bar). The change is strictly additive.

## Motivation

octoscode wants Claude-Code-style compaction UX:

```
✶ Compacting conversation… (12s · 87.4k tokens)
  ▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱ 49%
```

The percentage is honest only if the client knows the pre-compaction token
estimate AND the threshold that tripped the pass. Both live server-side at the
emission site (`appui_context_history_for_agent`). The completed event carries
before/after estimates but arrives after the work; a started event closes the
gap and future-proofs the UX for asynchronous/LLM summarizers where the window
is measured in seconds, not microseconds.

## Payload

```json
{
  "method": "context/compaction_started",
  "params": {
    "session_id": "<key>",
    "context_state": { "…": "UiContextState — token_estimate = BEFORE size" },
    "trigger": "preflight",
    "threshold_tokens": 96000
  }
}
```

- `context_state` — pre-compaction `UiContextState` (same shape as
  `UPCR-2026-022`).
- `trigger` — mirrors the eventual completed record's `trigger`.
- `threshold_tokens` — the context-window-derived limit that tripped the pass;
  `token_estimate / threshold_tokens` (or vs. the model context window) renders
  an honest fullness percentage.

## Semantics

- Emitted at most once per compaction pass, always before the pass mutates the
  context manager.
- Always followed by `context/compaction_completed` for the same pass (the
  completed record's `token_estimate_before` equals the started event's
  `context_state.token_estimate`).
- Today's serve compaction is synchronous: both notifications may arrive in one
  delivery batch. Clients MUST tolerate a zero-duration started→completed
  window (render the final state directly).
- Ephemeral-class: carries no durable replay cursor of its own (same
  classification as the other `context.lifecycle.v1` notifications).

## Gating

- Capability gate: `context.lifecycle.v1` (same as its siblings). Clients that
  did not negotiate the feature never receive it.

## Compatibility

- Strictly additive: no existing method, notification, payload, enum variant,
  capability bit, or protocol identifier changes.
- Old clients ignore unknown notifications per § 4 versioning rules.

## Implementation anchors

- Type: `octos-core/src/ui_protocol.rs` `ContextCompactionStartedEvent`
- Emission: `octos-cli/src/api/ui_protocol.rs` `appui_context_history_for_agent`
- Gating/classification: notification feature filter + cursorless list +
  ledger session-id mapping
- Test: `compaction_started_precedes_completed_in_lifecycle_batch`
