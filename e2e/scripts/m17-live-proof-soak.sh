#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
run_id="${OCTOS_M17_LIVE_PROOF_RUN_ID:-m17-live-proof-$(date -u +%Y%m%dT%H%M%SZ)}"
out_root="${OCTOS_M17_LIVE_PROOF_OUT_ROOT:-$repo_root/e2e/test-results-m17-live-proof}"
out_dir="${OCTOS_M17_LIVE_PROOF_OUT_DIR:-$out_root/$run_id}"
native_dir="${OCTOS_M17_M15_NATIVE_DIR:-$out_dir/m15-native-review-start-stdio}"
loop_dir="${OCTOS_M17_LOOP_DIR:-$out_dir/m15-loop-runtime-stdio}"
goal_dir="${OCTOS_M17_GOAL_DIR:-$out_dir/m15-goal-runtime-stdio}"
tmux_dir="${OCTOS_M17_TMUX_DIR:-$out_dir/m16-tmux-ux}"
spawn_dir="${OCTOS_M17_SPAWN_DIR:-}"
budget_grace_dir="${OCTOS_M17_BUDGET_GRACE_DIR:-}"
validation_dir="${OCTOS_M17_VALIDATION_DIR:-$out_dir/validation}"
octos_bin="${OCTOS_BIN:-$repo_root/target/debug/octos}"

usage() {
  cat <<'USAGE'
Usage: e2e/scripts/m17-live-proof-soak.sh <run|validate|self-test|help>

run       Build/check prerequisites, run known live soaks, then run the M17 validator.
validate  Run only the M17 validator over supplied evidence directories.
self-test Syntax-check scripts and exercise the validator with synthetic evidence.

Live key inputs:
  DEEPSEEK_API_KEY or OCTOS_M15_NATIVE_API_KEY     Required for run.
  OCTOS_BIN                                       Default: target/debug/octos.
  OCTOS_M17_SPAWN_DIR                             Optional direct spawn_agent evidence dir.
  OCTOS_M17_BUDGET_GRACE_DIR                      Optional explicit budget grace evidence dir.
  OCTOS_M17_SKIP_TUI=1                            Skip m16 tmux run when octos-tui/tmux are unavailable.
USAGE
}

die() {
  echo "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

has_provider_key() {
  [[ -n "${OCTOS_M15_NATIVE_API_KEY:-${OCTOS_M16_NATIVE_API_KEY:-${DEEPSEEK_API_KEY:-}}}" ]]
}

build_octos() {
  if [[ "${OCTOS_M17_BUILD:-1}" == "1" ]]; then
    (cd "$repo_root" && cargo build -p octos-cli --bin octos --features api)
  fi
  [[ -x "$octos_bin" ]] || die "octos binary is not executable: $octos_bin"
}

validator_args=()
append_if_dir() {
  local flag="$1"
  local dir="$2"
  if [[ -n "$dir" && -d "$dir" ]]; then
    validator_args+=("$flag" "$dir")
  fi
}

run_validator() {
  validator_args=(--out-dir "$validation_dir")
  append_if_dir --m15-native-dir "$native_dir"
  append_if_dir --m16-tmux-dir "$tmux_dir"
  append_if_dir --loop-dir "$loop_dir"
  append_if_dir --goal-dir "$goal_dir"
  append_if_dir --spawn-dir "$spawn_dir"
  append_if_dir --budget-grace-dir "$budget_grace_dir"
  python3 "$script_dir/validate-m17-live-proof.py" "${validator_args[@]}"
}

run_live() {
  require_cmd node
  require_cmd python3
  has_provider_key || die "missing DeepSeek key; set DEEPSEEK_API_KEY or OCTOS_M15_NATIVE_API_KEY before live run"
  mkdir -p "$out_dir"
  build_octos

  local status=0
  OCTOS_M15_NATIVE_STDIO_SOAK_DIR="$native_dir" node "$script_dir/m15-native-review-start-stdio-soak.mjs" || status=$?
  OCTOS_M15_LOOP_SOAK_DIR="$loop_dir" node "$script_dir/m15-loop-runtime-stdio-soak.mjs" || status=$?
  OCTOS_M15_GOAL_SOAK_DIR="$goal_dir" node "$script_dir/m15-goal-runtime-stdio-soak.mjs" || status=$?

  if [[ "${OCTOS_M17_SKIP_TUI:-0}" == "1" ]]; then
    echo "Skipping m16 tmux soak because OCTOS_M17_SKIP_TUI=1" >&2
  else
    require_cmd tmux
    OCTOS_M16_UX_OUT_DIR="$tmux_dir" bash "$script_dir/m16-live-tui-tmux-soak.sh" run || status=$?
  fi

  run_validator || status=$?
  return "$status"
}

self_test() {
  bash -n "$0"
  python3 -m py_compile "$script_dir/validate-m17-live-proof.py"
  python3 "$script_dir/validate-m17-live-proof.py" --self-test
  echo "M17 live proof harness self-test passed"
}

case "${1:-help}" in
  run) run_live ;;
  validate) run_validator ;;
  self-test) self_test ;;
  help|-h|--help) usage ;;
  *) usage; exit 2 ;;
esac
