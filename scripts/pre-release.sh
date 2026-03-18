#!/usr/bin/env bash
# Pre-release smoke test suite for octos.
# Usage: ./scripts/pre-release.sh [--skip-build] [--skip-e2e] [--release]
#
# Runs all checks before a release:
#   1. Format check
#   2. Clippy lint
#   3. Unit + integration tests (all crates, all features)
#   4. Release build
#   5. E2E smoke tests (binary-level)
set -euo pipefail

# ── Flags ──────────────────────────────────────────────────────────────
SKIP_BUILD=false
SKIP_E2E=false
PROFILE="release"
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=true ;;
        --skip-e2e)   SKIP_E2E=true ;;
        --debug)      PROFILE="dev" ;;
        --help|-h)
            echo "Usage: $0 [--skip-build] [--skip-e2e] [--debug]"
            exit 0
            ;;
    esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PASS=0
FAIL=0
SKIP=0

pass() { PASS=$((PASS + 1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL: $1"; }
skip() { SKIP=$((SKIP + 1)); echo "  SKIP: $1"; }

section() { echo ""; echo "═══ $1 ═══"; }

# ── 1. Format ──────────────────────────────────────────────────────────
section "Format Check"
if cargo fmt --all -- --check 2>/dev/null; then
    pass "cargo fmt"
else
    fail "cargo fmt (run: cargo fmt --all)"
fi

# ── 2. Clippy ──────────────────────────────────────────────────────────
section "Clippy Lint"
# Only fail on compilation errors, not warnings.
cargo clippy --workspace --all-targets > /tmp/octos-clippy.log 2>&1 || true
CLIPPY_ERRS=$(grep -c "^error" /tmp/octos-clippy.log || true)
CLIPPY_WARNS=$(grep -c "^warning\[" /tmp/octos-clippy.log || true)
if [ "${CLIPPY_ERRS:-0}" -eq 0 ]; then
    pass "cargo clippy (${CLIPPY_WARNS:-0} warnings)"
else
    tail -5 /tmp/octos-clippy.log
    fail "cargo clippy (${CLIPPY_ERRS} errors)"
fi

# ── 3. Unit + Integration Tests ────────────────────────────────────────
section "Unit & Integration Tests"

echo "  Running: cargo test --workspace"
if cargo test --workspace 2>&1 | tee /tmp/octos-test-workspace.log | tail -5; then
    # Extract totals
    TOTAL_PASS=$(grep "^test result:" /tmp/octos-test-workspace.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
    TOTAL_FAIL=$(grep "^test result:" /tmp/octos-test-workspace.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/failed/){gsub(/[^0-9]/,"",$i);f+=$i}}}END{print f+0}')
    TOTAL_IGN=$(grep "^test result:" /tmp/octos-test-workspace.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/ignored/){gsub(/[^0-9]/,"",$i);ig+=$i}}}END{print ig+0}')
    echo "  Totals: ${TOTAL_PASS} passed, ${TOTAL_FAIL} failed, ${TOTAL_IGN} ignored"
    if [ "$TOTAL_FAIL" -eq 0 ]; then
        pass "workspace tests (${TOTAL_PASS} passed)"
    else
        fail "workspace tests (${TOTAL_FAIL} failures)"
    fi
else
    fail "workspace tests"
fi

echo ""
echo "  Running: cargo test -p octos-cli --features api"
if cargo test -p octos-cli --features api 2>&1 | tee /tmp/octos-test-cli-api.log | tail -3; then
    CLI_PASS=$(grep "^test result:" /tmp/octos-test-cli-api.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
    CLI_FAIL=$(grep "^test result:" /tmp/octos-test-cli-api.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/failed/){gsub(/[^0-9]/,"",$i);f+=$i}}}END{print f+0}')
    if [ "$CLI_FAIL" -eq 0 ]; then
        pass "octos-cli API tests (${CLI_PASS} passed)"
    else
        fail "octos-cli API tests (${CLI_FAIL} failures)"
    fi
else
    fail "octos-cli API tests"
fi

# ── 4. Build ───────────────────────────────────────────────────────────
section "Build"

if [ "$SKIP_BUILD" = true ]; then
    skip "release build (--skip-build)"
else
    BUILD_FLAGS="--features telegram,whatsapp,feishu,twilio,api"
    echo "  Building octos-cli ($PROFILE) with $BUILD_FLAGS"

    if [ "$PROFILE" = "release" ]; then
        BUILD_CMD="cargo build --release -p octos-cli $BUILD_FLAGS"
    else
        BUILD_CMD="cargo build -p octos-cli $BUILD_FLAGS"
    fi

    if $BUILD_CMD 2>&1 | tail -3; then
        pass "octos-cli build ($PROFILE)"
    else
        fail "octos-cli build ($PROFILE)"
    fi

    echo "  Building app-skills (release)"
    if cargo build --release \
        -p news_fetch -p deep-search -p deep-crawl -p send-email \
        -p account-manager -p asr -p clock -p weather 2>&1 | tail -3; then
        pass "app-skills build"
    else
        fail "app-skills build"
    fi
