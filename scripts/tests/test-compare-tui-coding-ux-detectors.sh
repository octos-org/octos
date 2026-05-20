#!/usr/bin/env bash
# test-compare-tui-coding-ux-detectors.sh — Deterministic detector
# regression tests for scripts/compare-tui-coding-ux-tmux.sh.
#
# Pins the active/ready-state vocabulary the harness must recognize so
# detector drift (#854 — deepseek-v4-pro-live transcripts showed
# `state ◒ Working (...)` which the legacy `running|blocked` regex
# missed) does not slip back in unnoticed.
#
# Runs entirely offline. No tmux, no octos-tui process. The script is
# sourced so the detector helpers are loaded as bash functions; the
# soak `main` invocation is guarded by `BASH_SOURCE` so sourcing does
# not start the long-running flow.

set -eEuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET="$ROOT_DIR/scripts/compare-tui-coding-ux-tmux.sh"

PASS=0
FAIL=0

pass() {
  echo "  OK:   $*"
  PASS=$((PASS + 1))
}

fail() {
  echo "  FAIL: $*" >&2
  FAIL=$((FAIL + 1))
}

assert_active() {
  local label="$1"
  local capture="$2"
  if tui_capture_has_active_state "$capture"; then
    pass "$label"
  else
    fail "$label — capture not recognized as active"
  fi
}

assert_not_active() {
  local label="$1"
  local capture="$2"
  if tui_capture_has_active_state "$capture"; then
    fail "$label — capture wrongly recognized as active"
  else
    pass "$label"
  fi
}

assert_ready() {
  local label="$1"
  local capture="$2"
  if tui_capture_has_ready_state "$capture"; then
    pass "$label"
  else
    fail "$label — capture not recognized as ready"
  fi
}

echo "==> compare-tui-coding-ux-tmux.sh detector tests"
echo "  target: $TARGET"

# Source the script. The `main` invocation is guarded by BASH_SOURCE so
# only helpers are loaded.
# shellcheck source=/dev/null
source "$TARGET"

# #854: deepseek-v4-pro-live footer vocabulary.
assert_active "Working state (deepseek-v4-pro-live transcript)" "$(cat <<'EOF'
some pane content
state ◒ Working (foo bar)
> _ Octos TUI
EOF
)"

assert_active "Progress state" "$(cat <<'EOF'
state ◐ Progress (compiling)
EOF
)"

assert_active "Streaming state" "$(cat <<'EOF'
state ◑ Streaming (responding)
EOF
)"

# Legacy active states must still match so the regression is one-way.
assert_active "running state (legacy)" "$(cat <<'EOF'
state ◒ running (foo)
EOF
)"

assert_active "blocked state (legacy)" "$(cat <<'EOF'
state ◒ blocked (approval)
EOF
)"

assert_active "Approval Requested banner" "$(cat <<'EOF'
Approval Requested: please confirm
EOF
)"

# Idle/done snapshots must NOT be considered active.
assert_not_active "done state" "$(cat <<'EOF'
state ◒ done (turn completed)
>_ Octos TUI  idle
EOF
)"

assert_not_active "idle composer" "$(cat <<'EOF'
>_ Octos TUI  idle
Ask Octos to change code
EOF
)"

# Ready-state detector recognizes the same idle/done capture.
assert_ready "ready-state idle composer" "$(cat <<'EOF'
>_ Octos TUI  idle
Ask Octos to change code
EOF
)"

assert_ready "ready-state done footer" "$(cat <<'EOF'
state ◒ done (turn completed)
Composer
EOF
)"

echo
echo "==> Detector test summary: $PASS passed, $FAIL failed"
if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
