# Evidence-driven continuation for supervised work

This opt-in observer consumes completed check cycles. It does not call a critic
model, decide how to repair an artifact, or prove a producer's visual judgment.
The producer remains responsible for diagnosis and repair. The task coordinator
owns acceptance and replanning. No overall iteration limit is added.

## Activation and ownership

A host can install `.octos/continuation.json` in a task workspace before the run:

```json
{
  "schema_version": 1,
  "task_id": "independent-task-identifier",
  "owner_agent": "session:profile:local:tui#peer-producer",
  "evidence_file": ".octos/iteration-evidence.json",
  "stagnant_attempts": 3,
  "deliverables": ["exports/candidate.bin"],
  "required_checks": [
    {"id": "copy", "epsilon": 0},
    {"id": "readability", "epsilon": 0}
  ]
}
```

For session-backed conversation executors (including OctosCode peers), the owner
is `session:<exact-session-key>`. OUP recreates `Agent.id` each turn, so that
transient ID must not be used. For native `run_task` workers or sessionless chat,
use the actual `Agent.id`; native children never inherit a parent conversation
policy through their parent session key. The host supplies the actual identity,
not a model name or guessed worker label. A child sharing its parent's
workspace ignores a policy addressed to the parent; do not impose whole-project
checks on an independent page worker. Each supervised worker needs its own policy
and isolated workspace. No policy means the existing behavior and no extra model
calls. This is an explicitly scoped task facility, not a workspace-wide mandate
for every future user question. Archive the policy/state when the host closes the
task; keep them for its continuation. Do not install policies for ordinary edits.

Required checks and tolerance are fixed for that task. Editing/removing required
checks is not a repair. The host must explicitly reconcile scope changes and
preserve prior evidence, rather than resetting the detector to restart a loop.
This is a cooperation contract, not a security boundary against a malicious agent
with write access to its own workspace.

## Producer evidence

Write snapshots atomically after a completed modify/check cycle:

```json
{
  "schema_version": 1,
  "task_id": "independent-task-identifier",
  "attempt": 1,
  "phase": "checked",
  "artifacts": [{"path": "exports/candidate.bin", "sha256": "actual SHA-256"}],
  "checks": [
    {"id": "copy", "status": "pass", "remaining": 0},
    {"id": "readability", "status": "fail", "remaining": 4}
  ]
}
```

Statuses are `pass`, `fail`, `unknown`. `remaining` is optional and, when used,
is a nonnegative residual with a declared stable meaning (e.g. unresolved confirmed
findings or uninspected pages), where lower is better. It is not a subjective
quality score. Bind the actual artifact plus source/check/review evidence files.
All kernel-read paths must stay inside this workspace. Hashes establish identity
and freshness, not the adequacy of a checker or truth of a model's declaration.
Never convert an uncertain detector candidate into a confirmed failure solely
because it appeared in a report. Missing required checks remain unknown.

`phase: pending` reports an unfinished external job/check and consumes no round.
The producer still uses existing job handles/completion notifications. Attempt
numbers increase only for completed checks; duplicate/replayed snapshots and file
hash changes alone do not establish progress. Inspectors should preserve finding
identities and unknown items across revisions. The PPTX skill's optional
`continuation_evidence.py` implements that adapter without editing slide data.

## Decision and handoff

The observer polls at safe boundaries between completed tool batches and on
EndTurn, after cancellation/budget checks. It validates fresh artifact bindings
before comparing completed attempts, and again before allowing EndTurn. Each
declared delivery path must also be explicitly bound in the current snapshot;
checking an older working candidate cannot approve a different exported file. A changed
hash is activity, not improvement. Improvement means a new best checked status or
residual, with no simultaneous regression relative to the preceding snapshot.
Going 10 -> 9 -> 10 -> 9 is not repeated progress. Increasing actual inspection
coverage may be progress even while the task remains unknown/incomplete.

After the configured number of flat completed attempts (three is a starting
setting, not an empirically established universal threshold), one inline notice
asks the producer to diagnose or change approach. A further flat completed attempt
returns control with `[needs_replan]`. The producer can continue freely while
making measured progress. Merely rereading a report or waiting is not a round.

At EndTurn, missing/stale/failed/unknown required evidence gets one correction
notice. Repeating the same completion claim without new evidence returns control;
it does not cause an endless verifier loop. Explicit budget/cancellation behavior
remains authoritative. State is atomically persisted in
`.octos/continuation-state.json`, so restart/resume cannot erase the progress
baseline. A resumed executor may perform new work; unchanged records do not
consume more rounds. Another flat checked attempt will hand back again.

Conversation mode uses the existing failed-progress carrier: history/usage are
persisted and the host emits a failure, not `turn_completed`. Task mode returns
`success: false` with the handoff reason and retained file/usage metadata, through
the normal completion channel. The same-worker single-shot recovery wrapper
recognizes this handoff and does not immediately restart the identical worker. This patch does not introduce a new OUP/UI status.
A host can show the explicit reason; it must not equate terminal failure with
accepted completion. Existing peer/child completion handling can notify the
coordinator. It must choose a new diagnosis/input/approach or report a genuine
blocker; automatically resending 'continue' does not reset the detector.

## Boundaries

An agent that never publishes evidence cannot be assessed for semantic stagnation
by this observer; EndTurn still detects missing evidence. A producer can also
misclassify a visual defect as resolved. The adapter and checks improve feedback,
but do not establish a guarantee of artistic quality. A full model/PPTX benchmark
is separate from deterministic runtime tests. Running experiments keep their frozen
policies and skill versions until a new controlled run is explicitly started.
