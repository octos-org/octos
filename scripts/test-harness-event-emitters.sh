#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PY_HELPER="$ROOT_DIR/examples/harness-event/python/emit_progress.py"
JS_HELPER="$ROOT_DIR/examples/harness-event/node/emit_progress.mjs"
FIXTURE="$ROOT_DIR/examples/harness-event/fixtures/progress-event.jsonl"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

require_tool() {
    command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}

assert_file_matches_fixture() {
    local actual="$1"
    cmp -s "$actual" "$FIXTURE" || {
        echo "expected:" >&2
        cat "$FIXTURE" >&2
        echo "actual:" >&2
        cat "$actual" >&2
        fail "fixture mismatch for $actual"
    }
}

main() {
    require_tool python3
    require_tool node

    WORK_DIR="$(mktemp -d /tmp/octos-harness-event.XXXXXX)"
    trap 'rm -rf "$WORK_DIR"' EXIT

    PY_OUT="$WORK_DIR/python.jsonl"
    JS_OUT="$WORK_DIR/node.jsonl"

    OCTOS_EVENT_SINK="file://$PY_OUT" python3 "$PY_HELPER" \
        --session-id sess-123 \
        --task-id task-456 \
        --workflow deep_research \
        --phase fetching_sources \
        --message "Fetching source 3/12" \
        --progress 0.42 \
        >"$WORK_DIR/python.stdout" 2>"$WORK_DIR/python.stderr"
    [ -s "$PY_OUT" ] || fail "python emitter did not write the fixture line"
    [ ! -s "$WORK_DIR/python.stderr" ] || fail "python emitter should not write diagnostics on success"
    assert_file_matches_fixture "$PY_OUT"

    OCTOS_EVENT_SINK="file://$JS_OUT" node "$JS_HELPER" \
        --session-id sess-123 \
        --task-id task-456 \
        --workflow deep_research \
        --phase fetching_sources \
        --message "Fetching source 3/12" \
        --progress 0.42 \
        >"$WORK_DIR/node.stdout" 2>"$WORK_DIR/node.stderr"
    [ -s "$JS_OUT" ] || fail "node emitter did not write the fixture line"
    [ ! -s "$WORK_DIR/node.stderr" ] || fail "node emitter should not write diagnostics on success"
    assert_file_matches_fixture "$JS_OUT"

    OCTOS_EVENT_SINK= python3 "$PY_HELPER" \
        --session-id sess-123 \
        --task-id task-456 \
        --workflow deep_research \
        --phase fetching_sources \
        --message "Fetching source 3/12" \
        --progress 0.42 \
        >"$WORK_DIR/python-noop.stdout" 2>"$WORK_DIR/python-noop.stderr"
    [ ! -s "$WORK_DIR/python-noop.stdout" ] || fail "python emitter should stay silent when OCTOS_EVENT_SINK is missing"
    [ ! -s "$WORK_DIR/python-noop.stderr" ] || fail "python emitter should not emit diagnostics when OCTOS_EVENT_SINK is missing"

    OCTOS_EVENT_SINK= node "$JS_HELPER" \
        --session-id sess-123 \
        --task-id task-456 \
        --workflow deep_research \
        --phase fetching_sources \
        --message "Fetching source 3/12" \
        --progress 0.42 \
        >"$WORK_DIR/node-noop.stdout" 2>"$WORK_DIR/node-noop.stderr"
    [ ! -s "$WORK_DIR/node-noop.stdout" ] || fail "node emitter should stay silent when OCTOS_EVENT_SINK is missing"
    [ ! -s "$WORK_DIR/node-noop.stderr" ] || fail "node emitter should not emit diagnostics when OCTOS_EVENT_SINK is missing"

    echo "harness event emitter tests passed"
}

main "$@"
