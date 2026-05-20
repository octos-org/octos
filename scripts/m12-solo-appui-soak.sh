#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

run_id="${OCTOS_M12_SOAK_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
artifact_root="${OCTOS_M12_SOAK_ARTIFACT_ROOT:-$repo_root/e2e/test-results-m12-solo-soak}"
artifact_dir="${OCTOS_M12_SOAK_ARTIFACT_DIR:-$artifact_root/$run_id}"
runtime_root="${OCTOS_M12_SOAK_RUNTIME_ROOT:-/tmp/octos-m12-solo-$run_id}"
workspace="${OCTOS_M12_SOAK_WORKSPACE:-$runtime_root/workspace}"
data_dir="${OCTOS_M12_SOAK_DATA_DIR:-$runtime_root/data}"
logs_dir="${OCTOS_M12_SOAK_LOGS_DIR:-$runtime_root/logs}"
octos_bin="${OCTOS_BIN:-$repo_root/target/debug/octos}"
transport="${OCTOS_M12_SOAK_TRANSPORT:-both}"
host="${OCTOS_M12_SOAK_HOST:-127.0.0.1}"
port="${OCTOS_M12_SOAK_PORT:-50179}"
auth_token="${OCTOS_M12_SOAK_AUTH_TOKEN:-octos-m12-solo-soak-token}"
profile_id="${OCTOS_M12_SOAK_PROFILE:-m12solo}"
session_id="${OCTOS_M12_SOAK_SESSION:-$profile_id:local:m12-solo#$run_id}"
serve_args="${OCTOS_M12_SOAK_SERVE_ARGS:-}"
strict="${OCTOS_M12_SOAK_STRICT:-0}"
tenant_negative="${OCTOS_M12_SOAK_TENANT_NEGATIVE:-0}"
api_key_env="${OCTOS_M12_SOAK_API_KEY_ENV:-OPENAI_API_KEY}"
api_key="${OCTOS_M12_SOAK_API_KEY:-octos-m12-soak-test-key}"
endpoint="ws://$host:$port/api/ui-protocol/ws"

usage() {
  cat <<'USAGE'
Usage: scripts/m12-solo-appui-soak.sh <run|self-test|help>

Environment:
  OCTOS_M12_SOAK_TRANSPORT     ws, stdio, both, or fixture. Default: both.
  OCTOS_M12_SOAK_ARTIFACT_DIR  Artifact directory. Default: e2e/test-results-m12-solo-soak/<run-id>.
  OCTOS_M12_SOAK_RUNTIME_ROOT  Runtime root. Default: /tmp/octos-m12-solo-<run-id>.
  OCTOS_M12_SOAK_WORKSPACE     Workspace cwd requested through session/open.cwd.
  OCTOS_M12_SOAK_DATA_DIR      Backend data dir.
  OCTOS_BIN                    octos binary. Default: target/debug/octos.
  OCTOS_M12_SOAK_SERVE_ARGS    Extra args for `octos serve`.
  OCTOS_M12_SOAK_STRICT=1      Fail when M12-A/C methods are blocked instead of recording blockers.
  OCTOS_M12_SOAK_TENANT_NEGATIVE=1
                              Also run the tenant/cloud dangerous-mode negative probe. Default 0
                              because local solo live runs cannot change deployment mode.

The live runner captures, per transport:
  tui-capture.txt is owned by octos-tui's tmux runner; this backend runner captures
  server.log, appui-transcript.jsonl, runtime-policy-stamp.json,
  tool-registry-snapshot.json, approval-events.jsonl, and filesystem-probe.json.
USAGE
}

