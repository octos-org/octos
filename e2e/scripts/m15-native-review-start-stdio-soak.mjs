#!/usr/bin/env node

import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';

const repoRoot = path.resolve(import.meta.dirname, '..', '..');
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..+/, 'Z');
const runRoot = path.resolve(
  process.env.OCTOS_M15_NATIVE_STDIO_SOAK_DIR
    || path.join(repoRoot, 'e2e', 'test-results-m15-native-review-start-stdio', stamp),
);
const dataDir = path.join(runRoot, 'data');
const workspace = path.join(runRoot, 'workspace');
const octosBin = process.env.OCTOS_BIN || path.join(repoRoot, 'target', 'debug', 'octos');
const profileId = process.env.OCTOS_M15_NATIVE_PROFILE || 'm15-native';
const sessionId =
  process.env.OCTOS_M15_NATIVE_SESSION || `${profileId}:local:m15-native-review-${stamp}`;
const timeoutMs = Number(process.env.OCTOS_M15_NATIVE_TIMEOUT_MS || 180_000);
const providerFamily = process.env.OCTOS_M15_NATIVE_PROVIDER || 'deepseek';
const modelId = process.env.OCTOS_M15_NATIVE_MODEL || 'deepseek-chat';
const providerKey = process.env.OCTOS_M15_NATIVE_API_KEY || process.env.DEEPSEEK_API_KEY || '';

const observedTranscript = path.join(runRoot, 'client-observed-appui-transcript.jsonl');
const serverStderr = path.join(runRoot, 'server-stderr.log');
const cliFixture = path.join(runRoot, 'fixtures', 'review-cli-specialist.mjs');
const mcpFixture = path.join(runRoot, 'fixtures', 'review-mcp-specialist.mjs');

