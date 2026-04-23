# M7.8 swarm contract fixture

A 3-variant parallel fanout contract consumed by the M7.8 live release gate.

## What it exercises

Three sub-agents each produce a distinct fibonacci implementation (iterative,
recursive, memoized). The aggregate is three short code artifacts plus a
validator check that every subtask delivered non-empty output.

The point is orchestration coverage, not code quality:

- Sub-agents spawn and reach terminal state (M7.1 lifecycle).
- `HarnessEventPayload::SwarmDispatch` + `SubAgentDispatch` events fire.
- Cost ledger attributes spend per sub-contract (M7.4).
- Aggregate validator runs after all subtasks settle (M4.3).
- Redb persistence survives a config reload (`/admin/api/reload`).

## Consumers

- `scripts/validate-m7-swarm-live.sh` — POSTs this contract to
  `/api/swarm/dispatch` (M7.6) or to `octos mcp-serve` as a fallback.
- `e2e/tests/swarm-dispatch-gate.spec.ts` — authors this contract via the
  SwarmPage UI and watches live progress.
