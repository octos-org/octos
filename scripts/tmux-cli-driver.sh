#!/usr/bin/env bash
# Shell primitives for deterministic tmux-driven CLI/TUI acceptance tests.

if [ -n "${OCTOS_TMUX_DRIVER_SH:-}" ]; then
  return 0 2>/dev/null || exit 0
fi
OCTOS_TMUX_DRIVER_SH=1

set -euo pipefail

OCTOS_TMUX_ROOT="${OCTOS_TMUX_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
OCTOS_TMUX_COLS="${OCTOS_TMUX_COLS:-140}"
OCTOS_TMUX_ROWS="${OCTOS_TMUX_ROWS:-40}"
OCTOS_TMUX_PREFIX="${OCTOS_TMUX_PREFIX:-octos-tmux-}"
OCTOS_TMUX_RUN_ID="${OCTOS_TMUX_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
OCTOS_TMUX_RUN_PREFIX="${OCTOS_TMUX_PREFIX}${OCTOS_TMUX_RUN_ID}-"
OCTOS_TMUX_ARTIFACT_ROOT="${OCTOS_TMUX_ARTIFACT_ROOT:-${OCTOS_TMUX_ROOT}/e2e/test-results-tmux}"
OCTOS_TMUX_ARTIFACT_DIR="${OCTOS_TMUX_ARTIFACT_DIR:-${OCTOS_TMUX_ARTIFACT_ROOT}/${OCTOS_TMUX_RUN_ID}}"
OCTOS_TMUX_KEEP="${OCTOS_TMUX_KEEP:-0}"

declare -a OCTOS_TMUX_SESSIONS=()
declare -i OCTOS_TMUX_CAPTURE_SEQ=0

tmux_log() {
  printf '[tmux] %s\n' "$*"
}

tmux_skip() {
  printf 'SKIP: %s\n' "$*"
  exit 0
}

tmux_require() {
  if ! command -v tmux >/dev/null 2>&1; then
    tmux_skip "tmux is not installed; skipping tmux CLI/TUI harness"
  fi
}

tmux_init_artifacts() {
  mkdir -p "$OCTOS_TMUX_ARTIFACT_DIR"
}

tmux_session_name() {
  local slug="${1:-session}"
  slug="${slug//[^a-zA-Z0-9_-]/-}"
  printf '%s%s' "$OCTOS_TMUX_RUN_PREFIX" "$slug"
}

tmux_quote_command() {
  local quoted=""
  local arg
  local part

  if [ "$#" -eq 0 ]; then
    printf ':'
    return 0
  fi

  for arg in "$@"; do
    printf -v part '%q' "$arg"
    if [ -z "$quoted" ]; then
      quoted="$part"
    else
      quoted="$quoted $part"
    fi
  done
  printf '%s' "$quoted"
}

tmux_register_session() {
  local session="$1"
  OCTOS_TMUX_SESSIONS+=("$session")
}

tmux_unregister_session() {
  local session="$1"
  local next=()
  local item

  for item in "${OCTOS_TMUX_SESSIONS[@]}"; do
    if [ "$item" != "$session" ]; then
      next+=("$item")
    fi
  done
  if [ "${#next[@]}" -eq 0 ]; then
    OCTOS_TMUX_SESSIONS=()
  else
    OCTOS_TMUX_SESSIONS=("${next[@]}")
  fi
}

tmux_new() {
  local session="$1"
  local cols="$2"
  local rows="$3"
  shift 3

  if tmux has-session -t "$session" 2>/dev/null; then
    tmux kill-session -t "$session" 2>/dev/null || true
  fi

  local command
  command="$(tmux_quote_command "$@")"
  tmux_register_session "$session"
  tmux new-session -d -s "$session" -x "$cols" -y "$rows" "$command"
}

tmux_new_default() {
  local session="$1"
  shift
  tmux_new "$session" "$OCTOS_TMUX_COLS" "$OCTOS_TMUX_ROWS" "$@"
}

