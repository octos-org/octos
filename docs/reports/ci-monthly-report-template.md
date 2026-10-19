# Monthly CI & repo report — template

> Copy the body below into `docs/reports/ci-report-YYYY-MM.md`, fill the numbers
> from the weekly issues and `scripts/branch-audit.sh --deep`, and open it as a
> PR. The weekly workflow (`ci-weekly-report.yml`) produces the raw inputs.

---

# CI & repo report — YYYY-MM

## Branch health

| metric | start of month | end of month | Δ |
| --- | --- | --- | --- |
| Remote branches | | | |
| Merged into main, not yet deleted | | | |
| Idle > 30 days (unmerged) | | | |
| Idle > 90 days (unmerged) | | | |
| Open PRs | | | |

**Target:** branch count flat month-over-month. A rise means merged PRs are not
deleting their branches, or the sweep is failing.

## Cleanup activity

- Branches deleted this month:
- Branches restored from the recovery bundle (and why):
- Branches re-created after a sweep (should be zero):

## Quality ratchet

| metric | baseline | now | trend |
| --- | --- | --- | --- |
| `unwrap_panic_prod` | | | |
| `files_over_3000` | | | |
| `max_file_lines` | | | |

- Baselines raised this month (PR links + justification):
- Notable reductions:

## CI reliability

- Failed workflow runs (top 5 by count):
- Longest-running jobs / p95 duration:
- Flaky tests filed:

## Policy changes

- Naming rules, allowlist entries added/removed:
- Protection changes:
- New gates promoted to required:

## Actions for next month

1.
2.
3.