scrub_secrets() {
  # #1024 parity — seed_profile_runtime_config writes the operator's
  # provider API key into $data_dir/profiles/$profile_id.json. The
  # captured server.log and probe transcripts may also surface it.
  # Walk the artifact and runtime trees, redact provider key shapes,
  # and emit a secret-scan-report.txt next to the soak evidence.
  node - "$artifact_dir" "$data_dir" "$runtime_root" <<'NODE'
const fs = require('fs');
const path = require('path');
const roots = process.argv.slice(2);

const patterns = [
  /sk-(?:proj-|ant-|svcacct-|admin-|or-v1-)?[A-Za-z0-9._\-]{20,}/g,
  /sk-ant-oat01-[A-Za-z0-9._\-]{20,}/g,
  /AIza[0-9A-Za-z_\-]{30,}/g,
  /AC[0-9a-f]{32}/g,
  /Bearer [A-Za-z0-9._\-]{32,}/g,
];

const scanExtensions = /\.(json|jsonl|log|txt|md|env|sh|mjs|yaml|yml|toml|conf|ini)$/i;
const skipDirs = new Set(['.git', 'node_modules', 'target', '__pycache__']);

const report = [];
let totalRedactions = 0;
let filesScanned = 0;

function redact(text) {
  let next = text;
  let count = 0;
  for (const pattern of patterns) {
    pattern.lastIndex = 0;
    next = next.replace(pattern, () => { count += 1; return '<redacted>'; });
  }
  return { next, count };
}

function walk(p) {
  if (!p || !fs.existsSync(p)) return;
  const st = fs.statSync(p);
  if (st.isDirectory()) {
    for (const name of fs.readdirSync(p)) {
      if (skipDirs.has(name)) continue;
      walk(path.join(p, name));
    }
    return;
  }
  if (!scanExtensions.test(p)) return;
  filesScanned += 1;
  let text;
  try { text = fs.readFileSync(p, 'utf8'); } catch { return; }
  const { next, count } = redact(text);
  if (count > 0) {
    fs.writeFileSync(p, next);
    report.push({ path: p, count });
    totalRedactions += count;
  }
}

for (const root of roots) walk(root);

const evidenceRoot = roots[0];
if (evidenceRoot && fs.existsSync(evidenceRoot)) {
  const lines = [
    '# M12 solo AppUI soak secret-scan report',
    `roots: ${roots.join(', ')}`,
    `files_scanned: ${filesScanned}`,
    `total_redactions: ${totalRedactions}`,
    '',
    ...report.map((e) => `${e.count}\t${e.path}`),
  ];
  try {
    fs.writeFileSync(path.join(evidenceRoot, 'secret-scan-report.txt'), `${lines.join('\n')}\n`);
  } catch { /* don't crash the trap */ }
}
NODE
}

seed_profile_runtime_config() {
  mkdir -p "$data_dir/profiles"
  local profile_path="$data_dir/profiles/$profile_id.json"
  local now
  now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  cat > "$profile_path" <<JSON
{
  "id": "$profile_id",
  "name": "M12 Solo Soak",
  "username": "$profile_id",
  "email": "$profile_id@example.invalid",
  "enabled": true,
  "config": {
    "llm": {
      "primary": {
        "family_id": "openai",
        "model_id": "gpt-4o-mini",
          "route": {
            "route_id": "official",
          "api_key_env": "$api_key_env",
          "api_type": "openai"
        }
      },
      "fallbacks": []
    },
    "env_vars": {
      "$api_key_env": "$api_key"
    }
  },
  "created_at": "$now",
  "updated_at": "$now"
}
JSON
}

die() {
  echo "$*" >&2
  exit 1
}

require_node() {
  command -v node >/dev/null 2>&1 || die "node is required"
}

require_octos() {
  [ -x "$octos_bin" ] || die "OCTOS_BIN is not executable: $octos_bin"
  if ! "$octos_bin" serve --help >/dev/null 2>&1; then
    die "OCTOS_BIN does not expose 'serve'; build octos-cli with the api feature or set OCTOS_BIN to an API-enabled binary"
  fi
}

shell_quote() {
  printf '%q' "$1"
}

write_summary() {
  local dir="$1"
  mkdir -p "$dir"
  {
    printf 'run_id=%s\n' "$run_id"
    printf 'artifact_dir=%s\n' "$artifact_dir"
    printf 'runtime_root=%s\n' "$runtime_root"
    printf 'workspace=%s\n' "$workspace"
    printf 'data_dir=%s\n' "$data_dir"
    printf 'profile_id=%s\n' "$profile_id"
    printf 'session_id=%s\n' "$session_id"
    printf 'transport=%s\n' "$transport"
    printf 'endpoint=%s\n' "$endpoint"
  } > "$dir/summary.env"
}

probe_args() {
  local probe_transport="$1"
  local out_dir="$2"
  local server_log="$3"
  local stdio_command="${4:-}"
  local args=(
    "$script_dir/m12-solo-appui-probe.mjs"
    --transport "$probe_transport"
    --out-dir "$out_dir"
    --workspace "$workspace"
    --data-dir "$data_dir"
    --profile-id "$profile_id"
    --session-id "$session_id"
    --local-name "M12 Solo Soak"
    --local-username "$profile_id"
    --local-email "$profile_id@example.invalid"
    --server-log "$server_log"
  )
  if [ "$probe_transport" = "ws" ]; then
    args+=(--endpoint "$endpoint" --auth-token "$auth_token")
  fi
  if [ "$probe_transport" = "stdio" ]; then
    args+=(--stdio-command "$stdio_command")
  fi
  if [ "$strict" = "1" ]; then
    args+=(--strict)
  fi
  if [ "$tenant_negative" != "1" ]; then
    args+=(--no-tenant-negative)
  fi
  printf '%s\0' "${args[@]}"
}

run_probe() {
  local probe_transport="$1"
  local out_dir="$2"
  local server_log="$3"
  local stdio_command="${4:-}"
  mkdir -p "$out_dir" "$(dirname "$server_log")"
  write_summary "$out_dir"
  local -a args=()
  while IFS= read -r -d '' arg; do
    args+=("$arg")
  done < <(probe_args "$probe_transport" "$out_dir" "$server_log" "$stdio_command")
  node "${args[@]}"
}

