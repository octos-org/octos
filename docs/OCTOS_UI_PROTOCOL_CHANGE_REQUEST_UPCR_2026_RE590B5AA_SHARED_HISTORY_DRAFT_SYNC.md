# UPCR-2026-RE590B5AA — Test-only: shared-history draft synchronization

Status: proposed (test-only, no contract change)
PR: TBD

## Decision record

`UPCR-2026-RE590B5AA`.

Test-only change: `should_show_the_person_lane_a_system_agent_turn_waiting_for_approval`
waits for the running turn's live draft to contain the streamed text before
asserting the shared-history block. The pending approval is inserted by the
agent task while the draft is folded in by the turn-loop consumer task; on a
loaded runner (merge-group run 41804229471) the approval could be observed
first, dropping the `(in progress)` row and flaking the merge queue.

No wire shape, capability, or behavior change; the assertion content is
unchanged.
