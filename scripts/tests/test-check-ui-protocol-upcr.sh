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
#   7. Spec-only edit (no Rust protocol diff) -> exit 0 (self-coverage).
#   8. UPCR_ALLOW_NO_DOC=1 override -> exit 0 even without a UPCR doc.
#   9. Renamed protocol file with edits -> still gated (codex #2).
#  10. Rust protocol change + unrelated spec edit + no UPCR -> exit non-zero
#      (codex #3 — spec-as-coverage bypass closed for code-level changes).
#  11. No base ref resolvable -> exit non-zero (codex #1 — closes CI bypass).
#  12. Protocol file deletion (no UPCR) -> exit non-zero (codex #4 —
#      deletions are protocol-visible too).
#  13. Code change + deletion of an existing UPCR (no add/modify of any
#      UPCR) -> exit non-zero (codex #5 — deleted UPCR is not coverage).
#  14. Resolved base ref equals HEAD (single-branch CI) -> exit non-zero
#      (codex #6 — closes HEAD..HEAD silent-empty-diff bypass).
#  15. Missing base ref + committed protocol change + untracked UPCR
#      template (no uncommitted protocol/spec trigger) -> exit non-zero
#      (codex #7 — coverage docs must not unblock the missing-base probe).
#  16. base_equals_head + dirty uncommitted spec edit -> exit non-zero
#      (codex #8 — dirty triggers must not bypass the missing-base check).
#  17. Committed UPCR coverage + staged deletion of that same UPCR ->
#      exit non-zero (codex #9 — pending-delete UPCRs are not coverage).
#  18. feature == main (no baseline commit) + staged Rust trigger +
#      staged UPCR -> exit 0 (codex #10 — pre-commit on freshly-created
#      branch must NOT be rejected by the merge_base==HEAD guard).
#  19. Base ref resolves but `git merge-base` returns empty (disconnected
#      histories, e.g. shallow CI fetch of origin/main as a depth-1 tip)
#      -> exit non-zero (codex #11 — closes the empty-merge-base silent
#      uncommitted-only bypass).
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
# Optional first argument disables the origin/main pointer so the
# missing-base-ref scenarios can exercise that failure path.
make_repo() {
  local set_origin="${1:-1}"
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
    printf 'pub fn old() {}\n' > crates/octos-cli/src/api/ui_protocol_alpha.rs
    printf '# spec baseline\n' > api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md
    printf '# UPCR-2026-001 seed baseline\n' \
      > docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_SEED.md
    printf '# placeholder\n' > docs/.keep

    git add -A
    git commit --quiet -m "baseline"

    if [ "$set_origin" = "1" ]; then
      # Pretend main is also our upstream so resolve_base_ref picks it up.
      git update-ref refs/remotes/origin/main HEAD
      git checkout --quiet -b feature
      # Add an unrelated commit on feature so merge_base(origin/main, HEAD)
      # is the baseline commit, not HEAD itself. Otherwise HEAD..HEAD is
      # empty and the gate refuses to run.
      printf 'note\n' > FEATURE_BASELINE.md
      git add FEATURE_BASELINE.md
      git commit --quiet -m "chore: feature baseline"
    else
      # Move HEAD off main so the script can't find a base ref via
      # main/origin/main and there is nowhere to merge-base against.
      git checkout --quiet -b orphan
      git branch --quiet -D main
    fi
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
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
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

# Scenario 7: spec-only edit (no Rust protocol diff) -> exit 0 (self-coverage).
scenario_spec_only_self_coverage() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    printf '# spec v2 (text update)\nfoo bar baz\n' \
      > api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md
    git add -A
    git commit --quiet -m "spec: clarify wording"
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -eq 0 ] && grep -q "self-coverage" <<<"$out"; then
    pass "spec-only edit -> exit 0 (self-coverage)"
  else
    fail "spec-only: status=$status output=$out"
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

# Scenario 9: renamed protocol file with edits -> still gated (codex #2).
# A rename with content edits in the new path is a protocol-visible change.
scenario_renamed_protocol_file() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    git mv crates/octos-cli/src/api/ui_protocol_alpha.rs \
           crates/octos-cli/src/api/ui_protocol_beta.rs
    # Add content so it's a rename-with-edit (R<100).
    printf 'pub fn old() {}\npub fn new_wire() {}\n' \
      > crates/octos-cli/src/api/ui_protocol_beta.rs
    git add -A
    git commit --quiet -m "refactor: rename alpha -> beta"
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -ne 0 ] && grep -q "require a UPCR document" <<<"$out"; then
    pass "renamed protocol file is gated (rename detection)"
  else
    fail "renamed protocol file: status=$status output=$out"
  fi
  rm -rf "$dir"
}

