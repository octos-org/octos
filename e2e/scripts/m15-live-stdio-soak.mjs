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
  process.env.OCTOS_M15_STDIO_SOAK_DIR || path.join(repoRoot, 'e2e', 'test-results-m15-live-stdio', stamp),
);
const dataDir = path.join(runRoot, 'data');
const workspace = path.join(runRoot, 'workspace');
const evidenceDir = path.join(runRoot, 'evidence');
const octosBin = process.env.OCTOS_BIN || path.join(repoRoot, 'target', 'debug', 'octos');
const sessionId = process.env.OCTOS_M15_STDIO_SESSION || `api:m15-live-stdio-${stamp}`;
const profileId = process.env.OCTOS_M15_STDIO_PROFILE || '_main';
const timeoutMs = Number(process.env.OCTOS_M15_STDIO_TIMEOUT_MS || 45_000);

fs.mkdirSync(workspace, { recursive: true });
fs.mkdirSync(evidenceDir, { recursive: true });

const observedTranscript = path.join(runRoot, 'client-observed-appui-transcript.jsonl');
const serverStderr = path.join(runRoot, 'server-stderr.log');

function appendJsonl(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.appendFileSync(file, `${JSON.stringify(value)}\n`);
}

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

const child = spawn(octosBin, ['serve', '--stdio', '--data-dir', dataDir, '--cwd', workspace], {
  cwd: repoRoot,
  env: {
    ...process.env,
    OCTOS_M15_LIVE_SUBAGENT_FIXTURE: '1',
    OCTOS_TUI_M15_UX_OUTPUT_DIR: evidenceDir,
    OCTOS_TUI_M15_UX_WORKDIR: workspace,
    RUST_BACKTRACE: process.env.RUST_BACKTRACE || '1',
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
let finalMarkerSeen = false;
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

  if (!frame?.method) {
    return;
  }
  notifications.push(frame);
  const params = frame.params || {};
  if (frame.method === 'message/delta') {
    const text = typeof params.text === 'string' ? params.text : '';
    messageDeltas.push(text);
    if (text.includes('M15_CODE_REVIEW_FINAL_LINE') || text.includes('M15CODEREVIEWFINALLINE')) {
      finalMarkerSeen = true;
    }
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

function rpc(method, params = {}) {
  const id = `m15-${++nextSeq}-${crypto.randomBytes(3).toString('hex')}`;
  const frame = { jsonrpc: '2.0', id, method, params };
  appendJsonl(observedTranscript, { direction: 'client_to_server', frame });
  child.stdin.write(`${JSON.stringify(frame)}\n`);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`RPC timeout for ${method}`));
    }, 10_000);
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
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`Timed out waiting for ${description}`);
}

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

async function main() {
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('octos serve --stdio did not become writable')), 10_000);
    child.once('spawn', () => {
      clearTimeout(timer);
      resolve();
    });
    child.once('error', reject);
  });

  const capabilities = await rpc('config/capabilities/list');
  const supported = capabilities?.capabilities?.supported_methods || [];
  for (const method of ['agent/list', 'agent/status/read', 'agent/output/read', 'agent/artifact/list']) {
    assert(supported.includes(method), `missing AppUI method ${method}`);
  }

  await rpc('session/open', { session_id: sessionId });
  const turnId = crypto.randomUUID();
  await rpc('turn/start', {
    session_id: sessionId,
    turn_id: turnId,
    input: [
      {
        kind: 'text',
        text: 'Run M15 code review with live subagent orchestration through octos serve --stdio. Use supervised subagents and produce the final marker.',
      },
    ],
  });

  await waitFor(() => turnCompleted || turnErrored, 'turn terminal event');
  assert(!turnErrored, `turn errored: ${JSON.stringify(turnErrored)}`);
  await waitFor(() => finalMarkerSeen, 'M15 final marker in message/delta');

  const agentList = await rpc('agent/list', { session_id: sessionId, profile_id: profileId });
  const agents = agentList?.agents || [];
  const expectedAgents = ['reviewer-api', 'reviewer-tests', 'reviewer-security'];
  for (const agentId of expectedAgents) {
    assert(agents.some((agent) => agent.agent_id === agentId), `agent/list missing ${agentId}`);
  }

  const queriedAgents = [];
  for (const agentId of expectedAgents) {
    const status = await rpc('agent/status/read', { agent_id: agentId, session_id: sessionId, profile_id: profileId });
    const output = await rpc('agent/output/read', { agent_id: agentId, session_id: sessionId, profile_id: profileId, limit: 20_000 });
    const artifacts = await rpc('agent/artifact/list', { agent_id: agentId, session_id: sessionId, profile_id: profileId });
    assert(status?.agent?.status === 'completed', `${agentId} did not complete`);
    assert(typeof output?.text === 'string' && output.text.includes(agentId), `${agentId} output missing expected text`);
    assert((artifacts?.artifacts || []).length > 0, `${agentId} artifact list is empty`);
    queriedAgents.push({ agentId, status, output, artifacts });
  }

  const statusRead = await rpc('session/status/read', { session_id: sessionId, profile_id: profileId });
  const summary = {
    ok: true,
    runRoot,
    dataDir,
    workspace,
    evidenceDir,
    sessionId,
    turnId,
    notifications: notifications.length,
    agentUpdated: agentUpdated.length,
    agentOutputDelta: agentOutputDelta.length,
    artifactUpdated: artifactUpdated.length,
    finalMarkerSeen,
    turnCompleted,
    queriedAgents: queriedAgents.map(({ agentId, status, output, artifacts }) => ({
      agentId,
      status: status.agent.status,
      outputBytes: output.text.length,
      artifactCount: artifacts.artifacts.length,
    })),
    statusRead: {
      profile_id: statusRead?.profile_id,
      runtime_policy_stamp: statusRead?.runtime_policy_stamp,
      context_state: statusRead?.context_state,
    },
    evidenceFiles: fs.existsSync(evidenceDir)
      ? fs.readdirSync(evidenceDir).sort()
      : [],
    stderrPreview: stderrText.split(/\r?\n/).filter(Boolean).slice(-20),
    host: os.hostname(),
  };
  writeJson(path.join(runRoot, 'm15-live-stdio-summary.json'), summary);
  console.log(JSON.stringify(summary, null, 2));
}

main()
  .catch((error) => {
    const failure = {
      ok: false,
      error: String(error?.stack || error),
      runRoot,
      evidenceDir,
      notifications: notifications.length,
      turnCompleted,
      turnErrored,
      finalMarkerSeen,
      stderrPreview: stderrText.split(/\r?\n/).filter(Boolean).slice(-40),
    };
    writeJson(path.join(runRoot, 'm15-live-stdio-summary.json'), failure);
    console.error(JSON.stringify(failure, null, 2));
    process.exitCode = 1;
  })
  .finally(() => {
    try {
      child.stdin.end();
    } catch {
      // ignore cleanup failure
    }
    setTimeout(() => {
      if (!child.killed) child.kill('SIGTERM');
    }, 200);
  });