tmux_send() {
  local session="$1"
  local text="$2"
  tmux send-keys -t "$session" -l "$text"
}

tmux_key() {
  local session="$1"
  local key="$2"
  tmux send-keys -t "$session" "$key"
}

tmux_redact() {
  perl -0pe '
    BEGIN {
      @tokens = grep { defined && length } ($ENV{OCTOS_AUTH_TOKEN}, $ENV{OCTOS_TMUX_AUTH_TOKEN});
    }
    for my $token (@tokens) {
      s/\Q$token\E/[REDACTED]/g;
    }
    s/(Authorization:\s*Bearer\s+)[^\s]+/${1}[REDACTED]/gi;
    s/(--auth-token(?:=|\s+))[^\s]+/${1}[REDACTED]/g;
    s/(OCTOS_AUTH_TOKEN=)[^\s]+/${1}[REDACTED]/g;
  '
}

tmux_strip_ansi() {
  perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g; s/\e\][^\a]*(?:\a|\e\\)//g; s/\e[@-Z\\-_]//g'
}

tmux_artifact_path() {
  local session="$1"
  local label="$2"
  local suffix="$3"
  local safe_session="${session//[^a-zA-Z0-9_.-]/-}"
  local safe_label="${label//[^a-zA-Z0-9_.-]/-}"
  printf '%s/%03d-%s-%s.%s.log' \
    "$OCTOS_TMUX_ARTIFACT_DIR" \
    "$OCTOS_TMUX_CAPTURE_SEQ" \
    "$safe_session" \
    "$safe_label" \
    "$suffix"
}

tmux_capture() {
  local session="$1"
  local label="${2:-capture}"

  tmux_init_artifacts
  OCTOS_TMUX_CAPTURE_SEQ=$((OCTOS_TMUX_CAPTURE_SEQ + 1))

  local raw_path
  raw_path="$(tmux_artifact_path "$session" "$label" "raw")"

  if tmux has-session -t "$session" 2>/dev/null; then
    tmux capture-pane -t "$session" -p -e -J -S - | tmux_redact >"$raw_path"
  else
    printf 'tmux session not found: %s\n' "$session" | tmux_redact >"$raw_path"
    cat "$raw_path"
    return 1
  fi

  cat "$raw_path"
}

tmux_capture_clean() {
  local session="$1"
  local label="${2:-capture}"

  tmux_init_artifacts
  OCTOS_TMUX_CAPTURE_SEQ=$((OCTOS_TMUX_CAPTURE_SEQ + 1))

  local raw_path
  local clean_path
  raw_path="$(tmux_artifact_path "$session" "$label" "raw")"
  clean_path="$(tmux_artifact_path "$session" "$label" "clean")"

  if tmux has-session -t "$session" 2>/dev/null; then
    tmux capture-pane -t "$session" -p -e -J -S - | tmux_redact >"$raw_path"
    tmux_strip_ansi <"$raw_path" >"$clean_path"
  else
    printf 'tmux session not found: %s\n' "$session" | tmux_redact >"$raw_path"
    tmux_strip_ansi <"$raw_path" >"$clean_path"
    cat "$clean_path"
    return 1
  fi

  cat "$clean_path"
}

tmux_print_failure_capture() {
  local session="$1"
  local message="$2"
  printf '\nFAIL: %s\n' "$message" >&2
  printf -- '--- last clean tmux capture: %s ---\n' "$session" >&2
  tmux_capture_clean "$session" "failure" >&2 || true
  printf -- '--- artifacts: %s ---\n' "$OCTOS_TMUX_ARTIFACT_DIR" >&2
}

tmux_fail() {
  local session="$1"
  local message="$2"
  tmux_print_failure_capture "$session" "$message"
  exit 1
}