# Scenario 10: Rust protocol change + unrelated spec edit + no UPCR -> fail
# (codex #3 — spec-as-coverage must NOT satisfy the gate when code changed).
scenario_unrelated_spec_does_not_cover_code() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    printf '// added v2 field\n' > crates/octos-core/src/ui_protocol.rs
    printf '# spec baseline\n\nTypo fix.\n' \
      > api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md
    git add -A
    git commit --quiet -m "feat: extend + spec typo"
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -ne 0 ] && grep -q "require a UPCR document" <<<"$out" \
       && grep -q "spec edit alone does NOT" <<<"$out"; then
    pass "unrelated spec edit does not satisfy gate for code-level diff"
  else
    fail "unrelated spec coverage bypass: status=$status output=$out"
  fi
  rm -rf "$dir"
}

# Scenario 11: no base ref resolvable -> exit non-zero (codex #1).
scenario_no_base_ref_fails() {
  local dir
  dir="$(make_repo 0)"
  (
    cd "$dir"
    printf '// extend\n' > crates/octos-core/src/ui_protocol.rs
    git add -A
    git commit --quiet -m "feat: extend"
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -ne 0 ] && grep -q "could not resolve a usable merge-base" <<<"$out"; then
    pass "missing base ref fails loud (no silent CI bypass)"
  else
    fail "missing base ref: status=$status output=$out"
  fi
  rm -rf "$dir"
}

# Scenario 12: deleting a protocol file without a UPCR is itself a
# protocol-visible event and must be gated (codex review #4).
scenario_deleted_protocol_file() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    git rm --quiet crates/octos-cli/src/api/ui_protocol_alpha.rs
    git commit --quiet -m "chore: drop alpha"
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -ne 0 ] && grep -q "require a UPCR document" <<<"$out"; then
    pass "deleted protocol file is gated (no UPCR)"
  else
    fail "deleted protocol file: status=$status output=$out"
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
scenario_spec_only_self_coverage
scenario_reviewer_override
scenario_renamed_protocol_file
scenario_unrelated_spec_does_not_cover_code
scenario_no_base_ref_fails
scenario_deleted_protocol_file

# Scenario 13: deleting an existing UPCR while editing protocol code must
# NOT satisfy coverage. Closes the bypass codex caught in review #5.
scenario_deleted_upcr_is_not_coverage() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    printf '// extend\n' > crates/octos-core/src/ui_protocol.rs
    git rm --quiet docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_SEED.md
    git add -A
    git commit --quiet -m "feat: extend + drop old upcr"
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -ne 0 ] && grep -q "require a UPCR document" <<<"$out"; then
    pass "deleted UPCR does not satisfy coverage"
  else
    fail "deleted UPCR coverage bypass: status=$status output=$out"
  fi
  rm -rf "$dir"
}

scenario_deleted_upcr_is_not_coverage

# Scenario 14: resolved base ref equals HEAD (e.g. single-branch CI checkout
# where origin/main and the feature branch are the same commit). HEAD..HEAD
# is empty by construction; the script must refuse to run instead of silently
# reporting "no protocol-visible edits detected". Codex review #6.
scenario_base_equals_head_fails() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    # Commit a protocol change directly onto feature, then point
    # origin/main at HEAD so merge-base(origin/main, HEAD) == HEAD.
    printf '// changed\n' > crates/octos-core/src/ui_protocol.rs
    git add -A
    git commit --quiet -m "feat: extend"
    git update-ref refs/remotes/origin/main HEAD
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -ne 0 ] && grep -q "points to HEAD" <<<"$out"; then
    pass "merge_base == HEAD with clean tree fails loud (no empty-diff bypass)"
  else
    fail "merge_base == HEAD: status=$status output=$out"
  fi
  rm -rf "$dir"
}

scenario_base_equals_head_fails

# Scenario 15: shallow checkout with no base ref AND a committed protocol
# change AND a stray untracked UPCR-template. The recovery probe must look
# only at trigger files (protocol code + spec), not coverage docs, or a
# stale UPCR/template would mask the committed protocol bypass. Codex
# review #7.
scenario_missing_base_with_untracked_template_fails() {
  local dir
  dir="$(make_repo 0)"
  (
    cd "$dir"
    printf '// changed\n' > crates/octos-core/src/ui_protocol.rs
    git add -A
    git commit --quiet -m "feat: extend"
    # Drop a stray untracked UPCR template into the working tree.
    printf '# template stub\n' \
      > docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_TEMPLATE.md
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -ne 0 ] && grep -q "could not resolve a usable merge-base" <<<"$out"; then
    pass "stray untracked template does not unblock missing-base probe"
  else
    fail "stray template bypass: status=$status output=$out"
  fi
  rm -rf "$dir"
}

scenario_missing_base_with_untracked_template_fails

# Scenario 16: base_equals_head + uncommitted spec edit must still fail
# loud. Codex review #8 — a dirty trigger file must not promote the gate
# into uncommitted-only mode and bypass committed-but-undetectable changes.
scenario_base_equals_head_dirty_spec_fails() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    # Commit a protocol Rust change with no UPCR, then point origin/main
    # at HEAD so merge_base == HEAD.
    printf '// changed\n' > crates/octos-core/src/ui_protocol.rs
    git add -A
    git commit --quiet -m "feat: extend"
    git update-ref refs/remotes/origin/main HEAD
    # Add an uncommitted spec edit on top of that.
    printf '# spec tweak\n' >> api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  # With dirty spec, we enter strict uncommitted-only mode. The spec-only
  # self-coverage shortcut is disabled in that mode, so the gate falls
  # through to "require a UPCR document" — closing the bypass.
  if [ "$status" -ne 0 ] && grep -q "require a UPCR document" <<<"$out"; then
    pass "dirty spec does not unblock base_equals_head check"
  else
    fail "dirty spec bypass: status=$status output=$out"
  fi
  rm -rf "$dir"
}

