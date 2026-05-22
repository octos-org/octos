#!/usr/bin/env bash
# test-check-ui-protocol-upcr.sh — regression tests for
# scripts/check-ui-protocol-upcr.sh (#717).
#
# Builds throwaway git repos, copies the gate script into them, then
# exercises the scenarios documented in #717:
#
#   1. Protocol changed + UPCR added in same diff range -> exit 0.
#   2. Protocol changed + no UPCR change -> exit non-zero with clear msg.
#   3. No protocol change -> exit 0 regardless of UPCR state.
#   4. Protocol change split across two commits (not in HEAD's diff vs
#      HEAD~1) -> still gated because diff-range covers merge-base..HEAD.
#   5. Whitespace-only protocol diff -> exempt (exit 0).
#   6. Untracked UPCR file with staged protocol change -> exit 0.
#   7. Spec revision instead of UPCR doc -> exit 0.
#   8. UPCR_ALLOW_NO_DOC=1 override -> exit 0 even without a UPCR doc.
#
# Runs entirely offline. Each scenario uses an isolated temp repo so state
# does not leak between cases.

set -eEuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET="$REPO_ROOT/scripts/check-ui-protocol-upcr.sh"

if [ ! -x "$TARGET" ] && [ ! -r "$TARGET" ]; then
  echo "FAIL: cannot read $TARGET" >&2
  exit 2
fi

PASS=0
FAIL=0

pass() { echo "  OK:   $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*" >&2; FAIL=$((FAIL + 1)); }

# Make a throwaway git repo seeded with the placeholder protocol + spec
# files that the gate inspects, so "no change" is a sane baseline.
make_repo() {
  local dir
  dir="$(mktemp -d /tmp/upcr-gate-test.XXXXXX)"
  (
    cd "$dir"
    git init --quiet --initial-branch=main
    git config user.email "test@example.com"
    git config user.name "test"
    git config commit.gpgsign false

    mkdir -p scripts crates/octos-core/src crates/octos-cli/src/api api docs
    cp "$TARGET" scripts/check-ui-protocol-upcr.sh
    chmod +x scripts/check-ui-protocol-upcr.sh

    printf '// baseline\n' > crates/octos-core/src/ui_protocol.rs
    printf '// baseline\n' > crates/octos-cli/src/api/ui_protocol.rs
    printf '# spec baseline\n' > api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md
    printf '# placeholder\n' > docs/.keep

    git add -A
    git commit --quiet -m "baseline"

    # Pretend main is also our upstream so resolve_base_ref picks it up.
    git update-ref refs/remotes/origin/main HEAD

    # Create a feature branch for the case under test.
    git checkout --quiet -b feature
  )
  printf '%s\n' "$dir"
}

run_gate() {
  local dir="$1"
  shift
  (
    cd "$dir"
    "$@" bash scripts/check-ui-protocol-upcr.sh
  )
}

# Scenario 1: protocol changed + UPCR added -> exit 0.
scenario_protocol_plus_upcr() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    printf '// added v2 field\nstruct Foo { bar: u32 }\n' \
      > crates/octos-core/src/ui_protocol.rs
    printf '# UPCR-2026-099 Test\n\nChange description.\n' \
      > docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_099_TEST.md
    git add -A
    git commit --quiet -m "feat: extend protocol + upcr"
  )
  local out status
  out="$(run_gate "$dir" 2>&1)"
  status=$?
  if [ "$status" -eq 0 ] && grep -q "UPCR coverage" <<<"$out"; then
    pass "protocol + UPCR -> exit 0 with coverage line"
  else
    fail "protocol + UPCR: status=$status output=$out"
  fi
  rm -rf "$dir"
}

# Scenario 2: protocol changed without UPCR -> exit non-zero.
scenario_protocol_without_upcr() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    printf '// added v2 field\nstruct Foo { bar: u32 }\n' \
      > crates/octos-core/src/ui_protocol.rs
    git add -A
    git commit --quiet -m "feat: extend protocol no upcr"
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -ne 0 ] && grep -q "require a UPCR document" <<<"$out"; then
    pass "protocol without UPCR -> exit non-zero with clear msg"
  else
    fail "protocol without UPCR: status=$status output=$out"
  fi
  rm -rf "$dir"
}

