#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

run_id="${OCTOS_M16_CONTEXT_TMUX_RUN_ID:-m16-context-reconnect-tmux-$(date -u +%Y%m%dT%H%M%SZ)}"
tui_repo="${OCTOS_TUI_REPO:-/Users/yuechen/home/octos-tui}"
tui_runner="${OCTOS_M16_CONTEXT_TUI_RUNNER:-$tui_repo/scripts/run-m15-live-tmux-ux-soak.sh}"
out_root="${OCTOS_M16_CONTEXT_TMUX_OUT_ROOT:-$repo_root/e2e/test-results-m16-context-restart-tmux}"
out_dir="${OCTOS_M16_CONTEXT_TMUX_OUT_DIR:-$out_root/$run_id}"
runtime_root="${OCTOS_M16_CONTEXT_TMUX_RUNTIME_ROOT:-/tmp/octos-m16-context-tmux-$run_id}"
bootstrap_dir="$out_dir/bootstrap-stdio"
replay_file="$out_dir/context-reconnect-replay.txt"
octos_bin="${OCTOS_BIN:-$repo_root/target/debug/octos}"
tui_bin="${OCTOS_TUI_BIN:-$tui_repo/target/debug/octos-tui}"
session_name="${OCTOS_M16_CONTEXT_TMUX_SESSION:-octos-m16-context-$run_id}"

usage() {
  cat <<'USAGE'
Usage: e2e/scripts/m16-context-restart-tmux-soak.sh <run|self-test|help>

Creates a compacted ContextManager checkpoint with a direct stdio soak, then
launches real octos-tui in tmux against a restarted octos serve --stdio backend
and captures /status context visual evidence.
USAGE
}

die() {
  echo "$*" >&2
  exit 1
}

shell_quote() {
  printf '%q' "$1"
}

json_get() {
  local file="$1"
  local expr="$2"
  node -e "const fs=require('fs'); const j=JSON.parse(fs.readFileSync(process.argv[1],'utf8')); const v=($expr); if (v === undefined || v === null) process.exit(3); process.stdout.write(String(v));" "$file"
}

ensure_binaries() {
  if [[ "${OCTOS_M16_CONTEXT_BUILD:-1}" == "1" ]]; then
    (cd "$repo_root" && cargo build -p octos-cli --bin octos --features api)
  fi
  if [[ "${OCTOS_M16_CONTEXT_BUILD_TUI:-0}" == "1" || ! -x "$tui_bin" ]]; then
    (cd "$tui_repo" && cargo build --bin octos-tui)
  fi
  [[ -x "$octos_bin" ]] || die "octos binary is not executable: $octos_bin"
  [[ -x "$tui_bin" ]] || die "octos-tui binary is not executable: $tui_bin"
  [[ -x "$tui_runner" ]] || die "octos-tui tmux runner is not executable: $tui_runner"
}

write_replay() {
  mkdir -p "$out_dir"
  cat > "$replay_file" <<'REPLAY'
# M16 visual restart/reconnect ContextManager proof.
sleep 4
capture tui-capture-reconnected-session.txt
menu_select status Refresh
sleep 1
keys Escape
sleep 0.2
capture tui-capture-after-status-refresh.txt
menu status context
sleep 0.5
capture tui-capture-status-context-menu.txt
capture tui-capture.txt
exit
sleep 2
capture tui-exit-capture.txt
REPLAY
}

