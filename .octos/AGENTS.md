# Agent Instructions

## Release Control First

When working on release-critical tasks, do not drift into broad refactoring,
adjacent infrastructure work, or speculative cleanup. Work from the synced
baseline and follow the active tracked contract.

Primary control documents:

- `docs/OCTOS_RELEASE_CONTRACT_2026-04-17.md`
- `docs/OCTOS_RUNTIME_PHASE3_CONTRACT.md`
- `docs/OCTOS_HARNESS_MASTER_PLAN.md`

## Baseline Discipline

- Prefer synced `origin/main` over stale or dirty local worktrees.
- If the local checkout is dirty or behind, preserve the dirt, sync first, and
  continue from the clean baseline.
- Do not build or validate a release candidate from a diverged local `main`.

## Scope Discipline

For a release slice:

1. name the exact issue served
2. name the exact user-visible behavior improved
3. name the exact test that proves it

If a change cannot answer those three questions, cut it.

Do not add "while I am here" refactors during a release slice.

## Parallel Work Discipline

Use subagents only for bounded, non-overlapping lanes:

- one implementation owner on the critical path
- separate verification lanes for browser/deploy checks
- no overlapping write ownership

The main controller stays focused on orchestration, scope, deploy, and final
go/no-go decisions.

## Release Truth

- Use the designated public canary as release truth.
- Treat raw backend ports, broken ingress hosts, and bootstrap/admin-only hosts
  as verification lanes, not as release truth.

## Persistence

If a release contract or phase plan is important, persist it in tracked repo
docs before relying on chat context alone.
