# CI policy

Status: adopted 2026-10. Owner: repo maintainers. See also
[`.github/BRANCH_LIFECYCLE.md`](../.github/BRANCH_LIFECYCLE.md).

## Why this exists

The repo reached **732 remote branches** because two things were true at once:

1. `delete_branch_on_merge` was **off**, so every merged PR left its branch
   behind.
2. `main` had a `ci-required` aggregate job but **no required status checks**,
   so a red CI never actually blocked a merge.

Branch naming was also free-form, which makes automated cleanup risky. This
policy fixes the causes, not just the symptom.

## What is already enforced (do not duplicate)

`.github/workflows/ci.yml` already runs, on every PR:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- the test matrix (`octos-core`, `octos-agent`, `octos-cli`, `octos-bus`,
  `octos-llm`, `octos-pipeline`, skills, doc tests, …)
- `typos`, the UX gate report, Windows deploy-script checks, the security scan
- a `CI required` aggregate job

The additions in this policy are deliberately **thin** — one text-only ratchet
workflow, branch naming, the cleanup sweep, and the weekly report. Nothing here
recompiles the workspace.

## Rules

### 1. Branch naming is enforced

`.github/workflows/ci-branch-naming.yml` fails a PR whose head branch does not
match `<type>/<description>` (lower-case). Valid types: `feat`, `feature`,
`fix`, `hotfix`, `docs`, `ci`, `chore`, `refactor`, `release`, `test`, `perf`,
`review`, `codex`, `appcard`. Bots (`dependabot/`, `renovate/`,
`gh-readonly-queue/`) are exempt. Legacy open branches are listed in
`.github/branch-name-allowlist.txt`, which should only ever shrink.

### 2. Quality debt may not grow — the ratchet

`scripts/quality-ratchet.sh` measures three metrics and fails if any of them
gets **worse** than `.github/quality-baseline.tsv`:

| metric | meaning |
| --- | --- |
| `unwrap_panic_prod` | `unwrap()` / `panic!()` / `unreachable!()` / `todo!()` / `unimplemented!()` in production code (test modules and `*_tests.rs` excluded; `.expect("msg")` allowed) |
| `files_over_3000` | source files longer than 3000 lines |
| `max_file_lines` | the largest source file |

**Why a ratchet and not a hard limit.** The repo currently has 586 production
`unwrap/panic` calls, 53 files over 3000 lines, and a 50 509-line file. Any
absolute threshold would fail on `main` itself and be disabled within a day. A
ratchet lets existing debt stand while making new debt visible and blocking.

Baseline at adoption:

```
unwrap_panic_prod  586
files_over_3000     53
max_file_lines   50509
```

If a change legitimately increases a metric, run
`scripts/quality-ratchet.sh --update` and commit the baseline in the same PR so
reviewers can see the increase.

### 3. Branch cleanup runs weekly

`.github/workflows/cleanup-stale-branches.yml` runs
`scripts/branch-cleanup.sh` every Sunday. It deletes a branch **only if**

- it is an ancestor of `main`, **or** every patch on it is already in `main`
  (squash/rebase-merged — checked with `git cherry`),
- it does not match a protected pattern (`main`, `develop`, `master`,
  `release/*`, `hotfix*`, `chore/release*`, `appcard/*`), and
- it has no open PR.

Every deletion is recoverable: the job uploads a SHA manifest and a
self-contained `git bundle` (90-day artifact). To restore a branch, fetch the
bundle and push its SHA:
`git fetch <bundle> 'refs/remotes/origin/*:refs/remotes/origin/*' && git push origin <sha>:refs/heads/<branch>`.

Branches with **unmerged** work are never deleted automatically — they are
listed by `scripts/branch-audit.sh` for a human to review.

### 4. Required checks and branch protection

Run `scripts/setup-branch-protection.sh` (dry-run) then
`scripts/setup-branch-protection.sh --apply`. It enables
`delete_branch_on_merge` and configures `main` protection:

- required status checks: **`CI required`** (+ the new gates once they are
  required), strict (branch must be up to date)
- ≥ 1–2 approving reviews, dismiss stale approvals, code-owner review
- conversation resolution required
- no force-push, no branch deletion

> Do not add the new workflows to the required list until they have run green at
> least once on a real PR, or PRs will hang on "Expected — waiting for status".

### 5. Reporting

`.github/workflows/ci-weekly-report.yml` opens a labelled issue every Monday
with branch totals, the number of merged-but-undeleted branches, the count of
branches idle >30 days, open PRs, and the recent failing workflow runs —
mirroring
[`docs/reports/ci-monthly-report-template.md`](reports/ci-monthly-report-template.md)
for the monthly roll-up.

## Escape hatches

| situation | do this |
| --- | --- |
| legitimately more `unwrap`/large file | `scripts/quality-ratchet.sh --update`, commit baseline |
| legacy branch name on an open PR | add to `.github/branch-name-allowlist.txt` (reviewed, temporary) |
| keep a branch that would be swept | ensure it has an open PR, or add its prefix to `--protect` in the workflow |
| restore a swept branch | `git push origin <sha>:refs/heads/<name>` from the artifact manifest |
| one-off audit | `scripts/branch-audit.sh --deep` |

## Adoption checklist

- [x] `scripts/branch-audit.sh`, `scripts/branch-cleanup.sh`, `scripts/quality-ratchet.sh`
- [x] `.github/workflows/{ci-branch-naming,ci-quality-gates,cleanup-stale-branches,ci-weekly-report}.yml`
- [x] `.github/pull_request_template.md`, `.github/CODEOWNERS`
- [x] `.github/quality-baseline.tsv`, `.github/branch-name-allowlist.txt`
- [ ] enable `delete_branch_on_merge` (repo settings)
- [ ] add `CI required` as a required status check
- [ ] run the first branch-cleanup sweep
- [ ] announce the policy to contributors before the first scheduled sweep
