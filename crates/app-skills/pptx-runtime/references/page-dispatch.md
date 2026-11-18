# Ready page work and dispatch coverage

`page_gate` now returns each page's `work_stage` and `candidate_current`:

- `make`: no usable current native candidate; the original maker must create,
  rebuild or repair the publication chain. A newer mutable draft does not itself
  invalidate an immutable published source; the actual bound files decide.
- `review`: a current candidate lacks a valid matching independent review,
  including a missing/stale verdict or incomplete review evidence. Repair factual
  review metadata without repeating valid image inspection when bytes are unchanged.
  A rejecting current review with no recorded open finding also stays here with
  `review_disposition_needed:true`: ask that reviewer to clarify the actionable
  quality shortfall or decision. Never invent maker edits or inflate the grade.
- `repair`: a current, bound independent review rejects the page. Its original
  maker owns real repairs; if no actionable finding is given, ask that reviewer
  to clarify its decision rather than inventing edits or demanding a higher score.
- `complete`: the current candidate already meets the unchanged page gate.
  Keep it frozen; do not start cosmetic work to occupy an idle agent.
- `arbitrate`: assign an independent third peer to interpret the existing rubric
  on the bound evidence. Use the frozen arbiter when configured. The original
  reviewer owns any revision to its verdict.
- `diagnose`: assign an authorized technical peer to reproduce the capability
  failure and identify a repair within the task's permissions.
- `blocked`: the original reviewer bound a specialist's report for the exact
  referral and candidate. See [page verdicts](page-verdict.md). Dispatch other
  pages. When `only_blocked_pages_remain` is true, report the blockers and end
  the attempt. Assembly still requires every page to pass.
  The terminal `handoff` sets `automatic_retry_allowed:false`. Retire consumed
  monitors and close the existing task goal as blocked. An idle coordinator
  has no peer work to reissue for this inventory.

Use `page_dispatch` for a read-only coverage check. It reuses the real page gate,
does not write files, start peers, author prompts, grade visuals or change limits.
Omitting `assignments`/`peers` returns a work inventory with uncovered pages.

```json
{
  "action": "page_dispatch",
  "assignments": [
    {
      "page": "overview",
      "phase": "review",
      "peer": "reviewer-overview",
      "state": "queued",
      "candidate_sha256": "<exact current candidate.json SHA-256>"
    }
  ],
  "peers": [
    {"peer": "reviewer-overview", "state": "queued", "authorized": true, "attached": true},
    {"peer": "reviewer-method", "state": "idle", "authorized": true, "attached": true}
  ]
}
```

Use exact identities from the page plan/native peer tools; `session:` is normalized.
Assignment `phase` matches the page's work stage; `state` is `running`, `queued`
or `waiting`. Active assignments after make name the exact current candidate digest.
Waiting assignments require a specific `dependency`; they are unverified claims,
not coverage of a ready phase. A waiting reviewer may remain staged while its own
maker prepares a candidate. Production must stay with the declared page maker;
reviewers must be independent of every maker and the coordinator.

Peer `state` is `idle`, `running`, `queued`, `unknown` or `unavailable`.
Authorization and attachment default to false. Read current native execution
facets, pending input and connection status; an old result or retained wire ID
does not establish current availability. Preserve unknown instead of guessing.
A running assignment with an idle/queued roster state is inconsistent. Queued
assignments require a queued/running peer; an idle peer has not received that job.
Dispatch
to different existing peers can use parallel `peer_send_input` calls; the four
new-peer handoff limit is separate and remains intact.

The checker returns `ready_unassigned`, `avoidable_serialization`,
`capacity_limited_serialization`, `invalid_assignments`, `eligible_idle_reviewers`,
`completed_page_assignments` and `declared_coverage_complete`. A shared reviewer
queue with unused eligible idle alternatives exposes avoidable serialization.
With no declared idle alternative, serialization is reported as a capacity
shortfall, not permission to override admission or create dummy turns.
`declared_running_count` counts distinct peers, not job rows.

All assignment/peer fields are coordinator declarations: **`host_verified` and
`agent_execution_verified` remain false even when declared coverage is complete**.
The checker does not authenticate those claims or enforce EndTurn. Verify real
turn start/end events separately. `all_work_waiting:false` rules out a blanket
wait when ready gaps/serialization exist; null means the checker cannot establish
external waiting. It never turns an unavailable/foreign peer into capacity.

Dispatch every ready independent page before long global audits or waiting.
Before reassigning queued work, confirm the old assignment is stood down; preserve
all review/finding history and avoid duplicate writers or reviewer shopping.


A current native review can name a peer role and a specific condition for
resuming. These pages appear in `needs_disposition` and, until assigned, in
`ready_unassigned` with their executable phase. Dispatch the peer through the
native host. Group pages sharing the same cause in one job and declare each
candidate separately. A technical result goes to the original makers; an
arbitration result goes to the original reviewers. Both require independent
evidence before any acceptance can change. If peer investigation establishes
that no authorized path meets the contract, finish the attempt as blocked with
the affected pages, tested alternatives and missing capability. Resume on a
material change. Repeated audits and human waivers are not resolution steps.
