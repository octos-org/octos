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
  node - "$data_dir/profiles/$profile_id.json" "$profile_id" <<'NODE'
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

scrub_secrets() {
  # Walks every artifact / runtime tree the soak writes and redacts
  # provider keys in place. Writes a `secret-scan-report.txt` next to
  # the soak evidence with per-file redaction counts (no secret values
  # are printed). #1024 — the prior implementation matched only a
  # narrow `sk-[A-Za-z0-9]{20,}` pattern and emitted no report, so
  # provider-specific shapes (`sk-ant-*`, `sk-proj-*`, gemini bearer
  # tokens, JWT-shaped Anthropic OAuth) could slip through unscanned.
  node - "$out_dir" "$data_dir" "$runtime_root" <<'NODE'
const fs = require('fs');
const path = require('path');
const roots = process.argv.slice(2);

const patterns = [
  // OpenAI / DeepSeek / generic OpenAI-compatible
  /sk-(?:proj-|ant-|svcacct-|admin-|or-v1-)?[A-Za-z0-9._\-]{20,}/g,
  // Anthropic OAuth bearer tokens (sk-ant-oat01-...)
  /sk-ant-oat01-[A-Za-z0-9._\-]{20,}/g,
  // Google Gemini AIza... keys
  /AIza[0-9A-Za-z_\-]{30,}/g,
  // Twilio account/auth pairs
  /AC[0-9a-f]{32}/g,
  // Generic Bearer secrets in headers/log lines
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
    next = next.replace(pattern, (match) => {
      count += 1;
      return '<redacted>';
    });
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
  try {
    text = fs.readFileSync(p, 'utf8');
  } catch {
    return;
  }
  const { next, count } = redact(text);
  if (count > 0) {
    fs.writeFileSync(p, next);
    report.push({ path: p, count });
    totalRedactions += count;
  }
}

for (const root of roots) walk(root);

// Emit the report under the FIRST root (out_dir). Pass arg-0 (out_dir)
// is the canonical evidence directory.
const evidenceRoot = roots[0];
if (evidenceRoot && fs.existsSync(evidenceRoot)) {
  const lines = [
    `# M16 tmux soak secret-scan report`,
    `roots: ${roots.join(', ')}`,
    `files_scanned: ${filesScanned}`,
    `total_redactions: ${totalRedactions}`,
    '',
    ...report.map((entry) => `${entry.count}\t${entry.path}`),
  ];
  try {
    fs.writeFileSync(path.join(evidenceRoot, 'secret-scan-report.txt'), `${lines.join('\n')}\n`);
  } catch {
    // Don't crash the trap on report write failure.
  }
}

// Non-zero exit on detected redactions would block the soak validator
// from running; surface the count via the report only.
NODE
}

run_soak() {
  command -v tmux >/dev/null 2>&1 || die "tmux is required"
  [[ -x "$tui_runner" ]] || die "octos-tui tmux runner not found or not executable: $tui_runner"
  ensure_binaries
  mkdir -p "$out_dir" "$runtime_root" "$data_dir" "$workdir"
  # Register the secret scrub BEFORE any provider config / key is
  # written so a startup failure between mkdir and the actual write
  # still emits a clean evidence tree. Catching INT/TERM as well
  # closes the gap #1024 flagged where Ctrl-C during the bootstrap
  # left the unscanned provider config behind.
  trap scrub_secrets EXIT INT TERM
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
  local status=0
  "$tui_runner" drive || status=$?
  "$tui_runner" capture || true
  python3 "$script_dir/validate-m16-tmux-orchestration.py" --out-dir "$out_dir" || status=$?
  if [[ "${OCTOS_M16_UX_KEEP_SESSION:-0}" != "1" ]]; then
    "$tui_runner" stop || true
  fi
  scrub_secrets
  trap - EXIT

  echo "M16 UX soak artifacts: $out_dir"
  return "$status"
}

self_test() {
  bash -n "$0"
  python3 -m py_compile "$script_dir/validate-m16-tmux-orchestration.py"
  # #1024: exercise scrub_secrets against a synthetic tree so the
  # broadened pattern set + report emission stay regression-tested.
  local fixture_root
  fixture_root="$(mktemp -d -t m16-scrub-test-XXXXXX)"
  trap 'rm -rf "$fixture_root"' RETURN
  local old_out_dir="${out_dir:-}"
  local old_data_dir="${data_dir:-}"
  local old_runtime_root="${runtime_root:-}"
  out_dir="$fixture_root/out"
  data_dir="$fixture_root/data"
  runtime_root="$fixture_root/runtime"
  mkdir -p "$out_dir" "$data_dir" "$runtime_root"
  cat > "$data_dir/profile.json" <<JSON
{ "api_key": "sk-test-${RANDOM}1234567890ABCDEFG1234567890" }
JSON
  cat > "$runtime_root/log.txt" <<TXT
auth: Bearer abcdefghijklmnopqrstuvwxyz0123456789ABCD
google: AIzaSyA-1234567890abcdefghijklmnopqrstuv
TXT
  cat > "$out_dir/anthropic.env" <<TXT
ANTHROPIC_API_KEY=sk-ant-oat01-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789
# codex P1+P3 follow-up to #1089: dotted JWT-shaped OAuth tokens must
# be scrubbed in full, not just up to the first \`.\` separator. The
# first JWT segment below is intentionally short so the old (no-\`.\`)
# regex cannot match the prefix-with-segment as a 20+ char run either;
# without dot-aware redaction the entire token survives. The residual
# grep then matches distinctive payload and signature markers so any
# regression that drops \`.\` from the char class fails this self-test.
ANTHROPIC_OAUTH_JWT=sk-ant-oat01-jwthdr.octosjwtpayloadABCDEFGH.octosjwtsignatureIJKLMNOP
TXT
  scrub_secrets
  if grep -RIEq -- 'sk-test|sk-ant-oat01|AIzaSy|Bearer abcdefghij|octosjwtpayload|octosjwtsignature' "$fixture_root"; then
    echo "scrub_secrets self-test FAIL: residual secrets in $fixture_root" >&2
    grep -RIEn -- 'sk-test|sk-ant-oat01|AIzaSy|Bearer abcdefghij|octosjwtpayload|octosjwtsignature' "$fixture_root" || true
    out_dir="$old_out_dir"; data_dir="$old_data_dir"; runtime_root="$old_runtime_root"
    return 1
  fi
  if [[ ! -s "$out_dir/secret-scan-report.txt" ]]; then
    echo "scrub_secrets self-test FAIL: secret-scan-report.txt missing or empty" >&2
    out_dir="$old_out_dir"; data_dir="$old_data_dir"; runtime_root="$old_runtime_root"
    return 1
  fi
  if ! grep -q '^total_redactions: [1-9]' "$out_dir/secret-scan-report.txt"; then
    echo "scrub_secrets self-test FAIL: report did not record redactions" >&2
    out_dir="$old_out_dir"; data_dir="$old_data_dir"; runtime_root="$old_runtime_root"
    return 1
  fi
  out_dir="$old_out_dir"; data_dir="$old_data_dir"; runtime_root="$old_runtime_root"
  echo "Self-test passed"
}

case "${1:-help}" in
  run) run_soak ;;
  self-test) self_test ;;
  help|-h|--help) usage ;;
  *) usage; exit 2 ;;
esac
