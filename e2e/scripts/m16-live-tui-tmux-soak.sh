#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

run_id="${OCTOS_M16_UX_RUN_ID:-m16-ux-soak-$(date -u +%Y%m%dT%H%M%SZ)}"
tui_repo="${OCTOS_TUI_REPO:-/Users/yuechen/home/octos-tui}"
tui_runner="${OCTOS_M16_TUI_RUNNER:-$tui_repo/scripts/run-m15-live-tmux-ux-soak.sh}"
out_root="${OCTOS_M16_UX_OUT_ROOT:-$repo_root/e2e/test-results-m16-tmux-ux}"
out_dir="${OCTOS_M16_UX_OUT_DIR:-$out_root/$run_id}"
runtime_root="${OCTOS_M16_UX_RUNTIME_ROOT:-/tmp/octos-m16-ux-$run_id}"
data_dir="${OCTOS_M16_UX_DATA_DIR:-$runtime_root/data}"
workdir="${OCTOS_M16_UX_WORKDIR:-$runtime_root/workspace}"
replay_file="${OCTOS_M16_UX_REPLAY:-$out_dir/m16-code-review-replay.txt}"
octos_bin="${OCTOS_BIN:-$repo_root/target/debug/octos}"
tui_bin="${OCTOS_TUI_BIN:-$tui_repo/target/debug/octos-tui}"
session_name="${OCTOS_M16_UX_TMUX_SESSION:-octos-m16-ux-$run_id}"
profile_id="${OCTOS_M16_UX_PROFILE:-coding}"
session_id="${OCTOS_M16_UX_SESSION_ID:-$profile_id:local:m16-ux:$run_id}"
delay_scale="${OCTOS_M16_UX_SUBAGENT_DELAY_SCALE:-8}"
final_marker="M16_CODE_REVIEW_FINAL_LINE"
fixture_dir="$out_dir/fixtures"
cli_fixture="$fixture_dir/review-cli-specialist.mjs"
mcp_fixture="$fixture_dir/review-mcp-specialist.mjs"
provider_key_source="${OCTOS_M16_NATIVE_PROVIDER_KEY_SOURCE:-$repo_root/e2e/test-results-m15-native-review-start-stdio/20260516T191424Z/data/profiles/m15-native.json}"
live_provider_config_path="$data_dir/profiles/$profile_id.json"
secret_cleanup_report="$out_dir/m16-secret-cleanup.json"
cleanup_secrets_done=0
cleanup_stop_done=0
tmux_session_started=0

usage() {
  cat <<'USAGE'
Usage: e2e/scripts/m16-live-tui-tmux-soak.sh <run|self-test|help>

Runs the M16 visual TUI tmux soak against a real octos serve --stdio backend.
The script reuses the octos-tui tmux driver but writes all M16 evidence under
octos/e2e/test-results-m16-tmux-ux/<run-id>.

Key environment:
  OCTOS_TUI_REPO              Path to octos-tui checkout. Default: /Users/yuechen/home/octos-tui.
  OCTOS_BIN                   octos binary. Default: octos/target/debug/octos.
  OCTOS_TUI_BIN               octos-tui binary. Default: octos-tui/target/debug/octos-tui.
  OCTOS_M16_BUILD             Set 0 to skip building octos with api. Default: 1.
  OCTOS_M16_BUILD_TUI         Set 1 to rebuild octos-tui. Default: build only if missing.
  OCTOS_M16_UX_KEEP_SESSION   Set 1 to keep tmux session after the run.
  OCTOS_M16_UX_OUT_DIR        Override evidence output directory.
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
  if [[ "${OCTOS_M16_BUILD:-1}" == "1" ]]; then
    (cd "$repo_root" && cargo build -p octos-cli --bin octos --features api)
  fi
  if [[ "${OCTOS_M16_BUILD_TUI:-0}" == "1" || ! -x "$tui_bin" ]]; then
    (cd "$tui_repo" && cargo build --bin octos-tui)
  fi
  [[ -x "$octos_bin" ]] || die "octos binary is not executable: $octos_bin"
  [[ -x "$tui_bin" ]] || die "octos-tui binary is not executable: $tui_bin"
}

