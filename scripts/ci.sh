#!/usr/bin/env bash
# Local CI — mirrors .github/workflows/ci.yml with expanded test coverage.
# Usage: ./scripts/ci.sh [--fix] [--quick] [--serial] [--subsystem <name>]
#   --fix       : auto-fix formatting instead of checking
#   --quick     : skip clippy (just fmt + test)
#   --serial    : run tests single-threaded (avoids OOM on constrained machines)
#   --subsystem : run only tests for a specific subsystem (core, llm, agent, pipeline, bus, cli, memory)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

FIX=false
QUICK=false
SERIAL=false
SUBSYSTEM=""
for arg in "$@"; do
    case "$arg" in
        --fix)    FIX=true ;;
        --quick)  QUICK=true ;;
        --serial) SERIAL=true ;;
        --subsystem) shift; SUBSYSTEM="${1:-}" ;;
        core|llm|agent|pipeline|bus|cli|memory)
            SUBSYSTEM="$arg" ;;
        --help|-h)
            echo "Usage: $0 [--fix] [--quick] [--serial] [--subsystem <name>]"
            echo "  --fix       : auto-fix formatting"
            echo "  --quick     : skip clippy"
            echo "  --serial    : single-threaded tests (avoids OOM)"
            echo "  --subsystem : core, llm, agent, pipeline, bus, cli, memory"
            exit 0
            ;;
    esac
done

PASS=0
FAIL=0
STARTED=$(date +%s)

pass() { PASS=$((PASS + 1)); printf "  \033[32m✓\033[0m %s\n" "$1"; }
fail() { FAIL=$((FAIL + 1)); printf "  \033[31m✗\033[0m %s\n" "$1"; }
section() { printf "\n\033[1m── %s ──\033[0m\n" "$1"; }

TEST_THREADS_FLAG=""
if [ "$SERIAL" = true ]; then
    TEST_THREADS_FLAG="-- --test-threads=1"
fi

detect_base_ref() {
    if [ -n "${CI_PR_BASE_REF:-}" ]; then
        printf "%s" "$CI_PR_BASE_REF"
        return 0
    fi
    if [ -n "${GITHUB_BASE_REF:-}" ]; then
        printf "%s" "$GITHUB_BASE_REF"
        return 0
    fi
    return 1
}

detect_base_remote() {
    if git show-ref --verify --quiet "refs/remotes/octos/$1"; then
        printf "octos"
        return 0
    fi
    if git show-ref --verify --quiet "refs/remotes/origin/$1"; then
        printf "origin"
        return 0
    fi
    return 1
}

check_changed_rustfmt() {
    local base_ref base_remote merge_base
    base_ref="$(detect_base_ref)" || return 1
    base_remote="$(detect_base_remote "$base_ref")" || return 1
    merge_base="$(git merge-base HEAD "$base_remote/$base_ref")"

    local changed_rs=()
    while IFS= read -r -d '' file; do
        changed_rs+=("$file")
    done < <(git diff -z --name-only --diff-filter=ACMRT "$merge_base"...HEAD -- '*.rs')

    if [ "${#changed_rs[@]}" -eq 0 ]; then
        echo "  No changed Rust files to format-check."
        return 0
    fi

    rustfmt --check --edition 2021 --config skip_children=true "${changed_rs[@]}"
}

# ── 1. Format ─────────────────────────────────────────────────────────
section "Format"
if [ "$FIX" = true ]; then
    cargo fmt --all
    pass "cargo fmt --all (fixed)"
else
    if check_changed_rustfmt 2>&1; then
        pass "rustfmt (changed files)"
    elif cargo fmt --all -- --check 2>&1; then
        pass "cargo fmt"
    else
        fail "cargo fmt (run with --fix or: cargo fmt --all)"
    fi
fi

# ── 2. Clippy ─────────────────────────────────────────────────────────
if [ "$QUICK" = false ]; then
    section "Clippy"
    if cargo clippy --workspace -- -D warnings 2>&1; then
        pass "cargo clippy"
    else
        fail "cargo clippy"
    fi
fi

