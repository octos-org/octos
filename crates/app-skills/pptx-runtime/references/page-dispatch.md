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

## Dispatch from a single page table

For a shared-workspace v2 per-page job, write the task-wide requirements once in
the plan's optional `shared_brief` (short text or workspace document path). The
same ordered `pages` table that freezes ownership can include `spec`,
`reference`, `inputs` and `instructions` on each row. Use the real
`maker_session` and `reviewer_session` of the attached peers. The plan's rubric
and minimum score apply to every row. Set one top-level `canvas` with `width_px`, `height_px`, `width_in`, and `height_in`. At initialization the runtime freezes this value, or uses the first page's existing canvas or image aspect ratio when available. Every maker and reviewer receives the same canvas. Page specs and post-build PPTX packages must match its physical dimensions before review.

After evidence_init, call `pptx_tool({"action":"page_assignments"})`, then send
the returned `peer_send_input` object to the host tool of that name. One call
queues all eligible makers. Each assigned reviewer receives a job when its page has a published candidate. The
runtime fills its two role templates with the page's own data. The host reports
each queued, previously dispatched or failed assignment.
After dispatch, consume outstanding publications with `peer_gather({"with_cursor":true})`. While page jobs are active, pass the returned cursor as `after_cursor` and set `wait_ms` (up to 60000) with the same peer selection for a wake on the next publication. A shell sleep delays downstream dispatch until its timer expires.

For several local colour checks on aligned originals, use one native request. Rectangles use image pixels with the same origin and scale on both inputs. A different bottom margin is supported when every rectangle fits both originals. The receipt records the input hashes and raw measurements for peer review.

```json
{"action":"compare","before":"inputs/reference.png","after":"run/page/render.png","regions":[{"id":"title-ink","box":[80,100,1,1]},{"id":"background","box":[40,90,12,10]}],"receipt":"run/page/colour-check.json"}
```

The host queues covered review, arbitration and repair events as each page publishes. Follow the dispatch receipt. On recovery or a missing receipt, refresh the full table with `page_assignments` and send its fresh batch. For a targeted response use `only_page` and reconcile the full table before the next wait, after a partial failure or when the coordinator resumes. The full-table dispatch skips covered work. The reviewer receives the current
candidate ID, and a repair goes to the original maker with the verdict ID.
The native build snapshots an assigned maker’s planned page source, including two-step build and post-build publication. A later draft edit leaves the reviewer’s version intact. With a prepared page table, publication pins a valid candidate while its assigned reviewer decides. An identical publish returns the same handoff. A current actionable rejection lets the maker publish its next candidate. Completed pages produce no jobs. Specialist referrals appear in
`specialist_pages` for the coordinator's existing resolution flow. A changed
batch or page requires fresh preparation. Retry a partial delivery after fixing
the reported peer binding; previously queued work keeps its original receipt.
If an assigned turn ends with a recoverable partial result, first reconcile the current page and the peer's latest terminal round. Call `page_assignments(only_page, resume_after_round)` using that round and send its batch. The owner receives its existing role template and continues from its page artifacts. The host checks that the original assignment actually finished, verifies the native terminal round and accepts at most one resume for this page, phase, evidence and round. A completed page yields no work. When the provider repeatedly rejects the same image at the same historical message index, a new turn can resend it unchanged. Use the host's supported context compaction on the idle affected peer, retain its identity and file evidence, and resume from the current candidate with bounded `review_preview` images. Confirm a subsequent model or tool event before reporting recovery. Use the existing stagnation and blocker rules if the same cause persists.
On hosts without the `pptx_batch` argument use the normal individual
peer_send_input calls. In all cases the host's turn events establish execution.