function appendJsonl(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.appendFileSync(file, `${JSON.stringify(redactSecrets(value))}\n`);
}

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(redactSecrets(value), null, 2)}\n`);
}

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

function sensitiveKey(key) {
  return /(?:api[_-]?key|secret|token|password|authorization|auth[_-]?header)$/i.test(key)
    || /(?:API_KEY|SECRET|TOKEN|PASSWORD)$/i.test(key);
}

function redactSecrets(value) {
  if (Array.isArray(value)) return value.map(redactSecrets);
  if (!value || typeof value !== 'object') return value;
  const out = {};
  for (const [key, child] of Object.entries(value)) {
    out[key] = sensitiveKey(key) && typeof child === 'string' ? '<redacted>' : redactSecrets(child);
  }
  return out;
}

function redactJsonFile(file) {
  if (!fs.existsSync(file)) return;
  try {
    const value = JSON.parse(fs.readFileSync(file, 'utf8'));
    writeJson(file, value);
  } catch {
    // ignore cleanup failure
  }
}

function redactGeneratedSecrets() {
  redactJsonFile(path.join(dataDir, 'profiles', `${profileId}.json`));
}

if (!providerKey) {
  const failure = {
    ok: false,
    error:
      'Missing provider key. Set OCTOS_M15_NATIVE_API_KEY or DEEPSEEK_API_KEY before running the native review/start soak.',
    runRoot,
  };
  writeJson(path.join(runRoot, 'm15-native-review-start-summary.json'), failure);
  console.error(JSON.stringify(failure, null, 2));
  process.exit(2);
}

fs.mkdirSync(workspace, { recursive: true });
fs.mkdirSync(path.dirname(cliFixture), { recursive: true });
fs.writeFileSync(
  path.join(workspace, 'review_target.rs'),
  [
    'pub fn divide(total: i32, count: i32) -> i32 {',
    '    total / count',
    '}',
    '',
    '#[cfg(test)]',
    'mod tests {',
    '    use super::*;',
    '',
    '    #[test]',
    '    fn divides() {',
    '        assert_eq!(divide(10, 2), 5);',
    '    }',
    '}',
    '',
  ].join('\n'),
);
fs.writeFileSync(
  cliFixture,
  [
    '#!/usr/bin/env node',
    "import fs from 'node:fs';",
    "import path from 'node:path';",
    "const artifactPath = process.env.OCTOS_REVIEW_ARTIFACT_PATH;",
    "if (!artifactPath) {",
    "  console.error('missing OCTOS_REVIEW_ARTIFACT_PATH');",
    '  process.exit(2);',
    '}',
    "const target = process.env.OCTOS_REVIEW_TARGET || 'unknown-target';",
    "const objective = process.env.OCTOS_REVIEW_OBJECTIVE || 'unknown-objective';",
    "const text = [`# Grace Hopper CLI Review`, '', `Medium: CLI specialist fixture reviewed ${target}.`, '', `Objective excerpt: ${objective.slice(0, 240)}`].join('\\n');",
    'fs.mkdirSync(path.dirname(artifactPath), { recursive: true });',
    'fs.writeFileSync(artifactPath, `${text}\\n`, "utf8");',
    'console.log(text);',
    '',
  ].join('\n'),
  { mode: 0o755 },
);
fs.chmodSync(cliFixture, 0o755);
fs.writeFileSync(
  mcpFixture,
  [
    '#!/usr/bin/env node',
    "import fs from 'node:fs';",
    "import path from 'node:path';",
    "import readline from 'node:readline';",
    'const rl = readline.createInterface({ input: process.stdin });',
    'function send(id, result) {',
    '  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\\n`);',
    '}',
    "function artifactText(args) {",
    "  const target = args?.target || 'unknown-target';",
    "  return [`# Marie Curie MCP Review`, '', `Medium: MCP specialist fixture reviewed ${target}.`, '', `Agent: ${args?.agent_id || 'unknown-agent'}`].join('\\n');",
    '}',
    "rl.on('line', (line) => {",
    '  let request;',
    '  try { request = JSON.parse(line); } catch { return; }',
    "  if (request.method === 'initialize') {",
    '    send(request.id, { protocolVersion: "2024-11-05", capabilities: {}, serverInfo: { name: "m15-review-fixture", version: "1.0.0" } });',
    '    return;',
    '  }',
    "  if (request.method === 'tools/call') {",
    '    const args = request.params?.arguments || {};',
    '    const artifactPath = args.artifact_path;',
    '    const text = artifactText(args);',
    '    if (artifactPath) {',
    '      fs.mkdirSync(path.dirname(artifactPath), { recursive: true });',
    '      fs.writeFileSync(artifactPath, `${text}\\n`, "utf8");',
    '    }',
    '    send(request.id, { content: [{ type: "text", text }], files_to_send: artifactPath ? [artifactPath] : [] });',
    '  }',
    '});',
    '',
  ].join('\n'),
  { mode: 0o755 },
);
fs.chmodSync(mcpFixture, 0o755);

const child = spawn(octosBin, [
  'serve',
  '--stdio',
  '--data-dir',
  dataDir,
  '--cwd',
  workspace,
  '--swarm-backend',
  'stdio',
  '--swarm-backend-cmd',
  mcpFixture,
], {
  cwd: repoRoot,
  env: {
    ...process.env,
    RUST_BACKTRACE: process.env.RUST_BACKTRACE || '1',
    OCTOS_REVIEW_CLI_SPECIALIST_ARGV_JSON: JSON.stringify([cliFixture]),
    OCTOS_REVIEW_MCP_TIMEOUT_SECS: process.env.OCTOS_REVIEW_MCP_TIMEOUT_SECS || '30',
  },
  stdio: ['pipe', 'pipe', 'pipe'],
});

const pending = new Map();
const notifications = [];
const messageDeltas = [];
const agentUpdated = [];
const agentOutputDelta = [];
const artifactUpdated = [];
let turnCompleted = false;
let turnErrored = null;
let stderrText = '';
let nextSeq = 0;

