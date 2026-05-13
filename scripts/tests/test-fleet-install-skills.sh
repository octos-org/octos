#!/usr/bin/env bash
# test-fleet-install-skills.sh — Lint + dry-run smoke tests for
# scripts/fleet-install-skills.sh.
#
# Runs entirely offline. Validates:
#   1. shellcheck passes
#   2. --help prints usage and exits 0
#   3. Unknown skill rejected with exit 1
#   4. Unknown skill error message mentions the bad name
#   5. Missing MOFA_SKILLS_DIR rejected with exit 1
#   6. --dry-run plan output covers every (host, skill) pair when explicit
#   7. --no-force omits --force flag in dry-run command preview
#
# This test creates an isolated mofa-skills tree under a temp dir so it does
# not depend on the operator's checkout.

set -eEuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET="$ROOT_DIR/scripts/fleet-install-skills.sh"

# ─── Helpers ─────────────────────────────────────────────────────────────
PASS=0
FAIL=0
fail() {
    echo "  FAIL: $*" >&2
    FAIL=$((FAIL + 1))
}
pass() {
    echo "  OK:   $*"
    PASS=$((PASS + 1))
}

# Build a fake mofa-skills tree the script can enumerate.
make_fixture() {
    local root="$1"
    mkdir -p "$root"
    for name in mofa-foo mofa-bar; do
        mkdir -p "$root/$name"
        : > "$root/$name/SKILL.md"
        : > "$root/$name/manifest.json"
    done
    # Decoy: dir without manifest should NOT be picked up.
    mkdir -p "$root/mofa-decoy"
    : > "$root/mofa-decoy/SKILL.md"
    # Decoy: non-mofa-* name with both files should NOT be picked up either.
    mkdir -p "$root/other-skill"
    : > "$root/other-skill/SKILL.md"
    : > "$root/other-skill/manifest.json"
}

run_dry() {
    local fixture="$1"; shift
    MOFA_SKILLS_DIR="$fixture" bash "$TARGET" --dry-run "$@"
}

echo "==> fleet-install-skills.sh tests"
echo "  target: $TARGET"

# ─── 1. shellcheck ───────────────────────────────────────────────────────
if command -v shellcheck >/dev/null 2>&1; then
    if shellcheck "$TARGET"; then
        pass "shellcheck"
    else
        fail "shellcheck reported issues"
    fi
else
    echo "  SKIP: shellcheck not installed (install via brew or apt)"
fi

# ─── 2. --help exits 0 ───────────────────────────────────────────────────
if bash "$TARGET" --help >/dev/null 2>&1; then
    pass "--help exits 0"
else
    fail "--help did not exit 0"
fi

# ─── 3. Unknown skill rejected ───────────────────────────────────────────
fixture="$(mktemp -d /tmp/fleet-test.XXXXXX)"
trap 'rm -rf "${fixture:-}"' EXIT
make_fixture "$fixture"

if out=$(MOFA_SKILLS_DIR="$fixture" bash "$TARGET" --dry-run --skill ghost-skill 2>&1); then
    fail "unknown-skill should have exited non-zero (got 0); output: $out"
else
    pass "unknown-skill exits non-zero"
fi

# ─── 4. Error message mentions the bad name ──────────────────────────────
# Capture into a variable first; piping `bash <failing>` directly into grep
# combines with pipefail and confuses the conditional.
ghost_out=$(MOFA_SKILLS_DIR="$fixture" bash "$TARGET" --dry-run --skill ghost-skill 2>&1 || true)
if echo "$ghost_out" | grep -q "ghost-skill"; then
    pass "unknown-skill error mentions skill name"
else
    fail "unknown-skill error did not mention the skill name (got: $ghost_out)"
fi

# ─── 5. Missing MOFA_SKILLS_DIR rejected ─────────────────────────────────
if out=$(MOFA_SKILLS_DIR="/nonexistent/octos/mofa" bash "$TARGET" --dry-run 2>&1); then
    fail "missing mofa-dir should have exited non-zero; output: $out"
else
    pass "missing mofa-dir exits non-zero"
fi

# ─── 6. Plan covers every (host, profile, skill) ─────────────────────────
out=$(run_dry "$fixture" \
        --host h1,h2 \
        --profile p1 \
        --skill mofa-foo,mofa-bar 2>&1)
# Expect 4 lines of "OK    dry-run" in the summary (2 hosts x 1 profile x 2 skills).
ok_count=$(echo "$out" | grep -c "OK    dry-run" || true)
if [ "$ok_count" -eq 4 ]; then
    pass "plan has 4 (host x profile x skill) rows"
else
    fail "plan should have 4 OK rows, got $ok_count"
    echo "$out" >&2
fi

# ─── 7. Decoy skills not picked up as defaults ───────────────────────────
out=$(MOFA_SKILLS_DIR="$fixture" bash "$TARGET" --dry-run --host h1 --profile p1 2>&1)
if echo "$out" | grep -q "mofa-decoy"; then
    fail "default skill list pulled in mofa-decoy (no manifest.json)"
elif echo "$out" | grep -q "other-skill"; then
    fail "default skill list pulled in non-mofa-* dir"
else
    pass "default skill list excludes decoys"
fi

# ─── 8. --no-force omits --force flag ────────────────────────────────────
out=$(run_dry "$fixture" \
        --host h1 \
        --profile p1 \
        --skill mofa-foo \
        --no-force 2>&1)
if echo "$out" | grep -- "install" | grep -- "--force" >/dev/null; then
    fail "--no-force still emitted --force flag"
else
    pass "--no-force omits --force from install command"
fi

# ─── 9. Default --force emits --force flag ───────────────────────────────
out=$(run_dry "$fixture" \
        --host h1 \
        --profile p1 \
        --skill mofa-foo 2>&1)
if echo "$out" | grep -- "install" | grep -- "--force" >/dev/null; then
    pass "default emits --force"
else
    fail "default did not emit --force"
fi

# ─── 10. OCTOS_FLEET_HOSTS env override ──────────────────────────────────
out=$(MOFA_SKILLS_DIR="$fixture" OCTOS_FLEET_HOSTS="env-host-1,env-host-2" \
        bash "$TARGET" --dry-run --profile p1 --skill mofa-foo 2>&1)
if echo "$out" | grep -q "env-host-1" && echo "$out" | grep -q "env-host-2"; then
    pass "OCTOS_FLEET_HOSTS env override applied"
else
    fail "OCTOS_FLEET_HOSTS env override not applied"
    echo "$out" >&2
fi

echo ""
echo "==> Results: PASS=$PASS  FAIL=$FAIL"
if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
echo "fleet-install-skills.sh tests passed"
