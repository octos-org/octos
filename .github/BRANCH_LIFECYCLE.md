# Branch lifecycle

This is the contributor-facing half of [`docs/CI_POLICY.md`](../docs/CI_POLICY.md).
It answers one question: *what do I name my branch, and when does it go away?*

## Naming

```
<type>/<description>
```

| type | use for | example |
| --- | --- | --- |
| `feat/` | new feature | `feat/2789-cache-economics` |
| `feature/` | legacy spelling of `feat/` | `feature/ominix-runtime` |
| `fix/` | bug fix | `fix/2498-monitor-budget` |
| `hotfix/` | urgent fix off a release | `hotfix/2.0.3-rc2` |
| `docs/` | documentation only | `docs/readme-refresh` |
| `refactor/` | behaviour-preserving refactor | `refactor/r1-peers-dead-code` |
| `ci/` | CI/CD, workflows, tooling | `ci/cancelled-not-fail` |
| `chore/` | maintenance, deps, cleanup | `chore/model-catalog` |
| `test/` | test-only change | `test/peer-supervisor-wiring` |
| `perf/` | performance | `perf/voice-tts-latency` |
| `release/` | release branch (protected) | `release/v2.0.3` |

Rules: lower-case; the part after `/` starts with a letter or digit and may
contain only `[a-z0-9._-]`. `dependabot/`, `renovate/` and
`gh-readonly-queue/` are exempt. The check is
`.github/workflows/ci-branch-naming.yml`; exceptions live in
`.github/branch-name-allowlist.txt` and should be removed once the branch is
gone.

## Create

- Branch from up-to-date `main`.
- Name it per the table above. CI enforces this on the PR.
- Open the PR early (draft is fine) and link the issue — a branch with an open
  PR is never swept by the automated cleanup.

## While working

- Commit and push often. Long-idle branches rot and become expensive to merge.
- Rebase on `main` before requesting review. `main` protection uses
  "require branches to be up to date", so a stale branch cannot merge.

## Merge

- At least one approval (see `docs/CI_POLICY.md` for the current setting);
  stale approvals are dismissed on new pushes.
- All required checks green — `CI required` is the single aggregate gate.
- Squash or rebase to keep `main` linear-friendly; both are enabled.
- The branch is deleted automatically on merge (`delete_branch_on_merge`).

## Delete

| branch | deleted |
| --- | --- |
| merged into `main` (incl. squash/rebase) | automatically on merge, and by the weekly sweep if missed |
| no commit in 30 days, unmerged, no open PR | **not** automatic — appears in the audit for human review |
| ancestor of `main` / patch-equivalent | automatically by the weekly sweep |
| `backup/`, `archive/` snapshots | treated as stale; swept once idle |
| `main`, `develop`, `master`, `release/*`, `hotfix*`, `chore/release*`, `appcard/*` | never |
| any branch with an open PR | never |

Every sweep is reversible: the workflow uploads a SHA manifest and a git bundle
for 90 days. Restore with
`git fetch <bundle> 'refs/remotes/origin/*:refs/remotes/origin/*'` then
`git push origin <sha>:refs/heads/<branch>`.

## Weekly sweep vs. audit

- `scripts/branch-cleanup.sh` — **deletes** only provably-merged branches.
- `scripts/branch-audit.sh --deep` — **reports** merged, patch-equivalent,
  stale, and valuable branches. Use it to decide what to review or close.

See [`docs/reports/branch-audit-2026-10.md`](../docs/reports/branch-audit-2026-10.md)
for the baseline audit that started this policy.
