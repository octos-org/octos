#!/usr/bin/env bash
#
# quality-ratchet.sh — stop quality debt from growing without rewriting the
# whole codebase first.
#
# The repo already carries a lot of pre-existing debt (thousands of
# unwrap/panic calls, several files over 3k lines). A hard threshold would fail
# on every PR — including main. Instead we ratchet: measure the current numbers
# into .github/quality-baseline.tsv and fail a PR only when a metric gets
# *worse* than the baseline. Improvements are always welcome; a deliberate
# increase has to be done explicitly with `--update`, so reviewers see it in
# the diff.
#
# Metrics (all "lower is better"):
#   unwrap_panic_prod  unwrap()/panic!()/unreachable!()/todo!()/unimplemented!()
#                      in production code. `#[cfg(test)]` modules, *_tests.rs
#                      and tests/ are excluded, and `.expect("msg")` is allowed.
#   files_over_3000    source files longer than 3000 lines — no new monoliths.
#   max_file_lines     the largest source file — must not keep growing.
#
# Usage:
#   scripts/quality-ratchet.sh            # check (exit 1 if any metric grew)
#   scripts/quality-ratchet.sh --update   # rewrite the baseline from current tree
#
set -euo pipefail

BASELINE=".github/quality-baseline.tsv"
SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/quality-ratchet.sh"
REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "${QUALITY_RATCHET_ROOT:-$REPO_ROOT}"

UPDATE=0
METRICS_ONLY=0
[[ "${1:-}" == "--update" ]] && UPDATE=1
[[ "${1:-}" == "--metrics-only" ]] && METRICS_ONLY=1

# --- production unwrap/panic, best-effort `#[cfg(test)]` stripping ----------
metric_unwrap_panic_prod() {
  find crates -path '*/src/*' -name '*.rs' ! -name '*_tests.rs' -print0 2>/dev/null \
  | while IFS= read -r -d '' f; do
      awk '
        {
          if (intest) {
            o=gsub(/\{/,"{"); c=gsub(/\}/,"}"); depth+=o-c; if (o>0) seen=1;
            if (seen && depth<=0) { intest=0; seen=0 }
            else if (!seen && depth<=0 && /;[[:space:]]*$/) { intest=0 }
            next
          }
          if (/#\[cfg\(test\)\]/) {
            intest=1; depth=0; seen=0;
            o=gsub(/\{/,"{"); c=gsub(/\}/,"}"); depth+=o-c; if (o>0) seen=1;
            if (seen && depth<=0) { intest=0; seen=0 }
            next
          }
          print FILENAME":"FNR":"$0
        }
      ' "$f"
    done \
  | grep -cE '\.unwrap\(\)|panic!\(|unreachable!\(|todo!\(|unimplemented!\(' || true
}

metric_files_over_3000() {
  find crates -path '*/src/*' -name '*.rs' -exec wc -l {} + 2>/dev/null \
    | awk '$2 != "total" && $1 > 3000' | wc -l | tr -d ' '
}

metric_max_file_lines() {
  find crates -path '*/src/*' -name '*.rs' -exec wc -l {} + 2>/dev/null \
    | awk '$2 != "total" {print $1}' | sort -rn | head -1
}

METRICS="unwrap_panic_prod files_over_3000 max_file_lines"

cur() {
  case "$1" in
    unwrap_panic_prod) metric_unwrap_panic_prod ;;
    files_over_3000)   metric_files_over_3000 ;;
    max_file_lines)    metric_max_file_lines ;;
    *) echo "unknown metric: $1" >&2; return 1 ;;
  esac
}

if [[ "$METRICS_ONLY" == 1 ]]; then
  for m in $METRICS; do printf '%s\t%s\n' "$m" "$(cur "$m")"; done
  exit 0
fi

valid_metrics_file() {
  local file=$1 name count
  [[ -f "$file" ]] || return 1
  for name in $METRICS; do
    count=$(awk -F '\t' -v name="$name" '$1 == name && $2 ~ /^[0-9]+$/ { n++ } END { print n + 0 }' "$file")
    [[ "$count" == 1 ]] || return 1
  done
}

