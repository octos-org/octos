# Octos UI Protocol Change Request: Hydrated Tool Result Outcome

## Header

- Request id: `UPCR-2026-086`
- Date: 2026-11-28
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: one additive optional field on `HydratedMessage`, the `messages`
  rows of `session/hydrate` (and of `session/rollback`'s `thread`, which
  carries the same row shape)

## Problem

After a reconnect, a client rebuilding its transcript from
`session/hydrate` cannot tell a failed tool result from a successful one:
the durable `role: "tool"` row carries the output text but no outcome. A
client that guesses "Error:"-looking text into a failure state both
mislabels real content (any text can start with `Error:`) and cannot
distinguish a failing retry from its successful rerun of the same call.
Octoscode-web hit exactly this (N11b-F02): after a refresh, failed exec
rows re-rendered as successes and the transcript doubled.

The canonical ledger already records the truth: every `tool/completed`
notification carries `tool_call_id` and `success`. The hydrate projection
dropped it, so the only failure signal available to a reloaded client was
the text itself.

## Contract

`HydratedMessage` gains one optional field:

- `is_error` (boolean): on a tool-result row (`role: "tool"`) whose
  `tool_call_id` has a retained `tool/completed` notification, `true` when
  that completion recorded `success: false` and `false` otherwise. Absent
  on every other row and whenever the transcript holds no completion for
  the call (an interrupted call, or one older than the retained ledger) —
  absent means "unknown", never a guess.

The value is derived server-side from the same canonical ledger events that
produce `replayed_projection_envelopes` and the live tool events, keyed by
`tool_call_id`. The row's stored content is untouched: the original output
text (including failure logs) is preserved verbatim.

## Compatibility

Additive and optional, like `tool_call_id`/`tool_name` (`UPCR-2026-039`):
rows that omit the field are identical on the wire to before, and older
clients ignore it. No capability gate is added. Clients that negotiated
`projection.envelope.v2` already see `success` on the live envelopes; this
field only aligns the reloaded durable rows with them.