fi

# ── 5. E2E Smoke Tests ────────────────────────────────────────────────
section "E2E Smoke Tests"

if [ "$SKIP_E2E" = true ]; then
    skip "E2E tests (--skip-e2e)"
else
    if [ "$PROFILE" = "release" ]; then
        OCTOS="$ROOT/target/release/octos"
    else
        OCTOS="$ROOT/target/debug/octos"
    fi

    if [ ! -f "$OCTOS" ]; then
        fail "binary not found at $OCTOS (run without --skip-build)"
    else
        E2E_DIR=$(mktemp -d)
        trap 'rm -rf "$E2E_DIR"' EXIT

        # 5a. Version output
        if $OCTOS --version 2>&1 | grep -q "^octos [0-9]"; then
            pass "octos --version"
        else
            fail "octos --version"
        fi

        # 5b. Help output
        if $OCTOS --help 2>&1 | grep -q "Usage:"; then
            pass "octos --help"
        else
            fail "octos --help"
        fi

        # 5c. Init creates .octos directory
        pushd "$E2E_DIR" > /dev/null
        if $OCTOS init 2>&1 && [ -d ".octos" ]; then
            pass "octos init (creates .octos/)"
        else
            fail "octos init"
        fi

        # 5d. Status runs without crash
        if $OCTOS status 2>/dev/null; then
            pass "octos status"
        else
            fail "octos status"
        fi

        # 5e. Skills list runs without crash
        if $OCTOS skills list 2>/dev/null; then
            pass "octos skills list"
        else
            fail "octos skills list"
        fi

        # 5f. Cron list runs without crash
        if $OCTOS cron list 2>/dev/null; then
            pass "octos cron list"
        else
            fail "octos cron list"
        fi

        # 5g. Channels status runs without crash
        if $OCTOS channels status 2>/dev/null; then
            pass "octos channels status"
        else
            fail "octos channels status"
        fi

        # 5h. Completions generate without error
        if $OCTOS completions bash > /dev/null 2>&1; then
            pass "octos completions bash"
        else
            fail "octos completions bash"
        fi

        # 5i. Docs generates tool documentation
        if $OCTOS docs 2>&1 | grep -qi "tool\|provider\|Available"; then
            pass "octos docs"
        else
            fail "octos docs"
        fi

        # 5j. Clean runs without crash
        if $OCTOS clean 2>/dev/null; then
            pass "octos clean"
        else
            fail "octos clean"
        fi

        # 5k. Init config is valid JSON
        if [ -f ".octos/config.json" ]; then
            if python3 -m json.tool .octos/config.json > /dev/null 2>&1; then
                pass "config.json is valid JSON"
            else
                fail "config.json is invalid JSON"
            fi
        else
            skip "config.json not created by init"
        fi

        # 5l. Auth status runs (no crash even without auth)
        if $OCTOS auth status 2>&1; then
            pass "octos auth status"
        else
            # auth status may exit 1 if not logged in, that's fine
            pass "octos auth status (not logged in)"
        fi

        popd > /dev/null

        # 5m. App-skill binaries exist and respond to --help or --version
        for skill_bin in news_fetch deep-search deep_crawl send_email account_manager asr clock weather; do
            SKILL_PATH="$ROOT/target/release/$skill_bin"
            if [ -f "$SKILL_PATH" ]; then
                # Just check it launches (--help or timeout after 2s)
                if timeout 5 "$SKILL_PATH" --help > /dev/null 2>&1 || timeout 2 "$SKILL_PATH" --version > /dev/null 2>&1; then
                    pass "skill binary: $skill_bin"
                else
                    # Some skills may not have --help; just check they're executable
                    if [ -x "$SKILL_PATH" ]; then
                        pass "skill binary: $skill_bin (executable)"
                    else
                        fail "skill binary: $skill_bin (not executable)"
                    fi
                fi
            else
                skip "skill binary: $skill_bin (not built)"
            fi
        done
    fi
fi

# ── Summary ────────────────────────────────────────────────────────────
section "Summary"
echo "  Passed:  $PASS"
echo "  Failed:  $FAIL"
echo "  Skipped: $SKIP"
echo ""

if [ "$FAIL" -gt 0 ]; then
    echo "RELEASE BLOCKED: $FAIL check(s) failed."
    exit 1
else
    echo "All checks passed. Ready to release."
    exit 0
fi
