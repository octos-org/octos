#!/usr/bin/env bash
# Drive the real octos-tui protocol client and Codex through tmux on the same
# coding fixture. The Octos server is only the AppUi/UI Protocol backend; the
# sole Octos product client under test is standalone octos-tui.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_ID="${OCTOS_TUI_UX_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
RUN_STARTED_AT_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
RUN_START_EPOCH="$(date +%s)"
export OCTOS_TMUX_RUN_ID="${OCTOS_TMUX_RUN_ID:-$RUN_ID}"

# shellcheck source=tmux-cli-driver.sh
source "$ROOT_DIR/scripts/tmux-cli-driver.sh"

OCTOS_TUI_DIR="${OCTOS_TUI_DIR:-$ROOT_DIR/../octos-tui}"
LONG_MODE="${OCTOS_TUI_UX_LONG:-0}"
if [ "$LONG_MODE" = "1" ] && [ -z "${OCTOS_TUI_UX_FIXTURE_DIR+x}" ]; then
  FIXTURE_DIR="$ROOT_DIR/e2e/fixtures/coding-agent-long-workspace"
else
  FIXTURE_DIR="${OCTOS_TUI_UX_FIXTURE_DIR:-$ROOT_DIR/e2e/fixtures/coding-agent-compare-multifile}"
fi
OUT_DIR="${OCTOS_TUI_UX_OUT_DIR:-$ROOT_DIR/e2e/test-results-tui-coding-ux/$RUN_ID}"
PROVIDER="${OCTOS_TUI_UX_PROVIDER:-deepseek}"
MODEL="${OCTOS_TUI_UX_MODEL:-${DEEPSEEK_MODEL:-deepseek-v4-pro}}"
case "$PROVIDER" in
  openai)
    API_KEY_ENV="${OCTOS_TUI_UX_API_KEY_ENV:-OPENAI_API_KEY}"
    ;;
  deepseek)
    API_KEY_ENV="${OCTOS_TUI_UX_API_KEY_ENV:-DEEPSEEK_API_KEY}"
    ;;
  *)
    API_KEY_ENV="${OCTOS_TUI_UX_API_KEY_ENV:-$(printf '%s_API_KEY' "$PROVIDER" | tr '[:lower:]' '[:upper:]')}"
    ;;
esac
PORT="${OCTOS_TUI_UX_PORT:-$((51000 + $$ % 10000))}"
AUTH_TOKEN="${OCTOS_TUI_UX_AUTH_TOKEN:-octos-tui-ux-token-$RUN_ID}"
SESSION_ID="${OCTOS_TUI_UX_SESSION_ID:-coding:local:prototype#tui-compare}"
MAX_WAIT_SHORT="${OCTOS_TUI_UX_WAIT_SHORT:-90}"
if [ "$LONG_MODE" = "1" ] && [ -z "${OCTOS_TUI_UX_WAIT_TURN+x}" ]; then
  MAX_WAIT_TURN=1800
else
  MAX_WAIT_TURN="${OCTOS_TUI_UX_WAIT_TURN:-900}"
fi
if [ "$LONG_MODE" = "1" ] && [ -z "${OCTOS_TUI_UX_TUI_CODING_ROUNDS+x}" ]; then
  MAX_TUI_CODING_ROUNDS=8
else
  MAX_TUI_CODING_ROUNDS="${OCTOS_TUI_UX_TUI_CODING_ROUNDS:-4}"
fi
COMPAT_PROXY_PORT="${OCTOS_TUI_UX_COMPAT_PROXY_PORT:-18081}"
RUN_TUI="${OCTOS_TUI_UX_RUN_TUI:-1}"
RUN_CODEX="${OCTOS_TUI_UX_RUN_CODEX:-1}"
STRICT="${OCTOS_TUI_UX_STRICT:-1}"
TUI_DENY_KEY="${OCTOS_TUI_UX_TUI_DENY_KEY:-n}"
FRAME_SAMPLE_ENABLED="${OCTOS_TUI_UX_FRAME_SAMPLE:-$LONG_MODE}"
FRAME_SAMPLE_INTERVAL="${OCTOS_TUI_UX_FRAME_SAMPLE_INTERVAL:-3}"
SERVER_ERROR_BUDGET="${OCTOS_TUI_UX_SERVER_ERROR_BUDGET:-0}"
QUESTION_REGEX="${OCTOS_TUI_UX_QUESTION_REGEX:-\\?}"
COMPLETION_REGEX="${OCTOS_TUI_UX_COMPLETION_REGEX:-test result: ok|Finished .*test.* profile|All tests passed|all tests passed}"
SERVER_SANDBOX_POLICY="${OCTOS_TUI_UX_SERVER_SANDBOX_POLICY:-auto}"
RESOLVED_SERVER_SANDBOX_MODE=""
COMPAT_PROXY_PID=""
declare -a FRAME_SAMPLER_PIDS=()
LAST_FRAME_SAMPLER_PID=""

DEFAULT_PROMPT_QUESTION="Before editing, ask exactly one concise implementation preference question about duplicate-task diagnostics. Do not modify files yet."
DEFAULT_PROMPT_CODING="The sudo probe was denied. Continue without sudo. Use deterministic duplicate-task errors that include the duplicate task name. State a short coding plan with checkbox steps. Fix the production Rust code so all existing tests pass. Do not change tests or Cargo.toml. Run cargo test once to inspect failures, then edit production files with file-edit tools. Do not keep retrying shell before editing. Update src/parser.rs, src/planner.rs, and src/summary.rs as needed, rerun cargo test after edits, and iterate until green. Preserve the public API shape. Finish with a short Session Summary listing files changed and validation."
DEFAULT_PROMPT_CONTINUE="Continue the current coding task now. Do not run shell again until you have edited production Rust files. Use read_file and file-edit tools on src/parser.rs, src/planner.rs, and src/summary.rs, then run cargo test and iterate until all six tests pass. Finish with a short Session Summary listing files changed and validation."
DEFAULT_PROMPT_SUMMARY="The fixture tests are green now. Provide only a short Session Summary with files changed and validation. Do not edit more files."
DEFAULT_PROMPT_STEERING="Tests are green. Make one small production-code maintainability refactor without changing public API or tests, rerun cargo test, and summarize changed files and validation."
DEFAULT_PROMPT_INTERRUPT="Start a careful second pass reviewing edge cases and propose one small production-code improvement if needed. Do not finish immediately; show progress first."
DEFAULT_PROMPT_RECONNECT="After client restart, continue from the current worktree state. Inspect git diff, rerun cargo test, and summarize whether worktree state and task context are clear."
DEFAULT_PROMPT_FINAL_LONG="Provide the final long-mode Session Summary with files changed, tests run, approval recovery, interrupt or restart recovery, risks, and next steps. Do not edit more files."

if [ "$LONG_MODE" = "1" ]; then
  DEFAULT_PROMPT_QUESTION="Before editing, ask exactly one concise implementation preference question about worklog owner and report-ordering diagnostics. Do not modify files yet."
  DEFAULT_PROMPT_CODING="The sudo probe was denied. Continue without sudo. Work in long mode on this Rust workspace. State a short coding plan with checkbox steps. Run cargo test --workspace once to inspect failures, then edit only production Rust files under crates/worklog-core/src and crates/worklog-report/src. Do not change tests or Cargo.toml. Preserve public API names and deterministic output ordering. Rerun cargo test --workspace after edits and iterate until green. Finish with a short Session Summary listing files changed, validation, risks, and next steps."
  DEFAULT_PROMPT_CONTINUE="Continue the current long coding task now. Inspect the current diff and any remaining failures. Edit production Rust files before rerunning broad tests, then run cargo test --workspace and iterate until green. Finish with a short Session Summary listing files changed and validation."
  DEFAULT_PROMPT_SUMMARY="The workspace tests are green now. Provide only a short Session Summary with files changed, validation, risks, and next steps. Do not edit more files."
  DEFAULT_PROMPT_STEERING="Tests are green. Make one small production-code maintainability refactor without changing public API, tests, or Cargo.toml. Rerun cargo test --workspace and summarize changed files and validation."
  DEFAULT_PROMPT_INTERRUPT="Start a careful second pass reviewing edge cases in the worklog parser, scheduler, owner aggregation, and renderer. Do not finish immediately; show progress before finalizing."
  DEFAULT_PROMPT_RECONNECT="After client restart, continue from the current worktree state. Inspect git diff, rerun cargo test --workspace, and summarize whether worktree state and task context are clear."
fi

PROMPT_QUESTION="${OCTOS_TUI_UX_PROMPT_QUESTION:-$DEFAULT_PROMPT_QUESTION}"
PROMPT_APPROVAL="${OCTOS_TUI_UX_PROMPT_APPROVAL:-Before coding, invoke exactly one shell command now: sudo true. Do not inspect files, do not edit files, and do not explain first.}"
PROMPT_CODING="${OCTOS_TUI_UX_PROMPT_CODING:-$DEFAULT_PROMPT_CODING}"
PROMPT_CONTINUE="${OCTOS_TUI_UX_PROMPT_CONTINUE:-$DEFAULT_PROMPT_CONTINUE}"
PROMPT_SUMMARY="${OCTOS_TUI_UX_PROMPT_SUMMARY:-$DEFAULT_PROMPT_SUMMARY}"
PROMPT_STEERING="${OCTOS_TUI_UX_PROMPT_STEERING:-$DEFAULT_PROMPT_STEERING}"
PROMPT_INTERRUPT="${OCTOS_TUI_UX_PROMPT_INTERRUPT:-$DEFAULT_PROMPT_INTERRUPT}"
PROMPT_RECONNECT="${OCTOS_TUI_UX_PROMPT_RECONNECT:-$DEFAULT_PROMPT_RECONNECT}"
PROMPT_FINAL_LONG="${OCTOS_TUI_UX_PROMPT_FINAL_LONG:-$DEFAULT_PROMPT_FINAL_LONG}"

