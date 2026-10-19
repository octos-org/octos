<!--
Keep PRs small and single-purpose. A PR that fixes one thing and refactors
three others is hard to review and hard to revert. Split it.
-->

## What & why

<!-- One paragraph: what changed and the problem it solves. Link the issue. -->

Closes #

## Type

- [ ] `feat/` — new feature
- [ ] `fix/` — bug fix
- [ ] `refactor/` — behaviour-preserving refactor
- [ ] `docs/` — documentation only
- [ ] `ci/` — CI/CD only
- [ ] `chore/` — maintenance, deps, cleanup
- [ ] `perf/` — performance

## Checklist

- [ ] Branch name follows `.github/BRANCH_LIFECYCLE.md`
- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] Relevant tests pass; new behaviour has a test
- [ ] `scripts/quality-ratchet.sh` passes (no new debt — see `docs/CI_POLICY.md`)
- [ ] No new `unwrap()` / `panic!()` in production paths (tests may use them)
- [ ] New source files are under 3000 lines; large existing files were not grown
- [ ] No new dependency, or the PR description justifies it
- [ ] Docs updated for user-visible changes
- [ ] CHANGELOG / release notes not required (release.yml generates them)

## Performance impact

- [ ] None / negligible
- [ ] Improvement (describe)
- [ ] Regression (describe, and why it is acceptable)

## Breaking changes

- [ ] None
- [ ] Yes — describe the migration and note it for the release notes

## How it was verified

<!-- Exact commands, environments, and results. "It builds" is not verification
     for anything that touches runtime behaviour. -->