write_replay() {
  mkdir -p "$out_dir"
  cat > "$replay_file" <<'REPLAY'
# M16 real octos serve stdio visual review/start orchestration soak.
sleep 8
capture tui-capture-before-scroll.txt

line /review M16 code review UX soak through AppUI review/start: Run a deep code review using supervised specialist child agents reviewer-api, reviewer-tests, reviewer-policy, reviewer-cli, and reviewer-mcp. Use visible specialist names Ada Lovelace for API review, Hypatia for test coverage review, Socrates for workspace policy/security review, Grace Hopper for CLI review, and Marie Curie for MCP review. Show child start, child progress, one-child-finished summaries, artifacts, and a final joined scatter-join answer. Return findings first with severity and file:line references, include subagent summaries and artifacts, and end with M16_CODE_REVIEW_FINAL_LINE.
sleep 2
capture tui-capture-child-start.txt
sleep 12
capture tui-capture-child-progress.txt
sleep 30
capture tui-capture-one-child-finished.txt
sleep 45
capture tui-capture-code-review-summary.txt

line /agents list
sleep 1
capture menu-capture-agents.txt
line /agents status reviewer-api
sleep 1
line /agents output reviewer-api
sleep 1
line /agents artifacts reviewer-api
sleep 1
capture diff-preview-capture.txt

keys PPage PPage
sleep 1
capture tui-capture-after-scroll.txt
keys NPage NPage
sleep 1
capture tui-capture-live-final.txt
capture tui-capture.txt

exit
sleep 2
capture tui-exit-capture.txt
REPLAY
}

