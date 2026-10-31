# Optional evidence for a supervised continuation loop

Use this only when the user/host explicitly enables an evidence-controlled run.
Ordinary PPTX tasks keep the model-directed workflow. The host owns the generic
outer loop; this skill supplies artifact-bound check evidence. Do not write a
second scheduler, critic model, or fixed total-round cap into the deck project.

The host initializes the task once with the exact execution identity (not a model name):
`session:<exact-session-key>` for OctosCode/peer conversation sessions, or the
actual AgentId for native task workers and sessionless chat. OUP AgentIds change
every turn and must not be used for conversation policies. A whole-deck policy belongs to the coordinator/whole-deck producer, not to
page workers sharing its directory. Workers need independent policy/workspace
scope if they are themselves supervised. Never guess the execution identity.

```sh
python scripts/continuation_evidence.py init --workspace TASK_WORKSPACE \
  --task-id TASK_ID --owner-agent EXACT_EXECUTION_ID --deliverable exports/editable.pptx --window 3
```

This creates a small Octos policy for package, copy, artwork and readability.
The window is a configurable diagnostic trigger, not a maximum number of repairs.
Do not weaken it or reset the task merely to get a pass. Existing ordinary tasks
and already running benchmarks must not be silently opted into a new policy.

After an existing native cycle finishes, inspect the actual source/full/blank
views. Record a concise self-review in the project record, for example:

```json
{
  "cycle_sha256": "SHA-256 of the exact cycle.json",
  "inspected": {"copy": ["actual-page-id"], "artwork": [], "readability": []},
  "findings": [
    {"id": "stable-finding-id", "check": "readability", "state": "open",
     "evidence": "path/to/this/cycle/rendered-page.png",
     "note": "The caption crosses its own label edge."}
  ]
}
```

Page IDs come from this task. `inspected` records actual review coverage, not
images merely generated or opened. `findings` states are `open`, `unknown`,
`resolved`, or `dismissed`; explain a resolution/dismissal and reference current
bound visual evidence. Retain finding IDs. Omitting an unresolved finding cannot
close it. Do not treat every geometry/OCR candidate as a confirmed defect. Check
unflagged regions too. Copy review compares the authoritative manuscript; source
page JSON alone cannot prove that it contains everything the user requested.

```sh
python scripts/continuation_evidence.py record --workspace TASK_WORKSPACE \
  --cycle PATH_TO_CYCLE_JSON --review PATH_TO_SELF_REVIEW_JSON
```

The adapter reuses existing build/render bindings, runs scoped package preflight,
retains unresolved findings, and emits `.octos/iteration-evidence.json`. Missing
review categories/pages stay unknown. Repeating an identical record does not
consume another round. It never edits a deck or automatically approves visual
quality. Package results are deterministic diagnostics; copy/artwork/readability
remain explicitly producer self-review. `remaining` counts unresolved findings
plus unreviewed pages within each stable category. It is not an aesthetic score.

Do not publish a checked cycle while rendering is pending. Use the existing
external-job wait/notification mechanism. The runtime ignores pending snapshots
and ordinary tool polling when counting repair cycles.

At a stagnation notice, use the source, actual render and local measurement to
test a different cause. Do not just alter numbers or re-render the entire deck.
At a `needs_replan` handoff, preserve outputs, open findings, job handles and
attempt records. The coordinator chooses the next approach; the incomplete task
is not marked complete. A terminal error is not visual rejection of every page.

The delivery path is chosen for this task, not inferred from a filename. After
publishing the final PPTX, record again so its bytes are bound to the inspected
candidate. Evidence for an old working candidate cannot approve a different
export. This command only reads the delivered file; it does not publish it.
