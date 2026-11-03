#!/usr/bin/env bash
# test-upcr-decentralized-registry.sh — merge-lane proof for the T-0328
# decentralized UPCR id scheme, exercising scripts/lint-upcr-registry.py.
#
# Builds a throwaway git repo with two real branches, each adding its own
# new-namespace UPCR record, and proves:
#
#   1. Merging the branches in EITHER order needs no renumber, no README
#      edit, and no rebase — and the merged tree passes the lint.
#   2. A negative control: two files sharing one R-id still fail with an
#      explicit duplicate diagnostic, and a new record whose body omits its
#      id fails too.
#   3. Legacy numbered records keep working: a legacy duplicate is still
#      diagnosed and a clean legacy set passes.
#
# Runs entirely offline.

set -eEuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LINT="$REPO_ROOT/scripts/lint-upcr-registry.py"

if [ ! -r "$LINT" ]; then
  echo "FAIL: cannot read $LINT" >&2
  exit 2
fi

PASS=0
FAIL=0
pass() { echo "  OK:   $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*" >&2; FAIL=$((FAIL + 1)); }

lint_repo() { python3 "$1/scripts/lint-upcr-registry.py" >/dev/null; }

# Baseline repo: the lint script at its canonical relative path (the script
# resolves the repo root as scripts/..), one legacy record, and the frozen
# historical README — no rows for the new records, matching production.
REPO="$(mktemp -d /tmp/upcr-decentralized-test.XXXXXX)"
(
  cd "$REPO"
  git init --quiet --initial-branch=main
  git config user.email "test@example.com"
  git config user.name "test"
  git config commit.gpgsign false
  mkdir -p scripts docs/upcr
  cp "$LINT" scripts/lint-upcr-registry.py
  printf '# historical registry (frozen)\n' > docs/upcr/README.md
  printf '# Octos UI Protocol Change Request: Seed\n' \
    > docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_SEED.md
  git add -A
  git commit --quiet -m "baseline"
)

# Branch A adds UPCR-2026-RAAAA0001; branch B adds UPCR-2026-RBBBB0002.
# Neither touches docs/upcr/README.md — the row-conflict class the old
# scheme forced on every UPCR PR is gone by construction.
for lane in A:AAAA0001:LEDGER_DURABILITY B:BBBB0002:CURSOR_RECOVERY; do
  branch="lane-${lane%%:*}"
  rest="${lane#*:}"; token="${rest%%:*}"; slug="${rest#*:}"
  (
    cd "$REPO"
    git checkout --quiet -b "$branch" main
    printf '# UPCR-2026-R%s — %s\n' "$token" "${slug//_/ }" \
      > "docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_R${token}_${slug}.md"
    git add -A
    git commit --quiet -m "docs: upcr R${token}"
  )
done

# Lane 1: merge A then B into main.
(
  cd "$REPO"
  git checkout --quiet main
  git merge --quiet --no-edit lane-A lane-B
)
if lint_repo "$REPO"; then
  pass "merge A-then-B: no rebase/renumber/README edit, lint green"
else
  fail "merge A-then-B lint failed"
fi

# Lane 2: reset main to the baseline and merge B then A.
(
  cd "$REPO"
  git checkout --quiet main
  git reset --quiet --hard "$(git rev-list --max-parents=0 main)"
  git merge --quiet --no-edit lane-B lane-A
)
if lint_repo "$REPO"; then
  pass "merge B-then-A: order-independent, lint green"
else
  fail "merge B-then-A lint failed"
fi

# Negative 1: a third branch reuses A's id under a different slug.
(
  cd "$REPO"
  git checkout --quiet -b lane-collision "$(git rev-list --max-parents=0 main)"
  printf '# UPCR-2026-RAAAA0001 — Different Record\n' \
    > docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_RAAAA0001_OTHER.md
  git add -A
  git commit --quiet -m "docs: colliding upcr id"
  git merge --quiet --no-edit lane-A lane-collision
)
if lint_repo "$REPO"; then
  fail "same R-id under two filenames passed lint"
else
  out="$(cd "$REPO" && python3 scripts/lint-upcr-registry.py 2>&1 || true)"
  if grep -q "duplicate UPCR-2026-RAAAA0001" <<<"$out"; then
    pass "duplicate R-id fails with an explicit diagnostic"
  else
    fail "duplicate R-id rejected without the expected diagnostic: $out"
  fi
fi

# Negative 2: a new record whose body never cites its id.
(
  cd "$REPO"
  git checkout --quiet -b lane-no-body-id "$(git rev-list --max-parents=0 main)"
  printf '# A Record That Forgot To Name Itself\n' \
    > docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_RCCCC0003_NO_BODY_ID.md
  git add -A
  git commit --quiet -m "docs: id-less upcr"
)
if lint_repo "$REPO"; then
  fail "new record without an in-body id citation passed lint"
else
  out="$(cd "$REPO" && python3 scripts/lint-upcr-registry.py 2>&1 || true)"
  if grep -q "body never cites its id UPCR-2026-RCCCC0003" <<<"$out"; then
    pass "missing in-body id fails with an explicit diagnostic"
  else
    fail "missing in-body id rejected without the expected diagnostic: $out"
  fi
fi

# Legacy compatibility: a second file claiming 001 is still diagnosed, and
# the baseline legacy-only tree passes.
(
  cd "$REPO"
  git checkout --quiet -b lane-legacy-collision "$(git rev-list --max-parents=0 main)"
  printf '# Another\n' > docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_ANOTHER.md
  git add -A
  git commit --quiet -m "docs: legacy collision"
)
if lint_repo "$REPO"; then
  fail "legacy number collision passed lint"
else
  out="$(cd "$REPO" && python3 scripts/lint-upcr-registry.py 2>&1 || true)"
  if grep -q "duplicate UPCR-2026-001" <<<"$out"; then
    pass "legacy number collision fails with an explicit diagnostic"
  else
    fail "legacy collision rejected without the expected diagnostic: $out"
  fi
fi
(
  cd "$REPO"
  git checkout --quiet -b lane-legacy-clean "$(git rev-list --max-parents=0 main)"
)
if lint_repo "$REPO"; then
  pass "legacy-only tree still passes"
else
  fail "legacy-only tree failed lint"
fi

rm -rf "$REPO"

echo
echo "==> Summary: $PASS passed, $FAIL failed"
if [ "$FAIL" -ne 0 ]; then
  exit 1
fi
exit 0
