# UPCR-2026-RE3B7C892 — WS goal observer test coverage (test-only)

Status: proposed
PR: #3315

## Decision record

This PR adds two test-only regressions for the existing goal task-row observer installation the issue lists as its two WS/transport sites: the per-turn installer in `run_standalone_turn` and the cached-supervisor adapter (`wire_goal_task_row_observers_for_cached_supervisor`, one body shared by its three call sites, covered via `skill/action/job/list`). It also records those exact test names in `crates/octos-cli/TESTS-KEPT.md`. The background-invoke and native-review call sites are not individually covered; they were dropped to fit the 150-line per-PR test budget (shu 11-25).

**No production code or UI Protocol contract changes.** No RPC method, payload schema, notification variant, capability, feature token, serialization, replay envelope, dispatcher behavior, or client-visible compatibility rule is added, removed, or altered. The OUP specification and client codecs need no corresponding change. The protocol-visible gate is triggered only because the tests live in UI-protocol test source files.

Each regression drives the real request/turn and checks the actual SQLite effect: an owed crash-window row (task JSONL says Completed, goal row still running) must be reconciled to complete by the production restore. Independently deleting the corresponding production installer makes the test fail. This adds deletion-fail coverage of existing behavior; it does not implement the separate cleared/replaced-goal global sweep tracked by #2089.

Cite this record as `UPCR-2026-RE3B7C892`.
