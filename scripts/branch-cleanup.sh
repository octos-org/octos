#!/usr/bin/env bash
#
# branch-cleanup.sh — delete remote branches whose changes are already in the
# base branch. Dry-run by default; nothing is deleted without --apply.
#
# A branch is only eligible when it is either
#   (a) an ancestor of the base branch, or
#   (b) patch-equivalent to the base branch
#       (git cherry shows no commit unique to the branch), i.e. it was
#       squash/rebase-merged.
# ...and it does not match a protected pattern, and has no open PR.
#
# Before deleting (with --apply) the script writes:
#   <logdir>/branch-cleanup-<date>.manifest  — ref + tip SHA (for recovery)
#   <logdir>/branch-cleanup-<date>.log       — human-readable action log
#   <logdir>/deleted-branches-<date>.bundle  — a self-contained git bundle so
#       every deleted branch can be restored offline, even after GitHub GC.
#
# Restore a single branch:
#   git push <remote> <sha>:refs/heads/<branch>
# Restore everything from the bundle:
#   git fetch <bundle> 'refs/heads/*:refs/heads/restored/*'
#
# Usage:
#   scripts/branch-cleanup.sh [--base origin/main] [--remote origin]
#                             [--open-pr-file FILE] [--list FILE]
#                             [--logdir DIR] [--protect REGEX]...
#                             [--no-bundle] [--apply]
#
# Exit codes: 0 = ok, 1 = error.

set -euo pipefail

BASE="origin/main"
REMOTE="origin"
OPEN_PR_FILE=""
LIST_FILE=""
LOGDIR="${TMPDIR:-/tmp}/octos-branch-cleanup"
DO_BUNDLE=1
APPLY=0

PROTECT_REGEXES=(
  '^origin/(HEAD|main|develop|master)$'
  '^origin/release/'
  '^origin/hotfix/'
  '^origin/hotfix-'
  '^origin/chore/release'
  '^origin/appcard/'
)

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base)         BASE="$2"; shift 2 ;;
    --remote)       REMOTE="$2"; shift 2 ;;
    --open-pr-file) OPEN_PR_FILE="$2"; shift 2 ;;
    --list)         LIST_FILE="$2"; shift 2 ;;
    --logdir)       LOGDIR="$2"; shift 2 ;;
    --protect)      PROTECT_REGEXES+=("$2"); shift 2 ;;
    --no-bundle)    DO_BUNDLE=0; shift ;;
    --apply)        APPLY=1; shift ;;
    -h|--help)      sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done

cd "$(git rev-parse --show-toplevel)"

is_protected() {
  local ref="$1" re
  [[ "$ref" == "$REMOTE" || "$ref" == "$BASE" ]] && return 0
  for re in "${PROTECT_REGEXES[@]}"; do
    [[ "$ref" =~ $re ]] && return 0
  done
  return 1
}

mkdir -p "$LOGDIR"
STAMP="$(date -u +%Y%m%d-%H%M%S)"
MANIFEST="$LOGDIR/branch-cleanup-$STAMP.manifest"
LOGFILE="$LOGDIR/branch-cleanup-$STAMP.log"
BUNDLE="$LOGDIR/deleted-branches-$STAMP.bundle"

is_open_pr() {
  [[ -n "$OPEN_PR_FILE" && -f "$OPEN_PR_FILE" ]] && grep -qxF "$1" "$OPEN_PR_FILE"
}

# --- assemble the candidate list -------------------------------------------
CANDIDATES="$(mktemp "${TMPDIR:-/tmp}/cleanup-candidates.XXXXXX")"
trap 'rm -f "$CANDIDATES"' EXIT

if [[ -n "$LIST_FILE" ]]; then
  # explicit list: trust it (minus protections) but still record SHAs.
  sed 's/^ *//' "$LIST_FILE" | grep -v '^$' > "$CANDIDATES"
