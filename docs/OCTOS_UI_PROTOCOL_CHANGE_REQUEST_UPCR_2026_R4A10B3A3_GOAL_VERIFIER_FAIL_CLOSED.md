# UPCR-2026-R4A10B3A3 — Goal verifier fail-closed lane selection

Status: proposed (implemented for review)
Task: T-0276 PR A / D-20 (T-0265)

## Decision record

No wire or DTO contract change. The interactive/sentinel goal-verification
turns in `ui_protocol_transport.rs` now select the verifier lane through
`runtime::profile::select_goal_verifier_provider`:

- an UNCONFIGURED `goal_verifier` sub-provider keeps the pre-#1935 default —
  the turn's own provider grades (self-grading fallback);
- a CONFIGURED lane whose construction fails yields a persistent
  fail-closed `FailedGoalVerifierProvider` (verification reports a
  typed failure) instead of silently falling back to self-grading.

This preserves the existing wire surface, turn/ledger semantics, and
verification pairing; only the failure semantics of a misconfigured
verifier lane change (degraded visibility instead of false success).
