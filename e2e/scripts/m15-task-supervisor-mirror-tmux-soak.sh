#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

run_id="${OCTOS_M15_TASK_MIRROR_TMUX_RUN_ID:-m15-task-mirror-tmux-$(date -u +%Y%m%dT%H%M%SZ)}"
tui_repo="${OCTOS_TUI_REPO:-/Users/yuechen/home/octos-tui}"
tui_runner="${OCTOS_M15_TASK_MIRROR_TUI_RUNNER:-$tui_repo/scripts/run-m15-live-tmux-ux-soak.sh}"
out_root="${OCTOS_M15_TASK_MIRROR_TMUX_OUT_ROOT:-$repo_root/e2e/test-results-m15-task-supervisor-mirror-tmux}"
out_dir="${OCTOS_M15_TASK_MIRROR_TMUX_OUT_DIR:-$out_root/$run_id}"
runtime_root="${OCTOS_M15_TASK_MIRROR_TMUX_RUNTIME_ROOT:-/tmp/octos-m15-task-mirror-$run_id}"
data_dir="${OCTOS_M15_TASK_MIRROR_TMUX_DATA_DIR:-$runtime_root/data}"
workdir="${OCTOS_M15_TASK_MIRROR_TMUX_WORKDIR:-$runtime_root/workspace}"
replay_file="${OCTOS_M15_TASK_MIRROR_TMUX_REPLAY:-$out_dir/m15-task-supervisor-mirror-replay.txt}"
octos_bin="${OCTOS_BIN:-$repo_root/target/debug/octos}"
tui_bin="${OCTOS_TUI_BIN:-$tui_repo/target/debug/octos-tui}"
session_name="${OCTOS_M15_TASK_MIRROR_TMUX_SESSION:-octos-m15-task-mirror-$run_id}"
profile_id="${OCTOS_M15_TASK_MIRROR_PROFILE:-coding}"
session_id="${OCTOS_M15_TASK_MIRROR_SESSION_ID:-$profile_id:local:m15-task-mirror:$run_id}"

usage() {
  cat <<'USAGE'
Usage: e2e/scripts/m15-task-supervisor-mirror-tmux-soak.sh <run|self-test|help>

Runs a real tmux visual soak proving that octos-tui can display a backend
TaskSupervisor task mirrored into the AppUI agent lifecycle over stdio.

Environment:
  OCTOS_TUI_REPO                         Path to octos-tui checkout. Default: /Users/yuechen/home/octos-tui.
  OCTOS_BIN                              octos binary. Default: octos/target/debug/octos.
  OCTOS_TUI_BIN                          octos-tui binary. Default: octos-tui/target/debug/octos-tui.
  OCTOS_M15_TASK_MIRROR_BUILD            Set 0 to skip building octos. Default: 1.
  OCTOS_M15_TASK_MIRROR_BUILD_TUI        Set 1 to rebuild octos-tui. Default: build only if missing.
  OCTOS_M15_TASK_MIRROR_TMUX_KEEP_SESSION
                                         Set 1 to keep tmux session after the run.
USAGE
}

die() {
  echo "$*" >&2
  exit 1
}

shell_quote() {
  printf '%q' "$1"
}

ensure_binaries() {
  if [[ "${OCTOS_M15_TASK_MIRROR_BUILD:-1}" == "1" ]]; then
    (cd "$repo_root" && cargo build -p octos-cli --bin octos --features api)
  fi
  if [[ "${OCTOS_M15_TASK_MIRROR_BUILD_TUI:-0}" == "1" || ! -x "$tui_bin" ]]; then
    (cd "$tui_repo" && cargo build --bin octos-tui)
  fi
  [[ -x "$octos_bin" ]] || die "octos binary is not executable: $octos_bin"
  [[ -x "$tui_bin" ]] || die "octos-tui binary is not executable: $tui_bin"
  [[ -x "$tui_runner" ]] || die "octos-tui tmux runner is not executable: $tui_runner"
}

write_profile_config() {
  mkdir -p "$data_dir/profiles"
  node - "$data_dir/profiles/$profile_id.json" "$profile_id" <<'NODE'
const fs = require('fs');
const path = require('path');
const [file, profileId] = process.argv.slice(2);
const now = new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
const profile = {
  id: profileId,
  name: profileId === 'coding' ? 'Coding' : profileId,
  username: profileId,
  email: `${profileId}@example.test`,
  enabled: true,
  data_dir: null,
  created_at: now,
  updated_at: now,
  config: {
    admin_mode: false,
    email: null,
    api_type: null,
    channels: [],
    hooks: [],
    adaptive_routing: null,
    content_routing: null,
    env_vars: {},
    llm: {
      primary: {
        family_id: 'deepseek',
        model_id: 'deepseek-chat',
        route: {
          route_id: 'deepseek',
          api_type: 'openai',
          api_key_env: 'DEEPSEEK_API_KEY',
        },
      },
      fallbacks: [],
    },
    gateway: {
      browser_timeout_secs: null,
      max_concurrent_sessions: null,
      max_history: null,
      max_iterations: null,
      max_output_tokens: null,
      system_prompt: null,
    },
    sandbox: {
      enabled: true,
      mode: 'auto',
      profile_name: null,
      allow_network: false,
      read_allow_paths: [],
      docker: {
        image: 'ubuntu:24.04',
        mount_mode: 'rw',
        memory_limit: null,
        cpu_limit: null,
        pids_limit: null,
        extra_binds: [],
      },
    },
  },
};
fs.mkdirSync(path.dirname(file), { recursive: true });
fs.writeFileSync(file, `${JSON.stringify(profile, null, 2)}\n`);
NODE
}