run_ws() {
  require_octos
  local out_dir="$artifact_dir/ws"
  local server_log="$out_dir/server.log"
  mkdir -p "$workspace" "$data_dir" "$logs_dir" "$out_dir"
  seed_profile_runtime_config
  : > "$server_log"

  local server_cmd=("$octos_bin" serve --host "$host" --port "$port" --data-dir "$data_dir" --auth-token "$auth_token" --cwd "$workspace")
  if [ -n "$serve_args" ]; then
    # shellcheck disable=SC2206
    server_cmd+=($serve_args)
  fi
  env "$api_key_env=$api_key" "${server_cmd[@]}" >"$server_log" 2>&1 &
  local server_pid=$!
  trap 'kill "$server_pid" 2>/dev/null || true' RETURN
  sleep "${OCTOS_M12_SOAK_SERVER_WAIT_SECS:-3}"
  run_probe ws "$out_dir" "$server_log"
  kill "$server_pid" 2>/dev/null || true
  wait "$server_pid" 2>/dev/null || true
  trap - RETURN
}

run_stdio() {
  require_octos
  local out_dir="$artifact_dir/stdio"
  local server_log="$out_dir/server.log"
  mkdir -p "$workspace" "$data_dir" "$logs_dir" "$out_dir"
  seed_profile_runtime_config
  : > "$server_log"
  local stdio_command
  stdio_command="env $(shell_quote "$api_key_env=$api_key") $(shell_quote "$octos_bin") serve --stdio --data-dir $(shell_quote "$data_dir") --cwd $(shell_quote "$workspace")"
  if [ -n "$serve_args" ]; then
    stdio_command="$stdio_command $serve_args"
  fi
  run_probe stdio "$out_dir" "$server_log" "$stdio_command"
}

run_fixture() {
  local out_dir="$artifact_dir/fixture"
  mkdir -p "$workspace" "$data_dir" "$out_dir"
  run_probe fixture "$out_dir" "$out_dir/server.log"
  printf 'fixture transport has no backend server\n' > "$out_dir/server.log"
}

run_all() {
  require_node
  mkdir -p "$artifact_dir" "$workspace" "$data_dir" "$logs_dir"
  # #1024 parity — register the scrub BEFORE any provider key is
  # written by seed_profile_runtime_config, and fire on signals so a
  # Ctrl-C between server start and validator still leaves a clean
  # evidence tree.
  trap scrub_secrets EXIT INT TERM
  case "$transport" in
    ws) run_ws ;;
    stdio) run_stdio ;;
    both)
      run_ws
      run_stdio
      ;;
    fixture) run_fixture ;;
    *) die "OCTOS_M12_SOAK_TRANSPORT must be ws, stdio, both, or fixture; got: $transport" ;;
  esac
  echo "M12 solo soak artifacts: $artifact_dir"
}

self_test() {
  require_node
  local tmp_root
  tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/octos-m12-solo-self-test.XXXXXX")"
  OCTOS_M12_SOAK_ARTIFACT_DIR="$tmp_root/artifacts" \
  OCTOS_M12_SOAK_RUNTIME_ROOT="$tmp_root/runtime" \
  OCTOS_M12_SOAK_TRANSPORT=fixture \
  "$0" run >/tmp/octos-m12-solo-self-test.out
  local out_dir="$tmp_root/artifacts/fixture"
  [ -f "$out_dir/appui-transcript.jsonl" ] || die "self-test missing appui-transcript.jsonl"
  [ -f "$out_dir/runtime-policy-stamp.json" ] || die "self-test missing runtime-policy-stamp.json"
  [ -f "$out_dir/tool-registry-snapshot.json" ] || die "self-test missing tool-registry-snapshot.json"
  [ -f "$out_dir/approval-events.jsonl" ] || die "self-test missing approval-events.jsonl"
  [ -f "$out_dir/filesystem-probe.json" ] || die "self-test missing filesystem-probe.json"
  [ -f "$out_dir/workspace-contract-status.json" ] || die "self-test missing workspace-contract-status.json"
  if grep -E 'auth/(send_code|verify)' "$out_dir/appui-transcript.jsonl" >/dev/null 2>&1; then
    die "self-test transcript contains OTP method traffic"
  fi
  if ! grep -q '"status": "passed"' "$out_dir/soak-summary.json"; then
    die "self-test fixture did not pass"
  fi
  if [ "${OCTOS_M12_SOAK_SELF_TEST_KEEP:-0}" = "1" ]; then
    echo "Self-test passed; artifacts kept at $tmp_root/artifacts"
  else
    rm -rf "$tmp_root"
    echo "Self-test passed"
  fi
}

case "${1:-help}" in
  run) run_all ;;
  self-test) self_test ;;
  help|-h|--help) usage ;;
  *) usage; exit 2 ;;
esac
