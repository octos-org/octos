#!/usr/bin/env bash
#
# branch-audit.sh — classify remote branches so they can be cleaned up safely.
#
# Read-only: this script never deletes anything, it only prints a report.
# Compatible with bash 3.2 (macOS) and bash 5 (CI).
#
# Categories:
#   merged      tip is an ancestor of the base branch — safe to delete.
#   upstream    (--deep) not an ancestor, but every non-merge patch on the
#               branch already exists in the base by patch-id, i.e. it was
#               squash/rebase-merged. Also safe to delete.
#   stale       no commit in N months and not a protected pattern — delete
#               candidates, but eyeball the list first (may hold unmerged work).
#   valuable    touched within the recent window and still close to the base
#               branch — review for merge / a real PR before deleting.
#
# Usage:
#   scripts/branch-audit.sh [--base origin/main] [--stale-months 3]
#                           [--recent-days 30] [--max-behind 100]
#                           [--remote origin] [--fetch] [--deep]
#                           [--protect <regex>]... [--open-pr-file FILE]
#                           [--output FILE] [--quiet]
#
# Exit codes: 0 = report produced, 1 = usage/environment error.

set -euo pipefail

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------
BASE="origin/main"
REMOTE="origin"
STALE_MONTHS=3
RECENT_DAYS=30
MAX_BEHIND=100
DO_FETCH=0
DEEP=0
QUIET=0
OUTPUT=""
OPEN_PR_FILE=""

# Branches matching any of these EREs are never reported as deletable.
PROTECT_REGEXES=(
  '^origin/(HEAD|main|develop|master)$'
  '^origin/release/'
  '^origin/hotfix/'
  '^origin/hotfix-'
  '^origin/chore/release'
  '^origin/appcard/'
)

usage() {
  sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base)          BASE="$2"; shift 2 ;;
    --remote)        REMOTE="$2"; shift 2 ;;
    --stale-months)  STALE_MONTHS="$2"; shift 2 ;;
    --recent-days)   RECENT_DAYS="$2"; shift 2 ;;
    --max-behind)    MAX_BEHIND="$2"; shift 2 ;;
    --open-pr-file)  OPEN_PR_FILE="$2"; shift 2 ;;
    --protect)       PROTECT_REGEXES+=("$2"); shift 2 ;;
    --output)        OUTPUT="$2"; shift 2 ;;
    --fetch)         DO_FETCH=1; shift ;;
    --deep)          DEEP=1; shift ;;
    --quiet)         QUIET=1; shift ;;
    -h|--help)       usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage 1 ;;
  esac
done

cd "$(git rev-parse --show-toplevel)"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
# -> epoch for "N months ago" / "N days ago", portable across GNU and BSD date.
months_ago_epoch() {
  if date -d "1970-01-01" +%s >/dev/null 2>&1; then
    date -d "$1 months ago" +%s
  else
    date -v-"$1"m +%s
  fi
}
days_ago_epoch() {
  if date -d "1970-01-01" +%s >/dev/null 2>&1; then
    date -d "$1 days ago" +%s
  else
    date -v-"$1"d +%s
  fi
}

is_protected() {
  local ref="$1" re
  for re in "${PROTECT_REGEXES[@]}"; do
    [[ "$ref" =~ $re ]] && return 0
  done
  return 1
}

TMPDIR_AUDIT="$(mktemp -d "${TMPDIR:-/tmp}/branch-audit.XXXXXX")"
trap 'rm -rf "$TMPDIR_AUDIT"' EXIT

MERGED_F="$TMPDIR_AUDIT/merged"
UPSTREAM_F="$TMPDIR_AUDIT/upstream"
STALE_F="$TMPDIR_AUDIT/stale"
VALUABLE_F="$TMPDIR_AUDIT/valuable"
PROTECTED_F="$TMPDIR_AUDIT/protected"
OPEN_PR_F="$TMPDIR_AUDIT/open_pr"
for f in "$MERGED_F" "$UPSTREAM_F" "$STALE_F" "$VALUABLE_F" "$PROTECTED_F" "$OPEN_PR_F"; do : > "$f"; done

# Normalise the open-PR list (one branch name per line, owner: prefix stripped).
if [[ -n "$OPEN_PR_FILE" && -f "$OPEN_PR_FILE" ]]; then
  sed 's/#.*//' "$OPEN_PR_FILE" \
    | tr -d ' \t\r' \
    | sed 's/^[^:]*://' \
    | grep -v '^$' > "$OPEN_PR_F" || true
fi

is_open_pr() {
  [[ -s "$OPEN_PR_F" ]] && grep -qxF "$1" "$OPEN_PR_F"
}

ahead_behind() {  # echoes "behind ahead"
  git rev-list --left-right --count "$BASE...$1" 2>/dev/null | awk '{print $1, $2}'
}

if [[ "$DO_FETCH" == 1 ]]; then
  echo "fetching $REMOTE ..." >&2
  git fetch --prune --no-tags "$REMOTE" >&2
fi

if ! git rev-parse --verify --quiet "$BASE" >/dev/null; then
  echo "error: base ref '$BASE' does not exist (try --fetch)" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Collect every remote branch once
# ---------------------------------------------------------------------------
total=0
cutoff_stale=$(months_ago_epoch "$STALE_MONTHS")
cutoff_recent=$(days_ago_epoch "$RECENT_DAYS")