# ── 3. Tests ──────────────────────────────────────────────────────────
if [ -n "$SUBSYSTEM" ]; then
    # Focused subsystem test
    section "Subsystem Tests: $SUBSYSTEM"
    CRATE="octos-$SUBSYSTEM"
    if cargo test -p "$CRATE" $TEST_THREADS_FLAG 2>&1 | tee /tmp/octos-ci-sub.log | tail -5; then
        SUB_PASS=$(grep "^test result:" /tmp/octos-ci-sub.log | awk '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "$CRATE tests ($SUB_PASS passed)"
    else
        fail "$CRATE tests"
    fi
else
    section "Tests"

    # 3a. Workspace tests (all crates)
    echo "  Running: cargo test --workspace"
    if cargo test --workspace $TEST_THREADS_FLAG 2>&1 | tee /tmp/octos-ci-test.log | tail -20; then
        TOTAL=$(grep "^test result:" /tmp/octos-ci-test.log | \
            awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "cargo test --workspace ($TOTAL passed)"
    else
        fail "cargo test --workspace"
    fi

    # 3b. Focused test groups — verify critical subsystems explicitly
    section "Focused Test Groups"

    # Adaptive routing (Off/Hedge/Lane, circuit breaker, scoring, metrics)
    echo "  Running: adaptive routing tests"
    if cargo test -p octos-llm --lib adaptive::tests $TEST_THREADS_FLAG 2>&1 | tee /tmp/octos-ci-adaptive.log | tail -5; then
        N=$(grep "^test result:" /tmp/octos-ci-adaptive.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "adaptive routing ($N tests)"
    else
        fail "adaptive routing"
    fi

    # QoS cold-start/runtime catalog and pipeline fallback ordering
    echo "  Running: QoS/runtime catalog regression tests"
    if cargo test -p octos-llm test_qos_ranking_changes_lane_selection $TEST_THREADS_FLAG 2>&1 | tee /tmp/octos-ci-qos.log | tail -5 \
        && cargo test -p octos-llm test_derive_cold_start_catalog_assigns_non_zero_scores $TEST_THREADS_FLAG 2>&1 | tee -a /tmp/octos-ci-qos.log | tail -5 \
        && cargo test -p octos-llm test_compatible_fallbacks_prefers_lower_seeded_qos_score $TEST_THREADS_FLAG 2>&1 | tee -a /tmp/octos-ci-qos.log | tail -5 \
        && cargo test -p octos-cli gateway_runtime::tests --features api $TEST_THREADS_FLAG 2>&1 | tee -a /tmp/octos-ci-qos.log | tail -5; then
        N=$(grep "^test result:" /tmp/octos-ci-qos.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "QoS/runtime catalog regressions ($N tests)"
    else
        fail "QoS/runtime catalog regressions"
    fi

    # Responsiveness observer (baseline, degradation, recovery)
    echo "  Running: responsiveness observer tests"
    if cargo test -p octos-llm --lib responsiveness::tests $TEST_THREADS_FLAG 2>&1 | tee /tmp/octos-ci-resp.log | tail -5; then
        N=$(grep "^test result:" /tmp/octos-ci-resp.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "responsiveness observer ($N tests)"
    else
        fail "responsiveness observer"
    fi

    # Queue modes + speculative overflow + auto-escalation
    echo "  Running: session actor tests (queue modes, speculative, escalation)"
    if cargo test -p octos-cli session_actor::tests -- --test-threads=1 2>&1 | tee /tmp/octos-ci-actor.log | tail -5; then
        N=$(grep "^test result:" /tmp/octos-ci-actor.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "session actor ($N tests)"
    else
        fail "session actor"
    fi

    # Security sandbox (write isolation, /tmp loophole, Python escapes, SSRF, symlinks)
    echo "  Running: security sandbox tests"
    if cargo test -p octos-agent --test security_sandbox $TEST_THREADS_FLAG 2>&1 | tee /tmp/octos-ci-security.log | tail -5; then
        N=$(grep "^test result:" /tmp/octos-ci-security.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "security sandbox ($N tests)"
    else
        fail "security sandbox"
    fi

    # Session persistence (JSONL, LRU, fork, rewrite, sort)
    echo "  Running: session persistence tests"
    if cargo test -p octos-bus session::tests $TEST_THREADS_FLAG 2>&1 | tee /tmp/octos-ci-session.log | tail -5; then
        N=$(grep "^test result:" /tmp/octos-ci-session.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "session persistence ($N tests)"
    else
        fail "session persistence"
    fi

    # octos-cli with API feature
    if [ "$QUICK" = false ]; then
        echo "  Running: octos-cli with API feature"
        if cargo test -p octos-cli --features api $TEST_THREADS_FLAG 2>&1 | tail -3; then
            pass "octos-cli --features api"
        else
            fail "octos-cli --features api"
        fi
    fi
fi

# ── 4. Build check (quick mode skips this) ────────────────────────────
if [ "$QUICK" = false ] && [ -z "$SUBSYSTEM" ]; then
    section "Build Check"
    if cargo build --workspace 2>&1 | tail -3; then
        pass "workspace build"
    else
        fail "workspace build"
    fi
fi

# ── Summary ───────────────────────────────────────────────────────────
ELAPSED=$(( $(date +%s) - STARTED ))
section "Done"
echo "  $PASS passed, $FAIL failed (${ELAPSED}s)"
echo ""
echo "  Test coverage:"
echo "    • Adaptive routing: Off, Hedge (racing), Lane (score-based)"
echo "    • Circuit breaker, failover, metrics, scoring"
echo "    • QoS cold-start/runtime catalog materialization and pipeline fallback ordering"
echo "    • Responsiveness: baseline learning, degradation, recovery"
echo "    • Queue modes: Followup, Collect, Steer, Speculative"
echo "    • Speculative overflow: concurrent, patience threshold, background tasks"
echo "    • Auto-escalation: degradation → Hedge+Speculative, recovery → Off+Followup"
echo "    • Security: write isolation, /tmp loophole, Python escapes, cross-user, SSRF, symlinks"
echo "    • Session: persistence, LRU, fork, rewrite, timestamp sort"

if [ "$FAIL" -gt 0 ]; then
    exit 1
else
    echo "All checks passed."
    exit 0
fi
