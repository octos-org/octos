# Evidence-driven continuation

Issue: workers can finish with known unresolved checks, or repeatedly alter artifacts
without changing checked outcomes. Benefit: an explicitly supervised task gets a
bounded evidence-based repair opportunity and a truthful handoff; no total iteration
cap or automatic visual approval.

Scope: opt-in `.octos/continuation.json` in an isolated task workspace, a versioned
producer evidence snapshot, a small durable observer shared by conversation and task
loops, and a PPTX skill adapter. No slide-specific data or model-name logic. No new
model calls, quality scoring, or repair decisions in the kernel. Existing failure
transport carries `needs_replan`; a new UI status is not part of this slice.

A round is a fresh completed check snapshot bound to actual artifact bytes. Pending
jobs and rereads never count. Required check identities are frozen by the policy.
Missing checks stay unknown. Progress is a new best checked status or decreasing
residual, without regressions against the preceding snapshot. Oscillation is not
progress. A warning after a configurable flat window precedes a subsequent handoff.
EndTurn permits one correction request for each incomplete evidence snapshot; a
repeated unchanged claim yields instead of starting an endless veto loop.

Persist the last attempt and progress baseline outside model context. Restart cannot
reset the detector merely by saying continue. A new diagnosis can be checked as a
separate declared check; model-authored prose and modified timestamps are not evidence.

Proving tests: productive changes beyond the window; flat artifact churn; pending
jobs; duplicate/replayed attempts; oscillation; regressions; missing/deleted checks;
stale hashes; path escape; observer restart; no-policy behavior; premature EndTurn
in conversation and task execution; no extra verifier model calls. Adapter tests
cover check provenance, exact bindings, and unknown visual checks.

Session-backed chat ownership uses `session:<exact-session-key>` across recreated
OUP Agents and resumes. Native run_task retains its own AgentId, even when it
has a parent session key. Different peer/coordinator sessions sharing a workspace
do not consume each other's evidence policy. Tests exercise this execution wiring.