while IFS= read -r ref; do
  [[ -z "$ref" ]] && continue
  short="${ref#"$REMOTE"/}"
  # for-each-ref renders the remote HEAD symref as just "origin".
  [[ "$ref" == "$REMOTE" || "$ref" == "$BASE" ]] && continue
  total=$((total + 1))

  if is_protected "$ref"; then
    printf '%s\n' "$ref" >> "$PROTECTED_F"
    continue
  fi

  # A branch with an open PR is never a deletion candidate.
  if is_open_pr "$short"; then
    printf '%s (open PR)\n' "$ref" >> "$PROTECTED_F"
    continue
  fi

  ts=$(git log -1 --format=%ct "$ref" 2>/dev/null || echo 0)
  when=$(git log -1 --format=%cd --date=short "$ref" 2>/dev/null || echo "?")
  author=$(git log -1 --format=%an "$ref" 2>/dev/null || echo "?")
  subject=$(git log -1 --format=%s "$ref" 2>/dev/null || echo "")

  ab=$(ahead_behind "$ref" || echo "0 0")
  behind="${ab%% *}"; ahead="${ab##* }"

  merged=0
  [[ "$behind" == "0" ]] && merged=1

  if [[ "$merged" == 1 ]]; then
    printf '%s\n' "$ref" >> "$MERGED_F"
  elif [[ "$DEEP" == 1 ]]; then
    # every non-merge patch already present upstream => squash/rebase merged
    plus=$(git cherry "$BASE" "$ref" 2>/dev/null | grep -c '^+' || true)
    if [[ "$plus" == "0" ]]; then
      upstream_n=$(git cherry "$BASE" "$ref" 2>/dev/null | grep -c '^-' || true)
      printf '%s (%s upstream patches)\n' "$ref" "$upstream_n" >> "$UPSTREAM_F"
    fi
  fi

  if (( ts < cutoff_stale )); then
    printf '%s (最后更新: %s, 作者: %s, 落后 base %s commits, 最后一次提交: "%s")\n' \
      "$ref" "$when" "$author" "$behind" "$subject" >> "$STALE_F"
  fi

  if (( ts >= cutoff_recent )) && (( behind < MAX_BEHIND )) \
     && [[ ! "$short" =~ ^(backup|archive)/ ]]; then
    printf '%s (最后更新: %s, 新增: %s commits, 落后: %s, 最后: "%s")\n' \
      "$ref" "$when" "$ahead" "$behind" "$subject" >> "$VALUABLE_F"
  fi
done < <(git for-each-ref --format='%(refname:short)' "refs/remotes/$REMOTE" \
           | grep -v '/HEAD$' | sort)

count_lines() { [ -s "$1" ] && wc -l < "$1" | tr -d ' ' || echo 0; }

MERGED_N=$(count_lines "$MERGED_F")
UPSTREAM_N=$(count_lines "$UPSTREAM_F")
STALE_N=$(count_lines "$STALE_F")
VALUABLE_N=$(count_lines "$VALUABLE_F")
PROTECTED_N=$(count_lines "$PROTECTED_F")

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------
{
  printf '=== 已合并到 %s 的分支（祖先，可安全删除）===\n' "$BASE"
  if [ "$MERGED_N" -eq 0 ]; then printf '(none)\n'; else cat "$MERGED_F"; fi
  printf '...共 %s 个\n\n' "$MERGED_N"

  if [[ "$DEEP" == 1 ]]; then
    printf '=== 已 squash/rebase 合入 base 的分支（patch 等价，可安全删除）===\n'
    if [ "$UPSTREAM_N" -eq 0 ]; then printf '(none)\n'; else cat "$UPSTREAM_F"; fi
    printf '...共 %s 个\n\n' "$UPSTREAM_N"
  fi

  printf '=== 长期未更新（超过 %s 个月未提交，需人工确认）===\n' "$STALE_MONTHS"
  if [ "$STALE_N" -eq 0 ]; then printf '(none)\n'; else cat "$STALE_F"; fi
  printf '...共 %s 个\n\n' "$STALE_N"

  printf '=== 有价值的分支（最近 %s 天有更新且落后 base < %s commits，需审查是否合并）===\n' \
    "$RECENT_DAYS" "$MAX_BEHIND"
  if [ "$VALUABLE_N" -eq 0 ]; then printf '(none)\n'; else cat "$VALUABLE_F"; fi
  printf '...共 %s 个\n\n' "$VALUABLE_N"

  printf '=== 受保护（不参与清理）===\n'
  if [ "$PROTECTED_N" -eq 0 ]; then printf '(none)\n'; else cat "$PROTECTED_F"; fi
  printf '...共 %s 个\n\n' "$PROTECTED_N"

  safe=$((MERGED_N + UPSTREAM_N))
  printf '=== 汇总 ===\n'
  printf '总分支数(不含 base): %s\n' "$total"
  printf '受保护: %s\n' "$PROTECTED_N"
  printf '已合并(可删): %s\n' "$MERGED_N"
  [ "$DEEP" = 1 ] && printf 'patch 等价(可删): %s\n' "$UPSTREAM_N"
  [ "$DEEP" = 1 ] && printf '可安全删除合计: %s\n' "$safe"
  printf '长期未更新(候选): %s\n' "$STALE_N"
  printf '有价值(需审查): %s\n' "$VALUABLE_N"
  printf '报告时间: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} | if [ -n "$OUTPUT" ]; then tee "$OUTPUT"; else cat; fi
