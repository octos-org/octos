# Progress feedback without periodic model calls

The default conversation loop uses `OCTOS_CONVERGENCE_MODE=inline`. It does
not run a separate reflection after a number of calls, accumulated tokens,
elapsed time, or edits to one file. These counters measure work; they do not
prove that work has stalled.

The result-aware detector appends a warning to the actual tool result when a
one-, two- or three-step pattern of tool names, arguments and returned text
repeats three times. Unchanged peer reads
receive the asynchronous-polling warning: the peer may still progress, so use
other work or a bounded wait and gather the actual result. A changed result
breaks the identical-result streak. This feedback is available to the next
ordinary model request, with its normal tools and configuration. It does not
add a tools-disabled request, replace the tool result, fabricate a final answer,
or claim that a successful command proves task quality.

Count-only file-churn warnings and their model-escalation requests are disabled
by default, including in `LoopDetector::new`. The optional periodic mode enables
them in the conversation loop. An embedding that explicitly needs this legacy
heuristic can call `set_file_churn_enabled(true)`.

## Compatibility and experiment controls

Set `OCTOS_CONVERGENCE_MODE=periodic` on the runtime process to opt into the old
standalone synthesis and legacy pre-execution signature guards. Only that mode
uses these existing thresholds:

- `OCTOS_CONVERGENCE_LLM_CALLS` (default 20 completed action calls).
- `OCTOS_CONVERGENCE_ACTIVE_TOKENS` (default 100,000 active tokens).
- `OCTOS_CONVERGENCE_SECS` (default 300 seconds).
- `OCTOS_FILE_CHURN_THRESHOLD` (default 5 successful mutations per file).

Threshold variables alone no longer enable periodic synthesis. An absent,
empty or unrecognized mode selects inline feedback. A running turn keeps the
mode selected when that turn starts; set the runtime environment before
starting the process for reproducible comparisons.

The periodic mode still has its existing limitations: edit counts can produce
false positives, and `tool_choice=none` can have provider-specific context/cache
behavior. An identical serialized message prefix alone does not prove cache
reuse inside a provider. This patch does not claim to fix provider preprocessing.

## Scope

No new model, API, tool schema or presentation-specific detector is introduced
in the runtime. Tool-declared `do_not_retry_same_turn` restrictions, provider
error recovery, approval handling, budget limits, context compaction, optional
verifier and final-answer checks retain their own paths.
Inline warnings remain part of real tool results; no new persistence or
rewind format is added. The standalone reflection controller does not become
a replacement task validator.

### Execution and limits

Default inline mode bypasses both pre-execution signature heuristics: neither a
third identical request nor an alternating request pattern stops execution
before observing results. This applies to arbitrary tools, not a list of known
polling tool names. The generic hint no longer predicts that a future result
must be identical or instructs the agent to declare completion.

Repeated results alone do not force termination in inline mode. Configured
iteration/token/time budgets still stop bounded tasks; the regression suite
checks an indefinitely repeating provider against an explicit iteration budget.
With no configured cap, a model can ignore hints and keep working. This change
does not claim to solve semantic convergence. Timestamp differences, different
arguments and media changes not represented in returned text remain limitations
of text-result fingerprints.

Periodic mode explicitly restores the old pre-execution guards, including their
false-positive risks, for compatibility. In that mode only peer reads have the
legacy asynchronous exception. It is not the default.

This is deliberately a small change on top of the tested tool-I/O candidate.
It does not import other experimental branches or change deployed runtime pins.

## Verification

The real conversation loop is tested with a scripted provider and actual file
writes. Productive edits beyond the old call/churn thresholds must all execute,
with only normal action requests, append-only message history and the genuine
final answer. An unchanged peer snapshot must yield an inline hint while a
later read can still observe completion, without an extra reflection request.
Unit tests cover default/explicit mode selection, time/token thresholds,
count-only churn suppression and preservation of result-aware warnings.
Existing explicit-periodic and verifier tests remain regression coverage.
Generic polling tests cover changing results and later completion after three
unchanged results. Alternating-operation tests distinguish repeated results
from changing ones without skipping tool execution.

Validation on this branch:

- The two reflection regressions and four additional pre-execution-guard
  regressions failed before their respective implementation changes.
- `cargo test -p octos-agent --no-default-features --lib -- --test-threads=4`:
  3,038 passed, 0 failed, 3 ignored.
- `cargo clippy -p octos-agent --no-default-features --lib --tests -- -D warnings`:
  passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

These tests establish orchestration behavior. Model quality, task duration and
provider cache savings need another matched live trial. The previously
observed checkpoint token cost is not a guaranteed saving after this change.