scenario_base_equals_head_dirty_spec_fails

# Scenario 17: a committed UPCR that is then staged for deletion is not
# coverage. Codex review #9 — collect_names in coverage mode must subtract
# uncommitted deletions even if the committed AMR range still lists them.
scenario_staged_upcr_deletion_invalidates_coverage() {
  local dir
  dir="$(make_repo)"
  (
    cd "$dir"
    printf '// changed\n' > crates/octos-core/src/ui_protocol.rs
    printf '# UPCR-2026-099 added\n' \
      > docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_099_TEST.md
    git add -A
    git commit --quiet -m "feat: protocol + new upcr"
    # Now stage the deletion of the UPCR that was just added.
    git rm --quiet docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_099_TEST.md
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -ne 0 ] && grep -q "require a UPCR document" <<<"$out"; then
    pass "staged UPCR deletion invalidates committed coverage"
  else
    fail "staged UPCR deletion bypass: status=$status output=$out"
  fi
  rm -rf "$dir"
}

scenario_staged_upcr_deletion_invalidates_coverage

# Scenario 18: freshly-created feature branch where feature == main (no
# baseline commit) with staged protocol code change AND staged UPCR.
# This is the legitimate pre-commit case codex #10 worried about. The
# gate must NOT reject with "base ref resolves to HEAD" when real
# uncommitted trigger work justifies an uncommitted-only run.
scenario_freshly_created_branch_pre_commit_ok() {
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
    printf '# spec baseline\n' > api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md

    git add -A
    git commit --quiet -m "baseline"
    git update-ref refs/remotes/origin/main HEAD
    # Branch off main WITHOUT a baseline commit on feature; merge_base == HEAD.
    git checkout --quiet -b feature

    # Stage a protocol change + the UPCR pre-commit (untracked or staged is
    # both fine — uncommitted_names covers both).
    printf '// extend\n' > crates/octos-core/src/ui_protocol.rs
    printf '# UPCR-2026-099 Pre-commit\n' \
      > docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_099_PRE.md
    git add crates/octos-core/src/ui_protocol.rs
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -eq 0 ] && grep -q "UPCR coverage" <<<"$out"; then
    pass "feature == main pre-commit with staged trigger + UPCR is accepted"
  else
    fail "freshly created branch rejected: status=$status output=$out"
  fi
  rm -rf "$dir"
}

scenario_freshly_created_branch_pre_commit_ok

# Scenario 19: base ref resolves to a real commit but `git merge-base` finds
# no common ancestor. Simulated by creating origin/main as an orphan branch
# whose history is disjoint from HEAD's history. Without the empty-merge-base
# guard the gate would silently fall into uncommitted-only mode and miss
# committed protocol changes. Codex review #11.
scenario_empty_merge_base_fails() {
  local dir
  dir="$(mktemp -d /tmp/upcr-gate-test.XXXXXX)"
  (
    cd "$dir"
    git init --quiet --initial-branch=feature
    git config user.email "test@example.com"
    git config user.name "test"
    git config commit.gpgsign false

    mkdir -p scripts crates/octos-core/src crates/octos-cli/src/api api docs
    cp "$TARGET" scripts/check-ui-protocol-upcr.sh
    chmod +x scripts/check-ui-protocol-upcr.sh

    # feature branch with a committed protocol change but no UPCR.
    printf '// changed\n' > crates/octos-core/src/ui_protocol.rs
    printf '# spec baseline\n' > api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md
    git add -A
    git commit --quiet -m "feat: protocol change"

    # Create an orphan commit and point origin/main at it; merge-base
    # (origin/main, feature) is empty (no common ancestor).
    git checkout --quiet --orphan orphan-base
    git rm -rf --quiet .
    mkdir -p docs
    printf '# orphan\n' > docs/.keep
    git add -A
    git commit --quiet -m "orphan base"
    git update-ref refs/remotes/origin/main HEAD
    git checkout --quiet feature
  )
  local out status=0
  out="$(run_gate "$dir" 2>&1)" || status=$?
  if [ "$status" -ne 0 ] && grep -q "could not resolve a usable merge-base" <<<"$out"; then
    pass "empty merge-base fails loud (no disconnected-history bypass)"
  else
    fail "empty merge-base: status=$status output=$out"
  fi
  rm -rf "$dir"
}

scenario_empty_merge_base_fails

echo
echo "==> Summary: $PASS passed, $FAIL failed"
if [ "$FAIL" -ne 0 ]; then
  exit 1
fi
exit 0