write_replay() {
  mkdir -p "$out_dir"
  cat > "$replay_file" <<'REPLAY'
# M15 TaskSupervisor mirrored-agent live tmux soak.
sleep 3
capture tui-capture-before-scroll.txt

line M9 task output fixture: create deterministic background task output and mirror it into agent supervision.
sleep 4
capture tui-capture-task-mirror.txt

keys Tab
sleep 0.2
keys o
sleep 1
capture task-output-modal.txt
keys Escape
sleep 0.2

line /agents list
sleep 1
capture menu-capture-agents.txt
capture tui-capture-live-final.txt
capture tui-capture.txt

exit
sleep 2
capture tui-exit-capture.txt
REPLAY
}

scrub_fixture_key() {
  node - "$out_dir" "$data_dir" "$runtime_root" <<'NODE'
const fs = require('fs');
const path = require('path');
const roots = process.argv.slice(2);
const fixtureKey = /dummy-key-for-m15-task-supervisor-fixture/g;
function walk(p) {
  if (!p || !fs.existsSync(p)) return;
  const st = fs.statSync(p);
  if (st.isDirectory()) {
    for (const name of fs.readdirSync(p)) walk(path.join(p, name));
    return;
  }
  if (!/\.(json|jsonl|log|txt|env|sh)$/.test(p)) return;
  const text = fs.readFileSync(p, 'utf8');
  const next = text.replace(fixtureKey, '<fixture-key>');
  if (next !== text) fs.writeFileSync(p, next);
}
for (const root of roots) walk(root);
NODE
}

run_soak() {
  command -v tmux >/dev/null 2>&1 || die "tmux is required"
  ensure_binaries
  mkdir -p "$out_dir" "$runtime_root" "$data_dir" "$workdir"
  trap scrub_fixture_key EXIT
  write_profile_config
  write_replay

  local backend_command
  backend_command="env OCTOS_M9_PROTOCOL_FIXTURES=1 DEEPSEEK_API_KEY=dummy-key-for-m15-task-supervisor-fixture $(shell_quote "$octos_bin") serve --stdio --data-dir $(shell_quote "$data_dir") --cwd $(shell_quote "$workdir")"

  export OCTOS_TUI_M15_UX_RUN_ID="$run_id"
  export OCTOS_TUI_M15_UX_OUT_DIR="$out_dir"
  export OCTOS_TUI_M15_UX_RUNTIME_ROOT="$runtime_root"
  export OCTOS_TUI_M15_UX_WORKDIR="$workdir"
  export OCTOS_TUI_M15_UX_CHILD_OUT_DIR="$runtime_root/artifacts"
  export OCTOS_TUI_M15_UX_BIN="$tui_bin"
  export OCTOS_TUI_M15_UX_BACKEND_COMMAND="$backend_command"
  export OCTOS_TUI_M15_UX_TMUX_SESSION="$session_name"
  export OCTOS_TUI_M15_UX_REPLAY="$replay_file"
  export OCTOS_TUI_M15_UX_SCENARIO="task_supervisor_mirror"
  export OCTOS_TUI_M15_UX_FINAL_MARKER="M15_TASK_SUPERVISOR_MIRROR_FINAL_LINE"
  export OCTOS_TUI_M15_UX_SESSION_ID="$session_id"
  export OCTOS_TUI_M15_UX_PROFILE="$profile_id"
  export OCTOS_TUI_M15_UX_REPLACE_SESSION=1
  export OCTOS_TUI_M15_UX_COLS="${OCTOS_M15_TASK_MIRROR_TMUX_COLS:-120}"
  export OCTOS_TUI_M15_UX_ROWS="${OCTOS_M15_TASK_MIRROR_TMUX_ROWS:-40}"

  "$tui_runner" start
  local status=0
  "$tui_runner" drive || status=$?
  "$tui_runner" capture || true
  scrub_fixture_key
  python3 "$script_dir/validate-m15-task-supervisor-mirror-tmux.py" --out-dir "$out_dir" || status=$?
  if [[ "${OCTOS_M15_TASK_MIRROR_TMUX_KEEP_SESSION:-0}" != "1" ]]; then
    "$tui_runner" stop || true
  fi
  trap - EXIT

  echo "M15 TaskSupervisor mirror tmux artifacts: $out_dir"
  return "$status"
}

self_test() {
  bash -n "$0"
  python3 -m py_compile "$script_dir/validate-m15-task-supervisor-mirror-tmux.py"
  echo "Self-test passed"
}

case "${1:-help}" in
  run) run_soak ;;
  self-test) self_test ;;
  help|-h|--help) usage ;;
  *) usage; exit 2 ;;
esac
