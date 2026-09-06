spec: task
name: "Failed OUP JSON turns preserve actual partial output"
tags: [chat, oup, json, truncation, persistence]
---

## Contract

A failed typed OUP terminal remains `Err`, including provider output truncation.
The local session adapter carries the current turn's canonical answer, answering
model (if known), terminal error and exact terminal usage in `OupTurnFailure`.
Neither a previous turn nor a pre-tool commentary row is a partial final answer.
Reasoning-only/empty failures cannot fabricate an assistant answer.
The optional durable `partial_result` carries a `TurnSessionResult` identity
or an explicit null final. Both survive native error.data projection/replay.
The adapter selects only an exact message ID already received for this turn;
absent legacy metadata, malformed data, an unknown/prior-turn pointer or an
explicit no-final marker yields no partial answer. Older/generic failures
without authoritative identity retain their error/usage but cannot promise
lossless partial-answer recovery; ordinary live activity is still streamed.

`chat --json` still emits exactly one JSON object and exits nonzero on failure.
Generic/bootstrap errors retain exactly `{"error":"..."}`. Typed OUP failures
add their available `code` and one `usage` object; a nonempty actual answer is
available as `partial: {text, model}`. Unknown answering models remain null,
not the configured default. The regular success envelope is unchanged.

Truncated results carry their typed current-turn usage through the durable
`TurnErrorEvent.token_usage` optional field into native terminal projections.
Missing legacy fields deserialize as None and remain omitted on serialization;
ordinary failures do not gain invented usage. Cold replay preserves all five
counters. Session cumulative cost events are not a substitute for turn usage.

This preserves output before `--no-session-persistence` removes its temporary
transcript. It does not convert failure to success, print the JSON object twice,
replay streamed text, change ACP error handling, or add another history manager.

## Regression mapping

- `terminal_integrity_oup_preserves_truncation_without_success`: actual OUP
  truncation remains an error carrying exact canonical partial and turn usage.
- `terminal_integrity_oup_exhausted_reasoning_only_errors`: no fabricated
  partial answer for an exhausted reasoning-only turn.
- `should_not_attach_prior_or_pretool_answer_to_failed_oup_turn`: a successful
  prior answer plus a new tool-bearing turn cannot contaminate a later empty
  failed result.
- `should_not_attach_pretool_answer_when_truncated_tool_call_has_no_final`:
  actual nonstream MaxTokens with a valid parsed tool call but no answer must
  not select the late batched pre-tool assistant commit.
- `should_preserve_exact_nonstream_partial_after_pretool_activity`: the same
  actual nonstream lifecycle with a real final fragment preserves that row.
- `should_require_current_turn_canonical_identity_for_error_partial`: absent,
  malformed and prior-turn pointers fail closed; exact owned identity succeeds.
- `should_replay_authoritative_no_final_without_promoting_legacy_unknown`:
  cold replay preserves the explicit-no-final versus old-unknown distinction.
- `should_preserve_failed_turn_partial_in_json_without_claiming_success`:
  exact code/usage/body, empty-body omission, null unknown model, escaped text,
  and unchanged generic-error JSON.
- `should_replay_exact_failed_turn_usage_without_changing_old_error_wire`:
  cold durable replay keeps exact counters and old legacy bytes remain stable.
- `should_not_overwrite_terminal_usage_or_fabricate_it_for_ordinary_failures`:
  the single terminal gate preserves its first result and omits unknown usage.
- `should_report_only_current_turn_usage_for_repeated_oup_failures`:
  two actual failed OUP turns each report their own total, not session totals.
- Local fake-HTTP acceptance: actual read_file followed by provider length
  termination in persistent JSON, ephemeral JSON and ephemeral text modes;
  all fail nonzero, JSON partial remains accessible, streamed text appears once,
  and ephemeral transcript directories are removed.
