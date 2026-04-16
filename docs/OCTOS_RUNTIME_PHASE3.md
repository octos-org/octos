# Octos Runtime Phase 3

Phase 3 starts after the Phase 2 runtime hardening work is green on live canary.
The goal is to exploit the new foundation instead of continuing to churn the same
runtime seams.

## Lanes

1. Canary soak and regression triage
   - Watch `#407` counters on real traffic.
   - File bugs only for observed failures.
   - Avoid speculative redesign during the soak.

2. Open-ended coding and debugging loops
   - Improve long code-task retries, repair turns, and bounded delegation.
   - Keep child-session fanout policy explicit and observable.

3. Hard-case live acceptance
   - Move from workflow demos to adversarial coding tasks.
   - Cover repo edits, failing-test repair, fanout/join, idle resume, and
     concurrent load.

4. Operator surface
   - Turn raw counters into a human-usable runtime summary.
   - Keep the first surface simple and scriptable.

## First Operator Surface

The first operator-facing summary is intentionally small:

- API endpoint: `/api/admin/operator/summary`
- CLI command: `octos admin operator-summary`

It summarizes the existing Prometheus counters into a compact JSON or terminal
view with these categories:

- retries
- timeouts
- duplicate suppressions
- orphaned child sessions
- workflow phase transitions
- result delivery paths/outcomes
- session replay/persist/rewrite counts
- child-session lifecycle counts

Example:

```bash
octos admin operator-summary \
  --base-url https://dspfac.crew.ominix.io \
  --auth-token "$OCTOS_AUTH_TOKEN"
```

For automation:

```bash
octos admin operator-summary --json
```

## Hard-Case E2E Scaffold

Phase 3 adds a dedicated repo-level scaffold:

- `e2e/tests/coding-hardcases.spec.ts`
- script: `npm run test:live:coding`

The scaffold defines the target live proofs without pretending they are already
green:

- bounded repo edit with reviewable diff
- failing test then repair in one session
- bounded child-session fanout/join for coding work
- long idle resume without duplicate turns
- concurrent coding sessions under load

These remain `fixme` until the coding-runtime lanes provide deterministic
fixtures and orchestration hooks.

## Acceptance for Phase 3 Kickoff

The kickoff is complete when:

- the issue set exists on GitHub
- the operator summary endpoint and CLI command are merged
- the coding hard-case suite is scaffolded in repo `e2e`

The broader Phase 3 program is complete only when the new coding hard cases run
green against a live canary.