child.stderr.on('data', (chunk) => {
  const text = chunk.toString();
  stderrText += text;
  fs.appendFileSync(serverStderr, text);
});

const rl = readline.createInterface({ input: child.stdout });
rl.on('line', (line) => {
  let frame;
  try {
    frame = JSON.parse(line);
  } catch {
    appendJsonl(observedTranscript, { direction: 'server_to_client_non_json', line });
    return;
  }
  appendJsonl(observedTranscript, { direction: 'server_to_client', frame });

  if (frame && Object.prototype.hasOwnProperty.call(frame, 'id') && frame.id != null) {
    const request = pending.get(String(frame.id));
    if (request) {
      pending.delete(String(frame.id));
      if (frame.error) {
        request.reject(new Error(`RPC ${request.method} failed: ${JSON.stringify(frame.error)}`));
      } else {
        request.resolve(frame.result);
      }
    }
    return;
  }

  if (!frame?.method) return;
  notifications.push(frame);
  const params = frame.params || {};
  if (frame.method === 'message/delta') {
    messageDeltas.push(typeof params.text === 'string' ? params.text : '');
  } else if (frame.method === 'agent/updated') {
    agentUpdated.push(params);
  } else if (frame.method === 'agent/output/delta') {
    agentOutputDelta.push(params);
  } else if (frame.method === 'agent/artifact/updated') {
    artifactUpdated.push(params);
  } else if (frame.method === 'turn/completed') {
    turnCompleted = true;
  } else if (frame.method === 'turn/error') {
    turnErrored = params;
  }
});

function rpc(method, params = {}, rpcTimeoutMs = 15_000) {
  const id = `m15-native-${++nextSeq}-${crypto.randomBytes(3).toString('hex')}`;
  const frame = { jsonrpc: '2.0', id, method, params };
  appendJsonl(observedTranscript, { direction: 'client_to_server', frame });
  child.stdin.write(`${JSON.stringify(frame)}\n`);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`RPC timeout for ${method}`));
    }, rpcTimeoutMs);
    pending.set(id, {
      method,
      resolve: (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      reject: (error) => {
        clearTimeout(timer);
        reject(error);
      },
    });
  });
}

async function waitFor(predicate, description) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`Timed out waiting for ${description}`);
}

function agentIdWithPrefix(prefix, agents) {
  return agents.find((agent) => String(agent.agent_id || '').startsWith(prefix))?.agent_id;
}