run_soak() {
  command -v tmux >/dev/null 2>&1 || die "tmux is required"
  ensure_binaries
  mkdir -p "$out_dir" "$runtime_root"

  OCTOS_BIN="$octos_bin" \
    OCTOS_M16_CONTEXT_RESTART_DIR="$bootstrap_dir" \
    "$script_dir/m16-context-restart-stdio-soak.mjs" > "$out_dir/bootstrap-stdio.stdout.json"

  local summary="$bootstrap_dir/m16-context-restart-stdio-summary.json"
  [[ -s "$summary" ]] || die "bootstrap summary missing: $summary"
  local ok
  ok="$(json_get "$summary" "j.ok")"
  [[ "$ok" == "true" ]] || die "bootstrap stdio restart proof did not pass"

  local data_dir workspace session_id profile_id generation compaction_id transcript_hash
  data_dir="$(json_get "$summary" "j.dataDir")"
  workspace="$(json_get "$summary" "j.workspace")"
  session_id="$(json_get "$summary" "j.sessionId")"
  profile_id="$(json_get "$summary" "j.profileId")"
  generation="$(json_get "$summary" "j.secondContext.generation")"
  compaction_id="$(json_get "$summary" "j.secondContext.last_compaction_id")"
  transcript_hash="$(json_get "$summary" "j.secondContext.transcript_hash")"

  write_replay

  local backend_command
  backend_command="env OCTOS_CONTEXT_COMPACT_THRESHOLD_TOKENS=1 OCTOS_CONTEXT_COMPACT_KEEP_ITEMS=4 $(shell_quote "$octos_bin") serve --stdio --data-dir $(shell_quote "$data_dir") --cwd $(shell_quote "$workspace")"

  export OCTOS_TUI_M15_UX_RUN_ID="$run_id"
  export OCTOS_TUI_M15_UX_OUT_DIR="$out_dir"
  export OCTOS_TUI_M15_UX_RUNTIME_ROOT="$runtime_root"
  export OCTOS_TUI_M15_UX_WORKDIR="$workspace"
  export OCTOS_TUI_M15_UX_CHILD_OUT_DIR="$runtime_root/artifacts"
  export OCTOS_TUI_M15_UX_BIN="$tui_bin"
  export OCTOS_TUI_M15_UX_BACKEND_COMMAND="$backend_command"
  export OCTOS_TUI_M15_UX_TMUX_SESSION="$session_name"
  export OCTOS_TUI_M15_UX_REPLAY="$replay_file"
  export OCTOS_TUI_M15_UX_SCENARIO="context_restart_reconnect"
  export OCTOS_TUI_M15_UX_SESSION_ID="$session_id"
  export OCTOS_TUI_M15_UX_PROFILE="$profile_id"
  export OCTOS_TUI_M15_UX_REPLACE_SESSION=1
  export OCTOS_TUI_M15_UX_COLS="${OCTOS_M16_CONTEXT_TMUX_COLS:-120}"
  export OCTOS_TUI_M15_UX_ROWS="${OCTOS_M16_CONTEXT_TMUX_ROWS:-40}"

  {
    printf 'bootstrap_summary=%s\n' "$summary"
    printf 'data_dir=%s\n' "$data_dir"
    printf 'workspace=%s\n' "$workspace"
    printf 'session_id=%s\n' "$session_id"
    printf 'profile_id=%s\n' "$profile_id"
    printf 'expected_generation=%s\n' "$generation"
    printf 'expected_compaction_id=%s\n' "$compaction_id"
    printf 'expected_transcript_hash=%s\n' "$transcript_hash"
  } > "$out_dir/context-reconnect-expected.env"

  "$tui_runner" start
  local status=0
  "$tui_runner" drive || status=$?
  "$tui_runner" capture || true
  python3 "$script_dir/validate-m16-context-restart-tmux.py" --out-dir "$out_dir" || status=$?
  "$tui_runner" stop || true

  echo "M16 context restart tmux artifacts: $out_dir"
  return "$status"
}

self_test() {
  bash -n "$0"
  node --check "$script_dir/m16-context-restart-stdio-soak.mjs"
  python3 -m py_compile "$script_dir/validate-m16-context-restart-tmux.py"
  echo "Self-test passed"
}

case "${1:-help}" in
  run) run_soak ;;
  self-test) self_test ;;
  help|-h|--help) usage ;;
  *) usage; exit 2 ;;
esac
