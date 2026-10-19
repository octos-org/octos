#!/usr/bin/env bash
#
# branch-cleanup.test.sh — regression test for the classification direction.
#
# The dangerous failure mode is deleting a branch that is *ahead* of the base
# (it contains every base commit plus new work). That is `behind == 0`, NOT
# `ahead == 0`; getting the direction wrong once deleted nothing only because no
# such branch happened to exist outside protection. This test pins it down.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
CLEANUP="$SCRIPT_DIR/scripts/branch-cleanup.sh"
[ -x "$CLEANUP" ] || { echo "cannot find branch-cleanup.sh at $CLEANUP" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com
export GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null

git init -q --bare "$TMP/origin.git"
git init -q "$TMP/work"
cd "$TMP/work"
git remote add origin "$TMP/origin.git"
git checkout -q -b main
echo A > a.txt; git add a.txt; git commit -qm "A"
git push -q origin main

# 1. merged branch: fast-forwarded into main, so its tip is an ancestor.
git checkout -q -b fix/merged main
echo B > b.txt; git add b.txt; git commit -qm "B"
git checkout -q main; git merge -q --ff-only fix/merged

# 2. ahead branch: contains all of main plus a new commit, NOT merged.
git checkout -q -b feat/ahead main
echo C > c.txt; git add c.txt; git commit -qm "C"

# 3. squash-merged branch: patch-equivalent, tip is NOT an ancestor.
git checkout -q -b fix/squashed main
echo D > d.txt; git add d.txt; git commit -qm "D"
git checkout -q main
git merge -q --squash fix/squashed >/dev/null
git commit -qm "squash D"

git push -q origin main fix/merged feat/ahead fix/squashed
git fetch -q origin --prune

out="$("$CLEANUP" --base origin/main --logdir "$TMP/logs" 2>&1)"
echo "$out"
echo "---"

fail=0
grep -q 'origin/fix/merged'   <<<"$out" || { echo "FAIL: merged branch should be eligible";   fail=1; }
grep -q 'origin/fix/squashed' <<<"$out" || { echo "FAIL: squashed branch should be eligible"; fail=1; }
if grep -q 'origin/feat/ahead' <<<"$out"; then
  echo "FAIL: branch ahead of main must NOT be eligible for deletion"
  fail=1
fi

[ "$fail" = 0 ] && echo "PASS: classification direction is correct" || exit 1
