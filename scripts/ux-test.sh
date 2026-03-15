#!/usr/bin/env bash
# Run all real-LLM UX integration tests.
#
# Requires: KIMI_API_KEY, DEEPSEEK_API_KEY (exported or in environment)
#
# Usage:
#   ./scripts/ux-test.sh              # Run all UX tests
#   ./scripts/ux-test.sh queue        # Run only queue mode tests
#   ./scripts/ux-test.sh adaptive     # Run only adaptive routing tests
#   ./scripts/ux-test.sh session      # Run only session switching test
set -euo pipefail

: "${KIMI_API_KEY:?KIMI_API_KEY required}"
: "${DEEPSEEK_API_KEY:?DEEPSEEK_API_KEY required}"

FILTER="${1:-all}"

# Build test binaries (suppressing warnings)
echo "Building test binaries..."
cargo test -p crew-llm --test ux_adaptive --no-run 2>/dev/null
cargo test -p crew-cli queue_ux --no-run 2>/dev/null

# Find the test binaries
LLM_BIN=$(cargo test -p crew-llm --test ux_adaptive --no-run 2>&1 | grep 'Executable tests/ux_adaptive' | awk '{print $NF}' | tr -d '()')
CLI_BIN=$(cargo test -p crew-cli queue_ux --no-run 2>&1 | grep 'Executable unittests' | awk '{print $NF}' | tr -d '()')

run_test() {
    local bin="$1" name="$2"
    echo ""
    echo "=== $name ==="
    if "$bin" "$name" --ignored --nocapture 2>&1; then
        echo "PASS: $name"
    else
        echo "FAIL: $name"
        FAILED=1
    fi
}

FAILED=0

# Adaptive routing tests (crew-llm)
if [[ "$FILTER" == "all" || "$FILTER" == "adaptive" ]]; then
    run_test "$LLM_BIN" test_kimi_responds
    run_test "$LLM_BIN" test_deepseek_responds
    run_test "$LLM_BIN" test_hedge_mode_races_two_providers
    run_test "$LLM_BIN" test_hedge_mode_3_queries_builds_metrics
    run_test "$LLM_BIN" test_lane_mode_selects_best_provider
    run_test "$LLM_BIN" test_failover_from_broken_to_working
    run_test "$LLM_BIN" test_multi_turn_context_preservation
    run_test "$LLM_BIN" test_responsiveness_baseline_learning
fi

# Queue mode tests (crew-cli)
if [[ "$FILTER" == "all" || "$FILTER" == "queue" ]]; then
    run_test "$CLI_BIN" queue_ux_followup_real_llm
    run_test "$CLI_BIN" queue_ux_collect_real_llm
    run_test "$CLI_BIN" queue_ux_steer_real_llm
    run_test "$CLI_BIN" queue_ux_speculative_real_llm
    run_test "$CLI_BIN" queue_ux_hedge_mode_real_llm
fi

# Session switching + deep search buffering tests (crew-cli)
if [[ "$FILTER" == "all" || "$FILTER" == "session" ]]; then
    run_test "$CLI_BIN" queue_ux_session_switching_real_llm
    run_test "$CLI_BIN" queue_ux_deep_search_switch_back_real_llm
fi

echo ""
if [[ "$FAILED" == "0" ]]; then
    echo "All UX tests passed!"
else
    echo "Some tests FAILED"
    exit 1
fi
