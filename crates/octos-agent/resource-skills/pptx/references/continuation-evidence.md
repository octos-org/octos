# Continuation evidence

Use the native actions documented in [Rust runtime](rust-runtime.md):

- `evidence_init`: task_id, actual owner_agent, explicit stagnant_attempts and
  relative deliverables. Only a new task initializes policy; do not erase history.
- `evidence_record`: exact cycle and current review paths after a complete check.
  To extend a passed batch, add `extend_scope_reason` and keep every previous
  page in the new cycle. It archives prior evidence and preserves findings and
  attempt numbers. New or unchecked pages remain unknown; shrinking scope or
  extending an unfinished batch is rejected. A new current cycle may replace
  stale source bindings without restoring old source bytes.
- `evidence_retarget`: revised delivery paths plus reason, preserving the same
  task and all failure/stagnation history. Use this for obsolete output paths;
  reinitialization is not a repair operation.
- `evidence_status`: actual owner_agent; confirms whether the persisted runtime
  observer consumed the exact policy and evidence hashes.

The record binds reopened PPTX files, page images, resolved copy and source assets.
Checks are package, copy, artwork and readability. Package is deterministic;
remaining judgments are the explicitly identified human/model review. Missing
coverage is unknown. Omitted unresolved findings remain active. Repeat recording
of identical evidence does not increment the attempt. Findings require a current
rendered image, stable ID, category, state and explanation.

Current Rust builds also require `review.object_reviews` for every native text
object. Generate the unanswered template and matched image groups with
`review_pack`; see [object visual review](object-visual-review.md). Inspect
`object_coverage` in record/status results: unknown or stale object observations
keep artwork/readability incomplete even when the page-level checklist passes.
This validates evidence coverage, not the model's visual understanding.

For pages with design references, `cycle` now generates a whole-page comparison
manifest. `review.comparisons` must cover each such page and classify all marked
regions with reasons (`defect`, `acceptable_difference`, `unresolved`). Plain
`inspected` lists cannot substitute for this evidence. Missing coverage is unknown;
classified defects fail artwork/readability. Even zero differences require visual
inspection. These are model judgments, not automatic pixel-based quality scores.
Unchanged image hashes reuse recorded comparison reviews; changed design, blank
or render bytes invalidate only that page. The record retains supplied reference
requirements, so removing a design cannot silently bypass the check.

For legacy cycles, run `compare_cycle` into a fresh directory and pass its manifest
path as `comparisons` on `evidence_record`. This adds evidence without rebuilding
the deck or rewriting its old receipt. `evidence_status.comparison_coverage` shows
what remains. See [Rust runtime](rust-runtime.md) for the JSON schema.

After the first complete repair/check cycle, call evidence_status. Policy presence
or a peer being launched does not prove the kernel consumed it. Do not claim hard
threshold enforcement while runtime_consumed_current_evidence is false. Persisted
matching state does not establish process liveness or visual acceptance.
Require `delivery_bindings_current` too: a consumed snapshot can still name an
obsolete final preview. Copy checked outputs to stable final names and record
again. A source JSON delivery requires provenance from a fresh cycle.

Use [peer review and exit](peer-review-exit.md) for task-level decisions. No second
polling loop or alternative review model is installed by this helper.

## Native identity binding

When called through Octos, `evidence_init` and `evidence_status` use a host-supplied
identity. Omit `owner_agent`; a peer nickname is not a session ID. An explicit
conflict fails. `owner_matches: false` is diagnostic evidence that this loop does
not own the policy. The first complete review cycle must also show
`runtime_consumed_current_evidence: true` on a later tool turn.

Before freezing a new policy, `doctor.continuation.host_identity_present` must
be true under Octos. A missing owner is a host/plugin wiring problem, not a
request to guess a string. Never substitute an inherited `CODEX_SESSION_ID`,
process ID or peer nickname. Existing policy/history stays intact while the host
is repaired. An injected owner alone does not prove that the observer parses and
consumes the current evidence schema.

`evidence_record` and `evidence_status` return `check_feedback` to separate
measured violations, review-reported defects and evidence gaps. These counts
overlap and are not a count of unique visual defects. When the action is
`resolve_evidence_gaps`, supply the missing correspondence/coverage or report the
specific tool limitation; do not move correct labels to reduce unknowns. The
summary never changes acceptance or grants visual approval.

A task/scope migration must preserve the old policy, attempts and unresolved
findings in an explicit archive with the reason for migration. Do not delete
history or rename a task just to reset stagnation. A genuinely new diagnostic
method can be tested, with its result linked back to the unresolved defects.

For new tasks that combine mechanical and independent visual verification, see [review lifecycle](review-lifecycle.md). Freeze the independent reviewer before work; preserve strict legacy policies. Register defects before edits, not only during final evidence_record.