tmux_wait_for() {
  local session="$1"
  local regex="$2"
  local timeout="${3:-10}"
  local deadline=$((SECONDS + timeout))
  local capture

  while [ "$SECONDS" -le "$deadline" ]; do
    capture="$(tmux_capture_clean "$session" "wait" || true)"
    if printf '%s\n' "$capture" | grep -E -q -- "$regex"; then
      return 0
    fi
    sleep 0.2
  done

  tmux_fail "$session" "timed out waiting ${timeout}s for regex: ${regex}"
}

tmux_assert_capture() {
  local session="$1"
  local regex="$2"
  local capture
  capture="$(tmux_capture_clean "$session" "assert" || true)"
  if ! printf '%s\n' "$capture" | grep -E -q -- "$regex"; then
    tmux_fail "$session" "expected capture to match regex: ${regex}"
  fi
}

tmux_assert_not_capture() {
  local session="$1"
  local regex="$2"
  local capture
  capture="$(tmux_capture_clean "$session" "assert-not" || true)"
  if printf '%s\n' "$capture" | grep -E -q -- "$regex"; then
    tmux_fail "$session" "expected capture not to match regex: ${regex}"
  fi
}

tmux_kill() {
  local session="$1"
  if tmux has-session -t "$session" 2>/dev/null; then
    tmux kill-session -t "$session" 2>/dev/null || true
  fi
  tmux_unregister_session "$session"
}

tmux_cleanup() {
  local session
  if [ "${OCTOS_TMUX_KEEP:-0}" = "1" ]; then
    set +u
    if [ "${#OCTOS_TMUX_SESSIONS[@]}" -gt 0 ]; then
      tmux_log "OCTOS_TMUX_KEEP=1; keeping sessions: ${OCTOS_TMUX_SESSIONS[*]}"
    fi
    set -u
    return 0
  fi

  set +u
  for session in "${OCTOS_TMUX_SESSIONS[@]}"; do
    tmux kill-session -t "$session" 2>/dev/null || true
  done
  set -u
}

tmux_install_cleanup_trap() {
  trap tmux_cleanup EXIT INT TERM
}

tmux_run_line_command() {
  local session="$1"
  shift

  local command
  command="$(tmux_quote_command "$@")"
  local wrapped
  printf -v wrapped '%s\nrc=$?\nprintf "\\n__OCTOS_TMUX_EXIT:%%s__\\n" "$rc"\nwhile :; do sleep 3600; done\n' "$command"
  tmux_new_default "$session" bash -lc "$wrapped"
}

tmux_wait_for_exit() {
  local session="$1"
  local timeout="${2:-20}"
  tmux_wait_for "$session" '__OCTOS_TMUX_EXIT:[0-9]+__' "$timeout"
}

tmux_assert_exit_status() {
  local session="$1"
  local expected="$2"
  local capture
  capture="$(tmux_capture_clean "$session" "exit-status" || true)"
  if ! printf '%s\n' "$capture" | grep -E -q -- "__OCTOS_TMUX_EXIT:${expected}__"; then
    tmux_fail "$session" "expected exit status ${expected}"
  fi
}

tmux_assert_no_registered_sessions() {
  local live=()
  local session
  set +u
  for session in "${OCTOS_TMUX_SESSIONS[@]}"; do
    if tmux has-session -t "$session" 2>/dev/null; then
      live+=("$session")
    fi
  done
  set -u
  if [ "${#live[@]}" -gt 0 ]; then
    printf 'ASSERTION FAILED: registered tmux sessions remain:\n' >&2
    printf '  %s\n' "${live[@]}" >&2
    return 1
  fi
}

tmux_assert_no_orphan_sessions() {
  local live=()
  local session
  while IFS= read -r session; do
    if [[ "$session" == "$OCTOS_TMUX_RUN_PREFIX"* ]]; then
      live+=("$session")
    fi
  done < <(tmux list-sessions -F '#S' 2>/dev/null || true)

  if [ "${#live[@]}" -gt 0 ]; then
    printf 'ASSERTION FAILED: tmux sessions remain for this run:\n' >&2
    printf '  %s\n' "${live[@]}" >&2
    return 1
  fi
}