log() {
  printf '[tui-ux] %s\n' "$*"
}

require_command() {
  local command="$1"
  if ! command -v "$command" >/dev/null 2>&1; then
    printf 'missing required command: %s\n' "$command" >&2
    exit 2
  fi
}

require_fixture() {
  if [ ! -f "$FIXTURE_DIR/Cargo.toml" ]; then
    printf 'fixture is missing Cargo.toml: %s\n' "$FIXTURE_DIR" >&2
    exit 2
  fi
  if [ ! -d "$FIXTURE_DIR/src" ] && [ ! -d "$FIXTURE_DIR/crates" ]; then
    printf 'fixture is missing or incomplete: %s\n' "$FIXTURE_DIR" >&2
    exit 2
  fi
}

macos_sandbox_exec_blocked_by_outer_sandbox() {
  [ "$(uname -s 2>/dev/null || true)" = "Darwin" ] || return 1
  command -v sandbox-exec >/dev/null 2>&1 || return 1

  local probe_dir
  local stderr_path
  probe_dir="$(mktemp -d "$OUT_DIR/sandbox-probe.XXXXXX")"
  stderr_path="$probe_dir/stderr.log"
  if sandbox-exec -p '(version 1) (allow default)' /usr/bin/true \
    >"$probe_dir/stdout.log" 2>"$stderr_path"; then
    rm -rf "$probe_dir"
    return 1
  fi

  if grep -F -q 'sandbox_apply: Operation not permitted' "$stderr_path"; then
    rm -rf "$probe_dir"
    return 0
  fi
  rm -rf "$probe_dir"
  return 1
}

resolve_server_sandbox_mode() {
  case "$SERVER_SANDBOX_POLICY" in
    auto)
      if macos_sandbox_exec_blocked_by_outer_sandbox; then
        printf 'outer-sandbox-fallback'
      else
        printf 'product-default'
      fi
      ;;
    product-default | default | strict | never | 0)
      printf 'product-default'
      ;;
    outer-sandbox-fallback | disable-inner | force | 1)
      printf 'outer-sandbox-fallback'
      ;;
    *)
      printf 'unknown OCTOS_TUI_UX_SERVER_SANDBOX_POLICY=%s\n' "$SERVER_SANDBOX_POLICY" >&2
      exit 2
      ;;
  esac
}

maybe_write_harness_server_config() {
  local name="$1"
  local dir="$2"

  [ "$name" = "octos_tui" ] || return 0
  [ "$RESOLVED_SERVER_SANDBOX_MODE" = "outer-sandbox-fallback" ] || return 0

  mkdir -p "$dir/.octos"
  cat >"$dir/.octos/config.json" <<'JSON'
{
  "permission_mode": "danger-full-access",
  "approval_policy": "on-request",
  "sandbox": {
    "enabled": false,
    "mode": "none",
    "allow_network": true
  }
}
JSON
  printf '[tui-ux] server sandbox: inner sandbox disabled for throwaway octos-tui lane because outer macOS sandbox blocks sandbox-exec\n' >&2
}

cleanup_proxy() {
  if [ -n "$COMPAT_PROXY_PID" ]; then
    kill "$COMPAT_PROXY_PID" >/dev/null 2>&1 || true
    wait "$COMPAT_PROXY_PID" >/dev/null 2>&1 || true
  fi
}

cleanup_frame_samplers() {
  local pid

  set +u
  for pid in "${FRAME_SAMPLER_PIDS[@]}"; do
    kill "$pid" >/dev/null 2>&1 || true
    wait "$pid" >/dev/null 2>&1 || true
  done
  FRAME_SAMPLER_PIDS=()
  set -u
}

cleanup_all() {
  cleanup_frame_samplers
  cleanup_proxy
  tmux_cleanup
}

resolve_octos_bin() {
  if [ -n "${OCTOS_BIN:-}" ]; then
    printf '%s\n' "$OCTOS_BIN"
    return 0
  fi
  if [ ! -x "$ROOT_DIR/target/debug/octos" ]; then
    log "building octos backend"
    cargo build -p octos-cli --features api --bin octos
  fi
  printf '%s\n' "$ROOT_DIR/target/debug/octos"
}

resolve_tui_bin() {
  if [ -n "${OCTOS_TUI_BIN:-}" ]; then
    printf '%s\n' "$OCTOS_TUI_BIN"
    return 0
  fi
  if [ ! -x "$OCTOS_TUI_DIR/target/debug/octos-tui" ]; then
    log "building octos-tui client"
    cargo build --manifest-path "$OCTOS_TUI_DIR/Cargo.toml" --bin octos-tui
  fi
  printf '%s\n' "$OCTOS_TUI_DIR/target/debug/octos-tui"
}

start_compat_proxy_if_needed() {
  if [ "$RUN_CODEX" != "1" ] \
    || [ "$PROVIDER" != "deepseek" ] \
    || [ "${CODEX_COMPARE_USE_COMPAT_PROXY:-1}" != "1" ]; then
    return 0
  fi

  local proxy_log="$OUT_DIR/deepseek-compat-proxy.log"
  "$ROOT_DIR/scripts/deepseek-chat-compat-proxy.py" --port "$COMPAT_PROXY_PORT" \
    >"$proxy_log" 2>&1 &
  COMPAT_PROXY_PID=$!

  local deadline=$((SECONDS + 10))
  while [ "$SECONDS" -le "$deadline" ]; do
    if grep -q "listening http://127.0.0.1:$COMPAT_PROXY_PORT" "$proxy_log" 2>/dev/null; then
      log "Codex compat proxy: http://127.0.0.1:$COMPAT_PROXY_PORT"
      return 0
    fi
    if ! kill -0 "$COMPAT_PROXY_PID" >/dev/null 2>&1; then
      printf 'DeepSeek compat proxy exited early. See %s\n' "$proxy_log" >&2
      return 1
    fi
    sleep 0.2
  done

  printf 'DeepSeek compat proxy did not start within 10s. See %s\n' "$proxy_log" >&2
  return 1
}

prepare_candidate() {
  local name="$1"
  local dir="$OUT_DIR/$name"
  rm -rf "$dir"
  mkdir -p "$OUT_DIR"
  cp -R "$FIXTURE_DIR" "$dir"
  rm -rf "$dir/target"
  maybe_write_harness_server_config "$name" "$dir"
  (
    cd "$dir"
    git init -q
    git add .
    git -c user.name=octos-tui-ux -c user.email=octos-tui-ux@example.invalid \
      commit -q -m 'initial fixture'
  )
  printf '%s\n' "$dir"
}

validate_candidate() {
  local name="$1"
  local dir="$2"
  local log_path="$OUT_DIR/$name-cargo-test.log"
  (
    cd "$dir"
    cargo test --quiet
  ) >"$log_path" 2>&1
}

write_candidate_artifacts() {
  local name="$1"
  local dir="$2"
  (
    cd "$dir"
    git diff >"$OUT_DIR/$name-worktree.diff"
    git status --short >"$OUT_DIR/$name-git-status.txt"
  )
}

capture_clean() {
  local session="$1"
  if tmux has-session -t "$session" 2>/dev/null; then
    tmux capture-pane -t "$session" -p -e -J -S - | tmux_redact | tmux_strip_ansi
  fi
}

capture_visible_clean() {
  local session="$1"
  if tmux has-session -t "$session" 2>/dev/null; then
    tmux capture-pane -t "$session" -p -e -J | tmux_redact | tmux_strip_ansi
  fi
}

capture_clean_to_file() {
  local session="$1"
  local path="$2"
  capture_clean "$session" >"$path"
}

append_capture_clean_to_file() {
  local session="$1"
  local path="$2"
  local label="$3"
  {
    printf '\n===== %s =====\n' "$label"
    capture_clean "$session" || true
  } >>"$path"
}

capture_raw() {
  local session="$1"
  if tmux has-session -t "$session" 2>/dev/null; then
    tmux capture-pane -t "$session" -p -e -J -S - | tmux_redact
  fi
}

capture_visible_raw() {
  local session="$1"
  if tmux has-session -t "$session" 2>/dev/null; then
    tmux capture-pane -t "$session" -p -e -J | tmux_redact
  fi
}

append_capture_raw_to_file() {
  local session="$1"
  local path="$2"
  local label="$3"
  {
    printf '\n===== %s raw =====\n' "$label"
    capture_raw "$session" || true
  } >>"$path"
}

lane_frame_grep() {
  local lane="$1"
  local regex="$2"
  local frame_dir="$OUT_DIR/frames/$lane"

  [ -d "$frame_dir" ] || return 1
  grep -R -E -q -- "$regex" "$frame_dir" 2>/dev/null
}

