# Octos Swarm M7 Live Gate

Date: 2026-05-25
Issue: [`#511`](https://github.com/octos-org/octos/issues/511)
Milestone: `M7.8` (Swarm Dispatch Live Gate)

This runbook defines the repo-side live release gate for the M7 swarm
orchestrator family. The gate is canary-only and is skipped in default e2e
runs unless `OCTOS_M7_SWARM_LIVE=1` is explicitly set.

## What The Gate Proves

The M7 live gate validates one complete swarm dispatch on a real canary:

1. M7.5 dispatch starts a three-sub-agent swarm through the M7.1 MCP path.
2. M4.1A progress events are visible while the swarm runs.
3. Each sub-agent produces durable artifact evidence.
4. M7.4 cost ledger attribution is visible per sub-agent.
5. M7.3 Matrix room and puppet registration evidence is visible.
6. The M4.3 aggregate validator evidence is present for the combined result.
7. Re-opening the session and reloading the browser preserves swarm state.

The gate does not mock the backend. If the canary lacks any M7.1 through M7.6
dependency, the explicit live run must fail with a structured diagnostic.

## Mandatory Command

Supervisor runs:

```bash
./scripts/validate-m7-swarm-live.sh \
  --base-url https://dspfac.crew.ominix.io \
  --auth-token "$OCTOS_ADMIN_TOKEN" \
  --profile dspfac \
  --output-dir /tmp/m7-swarm-live-$(date -u +%Y%m%d-%H%M%S)
```

The script refuses `dspfac.ocean.ominix.io` because mini5 is reserved for the
coding-green lane.

## Playwright Spec

The script runs:

```bash
npm --prefix e2e run test:live:swarm
```

The spec at `e2e/tests/swarm-dispatch-gate.spec.ts` lists exactly five tests:

- `dispatches three sub-agents and emits progress`
- `records per-subagent task and cost attribution`
- `delivers artifacts and aggregate validator evidence`
- `creates Matrix room and puppet evidence`
- `preserves swarm state after protocol and browser reload`

To list the tests without live traffic:

```bash
./scripts/validate-m7-swarm-live.sh --list
```

## Fixture

`e2e/fixtures/m7-swarm-expected.json` is the authoritative contract shared by
the script and spec. It defines the required sub-agent count, artifact count,
Matrix evidence, validator evidence, prompt, markers, polling intervals, and
diagnostic schema.

Any change to the live-gate expectations must update the fixture, spec, and
runbook in the same PR.

## Diagnostics

Every failed script run writes:

```text
<output-dir>/diagnostic.json
```

The diagnostic JSON includes:

- `schema`: `octos.swarm.m7.live_gate.diagnostic.v1`
- `status`: `failed` or `passed`
- `kind`: machine-readable failure code
- `detail`: human-readable failure summary
- `issue`: `511`
- `base_url`
- `profile`
- `session_id`
- `timestamp`

The Playwright spec writes specific diagnostics for missing sub-agents,
progress, artifacts, cost rows, Matrix evidence, validator evidence, and reload
durability. If Playwright fails before the spec can write a specific diagnostic,
the shell script writes a generic `playwright_failed` diagnostic.

## Closure Criteria

Do not close issue `#511` from a PR that only adds this harness. Closure
requires an actual live canary result where:

- `./scripts/validate-m7-swarm-live.sh --base-url https://dspfac.crew.ominix.io --auth-token "$TOK"` exits `0`
- `npx playwright test --list tests/swarm-dispatch-gate.spec.ts` lists the five tests above
- the PR or issue comment includes the commit SHA, canary host, command,
  diagnostic path or artifact, result, and any remaining gaps

Until that evidence exists, PRs should use `Refs #511`, not `Closes #511`.