# Scenario 3: no protocol change -> exit 0 regardless of UPCR state.
scenario_no_protocol() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    mkdir -p src
    printf 'fn main() {}\n' > src/main.rs
    git add -A
    git commit --quiet -m "feat: unrelated change"
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -eq 0 ] && grep -q "no protocol-visible edits" <<<"$out"; then
    pass "no protocol change -> exit 0"
  else
    fail "no protocol change: status=$status output=$out"
  fi
  rm -rf "$dir"
}

# Scenario 4: protocol change in a parent commit that does not also touch the
# UPCR; the bypass the legacy script allowed. With diff-range covering
# merge-base..HEAD the gate must still fail.
scenario_split_commits_bypass() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    printf '// added v2 field\n' > crates/octos-core/src/ui_protocol.rs
    git add -A
    git commit --quiet -m "feat: protocol diff only"

    # A second commit that touches something unrelated. HEAD's diff vs HEAD~1
    # contains no protocol file, but merge-base..HEAD does.
    printf 'note\n' > NOTES.md
    git add NOTES.md
    git commit --quiet -m "docs: notes"
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -ne 0 ] && grep -q "require a UPCR document" <<<"$out"; then
    pass "split-commit bypass is closed (merge-base..HEAD diff)"
  else
    fail "split-commit bypass: status=$status output=$out"
  fi
  rm -rf "$dir"
}

# Scenario 5: whitespace-only protocol diff -> exempt.
scenario_whitespace_only() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    # Append trailing spaces only — `git diff -w --stat` must report empty,
    # which is how the gate detects whitespace-only diffs.
    printf '// baseline   \n' > crates/octos-core/src/ui_protocol.rs
    git add -A
    git commit --quiet -m "style: whitespace"
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -eq 0 ] && grep -q "no protocol-visible edits" <<<"$out"; then
    pass "whitespace-only protocol diff is exempt"
  else
    fail "whitespace-only: status=$status output=$out"
  fi
  rm -rf "$dir"
}

# Scenario 6: untracked UPCR file alongside staged protocol change -> exit 0.
# This is the pre-commit case: the original script's `git status` already
# handled it, and the new script must keep parity.
scenario_uncommitted_upcr() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    printf '// staged change\n' > crates/octos-core/src/ui_protocol.rs
    printf '# UPCR-2026-100 Untracked\n' \
      > docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_100_UNTRACKED.md
    git add crates/octos-core/src/ui_protocol.rs
    # UPCR doc stays untracked on purpose.
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -eq 0 ] && grep -q "UPCR coverage" <<<"$out"; then
    pass "untracked UPCR satisfies the gate (pre-commit parity)"
  else
    fail "untracked UPCR: status=$status output=$out"
  fi
  rm -rf "$dir"
}

# Scenario 7: protocol spec revision satisfies gate when no UPCR doc exists.
scenario_spec_change() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    printf '// extend\n' > crates/octos-core/src/ui_protocol.rs
    printf '# spec v2\n' > api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md
    git add -A
    git commit --quiet -m "spec: extend"
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -eq 0 ] && grep -q "protocol spec coverage" <<<"$out"; then
    pass "spec revision satisfies gate"
  else
    fail "spec revision: status=$status output=$out"
  fi
  rm -rf "$dir"
}

# Scenario 8: UPCR_ALLOW_NO_DOC=1 override.
scenario_reviewer_override() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    printf '// extend\n' > crates/octos-core/src/ui_protocol.rs
    git add -A
    git commit --quiet -m "feat: extend"
  )
  local out status=0
  out="$(run_gate "$dir" env UPCR_ALLOW_NO_DOC=1 2>&1)" || status=$?
  if [ "$status" -eq 0 ] && grep -q "reviewer override" <<<"$out"; then
    pass "reviewer override flag still works"
  else
    fail "reviewer override: status=$status output=$out"
  fi
  rm -rf "$dir"
}

echo "==> check-ui-protocol-upcr.sh scenario tests"
echo "  target: $TARGET"

scenario_protocol_plus_upcr
scenario_protocol_without_upcr
scenario_no_protocol
scenario_split_commits_bypass
scenario_whitespace_only
scenario_uncommitted_upcr
scenario_spec_change
scenario_reviewer_override

echo
echo "==> Summary: $PASS passed, $FAIL failed"
if [ "$FAIL" -ne 0 ]; then
  exit 1
fi
exit 0
