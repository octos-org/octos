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

# ── 1. Format ─────────────────────────────────────────────────────────
section "Format"
if [ "$FIX" = true ]; then
    cargo fmt --all
    pass "cargo fmt --all (fixed)"
else
    if cargo fmt --all -- --check 2>&1; then
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
    CRATE="crew-$SUBSYSTEM"
    if cargo test -p "$CRATE" $TEST_THREADS_FLAG 2>&1 | tee /tmp/crew-ci-sub.log | tail -5; then
        SUB_PASS=$(grep "^test result:" /tmp/crew-ci-sub.log | awk '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "$CRATE tests ($SUB_PASS passed)"
    else
        fail "$CRATE tests"
    fi
else
    section "Tests"

    # 3a. Workspace tests (all crates)
    echo "  Running: cargo test --workspace"
    if cargo test --workspace $TEST_THREADS_FLAG 2>&1 | tee /tmp/crew-ci-test.log | tail -20; then
        TOTAL=$(grep "^test result:" /tmp/crew-ci-test.log | \
            awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "cargo test --workspace ($TOTAL passed)"
    else
        fail "cargo test --workspace"
    fi

    # 3b. Focused test groups — verify critical subsystems explicitly
    section "Focused Test Groups"

    # Adaptive routing (Off/Hedge/Lane, circuit breaker, scoring, metrics)
    echo "  Running: adaptive routing tests"
    if cargo test -p crew-llm --lib adaptive::tests $TEST_THREADS_FLAG 2>&1 | tee /tmp/crew-ci-adaptive.log | tail -5; then
        N=$(grep "^test result:" /tmp/crew-ci-adaptive.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "adaptive routing ($N tests)"
    else
        fail "adaptive routing"
    fi

    # Responsiveness observer (baseline, degradation, recovery)
    echo "  Running: responsiveness observer tests"
    if cargo test -p crew-llm --lib responsiveness::tests $TEST_THREADS_FLAG 2>&1 | tee /tmp/crew-ci-resp.log | tail -5; then
        N=$(grep "^test result:" /tmp/crew-ci-resp.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "responsiveness observer ($N tests)"
    else
        fail "responsiveness observer"
    fi

    # Queue modes + speculative overflow + auto-escalation
    echo "  Running: session actor tests (queue modes, speculative, escalation)"
    if cargo test -p crew-cli session_actor::tests -- --test-threads=1 2>&1 | tee /tmp/crew-ci-actor.log | tail -5; then
        N=$(grep "^test result:" /tmp/crew-ci-actor.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "session actor ($N tests)"
    else
        fail "session actor"
    fi

    # Session persistence (JSONL, LRU, fork, rewrite, sort)
    echo "  Running: session persistence tests"
    if cargo test -p crew-bus session::tests $TEST_THREADS_FLAG 2>&1 | tee /tmp/crew-ci-session.log | tail -5; then
        N=$(grep "^test result:" /tmp/crew-ci-session.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
        pass "session persistence ($N tests)"
    else
        fail "session persistence"
    fi

    # crew-cli with API feature
    if [ "$QUICK" = false ]; then
        echo "  Running: crew-cli with API feature"
        if cargo test -p crew-cli --features api $TEST_THREADS_FLAG 2>&1 | tail -3; then
            pass "crew-cli --features api"
        else
            fail "crew-cli --features api"
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

# 3b. Focused test groups — verify critical subsystems explicitly
section "Focused Test Groups"

# Adaptive routing (Off/Hedge/Lane, circuit breaker, scoring, metrics)
echo "  Running: adaptive routing tests"
if cargo test -p crew-llm --lib adaptive::tests $TEST_THREADS_FLAG 2>&1 | tee /tmp/crew-ci-adaptive.log | tail -5; then
    N=$(grep "^test result:" /tmp/crew-ci-adaptive.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
    pass "adaptive routing ($N tests)"
else
    fail "adaptive routing"
fi

# Responsiveness observer (baseline, degradation, recovery)
echo "  Running: responsiveness observer tests"
if cargo test -p crew-llm --lib responsiveness::tests $TEST_THREADS_FLAG 2>&1 | tee /tmp/crew-ci-resp.log | tail -5; then
    N=$(grep "^test result:" /tmp/crew-ci-resp.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
    pass "responsiveness observer ($N tests)"
else
    fail "responsiveness observer"
fi

# Queue modes + speculative overflow + auto-escalation
echo "  Running: session actor tests (queue modes, speculative, escalation)"
if cargo test -p crew-cli session_actor::tests -- --test-threads=1 2>&1 | tee /tmp/crew-ci-actor.log | tail -5; then
    N=$(grep "^test result:" /tmp/crew-ci-actor.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
    pass "session actor ($N tests)"
else
    fail "session actor"
fi

# Security sandbox (write isolation, /tmp loophole, Python escapes, SSRF, symlinks)
echo "  Running: security sandbox tests"
if cargo test -p crew-agent --test security_sandbox $TEST_THREADS_FLAG 2>&1 | tee /tmp/crew-ci-security.log | tail -5; then
    N=$(grep "^test result:" /tmp/crew-ci-security.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
    pass "security sandbox ($N tests)"
else
    fail "security sandbox"
fi

# Session persistence (JSONL, LRU, fork, rewrite, sort)
echo "  Running: session persistence tests"
if cargo test -p crew-bus session::tests $TEST_THREADS_FLAG 2>&1 | tee /tmp/crew-ci-session.log | tail -5; then
    N=$(grep "^test result:" /tmp/crew-ci-session.log | awk -F'[;.]' '{for(i=1;i<=NF;i++){if($i~/passed/){gsub(/[^0-9]/,"",$i);p+=$i}}}END{print p+0}')
    pass "session persistence ($N tests)"
else
    fail "session persistence"
fi

# ── Summary ───────────────────────────────────────────────────────────
ELAPSED=$(( $(date +%s) - STARTED ))
section "Done"
echo "  $PASS passed, $FAIL failed (${ELAPSED}s)"
echo ""
echo "  Test coverage:"
echo "    • Adaptive routing: Off, Hedge (racing), Lane (score-based)"
echo "    • Circuit breaker, failover, metrics, scoring"
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