else
  while IFS= read -r ref; do
    [[ -z "$ref" ]] && continue
    is_protected "$ref" && continue
    short="${ref#"$REMOTE"/}"
    is_open_pr "$short" && continue
    behind=$(git rev-list --left-right --count "$BASE...$ref" 2>/dev/null | awk '{print $1}')
    if [[ "$behind" == "0" ]]; then
      printf '%s\n' "$ref" >> "$CANDIDATES"; continue
    fi
    plus=$(git cherry "$BASE" "$ref" 2>/dev/null | grep -c '^+' || true)
    [[ "$plus" == "0" ]] && printf '%s\n' "$ref" >> "$CANDIDATES"
  done < <(git for-each-ref --format='%(refname:short)' "refs/remotes/$REMOTE" \
             | grep -v '/HEAD$' | sort)
fi

N=$(grep -c . "$CANDIDATES" || true)
echo "eligible branches: $N"
[[ "$N" == "0" ]] && exit 0

# --- safety re-check + manifest --------------------------------------------
: > "$MANIFEST"
: > "$LOGFILE"
while IFS= read -r ref; do
  short="${ref#"$REMOTE"/}"
  if is_protected "$ref"; then
    printf 'SKIP (protected): %s\n' "$ref" >> "$LOGFILE"; continue
  fi
  if is_open_pr "$short"; then
    printf 'SKIP (open PR): %s\n' "$ref" >> "$LOGFILE"; continue
  fi
  sha=$(git rev-parse "$ref")
  behind=$(git rev-list --left-right --count "$BASE...$ref" 2>/dev/null | awk '{print $1}')
  if [[ "$behind" != "0" ]]; then
    plus=$(git cherry "$BASE" "$ref" 2>/dev/null | grep -c '^+' || true)
    if [[ "$plus" != "0" ]]; then
      printf 'SKIP (unmerged work, %s unique patches): %s\n' "$plus" "$ref" >> "$LOGFILE"
      continue
    fi
  fi
  printf '%s %s\n' "$ref" "$sha" >> "$MANIFEST"
done < "$CANDIDATES"

ELIGIBLE=$(grep -c . "$MANIFEST" || true)
echo "will delete: $ELIGIBLE"
echo "manifest:    $MANIFEST"
echo "log:         $LOGFILE"

if [[ "$APPLY" != "1" ]]; then
  echo "(dry-run — pass --apply to delete)"
  echo "--- branches ---"
  awk '{print $1}' "$MANIFEST"
  exit 0
fi

# --- bundle backup ----------------------------------------------------------
if [[ "$DO_BUNDLE" == 1 ]]; then
  echo "writing recovery bundle: $BUNDLE"
  # shellcheck disable=SC2046
  git bundle create "$BUNDLE" $(awk '{print $1}' "$MANIFEST") "$BASE" >/dev/null
  echo "bundle: $BUNDLE ($(du -h "$BUNDLE" | cut -f1))"
fi

# --- delete -----------------------------------------------------------------
# Delete in batches: one push per ~40 branches instead of one per branch. On a
# batch failure, fall back to per-branch so the log stays precise.
deleted=0
failed=0
batch=()
flush_batch() {
  [ ${#batch[@]} -eq 0 ] && return 0
  if git push "$REMOTE" --delete "${batch[@]}" >>"$LOGFILE" 2>&1; then
    for b in "${batch[@]}"; do printf 'DELETED %s\n' "$b" >> "$LOGFILE"; done
    deleted=$((deleted + ${#batch[@]}))
  else
    for b in "${batch[@]}"; do
      if git push "$REMOTE" --delete "$b" >>"$LOGFILE" 2>&1; then
        printf 'DELETED %s\n' "$b" >> "$LOGFILE"
        deleted=$((deleted + 1))
      else
        printf 'FAILED  %s\n' "$b" >> "$LOGFILE"
        failed=$((failed + 1))
      fi
    done
  fi
  batch=()
}

while read -r ref sha; do
  [ -z "$ref" ] && continue
  batch+=("${ref#"$REMOTE"/}")
  [ ${#batch[@]} -ge 40 ] && flush_batch
done < "$MANIFEST"
flush_batch

echo "deleted: $deleted / $ELIGIBLE  (failed: $failed)"
echo "log:     $LOGFILE"
