#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

usage() {
  cat <<'EOF'
Usage: ./scripts/ci-api-filter-test.sh <filter>... [-- <libtest args>...]

Run the `api`-gated octos-cli tests selected by one or more name filters,
failing when ANY filter selects zero tests.

A bare `cargo test <filter>` reports `ok` for a filter that matches nothing,
so a renamed test silently drops out of CI while the step stays green
(#2029). Guard each filter with `--list` first, then run the union exactly
as the unguarded command would — and fail if the run executed nothing
(e.g. the surviving selection is entirely `#[ignore]`d).
EOF
}

filters=()
while (($#)) && [[ $1 != -- ]]; do
  filters+=("$1")
  shift
done
(($#)) && shift # drop the `--` separator; the rest are libtest args

if ((${#filters[@]} == 0)); then
  usage >&2
  exit 2
fi

for filter in "${filters[@]}"; do
  # Same selection shape as the real run below; stderr streams to the log.
  if ! list="$(cargo test -p octos-cli --features api -- "$filter" --list "$@")"; then
    exit 1
  fi
  count="$(printf '%s\n' "$list" | grep -c ': test$' || true)"
  if ((count == 0)); then
    echo "::error::api-feature CI filter '$filter' matches zero tests — a dead filter reports 'ok' without running anything (#2029). Update the filter in the invoking workflow or restore the renamed test." >&2
    exit 1
  fi
done

run_log="$(mktemp)"
trap 'rm -f "$run_log"' EXIT
cargo test -p octos-cli --features api -- "${filters[@]}" "$@" 2>&1 | tee "$run_log"
# `0 passed` per binary is normal for unselected targets; the step only
# means something if at least one binary actually executed a test. Assert
# on the passed count rather than the `running N` line — the latter's
# format varies for ignored-only selections across toolchains.
if ! grep -Eq '^test result: ok\. [1-9][0-9]* passed' "$run_log"; then
  echo "::error::api-feature CI filters (${filters[*]}) passed zero tests — the selection survives only as ignored/non-executed entries (#2029)." >&2
  exit 1
fi