async function main() {
  await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error('octos serve --stdio did not spawn')),
      10_000,
    );
    child.once('spawn', () => {
      clearTimeout(timer);
      resolve();
    });
    child.once('error', reject);
  });

  const capabilities = await rpc('config/capabilities/list');
  const supported = capabilities?.capabilities?.supported_methods || [];
  for (const method of ['review/start', 'agent/list', 'agent/status/read', 'agent/output/read', 'agent/artifact/list']) {
    assert(supported.includes(method), `missing AppUI method ${method}`);
  }

  const created = await rpc('profile/local/create', {
    name: 'M15 Native Reviewer',
    username: profileId,
    email: `${profileId}@example.test`,
  });
  assert(created?.profile_id === profileId, 'local profile was not created for native soak');

  await rpc(
    'profile/llm/upsert',
    {
      profile_id: profileId,
      set_primary: true,
      selection: {
        family_id: providerFamily,
        model_id: modelId,
        route: {
          route_id: providerFamily,
          api_key_env: providerFamily === 'deepseek' ? 'DEEPSEEK_API_KEY' : `${providerFamily.toUpperCase()}_API_KEY`,
          api_type: 'openai',
        },
      },
      api_key: providerKey,
    },
    30_000,
  );

  await rpc('session/open', {
    session_id: sessionId,
    profile_id: profileId,
    cwd: workspace,
  });

  const turnId = crypto.randomUUID();
  const accepted = await rpc('review/start', {
    session_id: sessionId,
    profile_id: profileId,
    turn_id: turnId,
    target: { type: 'custom', path: workspace },
    prompt:
      'Run a concise code review of review_target.rs. The key risk is whether divide handles count == 0. Use the native specialist swarm and produce a final joined answer.',
    delivery: 'inline',
  });
  assert(accepted?.backend === 'native', 'review/start did not accept native backend');

  await waitFor(() => turnCompleted || turnErrored, 'review/start terminal event');
  assert(!turnErrored, `review/start errored: ${JSON.stringify(turnErrored)}`);
  await waitFor(
    () => agentUpdated.some((params) => String(params?.agent?.backend_kind || '') === 'native'),
    'native agent update',
  );

  const agentList = await rpc('agent/list', { session_id: sessionId, profile_id: profileId });
  const agents = agentList?.agents || [];
  const expectedAgents = [
    ['reviewer-api-', 'native'],
    ['reviewer-tests-', 'native'],
    ['reviewer-policy-', 'native'],
    ['reviewer-cli-', 'cli_process'],
    ['reviewer-mcp-', 'mcp_agent'],
  ];
  const queriedAgents = [];
  for (const [prefix, backendKind] of expectedAgents) {
    const agentId = agentIdWithPrefix(prefix, agents);
    assert(agentId, `agent/list missing ${prefix}*`);
    const status = await rpc('agent/status/read', { agent_id: agentId, session_id: sessionId, profile_id: profileId });
    const output = await rpc('agent/output/read', { agent_id: agentId, session_id: sessionId, profile_id: profileId, limit: 20_000 });
    const artifacts = await rpc('agent/artifact/list', { agent_id: agentId, session_id: sessionId, profile_id: profileId });
    assert(status?.agent?.backend_kind === backendKind, `${agentId} backend_kind ${status?.agent?.backend_kind} != ${backendKind}`);
    assert(status?.agent?.status === 'completed', `${agentId} did not complete: ${status?.agent?.status}`);
    assert(typeof output?.text === 'string' && output.text.trim().length > 0, `${agentId} output is empty`);
    assert((artifacts?.artifacts || []).length > 0, `${agentId} artifact list is empty`);
    queriedAgents.push({
      agentId,
      backendKind,
      status: status.agent.status,
      outputBytes: output.text.length,
      artifactCount: artifacts.artifacts.length,
    });
  }

  const joined = messageDeltas.join('\n');
  assert(/Ada Lovelace|Hypatia|Socrates|Grace Hopper|Marie Curie|Code Review|review/i.test(joined), 'joined review text was not streamed');

  const summary = {
    ok: true,
    runRoot,
    dataDir,
    workspace,
    cliFixture,
    mcpFixture,
    sessionId,
    profileId,
    turnId,
    providerFamily,
    modelId,
    notifications: notifications.length,
    agentUpdated: agentUpdated.length,
    agentOutputDelta: agentOutputDelta.length,
    artifactUpdated: artifactUpdated.length,
    turnCompleted,
    queriedAgents,
    stderrPreview: stderrText.split(/\r?\n/).filter(Boolean).slice(-20),
    host: os.hostname(),
  };
  writeJson(path.join(runRoot, 'm15-native-review-start-summary.json'), summary);
  console.log(JSON.stringify(summary, null, 2));
}

main()
  .catch((error) => {
    const failure = {
      ok: false,
      error: String(error?.stack || error),
      runRoot,
      notifications: notifications.length,
      turnCompleted,
      turnErrored,
      stderrPreview: stderrText.split(/\r?\n/).filter(Boolean).slice(-40),
    };
    writeJson(path.join(runRoot, 'm15-native-review-start-summary.json'), failure);
    console.error(JSON.stringify(failure, null, 2));
    process.exitCode = 1;
  })
  .finally(() => {
    redactGeneratedSecrets();
    try {
      child.stdin.end();
    } catch {
      // ignore cleanup failure
    }
    setTimeout(() => {
      if (!child.killed) child.kill('SIGTERM');
    }, 200);
  });
