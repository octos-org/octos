# Evidence wiring for the peer repair loop

For the default Octos PPTX peer production/repair workflow, initialize and verify
this existing host capability; installing the skill alone does not enable it.
The user may explicitly choose another workflow or a frozen tools-only benchmark;
do not silently change those runs. The host owns the generic observer, the peer
chooses repairs, and the independent coordinator applies
[acceptance and exit criteria](peer-review-exit.md). Do not write a second
scheduler, critic model, or fixed total-round cap into the deck project.

The host initializes the task once with the exact execution identity (not a model name):
`session:<exact-session-key>` for OctosCode/peer conversation sessions, or the
actual AgentId for native task workers and sessionless chat. OUP AgentIds change
every turn and must not be used for conversation policies. In the default peer
workflow, bind the whole-deck producer peer's durable session, obtained from the
host's actual peer/session record. Do not bind the coordinator by accident or
guess a session key from a model/name. Only one owner uses a workspace policy;
separately supervised page workers need separate policy/workspace scopes.

```sh
python scripts/continuation_evidence.py init --workspace TASK_WORKSPACE \
  --task-id TASK_ID --owner-agent EXACT_EXECUTION_ID --deliverable exports/editable.pptx --window 3
```

This creates a small Octos policy for package, copy, artwork and readability.
The window is a configurable diagnostic trigger, not a maximum number of repairs
or a wall-clock deadline. Do not weaken it or reset the task merely to get a pass.
If a policy already exists, inspect and preserve it instead of reinitializing it.

## Verify configuration and actual consumption

```sh
python scripts/continuation_evidence.py status --workspace TASK_WORKSPACE \
  --owner-agent EXACT_EXECUTION_ID
```

Before the first checked cycle, expect `configured: true` and
`runtime_consumed_current_evidence: false`. This means **configured**, not active.
After `record` below and the next host observer checkpoint, run the same command
with `--require-consumed`. It exits nonzero until the persisted runtime state
matches the policy digest, current checked snapshot digest and attempt number.
Save this small receipt in the existing task record. A copied script, policy
file, model declaration, or `max_iterations: 0` is not consumption evidence.

If it is not consumed, inspect the exact session owner, workspace, supported host
version and runtime diagnostics before dispatching further blind repair cycles.
Do not fabricate the state file, rename the peer, reset evidence, or switch to
an untracked workspace. Report a concrete integration gap if it cannot be fixed;
do not claim a kernel-enforced threshold. The status helper is read-only: it
proves a matching persisted observation, not process liveness, visual quality,
or enforcement of time/cost limits. Other hosts must expose their own equivalent
receipt; a Python helper cannot add an absent observer.

After each meaningful completed repair/check cycle, inspect the actual
source/full/blank views and record a concise self-review, for example:

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
Include the coordinator's unresolved mandatory findings in subsequent reviews;
the producer cannot omit them just because its own pass missed them. Keep optional
polish in a separate backlog, not as endlessly growing required findings. Never
dismiss a real defect merely to make `remaining` decrease. The coordinator also
tracks stagnation per mandatory finding: this aggregate observer cannot tell that
one stubborn defect stayed unchanged while other small defects were fixed.

Do not publish a checked cycle while rendering is pending. Use the existing
external-job wait/notification mechanism. The runtime ignores pending snapshots
and ordinary tool polling when counting repair cycles.

At a stagnation notice, stop the repeated repair strategy and use source, actual
render and local measurement to test a different cause. The current Octos observer
first issues a notice at the configured flat-cycle window, then yields if a
further checked cycle is still stagnant. Do not describe it as an immediate
three-total-round cap. At a `needs_replan` handoff, preserve outputs, findings,
job handles and attempt records. The coordinator may continue only with a concrete
new diagnostic plan within the user's scope/budget; "continue" or a fresh peer
with cleared history is not a plan. Without a viable next action, return an
incomplete result. Producer self-review passing also does not waive independent
final review. A terminal error is not visual rejection of every page.

The delivery path is chosen for this task, not inferred from a filename. After
publishing the final PPTX, record again so its bytes are bound to the inspected
candidate. Evidence for an old working candidate cannot approve a different
export. This command only reads the delivered file; it does not publish it.