lane_style_escape_seen() {
  local lane="$1"
  local session="${2:-}"
  local frame_dir="$OUT_DIR/frames/$lane"

  if [ -n "$session" ] && tui_style_escape_seen "$session"; then
    return 0
  fi
  [ -d "$frame_dir" ] || return 1
  grep -R -l -- $'\033' "$frame_dir"/*.raw.log >/dev/null 2>&1
}

start_frame_sampler() {
  local session="$1"
  local lane="$2"
  local frame_dir="$OUT_DIR/frames/$lane"
  local existing
  local pid

  LAST_FRAME_SAMPLER_PID=""
  if [ "$FRAME_SAMPLE_ENABLED" != "1" ]; then
    return 0
  fi

  mkdir -p "$frame_dir"
  existing="$(find "$frame_dir" -type f -name 'frame-*.clean.log' 2>/dev/null | wc -l | tr -d ' ')"
  (
    seq="${existing:-0}"
    while tmux has-session -t "$session" 2>/dev/null; do
      seq=$((seq + 1))
      frame_id="$(printf '%05d' "$seq")"
      tmux capture-pane -t "$session" -p -e -J -S "-$OCTOS_TMUX_ROWS" \
        | tmux_redact >"$frame_dir/frame-$frame_id.raw.log" 2>/dev/null || true
      tmux_strip_ansi <"$frame_dir/frame-$frame_id.raw.log" \
        >"$frame_dir/frame-$frame_id.clean.log" 2>/dev/null || true
      sleep "$FRAME_SAMPLE_INTERVAL"
    done
  ) &
  pid=$!
  FRAME_SAMPLER_PIDS+=("$pid")
  LAST_FRAME_SAMPLER_PID="$pid"
}

stop_frame_sampler() {
  local pid="$1"
  if [ -n "$pid" ]; then
    kill "$pid" >/dev/null 2>&1 || true
    wait "$pid" >/dev/null 2>&1 || true
  fi
}

frame_count_for_lane() {
  local lane="$1"
  local frame_dir="$OUT_DIR/frames/$lane"

  if [ ! -d "$frame_dir" ]; then
    printf '0'
    return 0
  fi
  find "$frame_dir" -type f -name 'frame-*.clean.log' 2>/dev/null | wc -l | tr -d ' '
}

tmux_pane_alive() {
  local session="$1"
  local pane_dead

  if ! tmux has-session -t "$session" 2>/dev/null; then
    return 1
  fi
  pane_dead="$(tmux list-panes -t "$session" -F '#{pane_dead}' 2>/dev/null | head -1)"
  [ "$pane_dead" = "0" ]
}

tui_fake_cursor_absent() {
  local session="$1"
  ! capture_clean "$session" | grep -q '▌'
}

tui_native_cursor_in_composer() {
  local session="$1"
  local metrics
  local cursor_x
  local cursor_y
  local pane_height

  metrics="$(tmux display-message -p -t "$session" '#{cursor_x} #{cursor_y} #{pane_height}' 2>/dev/null || true)"
  set -- $metrics
  cursor_x="${1:-}"
  cursor_y="${2:-}"
  pane_height="${3:-}"

  case "$cursor_x:$cursor_y:$pane_height" in
    *[!0-9:]* | :: | *::*)
      return 1
      ;;
  esac

  [ "$cursor_x" -ge 2 ] \
    && [ "$cursor_y" -ge $((pane_height - 6)) ] \
    && [ "$cursor_y" -le $((pane_height - 2)) ]
}

tui_style_escape_seen() {
  local session="$1"
  capture_raw "$session" \
    | perl -0777 -e 'my $s = do { local $/; <STDIN> // "" }; exit($s =~ /\e\[[0-9;:]*m/ ? 0 : 1)'
}

tui_capture_has_ready_state() {
  local capture="$1"
  printf '%s\n' "$capture" | grep -E -q -- \
    'state[[:space:]]+[^[:space:]]+[[:space:]]+(done|idle)|>_ Octos TUI[[:space:]]+idle|status[[:space:]]+Turn completed|system[[:space:]]+Turn completed|Ask Octos to change code'
}

tui_capture_has_active_state() {
  local capture="$1"
  printf '%s\n' "$capture" | grep -E -q -- \
    '>_ Octos TUI[[:space:]]+.*Thinking|state[[:space:]]+[^[:space:]]+[[:space:]]+(running|blocked)|status[[:space:]]+(Turn started|Tool started|Approval requested|Approval denied|Thinking)|model[[:space:]]+Waiting for model|Approval Requested|live assistant'
}

tui_capture_has_blocking_approval() {
  local capture="$1"
  printf '%s\n' "$capture" | grep -E -q -- 'Approval Requested|state[[:space:]]+[^[:space:]]+[[:space:]]+blocked'
}

tui_capture_has_composer() {
  local capture="$1"
  printf '%s\n' "$capture" | grep -E -q -- 'Composer|^[[:space:]│]*›[[:space:]]'
}

tui_prompt_prefix() {
  local text="$1"
  printf '%s' "${text:0:24}"
}

strip_prompt_echoes() {
  awk \
    -v p1="$(tui_prompt_prefix "$PROMPT_QUESTION")" \
    -v p2="$(tui_prompt_prefix "$PROMPT_APPROVAL")" \
    -v p3="$(tui_prompt_prefix "$PROMPT_CODING")" \
    -v p4="$(tui_prompt_prefix "$PROMPT_CONTINUE")" \
    -v p5="$(tui_prompt_prefix "$PROMPT_SUMMARY")" \
    -v p6="$(tui_prompt_prefix "$PROMPT_STEERING")" \
    -v p7="$(tui_prompt_prefix "$PROMPT_INTERRUPT")" \
    -v p8="$(tui_prompt_prefix "$PROMPT_RECONNECT")" \
    -v p9="$(tui_prompt_prefix "$PROMPT_FINAL_LONG")" '
      BEGIN {
        prompts[1] = p1; prompts[2] = p2; prompts[3] = p3;
        prompts[4] = p4; prompts[5] = p5; prompts[6] = p6;
        prompts[7] = p7; prompts[8] = p8; prompts[9] = p9;
      }
      /^│user/ || /^user[[:space:]:]/ {
        skip_user = 1;
        next;
      }
      skip_user && (/^│assistant/ || /^assistant[[:space:]:]/) {
        skip_user = 0;
        next;
      }
      skip_user {
        next;
      }
      {
        for (i = 1; i <= 9; i++) {
          if (length(prompts[i]) > 0 && index($0, prompts[i]) > 0) {
            next;
          }
        }
        print;
      }
    '
}

strip_transcript_prompt_echoes() {
  local file="$1"
  if [ ! -f "$file" ]; then
    return 0
  fi
  strip_prompt_echoes <"$file"
}

tui_capture_composer_has_prefix() {
  local capture="$1"
  local prefix="$2"

  [ -n "$prefix" ] || return 1
  printf '%s\n' "$capture" | grep -F -q "› $prefix"
}

wait_for_tui_composer_ready() {
  local session="$1"
  local timeout="$2"
  local deadline=$((SECONDS + timeout))
  local capture

  while [ "$SECONDS" -le "$deadline" ]; do
    if ! tmux_pane_alive "$session"; then
      return 1
    fi

    capture="$(capture_visible_clean "$session" || true)"
    if tui_capture_has_composer "$capture" \
      && tui_capture_has_ready_state "$capture" \
      && ! tui_capture_has_active_state "$capture" \
      && ! tui_capture_has_blocking_approval "$capture"; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

wait_for_tui_prompt_accepted() {
  local session="$1"
  local text="$2"
  local timeout="$3"
  local deadline=$((SECONDS + timeout))
  local prefix
  local capture
  local resend_deadline
  local resend_count=0

  prefix="$(tui_prompt_prefix "$text")"
  resend_deadline=$((SECONDS + 3))

  while [ "$SECONDS" -le "$deadline" ]; do
    if ! tmux_pane_alive "$session"; then
      return 1
    fi

    capture="$(capture_visible_clean "$session" || true)"
    if tui_capture_has_active_state "$capture"; then
      return 0
    fi

    # If the text is still visibly sitting in the composer, the previous Enter
    # did not reach the focused composer. Press Enter once more instead of
    # letting the harness advance against stale "done" text in scrollback.
    if [ "$SECONDS" -ge "$resend_deadline" ] \
      && [ "$resend_count" -lt 2 ] \
      && tui_capture_composer_has_prefix "$capture" "$prefix"; then
      tmux_key "$session" Enter
      resend_count=$((resend_count + 1))
      resend_deadline=$((SECONDS + 3))
    fi

    if ! tui_capture_composer_has_prefix "$capture" "$prefix" \
      && printf '%s\n' "$capture" | grep -F -q "$prefix"; then
      return 0
    fi

    sleep 0.5
  done
  return 1
}

wait_for_regex_soft() {
  local session="$1"
  local regex="$2"
  local timeout="$3"
  local deadline=$((SECONDS + timeout))
  local capture

  while [ "$SECONDS" -le "$deadline" ]; do
    capture="$(capture_visible_clean "$session" || true)"
    capture="$(printf '%s\n' "$capture" | strip_prompt_echoes)"
    if printf '%s\n' "$capture" | grep -E -q -- "$regex"; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

wait_for_tui_first_turn_complete() {
  local session="$1"
  local timeout="$2"
  local deadline=$((SECONDS + timeout))
  local capture

  while [ "$SECONDS" -le "$deadline" ]; do
    capture="$(capture_visible_clean "$session" || true)"
    if printf '%s\n' "$capture" | grep -E -q -- 'Turn completed in Protocol session|Turn completed'; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

wait_for_tui_turn_cycle() {
  local session="$1"
  local timeout="$2"
  local deadline=$((SECONDS + timeout))
  local capture
  local saw_active=0

  while [ "$SECONDS" -le "$deadline" ]; do
    capture="$(capture_visible_clean "$session" || true)"
    if tui_capture_has_active_state "$capture"; then
      saw_active=1
    fi
    if tui_capture_has_ready_state "$capture" \
      && ! tui_capture_has_active_state "$capture"; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

wait_for_tui_approval_prompt() {
  local session="$1"
  local timeout="$2"
  local deadline=$((SECONDS + timeout))
  local capture
  local without_prompt

  while [ "$SECONDS" -le "$deadline" ]; do
    capture="$(capture_visible_clean "$session" || true)"
    without_prompt="$(printf '%s\n' "$capture" | sed '/^│user/,/^│assistant/d')"
    if printf '%s\n' "$without_prompt" | grep -E -q -- 'Approval Requested|kind command|command sudo true|command[[:space:]]+sudo'; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

candidate_has_diff() {
  local dir="$1"
  (
    cd "$dir"
    ! git diff --quiet -- .
  )
}

candidate_tests_pass_live() {
  local name="$1"
  local dir="$2"
  local label="${3:-latest}"
  local safe_label="${label//[^a-zA-Z0-9_.-]/-}"
  local log_path="$OUT_DIR/$name-live-cargo-test-$safe_label.log"
  (
    cd "$dir"
    cargo test --quiet
  ) >"$log_path" 2>&1
  cp "$log_path" "$OUT_DIR/$name-live-cargo-test.log"
}

wait_for_tui_denial_ack() {
  local session="$1"
  local timeout="$2"
  local deadline=$((SECONDS + timeout))
  local capture
  local without_prompt

  while [ "$SECONDS" -le "$deadline" ]; do
    capture="$(capture_visible_clean "$session" || true)"
    without_prompt="$(printf '%s\n' "$capture" | sed '/^│user/,/^│assistant/d')"
    if printf '%s\n' "$without_prompt" | grep -E -q -- 'Approval denied|denied by client|without sudo|continuing without sudo|Continue without sudo'; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

wait_for_tui_interrupt_ack() {
  local session="$1"
  local timeout="$2"
  local deadline=$((SECONDS + timeout))
  local capture

  while [ "$SECONDS" -le "$deadline" ]; do
    capture="$(capture_visible_clean "$session" || true)"
    if printf '%s\n' "$capture" | grep -E -i -q -- 'interrupt|interrupted|cancelled|canceled|Turn canceled|No active turn'; then
      return 0
    fi
    if wait_for_tui_composer_ready "$session" 1; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

server_error_count() {
  local log_path="$1"
  if [ ! -f "$log_path" ]; then
    printf '0'
    return 0
  fi
  grep -E -c -- 'panicked at |thread .* panicked|(^|[[:space:]])ERROR([[:space:]]|:)|(^|[[:space:]])FATAL([[:space:]]|:)' "$log_path" 2>/dev/null || true
}

write_secret_runner() {
  local runner="$1"
  local fifo="$2"
  local command="$3"
  local api_key_env="${4:-$API_KEY_ENV}"
  cat >"$runner" <<EOF
#!/usr/bin/env bash
set -euo pipefail
IFS= read -r OCTOS_LIVE_API_KEY < "$fifo"
export "$api_key_env=\$OCTOS_LIVE_API_KEY"
rm -f "$fifo"
exec bash -lc $(printf '%q' "$command")
EOF
  chmod +x "$runner"
}

start_secret_session() {
  local session="$1"
  local runner="$2"
  local fifo="$3"
  local api_key_env="${4:-$API_KEY_ENV}"
  local keepalive="${runner%.sh}-tmux-keepalive.sh"

  rm -f "$fifo"
  mkfifo "$fifo"
  chmod 600 "$fifo"
  cat >"$keepalive" <<EOF
#!/usr/bin/env bash
"$runner"
rc=\$?
printf '\\n[tmux-runner-exit=%s]\\n' "\$rc"
sleep "${OCTOS_TUI_UX_EXIT_HOLD_SECS:-900}"
exit "\$rc"
EOF
  chmod +x "$keepalive"
  tmux_new_default "$session" "$keepalive"
  tmux set-option -t "$session" history-limit 80000 >/dev/null
  {
    printf '%s\n' "${!api_key_env}" >"$fifo"
  } &
}

start_plain_session() {
  local session="$1"
  local command="$2"
  local runner="$OUT_DIR/${session//[^a-zA-Z0-9_.-]/-}.sh"
  cat >"$runner" <<EOF
#!/usr/bin/env bash
set -euo pipefail
set +e
$command
rc=\$?
set -e
printf '\\n[tmux-runner-exit=%s]\\n' "\$rc"
sleep "${OCTOS_TUI_UX_EXIT_HOLD_SECS:-900}"
exit "\$rc"
EOF
  chmod +x "$runner"
  tmux_new_default "$session" "$runner"
  tmux set-option -t "$session" history-limit 80000 >/dev/null
}

send_line() {
  local session="$1"
  local text="$2"
  tmux_send "$session" "$text"
  tmux_key "$session" Enter
}

send_tui_prompt() {
  local session="$1"
  local text="$2"
  local label="${3:-prompt}"
  local ready_timeout="${OCTOS_TUI_UX_COMPOSER_READY_TIMEOUT:-60}"
  local submit_timeout="${OCTOS_TUI_UX_SUBMIT_TIMEOUT:-45}"

  if ! wait_for_tui_composer_ready "$session" "$ready_timeout"; then
    log "TUI composer was not ready before $label"
    return 1
  fi

  tmux_key "$session" C-u
  sleep 0.2
  tmux_send "$session" "$text"
  sleep 0.2
  tmux_key "$session" Enter

  if ! wait_for_tui_prompt_accepted "$session" "$text" "$submit_timeout"; then
    log "TUI prompt was not accepted for $label"
    return 1
  fi
}

send_codex_prompt() {
  local session="$1"
  local text="$2"
  tmux_key "$session" C-u
  sleep "${OCTOS_TUI_UX_CODEX_CLEAR_GRACE_SECS:-0.2}"
  tmux_send "$session" "$text"
  tmux_key "$session" Enter
  sleep "${OCTOS_TUI_UX_CODEX_SUBMIT_GRACE_SECS:-1}"
}

latest_codex_command_prompt_block() {
  local capture="$1"
  printf '%s\n' "$capture" | awk '
    /Would you like to run the following command/ {
      seen = 1
      block = ""
    }
    seen {
      block = block $0 "\n"
    }
    END {
      printf "%s", block
    }
  '
}

codex_capture_has_command_prompt() {
  local capture="$1"
  latest_codex_command_prompt_block "$capture" \
    | grep -E -q -- 'Would you like to run the following command|Press enter to confirm|Yes, proceed'
}

codex_capture_has_sudo_prompt() {
  local capture="$1"
  local command_block
  command_block="$(latest_codex_command_prompt_block "$capture")"
  codex_capture_has_command_prompt "$command_block" \
    && printf '%s\n' "$command_block" | grep -E -q -- '(^|[[:space:]])\$[[:space:]]+sudo true'
}

codex_accept_non_sudo_approval() {
  local session="$1"
  local capture="$2"
  if codex_capture_has_command_prompt "$capture" && ! codex_capture_has_sudo_prompt "$capture"; then
    tmux_key "$session" Enter
    return 0
  fi
  return 1
}

codex_deny_sudo_approval() {
  local session="$1"
  local capture="$2"
  if ! codex_capture_has_sudo_prompt "$capture"; then
    return 1
  fi
  tmux_key "$session" Escape
  sleep "${OCTOS_TUI_UX_CODEX_DENIAL_GRACE_SECS:-2}"
  send_codex_prompt "$session" "Denied. Continue without sudo and finish the task."
  return 0
}

wait_for_codex_signal() {
  local session="$1"
  local regex="$2"
  local timeout="$3"
  local deadline=$((SECONDS + timeout))
  local capture
  local sudo_denied=0

  while [ "$SECONDS" -le "$deadline" ]; do
    capture="$(capture_clean "$session" || true)"
    capture="$(printf '%s\n' "$capture" | strip_prompt_echoes)"
    if printf '%s\n' "$capture" | grep -E -q -- "$regex"; then
      return 0
    fi
    if [ "$sudo_denied" -eq 0 ] && codex_deny_sudo_approval "$session" "$capture"; then
      sudo_denied=1
    else
      codex_accept_non_sudo_approval "$session" "$capture" || true
    fi
    sleep 0.5
  done
  return 1
}

wait_for_codex_sudo_approval_denial() {
  local session="$1"
  local timeout="$2"
  local deadline=$((SECONDS + timeout))
  local capture

  while [ "$SECONDS" -le "$deadline" ]; do
    capture="$(capture_clean "$session" || true)"
    if codex_deny_sudo_approval "$session" "$capture"; then
      return 0
    fi
    codex_accept_non_sudo_approval "$session" "$capture" || true
    sleep 0.5
  done
  return 1
}

finish_session() {
  local session="$1"
  if [ "${OCTOS_TUI_UX_KEEP_SESSIONS:-0}" = "1" ]; then
    log "kept tmux session: $session"
  else
    tmux_kill "$session"
  fi
}

drive_tui() {
  local dir="$1"
  local octos_bin="$2"
  local tui_bin="$3"
  local server_session
  local tui_session
  local endpoint
  local server_fifo
  local server_runner
  local transcript
  local server_log
  local raw_transcript
  local tui_command
  local frame_sampler_pid=""
  local question_seen=0
  local approval_seen=0
  local denial_seen=0
  local completion_seen=0
  local reconnect_seen=0
  local interrupt_seen=0
  local server_errors=0
  local frames_seen=0
  local panes_seen=0
  local status_seen=0
  local client_alive_seen=0
  local fake_cursor_absent=0
  local native_cursor_composer_seen=0
  local styled_output_seen=0
  local test_rc=0

  server_session="$(tmux_session_name octos-tui-server)"
  tui_session="$(tmux_session_name octos-tui-client)"
  endpoint="ws://127.0.0.1:$PORT/api/ui-protocol/ws"
  server_fifo="$OUT_DIR/octos-tui-server-key.fifo"
  server_runner="$OUT_DIR/run-octos-tui-server.sh"
  transcript="$OUT_DIR/octos-tui-transcript.log"
  raw_transcript="$OUT_DIR/octos-tui-raw-transcript.log"
  server_log="$OUT_DIR/octos-tui-server.log"
  : >"$transcript"
  : >"$raw_transcript"

  write_secret_runner "$server_runner" "$server_fifo" \
    "cd '$ROOT_DIR' && RUST_LOG=off exec '$octos_bin' serve --host 127.0.0.1 --port '$PORT' --cwd '$dir' --data-dir '$OUT_DIR/octos-data' --provider '$PROVIDER' --model '$MODEL' --auth-token '$AUTH_TOKEN' >'$server_log' 2>&1"
  start_secret_session "$server_session" "$server_runner" "$server_fifo" "$API_KEY_ENV"

  local deadline=$((SECONDS + MAX_WAIT_SHORT))
  while [ "$SECONDS" -le "$deadline" ]; do
    if grep -q "Listening: http://127.0.0.1:$PORT" "$server_log" 2>/dev/null; then
      break
    fi
    sleep 0.5
  done
  if ! grep -q "Listening: http://127.0.0.1:$PORT" "$server_log" 2>/dev/null; then
    printf 'octos serve did not start. See %s\n' "$server_log" >&2
    return 1
  fi

  tui_command="cd '$OCTOS_TUI_DIR' && RUST_LOG=off '$tui_bin' --mode protocol --endpoint '$endpoint' --session '$SESSION_ID' --profile-id coding --cwd '$dir' --auth-token '$AUTH_TOKEN'"
  start_plain_session "$tui_session" "$tui_command"
  start_frame_sampler "$tui_session" octos_tui
  frame_sampler_pid="$LAST_FRAME_SAMPLER_PID"

  wait_for_regex_soft "$tui_session" 'Protocol backend connected|Opened .*coding:local|app-ui octos-app-ui|Sessions' "$MAX_WAIT_SHORT" || true
  if [ "${OCTOS_TUI_UX_ATTACH_GRACE_SECS:-0}" -gt 0 ]; then
    log "attach now: tmux attach -r -t $tui_session"
    sleep "${OCTOS_TUI_UX_ATTACH_GRACE_SECS:-0}"
  fi
  if wait_for_regex_soft "$tui_session" 'Ask Octos to change code|›' 10 \
    && wait_for_regex_soft "$tui_session" 'Composer' 1 \
    && wait_for_regex_soft "$tui_session" 'Tab inspector' 1; then
    panes_seen=1
  fi
  if wait_for_regex_soft "$tui_session" 'Status|approval gated|app-ui octos-app-ui' 10; then
    status_seen=1
  fi
  if tmux_pane_alive "$tui_session"; then
    client_alive_seen=1
  fi
  if tui_fake_cursor_absent "$tui_session"; then
    fake_cursor_absent=1
  fi
  if tui_native_cursor_in_composer "$tui_session"; then
    native_cursor_composer_seen=1
  fi
  if tui_style_escape_seen "$tui_session"; then
    styled_output_seen=1
  fi
  append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui connected"
  append_capture_raw_to_file "$tui_session" "$raw_transcript" "octos-tui connected"

  if ! send_tui_prompt "$tui_session" "$PROMPT_QUESTION" "question turn"; then
    append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui question submit failure"
  fi
  if wait_for_regex_soft "$tui_session" "$QUESTION_REGEX" "$MAX_WAIT_TURN"; then
    question_seen=1
  fi
  wait_for_tui_turn_cycle "$tui_session" 240 || wait_for_tui_first_turn_complete "$tui_session" 60 || true
  append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui question turn"

  if ! send_tui_prompt "$tui_session" "$PROMPT_APPROVAL" "approval probe turn"; then
    append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui approval submit failure"
  fi
  if wait_for_tui_approval_prompt "$tui_session" "$MAX_WAIT_TURN"; then
    approval_seen=1
    append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui approval prompt"
    tmux_key "$tui_session" "$TUI_DENY_KEY"
  fi
  if wait_for_tui_denial_ack "$tui_session" 180; then
    denial_seen=1
  fi
  wait_for_tui_turn_cycle "$tui_session" "$MAX_WAIT_TURN" || true
  append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui approval turn"

  if ! send_tui_prompt "$tui_session" "$PROMPT_CODING" "coding turn"; then
    append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui coding submit failure"
  fi
  wait_for_tui_turn_cycle "$tui_session" "$MAX_WAIT_TURN" || true
  append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui coding turn"

  local round=1
  while [ "$round" -le "$MAX_TUI_CODING_ROUNDS" ]; do
    append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui round $round"
    if candidate_has_diff "$dir" && candidate_tests_pass_live octos_tui "$dir" "round-$round"; then
      completion_seen=1
      break
    fi

    if [ "$round" -ge "$MAX_TUI_CODING_ROUNDS" ]; then
      break
    fi

    if ! send_tui_prompt "$tui_session" "$PROMPT_CONTINUE" "continue round $round"; then
      append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui continue submit failure $round"
    fi
    wait_for_tui_turn_cycle "$tui_session" "$MAX_WAIT_TURN" || true
    append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui continue round $round"
    round=$((round + 1))
  done

  if [ "$LONG_MODE" = "1" ] && [ "$completion_seen" -eq 1 ]; then
    if ! send_tui_prompt "$tui_session" "$PROMPT_STEERING" "long steering turn"; then
      append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui long steering submit failure"
    fi
    wait_for_tui_turn_cycle "$tui_session" "$MAX_WAIT_TURN" || true
    append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui long steering turn"
    if ! candidate_tests_pass_live octos_tui "$dir" "long-steering"; then
      completion_seen=0
    fi

    if ! send_tui_prompt "$tui_session" "$PROMPT_INTERRUPT" "long interrupt turn"; then
      append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui interrupt submit failure"
    else
      sleep "${OCTOS_TUI_UX_INTERRUPT_AFTER_SECS:-8}"
      tmux_key "$tui_session" C-c
      if wait_for_tui_interrupt_ack "$tui_session" 180; then
        interrupt_seen=1
      fi
      append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui interrupt turn"
      wait_for_tui_composer_ready "$tui_session" 60 || true
    fi

    stop_frame_sampler "$frame_sampler_pid"
    frame_sampler_pid=""
    tmux_kill "$tui_session"
    sleep 1
    start_plain_session "$tui_session" "$tui_command"
    start_frame_sampler "$tui_session" octos_tui
    frame_sampler_pid="$LAST_FRAME_SAMPLER_PID"
    if wait_for_regex_soft "$tui_session" 'Protocol backend connected|Opened .*coding:local|app-ui octos-app-ui|Sessions|Composer|›' "$MAX_WAIT_SHORT"; then
      reconnect_seen=1
    fi
    append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui reconnect"
    append_capture_raw_to_file "$tui_session" "$raw_transcript" "octos-tui reconnect"

    if ! send_tui_prompt "$tui_session" "$PROMPT_RECONNECT" "long reconnect turn"; then
      append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui reconnect submit failure"
    fi
    wait_for_tui_turn_cycle "$tui_session" "$MAX_WAIT_TURN" || true
    append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui reconnect turn"
    if ! candidate_tests_pass_live octos_tui "$dir" "reconnect"; then
      completion_seen=0
    fi
  fi

  if [ "$completion_seen" -eq 1 ]; then
    local summary_prompt="$PROMPT_SUMMARY"
    local summary_label="summary turn"
    if [ "$LONG_MODE" = "1" ]; then
      summary_prompt="$PROMPT_FINAL_LONG"
      summary_label="long final summary turn"
    fi
    if [ "$(summary_bool_from_grep_filtered "$transcript" 'Session Summary|Session summary|Files changed|Validation|All [0-9]+ tests pass|all .*tests pass')" != "1" ]; then
      if ! send_tui_prompt "$tui_session" "$summary_prompt" "$summary_label"; then
        append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui summary submit failure"
      fi
      wait_for_tui_turn_cycle "$tui_session" "$MAX_WAIT_TURN" || true
      append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui $summary_label"
    fi
  fi

  append_capture_clean_to_file "$tui_session" "$transcript" "octos-tui final"
  append_capture_raw_to_file "$tui_session" "$raw_transcript" "octos-tui final"
  frames_seen="$(frame_count_for_lane octos_tui)"
  server_errors="$(server_error_count "$server_log")"
  if ! tmux_pane_alive "$tui_session"; then
    client_alive_seen=0
  fi
  if ! tui_fake_cursor_absent "$tui_session"; then
    fake_cursor_absent=0
  fi
  stop_frame_sampler "$frame_sampler_pid"
  if lane_style_escape_seen octos_tui "$tui_session"; then
    styled_output_seen=1
  fi
  tmux_key "$tui_session" C-q
  sleep 1
  finish_session "$tui_session"
  finish_session "$server_session"

  validate_candidate octos_tui "$dir" || test_rc=$?
  write_candidate_artifacts octos_tui "$dir"
  write_lane_summary octos_tui "$transcript" "$dir" "$question_seen" "$approval_seen" \
    "$denial_seen" "$completion_seen" "$panes_seen" "$status_seen" "$test_rc" \
    "$client_alive_seen" "$fake_cursor_absent" "$native_cursor_composer_seen" "$styled_output_seen" \
    "$reconnect_seen" "$interrupt_seen" "$server_errors" "$frames_seen"
  return "$test_rc"
}

write_codex_runner() {
  local runner="$1"
  local fifo="$2"
  local dir="$3"
  local codex_bin="${CODEX_BIN:-codex}"
  local codex_wire_api="${CODEX_WIRE_API:-chat}"
  cat >"$runner" <<EOF
#!/usr/bin/env bash
set -euo pipefail
IFS= read -r OCTOS_LIVE_API_KEY < "$fifo"
export "$API_KEY_ENV=\$OCTOS_LIVE_API_KEY"
rm -f "$fifo"
cd "$dir"
export CODEX_HOME="$OUT_DIR/codex-home"
mkdir -p "\$CODEX_HOME"
printf '{"latest_version":"0.125.0","last_checked_at":"2026-04-27T00:00:00Z","dismissed_version":"0.125.0"}\n' >"\$CODEX_HOME/version.json"
if [ -f "\$HOME/.x-cmd.root/X" ]; then
  set +u
  source "\$HOME/.x-cmd.root/X"
  x env use codex=v0.80.0 >/dev/null 2>&1 || true
  set -u
fi
if [ "$PROVIDER" = "openai" ]; then
  printf '%s\n' "\$OCTOS_LIVE_API_KEY" | "$codex_bin" login --with-api-key >/dev/null 2>&1 || true
fi
if [ "$PROVIDER" = "deepseek" ]; then
exec "$codex_bin" \\
  -C "$dir" \\
  -s workspace-write \\
  -a untrusted \\
  -c model_provider='"deepseek"' \\
  -c model='"$MODEL"' \\
  -c model_providers.deepseek.name='"DeepSeek Compat"' \\
  -c model_providers.deepseek.base_url='"http://127.0.0.1:$COMPAT_PROXY_PORT/v1"' \\
  -c model_providers.deepseek.env_key='"DEEPSEEK_API_KEY"' \\
  -c model_providers.deepseek.wire_api='"$codex_wire_api"'
else
exec "$codex_bin" \\
  -C "$dir" \\
  -s workspace-write \\
  -a untrusted \\
  -c model_provider='"$PROVIDER"' \\
  -c model='"$MODEL"'
fi
EOF
  chmod +x "$runner"
}

drive_codex() {
  local dir="$1"
  local session
  local fifo
  local runner
  local transcript
  local raw_transcript
  local frame_sampler_pid=""
  local question_seen=0
  local approval_seen=0
  local denial_seen=0
  local completion_seen=0
  local reconnect_seen=0
  local interrupt_seen=0
  local frames_seen=0
  local test_rc=0

  session="$(tmux_session_name codex-client)"
  fifo="$OUT_DIR/codex-key.fifo"
  runner="$OUT_DIR/run-codex.sh"
  transcript="$OUT_DIR/codex-transcript.log"
  raw_transcript="$OUT_DIR/codex-raw-transcript.log"
  : >"$transcript"
  : >"$raw_transcript"

  write_codex_runner "$runner" "$fifo" "$dir"
  start_secret_session "$session" "$runner" "$fifo" "$API_KEY_ENV"
  start_frame_sampler "$session" codex
  frame_sampler_pid="$LAST_FRAME_SAMPLER_PID"

  wait_for_regex_soft "$session" 'OpenAI Codex|model:|directory:|›|Update available|Press enter' "$MAX_WAIT_SHORT" || true
  if capture_clean "$session" | grep -E -q 'Update available|Press enter to continue'; then
    send_line "$session" "3"
  fi
  wait_for_regex_soft "$session" 'OpenAI Codex|model:|directory:|›' "$MAX_WAIT_SHORT" || true
  append_capture_clean_to_file "$session" "$transcript" "codex connected"
  append_capture_raw_to_file "$session" "$raw_transcript" "codex connected"

  send_codex_prompt "$session" "$PROMPT_QUESTION"
  if wait_for_codex_signal "$session" "$QUESTION_REGEX" "$MAX_WAIT_TURN"; then
    question_seen=1
  fi
  append_capture_clean_to_file "$session" "$transcript" "codex question turn"

  send_codex_prompt "$session" "$PROMPT_APPROVAL"
  if wait_for_codex_sudo_approval_denial "$session" "$MAX_WAIT_TURN"; then
    approval_seen=1
    denial_seen=1
  fi
  if [ "$denial_seen" -ne 1 ] \
    && wait_for_codex_signal "$session" 'sudo denied|not approved|Denied|without sudo|Continue without sudo|continuing without sudo' 180; then
    denial_seen=1
  fi
  append_capture_clean_to_file "$session" "$transcript" "codex approval turn"

  send_codex_prompt "$session" "$PROMPT_CODING"
  wait_for_codex_signal "$session" "$COMPLETION_REGEX" "$MAX_WAIT_TURN" || true
  if candidate_has_diff "$dir" && candidate_tests_pass_live codex "$dir" "coding"; then
    completion_seen=1
  fi
  append_capture_clean_to_file "$session" "$transcript" "codex coding turn"

  if [ "$LONG_MODE" = "1" ] && [ "$completion_seen" -eq 1 ]; then
    send_codex_prompt "$session" "$PROMPT_STEERING"
    wait_for_codex_signal "$session" "$COMPLETION_REGEX" "$MAX_WAIT_TURN" || true
    append_capture_clean_to_file "$session" "$transcript" "codex long steering turn"
    if ! candidate_tests_pass_live codex "$dir" "long-steering"; then
      completion_seen=0
    fi

    send_codex_prompt "$session" "$PROMPT_INTERRUPT"
    sleep "${OCTOS_TUI_UX_INTERRUPT_AFTER_SECS:-8}"
    tmux_key "$session" C-c
    if wait_for_codex_signal "$session" 'interrupt|interrupted|cancelled|canceled|›|OpenAI Codex' 180; then
      interrupt_seen=1
    fi
    append_capture_clean_to_file "$session" "$transcript" "codex interrupt turn"

    stop_frame_sampler "$frame_sampler_pid"
    frame_sampler_pid=""
    tmux_kill "$session"
    sleep 1
    start_secret_session "$session" "$runner" "$fifo" "$API_KEY_ENV"
    start_frame_sampler "$session" codex
    frame_sampler_pid="$LAST_FRAME_SAMPLER_PID"
    if wait_for_regex_soft "$session" 'OpenAI Codex|model:|directory:|›|Update available|Press enter' "$MAX_WAIT_SHORT"; then
      reconnect_seen=1
    fi
    if capture_clean "$session" | grep -E -q 'Update available|Press enter to continue'; then
      send_line "$session" "3"
    fi
    wait_for_regex_soft "$session" 'OpenAI Codex|model:|directory:|›' "$MAX_WAIT_SHORT" || true
    append_capture_clean_to_file "$session" "$transcript" "codex reconnect"
    append_capture_raw_to_file "$session" "$raw_transcript" "codex reconnect"

    send_codex_prompt "$session" "$PROMPT_RECONNECT"
    wait_for_codex_signal "$session" "$COMPLETION_REGEX" "$MAX_WAIT_TURN" || true
    append_capture_clean_to_file "$session" "$transcript" "codex reconnect turn"
    if ! candidate_tests_pass_live codex "$dir" "reconnect"; then
      completion_seen=0
    fi

    if [ "$completion_seen" -eq 1 ]; then
      send_codex_prompt "$session" "$PROMPT_FINAL_LONG"
      wait_for_codex_signal "$session" 'Session Summary|Session summary|Files changed|Validation|risks|next steps' "$MAX_WAIT_TURN" || true
      append_capture_clean_to_file "$session" "$transcript" "codex long final summary turn"
    fi
  fi

  append_capture_clean_to_file "$session" "$transcript" "codex final"
  append_capture_raw_to_file "$session" "$raw_transcript" "codex final"
  frames_seen="$(frame_count_for_lane codex)"
  stop_frame_sampler "$frame_sampler_pid"
  tmux_key "$session" C-c
  sleep 1
  finish_session "$session"

  validate_candidate codex "$dir" || test_rc=$?
  write_candidate_artifacts codex "$dir"
  write_lane_summary codex "$transcript" "$dir" "$question_seen" "$approval_seen" \
    "$denial_seen" "$completion_seen" 1 1 "$test_rc" 1 1 1 1 \
    "$reconnect_seen" "$interrupt_seen" 0 "$frames_seen"
  return "$test_rc"
}

summary_bool_from_grep() {
  local file="$1"
  local regex="$2"
  if grep -E -q -- "$regex" "$file" 2>/dev/null; then
    printf '1'
  else
    printf '0'
  fi
}

summary_bool_without_grep() {
  local file="$1"
  local regex="$2"
  if grep -E -q -- "$regex" "$file" 2>/dev/null; then
    printf '0'
  else
    printf '1'
  fi
}

summary_bool_from_grep_filtered() {
  local file="$1"
  local regex="$2"
  if grep -E -q -- "$regex" < <(strip_transcript_prompt_echoes "$file"); then
    printf '1'
  else
    printf '0'
  fi
}

summary_bool_without_grep_filtered() {
  local file="$1"
  local regex="$2"
  if grep -E -q -- "$regex" < <(strip_transcript_prompt_echoes "$file"); then
    printf '0'
  else
    printf '1'
  fi
}

plan_seen_for_lane() {
  local name="$1"
  local transcript="$2"
  local regex='(^|[[:space:]])Plan([[:space:]]+live|:)|(^|[[:space:]])plan:|\[[ xX]\]'

  if [ "$(summary_bool_from_grep_filtered "$transcript" "$regex")" = "1" ]; then
    printf '1'
    return 0
  fi

  if [ "$name" = "octos_tui" ] && lane_frame_grep octos_tui "$regex"; then
    printf '1'
    return 0
  fi

  printf '0'
}

octos_tui_inline_diff_seen() {
  local transcript="$1"
  summary_bool_from_grep_filtered "$transcript" 'diff --git|(^|[[:space:]])@@|(^|[[:space:]])---[[:space:]]|(^|[[:space:]])\+\+\+[[:space:]]|Requested diff preview|Diff Preview|inline diff|diff_edit|apply_patch|^[[:space:]]*[-+][[:space:]]+[-+][[:space:]]+[0-9]+|^[[:space:]]*[-+][[:space:]]+-[[:space:]]+[0-9]+'
}

octos_tui_command_output_seen() {
  local transcript="$1"
  summary_bool_from_grep_filtered "$transcript" 'Finished .*test.* profile|Running unittests|Running tests|test result:|stdout|stderr|exit status|command[[:space:]]+sudo true|kind[[:space:]]+command|Approval probe:|Sudo denied'
}

octos_tui_approval_card_seen() {
  local transcript="$1"
  summary_bool_from_grep_filtered "$transcript" 'Approval Requested|kind[[:space:]]+command|command[[:space:]]+sudo true|sudo true'
}

octos_tui_no_overlay_required() {
  local transcript="$1"
  local inline_diff_seen="$2"

  if [ "$inline_diff_seen" -ne 1 ]; then
    printf '0'
    return 0
  fi

  summary_bool_without_grep_filtered "$transcript" 'press[[:space:]]+d|d[[:space:]]+to[[:space:]]+(view|open)[[:space:]]+diff|open[[:space:]]+diff[[:space:]]+overlay|diff[[:space:]]+overlay|diff[[:space:]]+modal|modal[[:space:]]+diff'
}

octos_tui_chat_first_layout_seen() {
  local transcript="$1"

  if ! grep -E -q -- 'Composer' "$transcript" 2>/dev/null \
    || ! grep -E -q -- 'Ask Octos to change code|^[[:space:]]*›[[:space:]]' "$transcript" 2>/dev/null; then
    printf '0'
    return 0
  fi

  summary_bool_without_grep_filtered "$transcript" 'Current Tasks|tasks/status'
}

summary_seen_for_lane() {
  local transcript="$1"
  local test_rc="$2"
  local diff_bytes="$3"

  if [ "$(summary_bool_from_grep_filtered "$transcript" 'Session Summary|Session summary|Files changed|Validation|All [0-9]+ tests pass|all .*tests pass')" = "1" ]; then
    printf '1'
    return 0
  fi

  # Some TUI runs finish with a visible file-by-file summary but not the exact
  # summary heading. Treat green tests plus real edits plus changed-file bullets
  # as a summary-equivalent signal so strict mode does not fail on phrasing.
  if [ "$test_rc" -eq 0 ] \
    && [ "$diff_bytes" -gt 0 ] \
    && grep -E -q -- '((crates/[^[:space:]]+/)?src/[^[:space:]]+\.rs)[[:space:]]+[-—:]' < <(strip_transcript_prompt_echoes "$transcript"); then
    printf '1'
    return 0
  fi

  # The TUI transcript is a viewport capture. For long inline diffs, the final
  # summary can be present in the durable AppUi/session ledger while scrolled out
  # of the captured viewport. Count that as summary evidence for the live gate.
  if [ -d "$OUT_DIR/octos-data/sessions" ] \
    && grep -R -E -q -- 'Session Summary|Files changed|Validation|All [0-9]+ tests pass|[0-9]+/[0-9]+ tests pass' "$OUT_DIR/octos-data/sessions" 2>/dev/null; then
    printf '1'
    return 0
  fi

  printf '0'
}

write_lane_summary() {
  local name="$1"
  local transcript="$2"
  local dir="$3"
  local question_seen="$4"
  local approval_seen="$5"
  local denial_seen="$6"
  local completion_seen="$7"
  local panes_seen="$8"
  local status_seen="$9"
  local test_rc="${10}"
  local client_alive_seen="${11:-1}"
  local fake_cursor_absent="${12:-1}"
  local native_cursor_composer_seen="${13:-1}"
  local styled_output_seen="${14:-1}"
  local reconnect_seen="${15:-0}"
  local interrupt_seen="${16:-0}"
  local server_errors="${17:-0}"
  local frames_seen="${18:-0}"
  local status="pass"
  local diff_bytes
  local plan_seen
  local summary_seen
  local trace_log_absent=1
  local inline_diff_seen=0
  local command_output_seen=0
  local approval_card_seen=0
  local no_overlay_required=0
  local chat_first_layout_seen=1

  diff_bytes="$(wc -c <"$OUT_DIR/$name-worktree.diff" | tr -d ' ')"
  plan_seen="$(plan_seen_for_lane "$name" "$transcript")"
  summary_seen="$(summary_seen_for_lane "$transcript" "$test_rc" "$diff_bytes")"

  if [ "$name" = "octos_tui" ]; then
    trace_log_absent="$(summary_bool_without_grep_filtered "$transcript" 'INFO calling LLM|parallel_tools|result_sizes|tool_ids=')"
    inline_diff_seen="$(octos_tui_inline_diff_seen "$transcript")"
    command_output_seen="$(octos_tui_command_output_seen "$transcript")"
    approval_card_seen="$(octos_tui_approval_card_seen "$transcript")"
    no_overlay_required="$(octos_tui_no_overlay_required "$transcript" "$inline_diff_seen")"
    chat_first_layout_seen="$(octos_tui_chat_first_layout_seen "$transcript")"
  fi

  if [ "$diff_bytes" -le 0 ] \
    || [ "$test_rc" -ne 0 ] \
    || [ "$question_seen" -ne 1 ] \
    || [ "$approval_seen" -ne 1 ] \
    || [ "$denial_seen" -ne 1 ] \
    || [ "$completion_seen" -ne 1 ] \
    || [ "$plan_seen" -ne 1 ] \
    || [ "$summary_seen" -ne 1 ] \
    || [ "$panes_seen" -ne 1 ] \
    || [ "$status_seen" -ne 1 ]; then
    status="fail"
  fi
  if [ "$name" = "octos_tui" ] && [ "$trace_log_absent" -ne 1 ]; then
    status="fail"
  fi
  if [ "$name" = "octos_tui" ] && [ "$chat_first_layout_seen" -ne 1 ]; then
    status="fail"
  fi
  if [ "$name" = "octos_tui" ] \
    && { [ "$client_alive_seen" -ne 1 ] \
      || [ "$fake_cursor_absent" -ne 1 ] \
      || [ "$native_cursor_composer_seen" -ne 1 ] \
      || [ "$styled_output_seen" -ne 1 ]; }; then
    status="fail"
  fi
  if [ "$LONG_MODE" = "1" ] && [ "$name" = "octos_tui" ] \
    && { [ "$reconnect_seen" -ne 1 ] \
      || [ "$interrupt_seen" -ne 1 ] \
      || [ "$server_errors" -gt "$SERVER_ERROR_BUDGET" ]; }; then
    status="fail"
  fi
  if [ "$LONG_MODE" = "1" ] && [ "$FRAME_SAMPLE_ENABLED" = "1" ] && [ "$frames_seen" -le 0 ]; then
    status="fail"
  fi

  {
    printf '%s.status=%s\n' "$name" "$status"
    printf '%s.long_mode=%s\n' "$name" "$LONG_MODE"
    printf '%s.test_exit=%s\n' "$name" "$test_rc"
    printf '%s.question_seen=%s\n' "$name" "$question_seen"
    printf '%s.approval_prompt_seen=%s\n' "$name" "$approval_seen"
    printf '%s.approval_denial_seen=%s\n' "$name" "$denial_seen"
    printf '%s.completion_seen=%s\n' "$name" "$completion_seen"
    printf '%s.reconnect_seen=%s\n' "$name" "$reconnect_seen"
    printf '%s.interrupt_seen=%s\n' "$name" "$interrupt_seen"
    printf '%s.frame_count=%s\n' "$name" "$frames_seen"
    printf '%s.plan_seen=%s\n' "$name" "$plan_seen"
    printf '%s.summary_seen=%s\n' "$name" "$summary_seen"
    printf '%s.panes_seen=%s\n' "$name" "$panes_seen"
    printf '%s.status_seen=%s\n' "$name" "$status_seen"
    if [ "$name" = "octos_tui" ]; then
      printf '%s.inline_diff_seen=%s\n' "$name" "$inline_diff_seen"
      printf '%s.command_output_seen=%s\n' "$name" "$command_output_seen"
      printf '%s.approval_card_seen=%s\n' "$name" "$approval_card_seen"
      printf '%s.no_overlay_required=%s\n' "$name" "$no_overlay_required"
      printf '%s.chat_first_layout_seen=%s\n' "$name" "$chat_first_layout_seen"
      printf '%s.trace_log_absent=%s\n' "$name" "$trace_log_absent"
      printf '%s.client_alive_seen=%s\n' "$name" "$client_alive_seen"
      printf '%s.fake_cursor_absent=%s\n' "$name" "$fake_cursor_absent"
      printf '%s.native_cursor_composer_seen=%s\n' "$name" "$native_cursor_composer_seen"
      printf '%s.styled_output_seen=%s\n' "$name" "$styled_output_seen"
      printf '%s.server_error_count=%s\n' "$name" "$server_errors"
      printf '%s.server_error_budget=%s\n' "$name" "$SERVER_ERROR_BUDGET"
    fi
    printf '%s.diff_bytes=%s\n' "$name" "$diff_bytes"
    printf '%s.worktree=%s\n' "$name" "$dir"
    printf '%s.transcript=%s\n' "$name" "$transcript"
    printf '%s.test_log=%s\n' "$name" "$OUT_DIR/$name-cargo-test.log"
    printf '%s.diff=%s\n' "$name" "$OUT_DIR/$name-worktree.diff"
  } >>"$OUT_DIR/summary.env"
}

run_self_tests() {
  local test_root
  local transcript
  local frame_dir

  test_root="$(mktemp -d "${TMPDIR:-/tmp}/octos-tui-ux-harness-test.XXXXXX")"
  OUT_DIR="$test_root"
  transcript="$OUT_DIR/transcript.log"
  frame_dir="$OUT_DIR/frames/octos_tui"
  mkdir -p "$frame_dir"

  printf '  › State a short coding plan with checkbox steps.\n' >"$transcript"
  if [ "$(plan_seen_for_lane octos_tui "$transcript")" != "0" ]; then
    printf 'self-test failed: prompt echo should not count as plan evidence\n' >&2
    rm -rf "$test_root"
    return 1
  fi

  printf '  Plan  live\n    [ ] 1. Run cargo test\n' >"$frame_dir/frame-00001.clean.log"
  if [ "$(plan_seen_for_lane octos_tui "$transcript")" != "1" ]; then
    printf 'self-test failed: sampled frame plan evidence was not counted\n' >&2
    rm -rf "$test_root"
    return 1
  fi

  printf 'plain frame\n' >"$frame_dir/frame-00001.raw.log"
  if lane_style_escape_seen octos_tui; then
    printf 'self-test failed: plain frame should not count as styled output\n' >&2
    rm -rf "$test_root"
    return 1
  fi

  printf '\033[38;2;110;188;255mstyled\033[0m\n' >"$frame_dir/frame-00002.raw.log"
  if ! lane_style_escape_seen octos_tui; then
    printf 'self-test failed: raw frame ANSI style evidence was not counted\n' >&2
    rm -rf "$test_root"
    return 1
  fi

  RESOLVED_SERVER_SANDBOX_MODE="outer-sandbox-fallback"
  mkdir -p "$OUT_DIR/candidate"
  maybe_write_harness_server_config octos_tui "$OUT_DIR/candidate"
  if ! grep -F -q '"permission_mode": "danger-full-access"' "$OUT_DIR/candidate/.octos/config.json"; then
    printf 'self-test failed: outer sandbox fallback config was not written\n' >&2
    rm -rf "$test_root"
    return 1
  fi

  rm -rf "$test_root"
  printf 'self-test passed\n'
}

main() {
  if [ "${OCTOS_TUI_UX_SELF_TEST:-0}" = "1" ]; then
    run_self_tests
    return $?
  fi

  require_command cargo
  require_command git
  tmux_require
  require_fixture

  if [ -z "${!API_KEY_ENV:-}" ]; then
    printf '%s is required for live octos-tui UX comparison runs with provider=%s model=%s.\n' \
      "$API_KEY_ENV" "$PROVIDER" "$MODEL" >&2
    exit 2
  fi

  mkdir -p "$OUT_DIR"
  RESOLVED_SERVER_SANDBOX_MODE="$(resolve_server_sandbox_mode)"
  : >"$OUT_DIR/summary.env"
  {
    printf 'PROMPT_QUESTION:\n%s\n\n' "$PROMPT_QUESTION"
    printf 'PROMPT_APPROVAL:\n%s\n\n' "$PROMPT_APPROVAL"
    printf 'PROMPT_CODING:\n%s\n\n' "$PROMPT_CODING"
    printf 'PROMPT_CONTINUE:\n%s\n\n' "$PROMPT_CONTINUE"
    printf 'PROMPT_SUMMARY:\n%s\n\n' "$PROMPT_SUMMARY"
    printf 'PROMPT_STEERING:\n%s\n\n' "$PROMPT_STEERING"
    printf 'PROMPT_INTERRUPT:\n%s\n\n' "$PROMPT_INTERRUPT"
    printf 'PROMPT_RECONNECT:\n%s\n\n' "$PROMPT_RECONNECT"
    printf 'PROMPT_FINAL_LONG:\n%s\n' "$PROMPT_FINAL_LONG"
  } >"$OUT_DIR/prompts.txt"
  trap cleanup_all EXIT
  trap 'cleanup_all; exit 130' INT TERM
  start_compat_proxy_if_needed

  log "artifacts: $OUT_DIR"
  log "provider: $PROVIDER"
  log "model: $MODEL"
  log "fixture: $FIXTURE_DIR"
  log "long mode: $LONG_MODE"
  log "server sandbox mode: $RESOLVED_SERVER_SANDBOX_MODE"
  log "octos-tui session: $(tmux_session_name octos-tui-client)"
  log "codex session: $(tmux_session_name codex-client)"

  local tui_rc=0
  local codex_rc=0
  local octos_bin=""
  local tui_bin=""

  if [ "$RUN_TUI" = "1" ]; then
    octos_bin="$(resolve_octos_bin)"
    tui_bin="$(resolve_tui_bin)"
    drive_tui "$(prepare_candidate octos_tui)" "$octos_bin" "$tui_bin" || tui_rc=$?
  fi

  if [ "$RUN_CODEX" = "1" ]; then
    if ! command -v codex >/dev/null 2>&1 && [ ! -f "$HOME/.x-cmd.root/X" ]; then
      printf 'No Codex command found. Install x-cmd Codex or set PATH.\n' >&2
      exit 127
    fi
    drive_codex "$(prepare_candidate codex)" || codex_rc=$?
  fi

  local ended_at_utc
  local elapsed_seconds
  ended_at_utc="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  elapsed_seconds=$(($(date +%s) - RUN_START_EPOCH))

  {
    printf 'run_id=%s\n' "$RUN_ID"
    printf 'started_at_utc=%s\n' "$RUN_STARTED_AT_UTC"
    printf 'ended_at_utc=%s\n' "$ended_at_utc"
    printf 'elapsed_seconds=%s\n' "$elapsed_seconds"
    printf 'out_dir=%s\n' "$OUT_DIR"
    printf 'fixture_dir=%s\n' "$FIXTURE_DIR"
    printf 'provider=%s\n' "$PROVIDER"
    printf 'api_key_env=%s\n' "$API_KEY_ENV"
    printf 'long_mode=%s\n' "$LONG_MODE"
    printf 'frame_sample_enabled=%s\n' "$FRAME_SAMPLE_ENABLED"
    printf 'server_sandbox_mode=%s\n' "$RESOLVED_SERVER_SANDBOX_MODE"
    printf 'server_sandbox_policy=%s\n' "$SERVER_SANDBOX_POLICY"
    printf 'max_wait_turn=%s\n' "$MAX_WAIT_TURN"
    printf 'max_tui_coding_rounds=%s\n' "$MAX_TUI_CODING_ROUNDS"
    printf 'model=%s\n' "$MODEL"
    printf 'port=%s\n' "$PORT"
    printf 'octos_tui_rc=%s\n' "$tui_rc"
    printf 'codex_rc=%s\n' "$codex_rc"
  } >>"$OUT_DIR/summary.env"

  log "summary:"
  sed 's/^/[tui-ux]   /' "$OUT_DIR/summary.env"

  if [ "$STRICT" = "1" ] && grep -q 'status=fail' "$OUT_DIR/summary.env"; then
    exit 1
  fi
}

main "$@"
