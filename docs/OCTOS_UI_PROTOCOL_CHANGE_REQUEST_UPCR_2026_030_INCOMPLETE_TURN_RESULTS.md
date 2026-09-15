# Octos UI Protocol Change Request: Incomplete Turn Results

## Header

- Request id: `UPCR-2026-030`
- Date: 2026-09-14
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented; local process and bindings validation passed
- Scope: additive error metadata in the OUP/backend convergence work

## Contract

A provider response terminated by its output limit is a failed turn with code
`output_truncated`. Actual partial output remains available, but cannot be
reported as a completed answer. The producer supplies the authoritative final
message identity; neither arrival order nor text matching establishes it.

`turn/error` retains its required `session_id`, `turn_id`, `code`, and `message`
fields, and optional `topic`. It adds two optional fields:

- `token_usage`: the current turn's measured `EnvelopeTokenUsage`, including
  input, output, reasoning, cache-read and cache-write counters. It is not the
  session's accumulated usage. Unknown usage is omitted; zero-valued optional
  counters retain the existing skip-zero serialization rule.
- `partial_result`: an object containing `session_result`, either a
  `TurnSessionResult` identifying an actual final fragment or explicit `null`
  when the producer established that there is no final answer.

An absent `partial_result` means legacy or unknown identity. It must not be
interpreted as proof that the most recent assistant row is a final fragment.
A pre-tool commentary row, earlier turn, or background answer cannot substitute
for an exact current-turn producer reference.

## Projection and replay

For `projection.envelope.v2`, the canonical `turn_terminal` payload keeps
`outcome: "errored"`. Its existing `token_usage` field carries the typed turn
usage. The partial identity is projected into
`error.data.partial_result.session_result` with the same object/null/absent
distinction.

The durable error record owns these fields. Cold replay preserves all counters
and the exact partial identity, including an explicit null final. The first
terminal wins; a duplicate completion cannot replace its usage or identity.
These optional metadata fields need no additional feature negotiation and do
not change older error frames that have no metadata.

## Host behavior

`chat --json` emits one JSON object and exits nonzero. A typed OUP failure
includes its available error code and usage; an actual nonempty final fragment
is exposed as `partial: {text, model}`. An unknown model is null. Generic
bootstrap errors retain the existing `{error: "..."}` shape. ACP continues to
return a failed prompt result after rendering any actual partial output.

Gateway/API completion, native specialist execution and C/UniFFI task bindings
also retain failure status and real partial output. Their presentation APIs
are separate from the OUP wire schema; they must not double-count the wrapped
partial usage or fabricate a persisted assistant error message.

## Validation

The accompanying runtime correction lets `peer/prepare` and `peer/gather`
resolve a persisted profile's storage before model bootstrap. Once a runtime
exists, these resource operations, result persistence, parent continuations,
closed-peer guards and snapshots resolve the same active profile runtime as
session execution. Their request and result schemas do not change.

- `specs/task-oup-json-partial-error.spec`
- `specs/task-non-oup-incomplete-response.spec`
- `scripts/tests/test-oup-runtime.py`: actual chat/ACP/OUP subprocesses,
  localhost provider/tool execution, failed JSON/text output, and cold replay.
- `scripts/check-oup-bindings.py`: generated binding parity, C declarations,
  and actual C/Python success and incomplete-result calls.
- `./scripts/milestone-ci.sh oup-runtime` and `oup-minimal` are CI entry points.
- Current results and evidence: [OUP/backend validation record](adr/oup-backend-validation-2026-09-14.md).