write_review_workspace_fixture() {
  mkdir -p "$workdir/src" "$workdir/tests"
  cat > "$workdir/Cargo.toml" <<'TOML'
[package]
name = "m16-review-fixture"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"
TOML
  cat > "$workdir/src/lib.rs" <<'RS'
pub mod appui;
RS
  cat > "$workdir/src/appui.rs" <<'RS'
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentState {
    pub id: String,
    pub status: String,
    pub output: String,
}

#[derive(Debug, Default)]
pub struct AgentRegistry {
    agents: HashMap<String, AgentState>,
}

impl AgentRegistry {
    pub fn upsert(&mut self, agent: AgentState) {
        self.agents.insert(agent.id.clone(), agent);
    }

    pub fn status_by_prefix(&self, prefix: &str) -> Option<&AgentState> {
        self.agents
            .iter()
            .find(|(id, _)| id.starts_with(prefix))
            .map(|(_, state)| state)
    }

    pub fn joined_answer(&self) -> String {
        self.agents
            .values()
            .map(|agent| format!("{}: {}", agent.id, agent.output))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn parse_runtime_policy_stamp(raw: &str) -> HashMap<String, String> {
    raw.split(',')
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}
RS
  cat > "$workdir/tests/appui_review.rs" <<'RS'
use m16_review_fixture::appui::{parse_runtime_policy_stamp, AgentRegistry, AgentState};

#[test]
fn registry_can_find_agent_by_prefix() {
    let mut registry = AgentRegistry::default();
    registry.upsert(AgentState {
        id: "reviewer-api-1234".into(),
        status: "completed".into(),
        output: "api ok".into(),
    });

    assert_eq!(
        registry.status_by_prefix("reviewer-api").unwrap().status,
        "completed"
    );
}

#[test]
fn runtime_policy_parser_keeps_required_fields() {
    let stamp = parse_runtime_policy_stamp("profile=coding, sandbox=workspace-write");
    assert_eq!(stamp["profile"], "coding");
    assert_eq!(stamp["sandbox"], "workspace-write");
}
RS
  if command -v git >/dev/null 2>&1; then
    git -C "$workdir" init -q
    git -C "$workdir" add Cargo.toml src tests
    git -C "$workdir" -c user.name=M16 -c user.email=m16@example.test commit -q -m "seed m16 review fixture"
  fi
}

write_review_fixtures() {
  mkdir -p "$fixture_dir"
  cat > "$cli_fixture" <<'JS'
#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
const artifactPath = process.env.OCTOS_REVIEW_ARTIFACT_PATH;
if (!artifactPath) {
  console.error('missing OCTOS_REVIEW_ARTIFACT_PATH');
  process.exit(2);
}
const target = process.env.OCTOS_REVIEW_TARGET || 'unknown-target';
const text = [`# Grace Hopper CLI Review`, '', `Medium: CLI specialist fixture reviewed ${target}.`].join('\n');
fs.mkdirSync(path.dirname(artifactPath), { recursive: true });
fs.writeFileSync(artifactPath, `${text}\n`, 'utf8');
console.log(text);
JS
  chmod +x "$cli_fixture"
  cat > "$mcp_fixture" <<'JS'
#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
const rl = readline.createInterface({ input: process.stdin });
function send(id, result) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', id, result })}\n`);
}
rl.on('line', (line) => {
  let request;
  try { request = JSON.parse(line); } catch { return; }
  if (request.method === 'initialize') {
    send(request.id, { protocolVersion: '2024-11-05', capabilities: {}, serverInfo: { name: 'm16-review-fixture', version: '1.0.0' } });
    return;
  }
  if (request.method === 'tools/call') {
    const args = request.params?.arguments || {};
    const artifactPath = args.artifact_path;
    const text = [`# Marie Curie MCP Review`, '', `Medium: MCP specialist fixture reviewed ${args.target || 'unknown-target'}.`].join('\n');
    if (artifactPath) {
      fs.mkdirSync(path.dirname(artifactPath), { recursive: true });
      fs.writeFileSync(artifactPath, `${text}\n`, 'utf8');
    }
    send(request.id, { content: [{ type: 'text', text }], files_to_send: artifactPath ? [artifactPath] : [] });
  }
});
JS
  chmod +x "$mcp_fixture"
}

deepseek_key_from_source() {
  node -e 'const fs=require("fs"); const p=process.argv[1]; if (!p || !fs.existsSync(p)) process.exit(0); const j=JSON.parse(fs.readFileSync(p,"utf8")); const key=j.config?.env_vars?.DEEPSEEK_API_KEY || ""; process.stdout.write(key === "<redacted>" ? "" : key);' "$provider_key_source"
}

write_profile_config() {
  mkdir -p "$data_dir/profiles"
  node - "$live_provider_config_path" "$profile_id" <<'NODE'
const fs = require('fs');
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
fs.mkdirSync(require('path').dirname(file), { recursive: true });
fs.writeFileSync(file, `${JSON.stringify(profile, null, 2)}\n`);
NODE
}

cleanup_runtime_secrets() {
  if [[ "${cleanup_secrets_done:-0}" == "1" ]]; then
    return 0
  fi
  cleanup_secrets_done=1
  local removed_provider_config=0
  if [[ -f "$live_provider_config_path" ]]; then
    rm -f "$live_provider_config_path" || true
    removed_provider_config=1
  fi
  unset DEEPSEEK_API_KEY
  node - "$out_dir" "$runtime_root" "$data_dir" "$secret_cleanup_report" "$live_provider_config_path" "$removed_provider_config" <<'NODE'
const fs = require('fs');
const path = require('path');
const [outDir, runtimeRoot, dataDir, reportPath, providerConfigPath, removedProviderConfigRaw] = process.argv.slice(2);
const roots = [outDir, runtimeRoot, dataDir].filter(Boolean);
const patterns = [
  { name: 'openai_compatible', re: /sk-(?:proj-|ant-|svcacct-|admin-|or-v1-)?[A-Za-z0-9._\-]{20,}/g },
  { name: 'anthropic_oauth', re: /sk-ant-oat01-[A-Za-z0-9._\-]{20,}/g },
  { name: 'google_ai', re: /AIza[0-9A-Za-z_\-]{30,}/g },
  { name: 'twilio', re: /AC[0-9a-f]{32}/g },
  { name: 'bearer', re: /Bearer [A-Za-z0-9._\-]{32,}/g },
];
const textSuffixes = new Set([
  '.json', '.jsonl', '.log', '.txt', '.md', '.env', '.sh', '.mjs', '.js',
  '.toml', '.yaml', '.yml', '.conf', '.ini',
]);
const skipDirs = new Set(['.git', 'node_modules', 'target', '__pycache__']);
const scannedRoots = [];
const scannedFiles = [];
const redactedFiles = [];
const seenFiles = new Set();
let redactionsTotal = 0;

function isTextFile(filePath) {
  return textSuffixes.has(path.extname(filePath).toLowerCase())
    || ['launch-command.txt', 'launch.sh'].includes(path.basename(filePath));
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
  if (!isTextFile(p)) return;
  const resolved = path.resolve(p);
  if (seenFiles.has(resolved) || resolved === path.resolve(reportPath)) return;
  seenFiles.add(resolved);
  let text;
  try {
    text = fs.readFileSync(p, 'utf8');
  } catch {
    return;
  }
  let next = text;
  let redactions = 0;
  for (const pattern of patterns) {
    pattern.re.lastIndex = 0;
    next = next.replace(pattern.re, () => {
      redactions += 1;
      redactionsTotal += 1;
      return `<redacted:${pattern.name}>`;
    });
  }
  scannedFiles.push(p);
  if (next !== text) {
    fs.writeFileSync(p, next);
    redactedFiles.push({ path: p, redactions });
  }
}

for (const root of roots) {
  if (!fs.existsSync(root)) continue;
  scannedRoots.push(root);
  walk(root);
}

const report = {
  schema: 'octos.m16.secret_cleanup.v1',
  scanned_roots: scannedRoots,
  scanned_files: scannedFiles,
  scanned_file_count: scannedFiles.length,
  redacted_files: redactedFiles,
  redacted_file_count: redactedFiles.length,
  redactions_total: redactionsTotal,
  removed_live_provider_config: Boolean(Number(removedProviderConfigRaw || '0')),
  live_provider_config_path: providerConfigPath,
};
fs.mkdirSync(path.dirname(reportPath), { recursive: true });
fs.writeFileSync(reportPath, `${JSON.stringify(report, null, 2)}\n`);

const legacyLines = [
  '# M16 tmux soak secret-scan report',
  `roots: ${roots.join(', ')}`,
  `files_scanned: ${scannedFiles.length}`,
  `total_redactions: ${redactionsTotal}`,
  '',
  ...redactedFiles.map((entry) => `${entry.redactions}\t${entry.path}`),
];
try {
  fs.writeFileSync(path.join(outDir, 'secret-scan-report.txt'), `${legacyLines.join('\n')}\n`);
} catch {
  // Best-effort compatibility report; the JSON report is canonical.
}
NODE
}

stop_tui_session() {
  if [[ "${cleanup_stop_done:-0}" == "1" ]]; then
    return 0
  fi
  cleanup_stop_done=1
  if [[ "${tmux_session_started:-0}" == "1" && "${OCTOS_M16_UX_KEEP_SESSION:-0}" != "1" && -x "$tui_runner" ]]; then
    "$tui_runner" stop || true
  fi
}

cleanup_on_exit() {
  local status="${1:-$?}"
  stop_tui_session
  cleanup_runtime_secrets || true
  return "$status"
}

cleanup_on_signal() {
  local status="$1"
  trap - EXIT
  cleanup_on_exit "$status"
  exit "$status"
}

run_soak() {
  mkdir -p "$out_dir" "$runtime_root" "$data_dir" "$workdir"
  # Register cleanup before writing live provider config, so startup failures
  # and signals still remove/redact runtime evidence before validation.
  trap 'cleanup_on_exit $?' EXIT
  trap 'cleanup_on_signal 130' INT
  trap 'cleanup_on_signal 143' TERM
  if [[ "${OCTOS_M16_UX_INJECT_FAIL_AFTER_PROFILE_WRITE:-0}" != "1" ]]; then
    command -v tmux >/dev/null 2>&1 || die "tmux is required"
    [[ -x "$tui_runner" ]] || die "octos-tui tmux runner not found or not executable: $tui_runner"
    ensure_binaries
  fi
  write_review_workspace_fixture
  write_review_fixtures
  write_replay

  local provider_key="${OCTOS_M16_NATIVE_API_KEY:-${OCTOS_M15_NATIVE_API_KEY:-${DEEPSEEK_API_KEY:-}}}"
  if [[ -z "$provider_key" || "$provider_key" == "<redacted>" ]]; then
    provider_key="$(deepseek_key_from_source)"
  fi
  [[ -n "$provider_key" && "$provider_key" != "<redacted>" ]] || die "missing provider key; set OCTOS_M16_NATIVE_API_KEY, OCTOS_M15_NATIVE_API_KEY, DEEPSEEK_API_KEY, or OCTOS_M16_NATIVE_PROVIDER_KEY_SOURCE"
  write_profile_config
  export DEEPSEEK_API_KEY="$provider_key"
  if [[ "${OCTOS_M16_UX_INJECT_FAIL_AFTER_PROFILE_WRITE:-0}" == "1" ]]; then
    echo "Injected failure after provider config write" >&2
    return 97
  fi

  local backend_command
  backend_command="env OCTOS_REVIEW_CLI_SPECIALIST_ARGV_JSON=$(shell_quote "[\"$cli_fixture\"]") OCTOS_REVIEW_MCP_TIMEOUT_SECS=30 $(shell_quote "$octos_bin") serve --stdio --data-dir $(shell_quote "$data_dir") --cwd $(shell_quote "$workdir") --swarm-backend stdio --swarm-backend-cmd $(shell_quote "$mcp_fixture")"

  export OCTOS_TUI_M15_UX_RUN_ID="$run_id"
  export OCTOS_TUI_M15_UX_OUT_DIR="$out_dir"
  export OCTOS_TUI_M15_UX_RUNTIME_ROOT="$runtime_root"
  export OCTOS_TUI_M15_UX_WORKDIR="$workdir"
  export OCTOS_TUI_M15_UX_CHILD_OUT_DIR="$runtime_root/artifacts"
  export OCTOS_TUI_M15_UX_BIN="$tui_bin"
  export OCTOS_TUI_M15_UX_BACKEND_COMMAND="$backend_command"
  export OCTOS_TUI_M15_UX_TMUX_SESSION="$session_name"
  export OCTOS_TUI_M15_UX_REPLAY="$replay_file"
  export OCTOS_TUI_M15_UX_SCENARIO="code_review_subagents"
  export OCTOS_TUI_M15_UX_FINAL_MARKER="$final_marker"
  export OCTOS_TUI_M15_UX_SESSION_ID="$session_id"
  export OCTOS_TUI_M15_UX_PROFILE="$profile_id"
  export OCTOS_TUI_M15_UX_REPLACE_SESSION=1
  export OCTOS_TUI_M15_UX_COLS="${OCTOS_M16_UX_COLS:-120}"
  export OCTOS_TUI_M15_UX_ROWS="${OCTOS_M16_UX_ROWS:-40}"

  "$tui_runner" start
  tmux_session_started=1
  local status=0
  "$tui_runner" drive || status=$?
  "$tui_runner" capture || true
  stop_tui_session
  cleanup_runtime_secrets
  python3 "$script_dir/validate-m16-tmux-orchestration.py" --out-dir "$out_dir" || status=$?

  echo "M16 UX soak artifacts: $out_dir"
  return "$status"
}

self_test() {
  bash -n "$0"
  python3 -m py_compile "$script_dir/validate-m16-tmux-orchestration.py"
  local tmp_root
  tmp_root="$(mktemp -d -t m16-cleanup-test-XXXXXX)"
  trap 'rm -rf "$tmp_root"' RETURN
  mkdir -p "$tmp_root/out" "$tmp_root/runtime/preexisting"
  cat > "$tmp_root/out/seed.env" <<TXT
OPENAI_API_KEY=sk-test-1234567890ABCDEFG1234567890
ANTHROPIC_OAUTH_JWT=sk-ant-oat01-jwthdr.octosjwtpayloadABCDEFGH.octosjwtsignatureIJKLMNOP
TXT
  cat > "$tmp_root/runtime/preexisting/log.txt" <<TXT
auth: Bearer abcdefghijklmnopqrstuvwxyz0123456789ABCD
google: AIzaSyA-1234567890abcdefghijklmnopqrstuv
TXT
  local output_file="$tmp_root/injected-failure.out"
  set +e
  OCTOS_M16_UX_OUT_DIR="$tmp_root/out" \
    OCTOS_M16_UX_RUNTIME_ROOT="$tmp_root/runtime" \
    OCTOS_M16_UX_DATA_DIR="$tmp_root/runtime/data" \
    OCTOS_M16_UX_WORKDIR="$tmp_root/runtime/workspace" \
    OCTOS_M16_UX_INJECT_FAIL_AFTER_PROFILE_WRITE=1 \
    OCTOS_M16_NATIVE_API_KEY="sk-testm16cleanup000000000000" \
    OCTOS_M16_BUILD=0 \
    "$0" run >"$output_file" 2>&1
  local status=$?
  set -e
  [[ "$status" == "97" ]] || die "self-test expected injected failure status 97, got $status"
  [[ ! -f "$tmp_root/runtime/data/profiles/coding.json" ]] || die "self-test provider config was not removed"
  if grep -RIEq -- 'sk-test|sk-ant-oat01|AIzaSy|Bearer abcdefghij|octosjwtpayload|octosjwtsignature' "$tmp_root/out" "$tmp_root/runtime"; then
    echo "secret cleanup self-test FAIL: residual secrets in $tmp_root" >&2
    grep -RIEn -- 'sk-test|sk-ant-oat01|AIzaSy|Bearer abcdefghij|octosjwtpayload|octosjwtsignature' "$tmp_root/out" "$tmp_root/runtime" || true
    return 1
  fi
  node - "$tmp_root/out/m16-secret-cleanup.json" <<'NODE'
const fs = require('fs');
const reportPath = process.argv[2];
if (!fs.existsSync(reportPath)) {
  throw new Error(`missing cleanup report: ${reportPath}`);
}
const report = JSON.parse(fs.readFileSync(reportPath, 'utf8'));
if (report.schema !== 'octos.m16.secret_cleanup.v1') {
  throw new Error(`unexpected cleanup schema: ${report.schema}`);
}
if (!report.removed_live_provider_config) {
  throw new Error('cleanup report did not record provider config removal');
}
if (!report.scanned_roots.some((root) => root.endsWith('/out'))) {
  throw new Error('cleanup report did not scan evidence output tree');
}
if (!report.scanned_roots.some((root) => root.endsWith('/runtime'))) {
  throw new Error('cleanup report did not scan runtime tree');
}
if (report.redactions_total < 4 || report.redacted_file_count < 2) {
  throw new Error(`cleanup report did not record expected redactions: ${JSON.stringify(report)}`);
}
NODE
  if [[ ! -s "$tmp_root/out/secret-scan-report.txt" ]]; then
    echo "secret cleanup self-test FAIL: legacy secret-scan-report.txt missing" >&2
    return 1
  fi
  echo "Self-test passed"
}

case "${1:-help}" in
  run) run_soak ;;
  self-test) self_test ;;
  help|-h|--help) usage ;;
  *) usage; exit 2 ;;
esac