if [[ "$UPDATE" == 1 ]]; then
  {
    echo "# metric<TAB>baseline — see scripts/quality-ratchet.sh"
    echo "# A PR fails when a metric grows past its baseline. Raise deliberately"
    echo "# with: scripts/quality-ratchet.sh --update"
    for m in $METRICS; do printf '%s\t%s\n' "$m" "$(cur "$m")"; done
  } > "$BASELINE"
  echo "updated $BASELINE:"
  grep -v '^#' "$BASELINE" | sed 's/^/  /'
  exit 0
fi

[[ -f "$BASELINE" ]] || { echo "error: missing $BASELINE" >&2; exit 1; }

# PRs fail only for debt introduced relative to their merge-base. Compute the
# base metrics in a detached temporary worktree so the current checkout and its
# files are never changed. If the base cannot be proven, retain the old strict
# baseline-only failure behavior rather than silently allowing a regression.
base_metrics_available=false
base_metrics_file=""
base_tmp=""
base_tree=""
cleanup_base() {
  if [[ -n "$base_tree" && -d "$base_tree" ]]; then
    git worktree remove --force "$base_tree" >/dev/null 2>&1 || true
  fi
  if [[ -n "$base_tmp" ]]; then
    rm -rf -- "$base_tmp"
  fi
}
trap cleanup_base EXIT
if [[ "${GITHUB_EVENT_NAME:-}" == "pull_request" ]]; then
  base_tmp=$(mktemp -d "${TMPDIR:-/tmp}/octos-quality-ratchet.XXXXXX")
  base_tree="$base_tmp/tree"
  base_metrics_file="$base_tmp/metrics.tsv"
  base_sha=""
  if ! base_sha=$(git rev-parse --verify HEAD^1 2>/dev/null); then
    # actions/checkout uses fetch-depth: 1 for this job. Deepen only the
    # current merge commit enough to expose HEAD^1; never switch the checkout.
    if [[ -n "${GITHUB_SHA:-}" ]]; then
      git fetch --no-tags --depth=2 origin "$GITHUB_SHA" >/dev/null 2>&1 || true
    fi
    base_sha=$(git rev-parse --verify HEAD^1 2>/dev/null) || base_sha=""
  fi
  if [[ -n "$base_sha" ]] \
    && git worktree add --detach "$base_tree" "$base_sha" >/dev/null 2>&1 \
    && QUALITY_RATCHET_ROOT="$base_tree" bash "$SCRIPT_PATH" --metrics-only > "$base_metrics_file" \
    && valid_metrics_file "$base_metrics_file"; then
    base_metrics_available=true
  else
    echo "::warning::quality ratchet: base metrics unavailable; falling back to baseline-only comparison"
  fi
fi

fail=0
printf '%-20s %10s %10s  %s\n' metric baseline current status
while IFS=$'\t' read -r name base; do
  [[ "$name" =~ ^#.*$ || -z "$name" ]] && continue
  now=$(cur "$name")
  status="ok"
  if (( now > base )); then
    if [[ "$base_metrics_available" == true ]]; then
      base_now=$(awk -F '\t' -v name="$name" '$1 == name { print $2 }' "$base_metrics_file")
      if (( now <= base_now )); then
        status="ok (base $base_now)"
      else
        status="REGRESSED (base $base_now)"
        fail=1
      fi
    else
      status="REGRESSED"
      fail=1
    fi
  elif (( now < base )); then status="improved"; fi
  printf '%-20s %10s %10s  %s\n' "$name" "$base" "$now" "$status"
done < "$BASELINE"

if (( fail )); then
  echo
  echo "::error::quality ratchet failed — a metric grew past its baseline."
  echo "Reduce the debt, or if the increase is intended run:"
  echo "    scripts/quality-ratchet.sh --update"
  echo "and commit the updated baseline so reviewers can see it."
  exit 1
fi

echo
echo "quality ratchet ok."