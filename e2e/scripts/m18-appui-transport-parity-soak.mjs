#!/usr/bin/env node

import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import WebSocket from 'ws';

const repoRoot = path.resolve(import.meta.dirname, '..', '..');
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..+/, 'Z');
const cliArgs = process.argv.slice(2);
const codexP0Soak = process.env.OCTOS_M18_CODEX_P0_SOAK === '1';
const modeArgIndex = cliArgs.findIndex((arg) => arg === '--mode');
const scenarioMode = process.env.OCTOS_M18_APPUI_PARITY_MODE
  || (cliArgs.includes('--backing-store') ? 'backing-store' : null)
  || (modeArgIndex >= 0 ? cliArgs[modeArgIndex + 1] : null)
  || 'headless';
if (!['headless', 'backing-store'].includes(scenarioMode)) {
  throw new Error(`Unsupported M18 AppUI parity mode: ${scenarioMode}`);
}
const runRoot = path.resolve(
  process.env.OCTOS_M18_APPUI_PARITY_DIR
    || path.join(
      repoRoot,
      'e2e',
      codexP0Soak ? 'test-results-m14-codex-tool-parity' : 'test-results-m18-appui-transport-parity',
      stamp,
    ),
);
const workspace = path.join(runRoot, 'workspace');
const wsDataDir = path.join(runRoot, 'data', 'ws');
const stdioDataDir = path.join(runRoot, 'data', 'stdio');
const serverLog = path.join(runRoot, 'server.log');
const wsTranscript = path.join(runRoot, 'appui-ws-transcript.jsonl');
const stdioTranscript = path.join(runRoot, 'appui-stdio-transcript.jsonl');
const diffPath = path.join(runRoot, 'normalized-diff.json');
const runtimePolicyStampPath = path.join(runRoot, 'runtime-policy-stamp.json');
const backingStoreSeedPath = path.join(runRoot, 'backing-store-seed.json');
const toolContractPath = path.join(runRoot, 'tool-contract.json');
const toolRegistrySnapshotPath = path.join(runRoot, 'tool-registry-snapshot.json');
const approvalEventsPath = path.join(runRoot, 'approval-events.jsonl');
const taskLedgerPath = path.join(runRoot, 'task-ledger.jsonl');
const tuiCapturePath = path.join(runRoot, 'tui-capture.txt');
const routeInventoryPath = path.join(repoRoot, 'e2e', 'fixtures', 'appui-conformance', 'm18-route-inventory.json');
const allowlistPath = path.join(repoRoot, 'e2e', 'fixtures', 'appui-conformance', 'm18-conformance-allowlist.json');
const octosBin = process.env.OCTOS_BIN || path.join(repoRoot, 'target', 'debug', 'octos');
const authToken = process.env.OCTOS_M18_APPUI_AUTH_TOKEN || `m18-${crypto.randomBytes(8).toString('hex')}`;
const profileId = process.env.OCTOS_M18_APPUI_PROFILE || 'm18-parity';
const sessionId = process.env.OCTOS_M18_APPUI_SESSION || `${profileId}:local:appui-parity-${stamp}`;
const timeoutMs = Number(process.env.OCTOS_M18_APPUI_TIMEOUT_MS || 45_000);
const wsUiFeatures = [
  'approval.typed.v1',
  'pane.snapshots.v1',
  'session.workspace_cwd.v1',
  'harness.task_control.v1',
  'harness.task_artifacts.v1',
  'state.session_hydrate.v1',
  'state.thread_graph.v1',
  'state.turn_state_get.v1',
  'event.message_persisted.v1',
  'event.spawn_complete.v1',
  'projection.envelope.v1',
  'auxiliary.rest_to_ws.v1',
  'coding.autonomy.v1',
  'coding.agent_control.v1',
  'coding.goal_runtime.v1',
  'coding.loop_runtime.v1',
  'review.start.v1',
  'context.lifecycle.v1',
];

fs.mkdirSync(workspace, { recursive: true });
fs.mkdirSync(wsDataDir, { recursive: true });
fs.mkdirSync(stdioDataDir, { recursive: true });
fs.writeFileSync(path.join(workspace, 'm18_appui_parity_fixture.txt'), 'M18 AppUI parity workspace fixture.\n');
for (const artifact of [
  serverLog,
  wsTranscript,
  stdioTranscript,
  diffPath,
  runtimePolicyStampPath,
  backingStoreSeedPath,
  toolContractPath,
  toolRegistrySnapshotPath,
  approvalEventsPath,
  taskLedgerPath,
  tuiCapturePath,
]) {
  fs.closeSync(fs.openSync(artifact, 'a'));
}

const requiredScenarioMethods = [
  'client_hello',
  'config/capabilities/list',
  'profile/local/create',
  'session/open',
  'session/status/read',
  'session/snapshot',
  'session/messages_page',
  'turn/start',
  'turn/interrupt',
  'approval/respond',
  'auth/status',
  'auth/me',
  'content/list',
  'content/delete',
  'router/set_mode',
  'router/get_metrics',
];
const liveNotificationMethods = new Set([
  'turn/started',
  'turn/completed',
  'turn/error',
  'message/delta',
  'message/persisted',
  'tool/started',
  'tool/progress',
  'tool/completed',
  'approval/requested',
  'approval/auto_resolved',
  'approval/decided',
  'approval/cancelled',
  'task/output/delta',
  'progress/updated',
  'protocol/replay_lossy',
  'turn/spawn_complete',
]);
const codexP0RequiredTools = [
  'apply_patch',
  'exec_command',
  'write_stdin',
  'update_plan',
  'request_user_input',
  'spawn_agent',
  'send_input',
  'resume_agent',
  'wait_agent',
  'close_agent',
];
const stdioAuthUnavailableMethods = new Set([
  'auth/me',
  'auth/logout',
  'content/bulk_delete',
  'content/delete',
  'content/list',
]);
const authContextProbeNames = new Set([
  'authMePreOpen',
  'authLogoutAuthProbe',
  'contentBulkDeleteAuthProbe',
  'contentDeleteInvalidParams',
  'contentDeleteAuthProbe',
  'contentListAuthProbe',
]);
const stdioAuthUnavailableShape = {
  code: -32120,
  kind: 'auth_unavailable',
  recoverable: true,
  recovery: 'authenticate before calling this method',
};

function unsupportedCapabilityMethods(capabilities) {
  return new Set((capabilities?.unsupported || []).map((entry) => entry.method));
}

function methodIsUnavailableOverStdioAuth(method) {
  return stdioAuthUnavailableMethods.has(method);
}

function appendText(file, text) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.appendFileSync(file, text);
}

function appendJsonl(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.appendFileSync(file, `${JSON.stringify({ ...value, ts: new Date().toISOString() })}\n`);
}

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function encodePathComponent(value) {
  let encoded = '';
  for (const byte of Buffer.from(value, 'utf8')) {
    const isDigit = byte >= 0x30 && byte <= 0x39;
    const isUpper = byte >= 0x41 && byte <= 0x5a;
    const isLower = byte >= 0x61 && byte <= 0x7a;
    if (isDigit || isUpper || isLower || byte === 0x2d || byte === 0x5f) {
      encoded += String.fromCharCode(byte);
    } else {
      encoded += `%${byte.toString(16).toUpperCase().padStart(2, '0')}`;
    }
  }
  return encoded;
}

function sessionBaseKey(sessionKey) {
  return String(sessionKey).split('#')[0];
}

function sessionTopic(sessionKey) {
  const index = String(sessionKey).indexOf('#');
  return index >= 0 ? String(sessionKey).slice(index + 1) : 'default';
}

function backingStoreCandidateKeys() {
  return [
    `${profileId}:api:${sessionId}`,
    `_main:api:${sessionId}`,
    `api:${sessionId}`,
  ];
}

function backingStoreSeedMessages() {
  const messages = [];
  for (let i = 0; i < 12; i += 1) {
    const pair = Math.floor(i / 2);
    const role = i % 2 === 0 ? 'user' : 'assistant';
    const threadId = `m18-backing-store-thread-${pair.toString().padStart(2, '0')}`;
    const timestamp = new Date(Date.UTC(2026, 4, 18, 12, 0, i)).toISOString();
    messages.push({
      role,
      content: `m18 backing-store seed message ${i.toString().padStart(2, '0')} ${role}`,
      media: [],
      tool_calls: null,
      tool_call_id: null,
      reasoning_content: null,
      client_message_id: role === 'user' ? threadId : null,
      thread_id: threadId,
      timestamp,
    });
  }
  return messages;
}

function writeSessionJsonl(dataDir, sessionKey, messages) {
  const baseKey = sessionBaseKey(sessionKey);
  const topic = sessionTopic(sessionKey);
  const sessionDir = path.join(dataDir, 'users', encodePathComponent(baseKey), 'sessions');
  fs.mkdirSync(sessionDir, { recursive: true });
  const sessionFile = path.join(sessionDir, `${encodePathComponent(topic)}.jsonl`);
  const meta = {
    schema_version: 1,
    session_key: sessionKey,
    parent_key: null,
    topic: topic === 'default' ? null : topic,
    summary: null,
    title: 'M18 backing-store replay parity seed',
    title_manual: false,
    child_contracts: [],
    created_at: '2026-05-18T12:00:00.000Z',
    updated_at: '2026-05-18T12:00:11.000Z',
  };
  const lines = [meta, ...messages].map((entry) => JSON.stringify(entry));
  fs.writeFileSync(sessionFile, `${lines.join('\n')}\n`);
  return sessionFile;
}

function seedBackingStores() {
  const messages = backingStoreSeedMessages();
  const keys = backingStoreCandidateKeys();
  const seeded = [];
  for (const [transport, dataDir] of [['websocket', wsDataDir], ['stdio', stdioDataDir]]) {
    for (const key of keys) {
      seeded.push({
        transport,
        key,
        file: writeSessionJsonl(dataDir, key, messages),
        message_count: messages.length,
      });
    }
  }
  return {
    schema: 'octos-m18-backing-store-seed-v1',
    issue: 'octos#1044',
    sessionId,
    profileId,
    mode: scenarioMode,
    message_count: messages.length,
    seeded,
  };
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

async function getFreePort() {
  const server = net.createServer();
  await new Promise((resolve, reject) => {
    server.listen(0, '127.0.0.1', resolve);
    server.once('error', reject);
  });
  const { port } = server.address();
  await new Promise((resolve) => server.close(resolve));
  return port;
}

function parseJsonl(file) {
  if (!fs.existsSync(file)) return [];
  return fs.readFileSync(file, 'utf8')
    .split(/\r?\n/)
    .filter(Boolean)
    .map((line) => JSON.parse(line));
}

function wait(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

class RpcFailure extends Error {
  constructor(message, error) {
    super(message);
    this.name = 'RpcFailure';
    this.error = error;
  }
}

class TransportFailure extends Error {
  constructor(message, details = {}) {
    super(message);
    this.name = 'TransportFailure';
    this.details = details;
  }
}

class AppUiClient {
  constructor(label, transcript, resetTranscript = true) {
    this.label = label;
    this.transcript = transcript;
    this.pending = new Map();
    this.notifications = [];
    this.nextSeq = 0;
    this.closed = false;
    if (resetTranscript) fs.rmSync(transcript, { force: true });
  }

  nextId() {
    this.nextSeq += 1;
    return `${this.label}-${this.nextSeq}-${crypto.randomBytes(3).toString('hex')}`;
  }

  observeFrame(direction, frame) {
    const pending = direction === 'server_to_client'
      && frame
      && Object.prototype.hasOwnProperty.call(frame, 'id')
      && frame.id != null
      ? this.pending.get(String(frame.id))
      : null;
    const transcriptEntry = { direction, frame };
    if (pending?.parity === false) transcriptEntry.parity = false;
    if (pending?.probeName) transcriptEntry.probe = pending.probeName;
    appendJsonl(this.transcript, transcriptEntry);
    if (direction !== 'server_to_client') return;
    if (frame && Object.prototype.hasOwnProperty.call(frame, 'id') && frame.id != null) {
      if (!pending) return;
      this.pending.delete(String(frame.id));
      if (frame.error) {
        pending.reject(new RpcFailure(`${this.label}: RPC ${pending.method} failed`, frame.error));
      } else {
        pending.resolve(frame.result);
      }
      return;
    }
    if (frame?.method) this.notifications.push(frame);
  }

  rejectAllPending(error) {
    for (const [, pending] of this.pending) {
      pending.reject(error);
    }
    this.pending.clear();
  }

  request(method, params = {}, rpcTimeoutMs = 15_000, options = {}) {
    const id = this.nextId();
    const frame = { jsonrpc: '2.0', id, method, params };
    const transcriptEntry = { direction: 'client_to_server', frame };
    if (options.parity === false) transcriptEntry.parity = false;
    if (options.probeName) transcriptEntry.probe = options.probeName;
    appendJsonl(this.transcript, transcriptEntry);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${this.label}: RPC timeout for ${method}`));
      }, rpcTimeoutMs);
      this.pending.set(id, {
        method,
        parity: options.parity,
        probeName: options.probeName,
        resolve: (value) => {
          clearTimeout(timer);
          resolve(value);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
      });
      try {
        if (this.closed) {
          throw new TransportFailure(`${this.label}: transport is closed before ${method}`);
        }
        this.sendFrame(frame);
      } catch (error) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(error);
      }
    });
  }

  async requestCapture(method, params = {}, rpcTimeoutMs = 15_000, options = {}) {
    try {
      const result = await this.request(method, params, rpcTimeoutMs, options);
      return { ok: true, method, result };
    } catch (error) {
      if (error instanceof RpcFailure) {
        return { ok: false, method, error: error.error };
      }
      if (error instanceof TransportFailure) {
        return {
          ok: false,
          method,
          transportError: {
            kind: 'transport_closed',
            message: error.message,
            details: error.details,
          },
        };
      }
      throw error;
    }
  }

  waitForNotification(method, predicate = () => true, waitMs = timeoutMs) {
    const existing = this.notifications.find((frame) => frame.method === method && predicate(frame));
    if (existing) return Promise.resolve(existing);
    const deadline = Date.now() + waitMs;
    return new Promise((resolve, reject) => {
      const tick = () => {
        const found = this.notifications.find((frame) => frame.method === method && predicate(frame));
        if (found) {
          resolve(found);
          return;
        }
        if (Date.now() > deadline) {
          reject(new Error(`${this.label}: timed out waiting for ${method}`));
          return;
        }
        setTimeout(tick, 50);
      };
      tick();
    });
  }
}

class WsClient extends AppUiClient {
  constructor(url) {
    super('ws', wsTranscript);
    const featureQuery = wsUiFeatures
      .map((feature) => `ui_feature=${encodeURIComponent(feature)}`)
      .join('&');
    this.url = url
      .replace(/^http:/, 'ws:')
      .replace(/^https:/, 'wss:')
      .replace(/\/$/, '')
      .concat(`/api/ui-protocol/ws?${featureQuery}`);
    this.ws = null;
  }

  async connect() {
    this.closed = false;
    this.ws = new WebSocket(this.url, {
      headers: {
        Authorization: `Bearer ${authToken}`,
        'X-Profile-Id': profileId,
        'X-Octos-Ui-Features': wsUiFeatures.join(','),
      },
    });
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`ws: connect timeout to ${this.url}`)), 10_000);
      this.ws.once('open', () => {
        clearTimeout(timer);
        resolve();
      });
      this.ws.once('error', (error) => {
        clearTimeout(timer);
        reject(error);
      });
      this.ws.on('message', (data) => {
        let frame;
        try {
          frame = JSON.parse(data.toString());
        } catch {
          appendJsonl(this.transcript, { direction: 'server_to_client_non_json', line: data.toString() });
          return;
        }
        this.observeFrame('server_to_client', frame);
      });
      this.ws.on('close', () => {
        this.closed = true;
        this.rejectAllPending(new TransportFailure('ws: closed before response'));
      });
    });
  }

  sendFrame(frame) {
    this.ws.send(JSON.stringify(frame));
  }

  async close() {
    if (!this.ws || this.closed) return;
    await new Promise((resolve) => {
      this.ws.once('close', resolve);
      this.ws.close();
      setTimeout(resolve, 1000);
    });
  }

  async reconnect() {
    await this.close();
    this.closed = false;
    await this.connect();
    await this.request('session/open', {
      session_id: sessionId,
      profile_id: profileId,
    });
    return this;
  }
}

class StdioClient extends AppUiClient {
  constructor(resetTranscript = true) {
    super('stdio', stdioTranscript, resetTranscript);
    this.stderrText = '';
    this.child = spawn(octosBin, ['serve', '--stdio', '--data-dir', stdioDataDir, '--cwd', workspace], {
      cwd: repoRoot,
      env: {
        ...process.env,
        OCTOS_M9_PROTOCOL_FIXTURES: '1',
        OCTOS_TUI_M15_UX_OUTPUT_DIR: runRoot,
        OCTOS_TUI_M15_UX_WORKDIR: workspace,
        RUST_BACKTRACE: process.env.RUST_BACKTRACE || '1',
      },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    this.child.stderr.on('data', (chunk) => {
      const text = chunk.toString();
      this.stderrText += text;
      appendText(serverLog, `[stdio stderr] ${text}`);
    });
    this.child.once('error', (error) => {
      this.closed = true;
      appendText(serverLog, `[stdio error] ${String(error?.stack || error)}\n`);
      this.rejectAllPending(new TransportFailure('stdio: child process error', {
        error: String(error?.message || error),
      }));
    });
    this.child.once('exit', (code, signal) => {
      this.closed = true;
      appendText(serverLog, `[stdio exit] code=${code} signal=${signal}\n`);
      this.rejectAllPending(new TransportFailure('stdio: closed before response', {
        code,
        signal,
      }));
    });
    const rl = readline.createInterface({ input: this.child.stdout });
    this.rl = rl;
    rl.on('line', (line) => {
      let frame;
      try {
        frame = JSON.parse(line);
      } catch {
        appendJsonl(this.transcript, { direction: 'server_to_client_non_json', line });
        appendText(serverLog, `[stdio stdout] ${line}\n`);
        return;
      }
      this.observeFrame('server_to_client', frame);
    });
  }

  async waitSpawn() {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('stdio: octos serve --stdio did not spawn')), 10_000);
      this.child.once('spawn', () => {
        clearTimeout(timer);
        resolve();
      });
      this.child.once('error', (error) => {
        clearTimeout(timer);
        reject(error);
      });
    });
  }

  sendFrame(frame) {
    if (this.child.stdin.destroyed) {
      throw new TransportFailure('stdio: stdin is closed');
    }
    this.child.stdin.write(`${JSON.stringify(frame)}\n`);
  }

  async close() {
    if (this.closed) return;
    this.closed = true;
    try {
      this.child.stdin.end();
    } catch {
      // ignore cleanup failure
    }
    if (this.rl) this.rl.close();
    if (this.child.exitCode !== null || this.child.signalCode !== null) return;
    if (!this.child.killed) this.child.kill('SIGTERM');
    await new Promise((resolve) => {
      const timer = setTimeout(() => {
        if (!this.child.killed) this.child.kill('SIGKILL');
        resolve();
      }, 2000);
      this.child.once('exit', () => {
        clearTimeout(timer);
        resolve();
      });
    });
  }

  async reconnect() {
    await this.close();
    const next = new StdioClient(false);
    await next.waitSpawn();
    await next.request('session/open', {
      session_id: sessionId,
      profile_id: profileId,
    });
    return next;
  }
}

async function startWsServer(port) {
  const child = spawn(octosBin, [
    'serve',
    '--host',
    '127.0.0.1',
    '--port',
    String(port),
    '--auth-token',
    authToken,
    '--data-dir',
    wsDataDir,
    '--cwd',
    workspace,
  ], {
    cwd: repoRoot,
    env: {
      ...process.env,
      OCTOS_M9_PROTOCOL_FIXTURES: '1',
      OCTOS_TUI_M15_UX_OUTPUT_DIR: runRoot,
      OCTOS_TUI_M15_UX_WORKDIR: workspace,
      RUST_BACKTRACE: process.env.RUST_BACKTRACE || '1',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  child.stdout.on('data', (chunk) => appendText(serverLog, `[ws stdout] ${chunk.toString()}`));
  child.stderr.on('data', (chunk) => appendText(serverLog, `[ws stderr] ${chunk.toString()}`));
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('ws: octos serve did not spawn')), 10_000);
    child.once('spawn', () => {
      clearTimeout(timer);
      resolve();
    });
    child.once('error', (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
  return child;
}

async function waitForHttp(url) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    try {
      await new Promise((resolve, reject) => {
        const req = http.get(url, { timeout: 1000 }, (res) => {
          res.resume();
          resolve();
        });
        req.on('error', reject);
        req.on('timeout', () => {
          req.destroy(new Error('timeout'));
        });
      });
      return;
    } catch {
      await wait(100);
    }
  }
  throw new Error(`timed out waiting for ${url}`);
}

async function stopProcess(child) {
  if (!child || child.killed) return;
  child.kill('SIGTERM');
  await new Promise((resolve) => {
    const timer = setTimeout(() => {
      if (!child.killed) child.kill('SIGKILL');
      resolve();
    }, 2000);
    child.once('exit', () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

function hasNotification(notifications, method) {
  return notifications.some((frame) => frame.method === method);
}

function summarizeLiveNotifications(notifications) {
  const methods = notifications.map((frame) => frame.method).filter(Boolean);
  return {
    methods: sortedSet(methods),
    turn_started: hasNotification(notifications, 'turn/started'),
    turn_completed: hasNotification(notifications, 'turn/completed'),
    turn_error: hasNotification(notifications, 'turn/error'),
    message_delta_observed: hasNotification(notifications, 'message/delta'),
    message_persisted: hasNotification(notifications, 'message/persisted'),
    tool_started: hasNotification(notifications, 'tool/started'),
    tool_completed: hasNotification(notifications, 'tool/completed'),
    approval_requested: hasNotification(notifications, 'approval/requested'),
    approval_decided: hasNotification(notifications, 'approval/decided'),
    approval_cancelled: hasNotification(notifications, 'approval/cancelled'),
    replay_lossy: hasNotification(notifications, 'protocol/replay_lossy'),
  };
}

function validateCodexToolStatus(label, capture) {
  assert(capture?.ok === true, `${label}: tool/status/list failed for #969`);
  const contract = capture.result?.coding_tool_contract;
  assert(contract?.status === 'ready', `${label}: coding tool contract not ready`);
  assert(
    Array.isArray(contract?.missing_required_tools) && contract.missing_required_tools.length === 0,
    `${label}: missing required Codex P0 tools: ${(contract?.missing_required_tools || []).join(', ')}`,
  );
  const required = new Map((contract?.required_tools || []).map((entry) => [entry.name, entry]));
  for (const tool of codexP0RequiredTools) {
    assert(required.has(tool), `${label}: contract missing required tool entry ${tool}`);
    assert(
      ['available', 'aliased'].includes(required.get(tool)?.status),
      `${label}: required tool ${tool} not available, status=${required.get(tool)?.status}`,
    );
  }
}

function codexToolCompletions(notifications) {
  return notifications
    .filter((frame) => frame.method === 'tool/completed' && codexP0RequiredTools.includes(frame.params?.tool_name))
    .map((frame) => ({
      tool_name: frame.params.tool_name,
      success: frame.params.success,
      output_preview: frame.params.output_preview || '',
    }));
}

async function runCodexP0Turn(client) {
  const turnId = crypto.randomUUID();
  const before = client.notifications.length;
  const accepted = await client.request('turn/start', {
    session_id: sessionId,
    profile_id: profileId,
    turn_id: turnId,
    input: [{
      kind: 'text',
      text: 'M14 Codex P0 tool parity fixture for #969: exercise update_plan, request_user_input, exec_command, write_stdin, apply_patch, spawn_agent, send_input, wait_agent, close_agent, and resume_agent.',
    }],
  });
  assert(accepted?.accepted === true, `${client.label}: #969 turn/start not accepted`);

  const terminal = await Promise.race([
    client.waitForNotification('turn/completed', (frame) => frame.params?.turn_id === turnId),
    client.waitForNotification('turn/error', (frame) => frame.params?.turn_id === turnId),
  ]);
  const notifications = client.notifications.slice(before);
  if (terminal.method === 'turn/error') {
    throw new Error(`${client.label}: #969 Codex P0 fixture errored: ${terminal.params?.code} ${terminal.params?.message}`);
  }

  const startedTools = new Set(notifications
    .filter((frame) => frame.method === 'tool/started')
    .map((frame) => frame.params?.tool_name));
  const completions = codexToolCompletions(notifications);
  const successfulTools = new Set(completions
    .filter((entry) => entry.success === true)
    .map((entry) => entry.tool_name));
  for (const tool of codexP0RequiredTools) {
    assert(startedTools.has(tool), `${client.label}: #969 missing tool/started for ${tool}`);
    assert(successfulTools.has(tool), `${client.label}: #969 missing successful tool/completed for ${tool}`);
  }
  const deniedPolicy = completions.find(
    (entry) => entry.tool_name === 'exec_command'
      && entry.success === false
      && entry.output_preview.includes('Command denied by security policy'),
  );
  assert(deniedPolicy, `${client.label}: #969 missing typed policy-denied exec_command evidence`);

  return {
    turnId,
    terminal: terminal.method,
    observed: summarizeLiveNotifications(notifications),
    toolCompletions: completions,
  };
}

async function runTurn(client, text, expect = {}) {
  const turnId = crypto.randomUUID();
  const before = client.notifications.length;
  const accepted = await client.request('turn/start', {
    session_id: sessionId,
    profile_id: profileId,
    turn_id: turnId,
    input: [{ kind: 'text', text }],
  });
  assert(accepted?.accepted === true, `${client.label}: turn/start not accepted`);

  let approvalResponse = null;
  if (expect.approval) {
    const requested = await client.waitForNotification(
      'approval/requested',
      (frame) => frame.params?.turn_id === turnId,
    );
    approvalResponse = await client.request('approval/respond', {
      session_id: sessionId,
      approval_id: requested.params.approval_id,
      decision: 'approve',
    });
    assert(approvalResponse?.accepted === true, `${client.label}: approval/respond not accepted`);
  }

  const terminal = await Promise.race([
    client.waitForNotification('turn/completed', (frame) => frame.params?.turn_id === turnId),
    client.waitForNotification('turn/error', (frame) => frame.params?.turn_id === turnId),
  ]);
  const notifications = client.notifications.slice(before);
  if (expect.tool) {
    assert(
      notifications.some((frame) => frame.method === 'tool/started' && frame.params?.turn_id === turnId),
      `${client.label}: tool/started missing`,
    );
    assert(
      notifications.some((frame) => frame.method === 'tool/completed' && frame.params?.turn_id === turnId),
      `${client.label}: tool/completed missing`,
    );
  }
  if (expect.approval) {
    assert(
      notifications.some((frame) => frame.method === 'approval/requested' && frame.params?.turn_id === turnId),
      `${client.label}: approval/requested missing`,
    );
  }
  return {
    turnId,
    terminal: terminal.method,
    approvalResponse: approvalResponse ? summarizeResult('approval/respond', approvalResponse) : null,
    observed: summarizeLiveNotifications(notifications),
  };
}

async function runInterrupt(client) {
  const turnId = crypto.randomUUID();
  const before = client.notifications.length;
  await client.request('turn/start', {
    session_id: sessionId,
    profile_id: profileId,
    turn_id: turnId,
    input: [{ kind: 'text', text: 'Write 200 separate lines, one line at a time for the M18 interrupt fixture.' }],
  });
  await client.waitForNotification('message/delta', (frame) => frame.params?.turn_id === turnId);
  const interrupted = await client.requestCapture('turn/interrupt', {
    session_id: sessionId,
    turn_id: turnId,
  });
  const terminal = await Promise.race([
    client.waitForNotification('turn/completed', (frame) => frame.params?.turn_id === turnId),
    client.waitForNotification('turn/error', (frame) => frame.params?.turn_id === turnId),
  ]);
  const notifications = client.notifications.slice(before);
  return {
    turnId,
    interrupted: summarizeCapture(interrupted),
    terminal: terminal.method,
    observed: summarizeLiveNotifications(notifications),
  };
}

async function runReplayProbe(client) {
  const turnId = crypto.randomUUID();
  const before = client.notifications.length;
  const accepted = await client.request('turn/start', {
    session_id: sessionId,
    profile_id: profileId,
    turn_id: turnId,
    input: [{ kind: 'text', text: 'M9 replay-lossy fixture for M18 reconnect-style replay.' }],
  });
  assert(accepted?.accepted === true, `${client.label}: replay turn/start not accepted`);
  const replayLossy = await client.waitForNotification(
    'protocol/replay_lossy',
    (frame) => frame.params?.session_id === sessionId,
  );
  const terminal = await Promise.race([
    client.waitForNotification('turn/completed', (frame) => frame.params?.turn_id === turnId),
    client.waitForNotification('turn/error', (frame) => frame.params?.turn_id === turnId),
  ]);
  const notifications = client.notifications.slice(before);
  return {
    turnId,
    terminal: terminal.method,
    replayLossy: {
      dropped_count: replayLossy.params?.dropped_count,
      has_last_durable_cursor: Boolean(replayLossy.params?.last_durable_cursor),
    },
    observed: summarizeLiveNotifications(notifications),
  };
}

async function captureProbe(client, probes, name, method, params = {}, rpcTimeoutMs = 15_000, options = {}) {
  const capture = await client.requestCapture(method, params, rpcTimeoutMs, { ...options, probeName: name });
  probes[name] = summarizeCapture(capture);
  return capture;
}

function assertStdioAuthUnavailableDirect(client, method, capture) {
  if (client.label !== 'stdio') return;
  const error = capture?.error || {};
  const data = error.data || {};
  assert(capture?.ok === false, client.label + ": " + method + " direct call should fail");
  assert(
    error.code === stdioAuthUnavailableShape.code,
    client.label + ": " + method + " expected -32120, got " + error.code,
  );
  assert(data.kind === stdioAuthUnavailableShape.kind, client.label + ": " + method + " expected auth_unavailable");
  assert(data.recoverable === true, client.label + ": " + method + " expected recoverable auth error");
}

function routeProbeParams(method) {
  const missingId = `m18-missing-${method.replaceAll('/', '-').replaceAll('.', '-')}-${stamp}`;
  const missingTurnId = crypto.randomUUID();
  const missingTaskId = crypto.randomUUID();
  const missingPreviewId = crypto.randomUUID();
  const missingAgentId = `m18-missing-agent-${stamp}`;
  const missingLoopId = `m18-missing-loop-${stamp}`;
  switch (method) {
    case 'client_hello':
    case 'config/capabilities/list':
    case 'profile/local/create':
    case 'session/open':
    case 'session/status/read':
    case 'session/snapshot':
    case 'session/messages_page':
    case 'turn/start':
    case 'turn/interrupt':
    case 'approval/respond':
    case 'auth/status':
    case 'auth/me':
    case 'content/list':
    case 'content/delete':
    case 'router/set_mode':
    case 'router/get_metrics':
      return null;
    case 'approval/scopes/list':
    case 'permission/profile/list':
    case 'task/list':
    case 'session/hydrate':
    case 'thread/graph/get':
    case 'session/goal/get':
    case 'session/goal/clear':
    case 'loop/list':
    case 'session/status.get':
    case 'session/files.list':
    case 'session/tasks.list':
    case 'session/workspace.get':
    case 'session/delete':
      return { session_id: sessionId };
    case 'permission/profile/set':
      return { session_id: sessionId, update: { mode: 'workspace_write', network: 'deny' } };
    case 'diff/preview/get':
      return { session_id: sessionId, preview_id: missingPreviewId };
    case 'task/cancel':
      return { session_id: sessionId, task_id: missingTaskId };
    case 'task/restart_from_node':
      return { session_id: sessionId, task_id: missingTaskId, node_id: 'design' };
    case 'task/output/read':
      return { session_id: sessionId, task_id: missingTaskId };
    case 'task/artifact/list':
      return { session_id: sessionId, task_id: missingTaskId };
    case 'task/artifact/read':
      return { session_id: sessionId, task_id: missingTaskId, artifact_id: missingId };
    case 'turn/state/get':
      return { session_id: sessionId, turn_id: missingTurnId };
    case 'agent/list':
      return { session_id: sessionId };
    case 'agent/status/read':
    case 'agent/artifact/list':
    case 'agent/interrupt':
    case 'agent/close':
      return { session_id: sessionId, agent_id: missingAgentId };
    case 'agent/output/read':
      return { session_id: sessionId, agent_id: missingAgentId, limit: 1 };
    case 'agent/artifact/read':
      return { session_id: sessionId, agent_id: missingAgentId, artifact_id: missingId, path: 'missing.txt' };
    case 'session/goal/set':
      return { session_id: sessionId, objective: 'M18 parity route probe', status: 'active', token_budget: 256 };
    case 'loop/create':
      return { session_id: sessionId, prompt: 'M18 parity loop probe', interval_seconds: 3600, mode: 'manual' };
    case 'loop/delete':
    case 'loop/pause':
    case 'loop/resume':
    case 'loop/fire_now':
      return { session_id: sessionId, loop_id: missingLoopId };
    case 'review/start':
      return {};
    case 'session/list':
    case 'system/status.get':
    case 'profile/llm/catalog':
    case 'profile/llm/fetch_models':
    case 'auth/send_code':
    case 'auth/verify':
      return {};
    case 'auth/logout':
      return null;
    case 'session/title.set':
      return { session_id: sessionId, title: 'M18 parity route probe' };
    case 'content/bulk_delete':
      return null;
    case 'profile/llm/list':
    case 'profile/llm/select':
    case 'mcp/status/list':
    case 'tool/status/list':
      return { session_id: sessionId, profile_id: profileId };
    case 'profile/llm/upsert':
    case 'profile/llm/delete':
    case 'profile/llm/test':
      return { profile_id: profileId };
    case 'profile/skills/list':
    case 'profile/skills/registry/search':
      return { profile_id: profileId };
    case 'profile/skills/install':
      return { profile_id: profileId, name: 'm18-missing-skill' };
    case 'profile/skills/remove':
      return { profile_id: profileId, name: 'm18-missing-skill' };
    default:
      throw new Error(`missing route probe params for ${method}`);
  }
}

async function captureRouteInventoryProbes(client, routeInventory, probes) {
  for (const method of routeInventoryMethods(routeInventory)) {
    const params = routeProbeParams(method);
    if (params == null) continue;
    await captureProbe(client, probes, `route:${method}`, method, params, 5000);
  }
}

function messagesFromCapture(capture) {
  return Array.isArray(capture?.result?.messages) ? capture.result.messages : [];
}

function assertMessagesPageUnavailable(label, capture) {
  assert(capture?.ok === false, `${label}: expected unavailable session/messages_page error`);
  assert(capture.error?.code === -32140, `${label}: expected -32140, got ${capture.error?.code}`);
  assert(
    capture.error?.data?.rest_status === 503,
    `${label}: expected REST 503 in typed error data`,
  );
}

function assertMessagesPage(label, capture, expected) {
  assert(capture?.ok === true, `${label}: expected populated session/messages_page result`);
  const messages = messagesFromCapture(capture);
  assert(messages.length === expected.count, `${label}: expected ${expected.count} messages, got ${messages.length}`);
  assert(capture.result?.has_more === expected.hasMore, `${label}: has_more mismatch`);
  assert(capture.result?.next_offset === expected.nextOffset, `${label}: next_offset mismatch`);
  if (expected.firstContent) {
    assert(
      messages[0]?.content === expected.firstContent,
      `${label}: first message content mismatch`,
    );
  }
  if (expected.contentPrefix) {
    assert(
      messages.every((message) => String(message.content || '').startsWith(expected.contentPrefix)),
      `${label}: page contains a message outside the expected backing-store seed`,
    );
  }
}

function assertTypedInvalidParams(label, capture) {
  assert(capture?.ok === false, `${label}: expected typed invalid params error`);
  assert(capture.error?.code === -32602, `${label}: expected -32602, got ${capture.error?.code}`);
}

async function captureBackingStoreMessagesPageProbes(client, probes) {
  const first = await captureProbe(client, probes, 'messagesPageSeededFirstPage', 'session/messages_page', {
    session_id: sessionId,
    limit: 5,
    offset: 0,
  });
  assertMessagesPage(`${client.label}: seeded first page`, first, {
    count: 5,
    hasMore: true,
    nextOffset: 5,
    contentPrefix: 'm18 backing-store seed message ',
  });

  const second = await captureProbe(client, probes, 'messagesPageSeededSecondPage', 'session/messages_page', {
    session_id: sessionId,
    limit: 5,
    offset: 5,
  });
  assertMessagesPage(`${client.label}: seeded second page`, second, {
    count: 5,
    hasMore: true,
    nextOffset: 10,
    contentPrefix: 'm18 backing-store seed message ',
  });

  const tail = await captureProbe(client, probes, 'messagesPageSeededTailPage', 'session/messages_page', {
    session_id: sessionId,
    limit: 5,
    offset: 10,
  });
  assertMessagesPage(`${client.label}: seeded tail page`, tail, {
    count: 2,
    hasMore: false,
    nextOffset: 12,
    contentPrefix: 'm18 backing-store seed message ',
  });

  const beyond = await captureProbe(client, probes, 'messagesPageSeededBeyondPage', 'session/messages_page', {
    session_id: sessionId,
    limit: 5,
    offset: 20,
  });
  assertMessagesPage(`${client.label}: seeded beyond page`, beyond, {
    count: 0,
    hasMore: false,
    nextOffset: 20,
  });

  const clamped = await captureProbe(client, probes, 'messagesPageSeededLimitClamp', 'session/messages_page', {
    session_id: sessionId,
    limit: 999,
    offset: 0,
  });
  assertMessagesPage(`${client.label}: seeded limit clamp`, clamped, {
    count: 12,
    hasMore: false,
    nextOffset: 12,
    firstContent: 'm18 backing-store seed message 00 user',
  });

  const invalidLimit = await captureProbe(client, probes, 'messagesPageInvalidLimitType', 'session/messages_page', {
    session_id: sessionId,
    limit: 'bad-limit',
    offset: 0,
  });
  assertTypedInvalidParams(`${client.label}: invalid limit`, invalidLimit);

  const missingSession = await captureProbe(client, probes, 'messagesPageMissingSessionId', 'session/messages_page', {
    limit: 2,
    offset: 0,
  });
  assertTypedInvalidParams(`${client.label}: missing session_id`, missingSession);
}

async function validateMessagesPageMode(client, probes, name, capture) {
  if (scenarioMode === 'headless') {
    assertMessagesPageUnavailable(`${client.label}: ${name}`, capture);
    return;
  }
  assertMessagesPage(`${client.label}: ${name}`, capture, {
    count: 12,
    hasMore: false,
    nextOffset: 12,
    firstContent: 'm18 backing-store seed message 00 user',
  });
  await captureBackingStoreMessagesPageProbes(client, probes);
}

async function validateMessagesPageReconnectMode(client, name, capture) {
  if (scenarioMode === 'headless') {
    assertMessagesPageUnavailable(`${client.label}: ${name}`, capture);
    return;
  }
  assertMessagesPage(`${client.label}: ${name}`, capture, {
    count: 12,
    hasMore: false,
    nextOffset: 12,
    firstContent: 'm18 backing-store seed message 00 user',
  });
}
function validateNegotiatedCapabilities(label, capabilities, routeInventory) {
  const supportedMethods = capabilities?.supported_methods || [];
  const unsupportedMethods = unsupportedCapabilityMethods(capabilities);
  const inventoryMethods = new Set((routeInventory.methods || []).map((entry) => entry.method));
  const expectedCallable = label === 'stdio'
    ? requiredScenarioMethods.filter((method) => !methodIsUnavailableOverStdioAuth(method))
    : requiredScenarioMethods;
  const missingRequired = expectedCallable.filter((method) => !supportedMethods.includes(method));
  assert(
    missingRequired.length === 0,
    label + ": missing advertised scenario methods: " + missingRequired.join(", "),
  );
  const contradictoryMethods = supportedMethods.filter((method) => unsupportedMethods.has(method));
  assert(
    contradictoryMethods.length === 0,
    label + ": methods cannot be both supported and explicitly unsupported: " + contradictoryMethods.join(", "),
  );
  if (label === 'stdio') {
    const missingUnsupported = [...stdioAuthUnavailableMethods].filter((method) => !unsupportedMethods.has(method));
    assert(
      missingUnsupported.length === 0,
      label + ": auth-bound methods missing explicit unsupported reports: " + missingUnsupported.join(", "),
    );
    const incorrectlySupported = [...stdioAuthUnavailableMethods].filter((method) => supportedMethods.includes(method));
    assert(
      incorrectlySupported.length === 0,
      label + ": auth-bound methods must not be advertised as supported: " + incorrectlySupported.join(", "),
    );
  }
  const unknownAdvertised = supportedMethods.filter((method) => !inventoryMethods.has(method));
  assert(
    unknownAdvertised.length === 0,
    label + ": advertised methods absent from route inventory: " + unknownAdvertised.join(", "),
  );
  const missingInventory = routeInventoryMethods(routeInventory).filter((method) => {
    if (supportedMethods.includes(method)) return false;
    return !(label === 'stdio' && methodIsUnavailableOverStdioAuth(method) && unsupportedMethods.has(method));
  });
  assert(
    missingInventory.length === 0,
    label + ": route inventory methods missing from advertised capabilities: " + missingInventory.join(", "),
  );
}

async function runScenario(client, routeInventory) {
  const probes = {};
  const hello = await client.request('client_hello', {
    transport: client.label === 'ws' ? 'websocket' : 'stdio',
    client: { name: 'm18-appui-transport-parity-soak' },
    supported_features: wsUiFeatures,
  });
  assert(hello?.type === 'server_hello', `${client.label}: client_hello did not return server_hello`);
  assert(
    hello?.capabilities?.supported_methods?.includes('config/capabilities/list'),
    `${client.label}: client_hello missing capabilities`,
  );

  const capabilities = await client.request('config/capabilities/list');
  const supportedMethods = capabilities?.capabilities?.supported_methods || [];
  validateNegotiatedCapabilities(client.label, capabilities?.capabilities, routeInventory);

  await captureProbe(client, probes, 'authStatusPreOpen', 'auth/status');
  const authMePreOpen = await captureProbe(client, probes, 'authMePreOpen', 'auth/me', {}, 15_000, { parity: false });
  assertStdioAuthUnavailableDirect(client, 'auth/me', authMePreOpen);

  const profileError = await client.requestCapture('session/status/read', {
    session_id: `${sessionId}:missing-profile-probe`,
    profile_id: `${profileId}-missing`,
  });
  assert(
    profileError.ok === false && profileError.error?.data?.kind === 'profile_unresolved',
    `${client.label}: missing profile did not return profile_unresolved`,
  );
  const profileCreate = await client.request('profile/local/create', {
    name: 'M18 AppUI Parity',
    username: profileId,
    email: `${profileId}@example.test`,
  });
  const opened = await client.request('session/open', {
    session_id: sessionId,
    profile_id: profileId,
  });
  const status = await client.request('session/status/read', {
    session_id: sessionId,
    profile_id: profileId,
  });
  let codexToolStatus = null;
  if (codexP0Soak) {
    codexToolStatus = await captureProbe(client, probes, 'codexP0ToolStatusList', 'tool/status/list', {
      session_id: sessionId,
      profile_id: profileId,
    });
    validateCodexToolStatus(client.label, codexToolStatus);
  }

  await captureProbe(client, probes, 'sessionSnapshot', 'session/snapshot', { session_id: sessionId });
  const messagesPagePreReplay = await captureProbe(client, probes, 'messagesPagePreReplay', 'session/messages_page', {
    session_id: sessionId,
    limit: 20,
    offset: 0,
  });
  await validateMessagesPageMode(client, probes, 'messagesPagePreReplay', messagesPagePreReplay);
  const contentDeleteInvalidParams = await captureProbe(client, probes, 'contentDeleteInvalidParams', 'content/delete', {}, 5000, { parity: false });
  assertStdioAuthUnavailableDirect(client, 'content/delete', contentDeleteInvalidParams);
  await captureProbe(client, probes, 'routerSetMode', 'router/set_mode', {
    session_id: sessionId,
    mode: 'off',
  });
  await captureProbe(client, probes, 'routerGetMetrics', 'router/get_metrics', { session_id: sessionId });
  const replayTurn = await runReplayProbe(client);
  client = await client.reconnect();
  const reconnectStatus = await client.request('session/status/read', {
    session_id: sessionId,
    profile_id: profileId,
  });
  const messagesPageAfterReconnect = await captureProbe(client, probes, 'messagesPageAfterReconnect', 'session/messages_page', {
    session_id: sessionId,
    limit: 50,
    offset: 0,
  });
  await validateMessagesPageReconnectMode(client, 'messagesPageAfterReconnect', messagesPageAfterReconnect);
  await captureProbe(client, probes, 'sessionSnapshotAfterReconnect', 'session/snapshot', { session_id: sessionId });
  await captureRouteInventoryProbes(client, routeInventory, probes);

  const toolTurn = await runTurn(
    client,
    "Use the list_dir tool to list the contents of '.' for the M18 same-scenario fixture.",
    { tool: true },
  );
  const codexP0Turn = codexP0Soak ? await runCodexP0Turn(client) : null;
  const approvalTurn = await runTurn(
    client,
    'M9 approval fixture: request approval for printf m18-approval-e2e',
    { approval: true },
  );
  const interruptTurn = await runInterrupt(client);

  const contentDeleteAuthProbe = await captureProbe(client, probes, 'contentDeleteAuthProbe', 'content/delete', {
    id: "m18-missing-content-" + stamp,
  }, 5000, { parity: false });
  assertStdioAuthUnavailableDirect(client, 'content/delete', contentDeleteAuthProbe);
  client = await client.reconnect();
  const contentListAuthProbe = await captureProbe(client, probes, 'contentListAuthProbe', 'content/list', {
    filters: { limit: 5, offset: 0 },
  }, 5000, { parity: false });
  assertStdioAuthUnavailableDirect(client, 'content/list', contentListAuthProbe);
  const contentBulkDeleteAuthProbe = await captureProbe(client, probes, 'contentBulkDeleteAuthProbe', 'content/bulk_delete', {
    ids: ["m18-missing-content-" + stamp],
  }, 5000, { parity: false });
  assertStdioAuthUnavailableDirect(client, 'content/bulk_delete', contentBulkDeleteAuthProbe);
  const authLogoutAuthProbe = await captureProbe(client, probes, 'authLogoutAuthProbe', 'auth/logout', {}, 5000, { parity: false });
  assertStdioAuthUnavailableDirect(client, 'auth/logout', authLogoutAuthProbe);

  return {
    hello,
    capabilities,
    supportedMethods,
    unsupportedMethods: [...unsupportedCapabilityMethods(capabilities?.capabilities)].sort(),
    probes,
    profileError,
    profileCreate,
    opened,
    status,
    codexToolStatus: codexToolStatus ? summarizeCapture(codexToolStatus) : null,
    codexToolStatusRaw: codexToolStatus?.result || null,
    reconnectStatus,
    runtimePolicyStamp: status?.runtime_policy_stamp || opened?.opened?.runtime_policy_stamp || null,
    replayTurn,
    toolTurn,
    codexP0Turn,
    approvalTurn,
    interruptTurn,
    client,
  };
}

function sortArray(value) {
  return Array.isArray(value) ? [...value].sort() : value;
}

function sortedSet(values) {
  return [...new Set(values)].sort();
}

function paritySupportedMethods(methods) {
  return sortArray((methods || []).filter((method) => !methodIsUnavailableOverStdioAuth(method)));
}

function routeInventoryMethods(routeInventory) {
  return (routeInventory.methods || []).map((entry) => entry.method);
}

function scrub(value) {
  if (Array.isArray(value)) return value.map(scrub);
  if (value && typeof value === 'object') {
    const out = {};
    for (const [key, raw] of Object.entries(value)) {
      if (['ts', 'timestamp', 'started_at', 'completed_at', 'recorded_at_ms', 'created_at_ms', 'updated_at_ms', 'finished_at_ms', 'duration_ms'].includes(key)) {
        continue;
      }
      if (['turn_id', 'approval_id', 'tool_call_id', 'task_id', 'event_id', 'cursor', 'next_cursor'].includes(key)) {
        out[key] = `<${key}>`;
        continue;
      }
      if (key === 'session_id') {
        out[key] = '<session_id>';
        continue;
      }
      if (key === 'cwd' || key === 'workspace_root' || key === 'artifact_path') {
        out[key] = '<path>';
        continue;
      }
      out[key] = scrub(raw);
    }
    return out;
  }
  if (typeof value === 'string') {
    return value
      .replaceAll(runRoot, '<runRoot>')
      .replaceAll(workspace, '<workspace>')
      .replaceAll(wsDataDir, '<dataDir>')
      .replaceAll(stdioDataDir, '<dataDir>')
      .replaceAll(sessionId, '<session_id>')
      .replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, '<uuid>')
      .replace(/\d{8}T\d{6}Z/g, '<stamp>');
  }
  return value;
}

function summarizeResult(method, result) {
  if (method === 'client_hello') {
    const caps = result?.capabilities || {};
    return {
      type: result?.type,
      transport: '<transport>',
      supported_features: sortArray(caps.supported_features || []),
      supported_methods: paritySupportedMethods(caps.supported_methods),
      supported_notifications: sortArray(caps.supported_notifications || []),
    };
  }
  if (method === 'config/capabilities/list') {
    const caps = result?.capabilities || {};
    return {
      supported_features: sortArray(caps.supported_features || []),
      supported_methods: paritySupportedMethods(caps.supported_methods),
      supported_notifications: sortArray(caps.supported_notifications || []),
    };
  }
  if (method === 'session/status/read') {
    return {
      profile_id: result?.profile_id,
      has_runtime_policy_stamp: Boolean(result?.runtime_policy_stamp),
      runtime_policy_stamp: scrub(result?.runtime_policy_stamp || null),
      context_state: scrub(result?.context_state || null),
    };
  }
  if (method === 'session/open') {
    return {
      active_profile_id: result?.opened?.active_profile_id,
      has_capabilities: Boolean(result?.opened?.capabilities),
      has_runtime_policy_stamp: Boolean(result?.opened?.runtime_policy_stamp),
    };
  }
  if (method === 'profile/local/create') {
    return {
      id: result?.profile?.id || result?.id,
      username: result?.profile?.username || result?.username,
      email: result?.profile?.email || result?.email,
      created: Boolean(result?.created),
    };
  }
  if (method === 'auth/status') {
    return {
      authenticated: result?.authenticated,
      email_login_enabled: result?.email_login_enabled,
      profile_id: result?.profile_id,
      scoped_profile_id: result?.scoped_profile?.id,
    };
  }
  if (method === 'auth/me') {
    return {
      profile_id: result?.profile_id,
      has_email: Boolean(result?.email),
    };
  }
  if (method === 'session/snapshot') {
    return {
      has_status: Boolean(result?.status),
      has_files: Boolean(result?.files),
      has_tasks: Boolean(result?.tasks),
    };
  }
  if (method === 'session/messages_page') {
    const messages = Array.isArray(result?.messages) ? result.messages : [];
    return {
      message_count: messages.length,
      has_more: result?.has_more,
      next_offset: result?.next_offset,
      message_roles: messages.map((message) => message.role || message.kind || message.source || '<unknown>'),
      message_fingerprints: messages.map((message) => crypto
        .createHash('sha256')
        .update(JSON.stringify({
          role: message.role || null,
          content: message.content || null,
          thread_id: message.thread_id || null,
        }))
        .digest('hex')
        .slice(0, 12)),
    };
  }
  if (method === 'content/list') {
    const entries = Array.isArray(result?.entries) ? result.entries : [];
    return {
      total: result?.total,
      entry_count: entries.length,
    };
  }
  if (method === 'content/delete') {
    return { deleted: result?.deleted };
  }
  if (method === 'router/set_mode') {
    return { mode: result?.mode };
  }
  if (method === 'system/status.get') {
    return {
      status: result?.status ? {
        agent_configured: result.status.agent_configured,
        base_domain: result.status.base_domain,
        model: result.status.model,
        provider: result.status.provider,
        version: result.status.version,
      } : undefined,
    };
  }
  if (method === 'router/get_metrics') {
    return {
      provider_name: result?.provider_name,
      mode: result?.mode,
      qos_ranking: result?.qos_ranking,
      lane_count: result?.lane_scores ? Object.keys(result.lane_scores).length : undefined,
      circuit_breaker_count: result?.circuit_breakers ? Object.keys(result.circuit_breakers).length : undefined,
    };
  }
  if (method === 'turn/start') return { accepted: result?.accepted === true };
  if (method === 'approval/respond') return { accepted: result?.accepted === true, status: result?.status };
  if (method === 'turn/interrupt') return scrub(result);
  return scrub(result);
}

function summarizeCapture(capture) {
  if (capture.ok) {
    return {
      ok: true,
      method: capture.method,
      result: summarizeResult(capture.method, capture.result),
    };
  }
  return {
    ok: false,
    method: capture.method,
    error: capture.error ? scrub(capture.error) : undefined,
    transportError: capture.transportError ? scrub(capture.transportError) : undefined,
  };
}

function summarizeRequest(method, params) {
  if (method === 'client_hello') {
    return {
      transport: '<transport>',
      client_name: params?.client?.name,
      supported_features: sortArray(params?.supported_features || []),
    };
  }
  if (method === 'turn/start') {
    return {
      session_id: '<session_id>',
      profile_id: params?.profile_id,
      turn_id: '<turn_id>',
      input: (params?.input || []).map((entry) => ({
        kind: entry.kind,
        text_fixture: classifyPrompt(entry.text || ''),
      })),
    };
  }
  return scrub(params || {});
}

function classifyPrompt(text) {
  const lower = text.toLowerCase();
  if (lower.includes('m14 codex p0 tool parity fixture')) return 'codex-p0-tool-parity';
  if (lower.includes('m9 replay-lossy fixture')) return 'replay-lossy';
  if (lower.includes('list_dir tool')) return 'tool-events';
  if (lower.includes('m9 approval fixture')) return 'approval';
  if (lower.includes('200 separate lines')) return 'interrupt-slow';
  return 'basic';
}

function summarizeNotification(frame) {
  const params = frame.params || {};
  if (frame.method === 'message/delta' || frame.method === 'task/output/delta') {
    return { method: frame.method, text: params.text || '' };
  }
  if (frame.method?.startsWith('tool/')) {
    return {
      method: frame.method,
      tool_name: params.tool_name,
      success: params.success,
      message: params.message,
      output_preview: params.output_preview,
    };
  }
  if (frame.method?.startsWith('approval/')) {
    return {
      method: frame.method,
      approval_kind: params.approval_kind,
      tool_name: params.tool_name,
      risk: params.risk,
      decision: params.decision,
      reason: params.reason,
    };
  }
  if (frame.method === 'turn/error') {
    return { method: frame.method, code: params.code, message: params.message };
  }
  if (frame.method === 'progress/updated') {
    return { method: frame.method, message: params.metadata?.message || params.message };
  }
  if (frame.method === 'protocol/replay_lossy') {
    return {
      method: frame.method,
      dropped_count: params.dropped_count,
      has_last_durable_cursor: Boolean(params.last_durable_cursor),
    };
  }
  if (frame.method === 'message/persisted') {
    return {
      method: frame.method,
      role: params.message?.role || params.role,
      source: params.source,
    };
  }
  return { method: frame.method };
}

function normalizeTranscript(file) {
  const idToExchange = new Map();
  const exchanges = [];
  const notifications = [];
  const liveNotificationCounts = {};
  for (const entry of parseJsonl(file)) {
    if (entry.parity === false) continue;
    const frame = entry.frame;
    if (!frame) continue;
    if (entry.direction === 'client_to_server') {
      const exchange = {
        direction: 'client_to_server',
        method: frame.method,
        params: summarizeRequest(frame.method, frame.params),
        response: null,
      };
      exchanges.push(exchange);
      if (frame.id != null) idToExchange.set(String(frame.id), exchange);
    } else if (entry.direction === 'server_to_client' && frame.id != null) {
      const exchange = idToExchange.get(String(frame.id));
      const method = exchange?.method || '<unknown>';
      const response = {
        direction: 'server_to_client',
        method,
        ok: !frame.error,
        error: frame.error ? scrub(frame.error) : undefined,
        result: frame.error ? undefined : summarizeResult(method, frame.result),
      };
      if (exchange) {
        exchange.response = response;
      } else {
        exchanges.push({
          direction: 'client_to_server',
          method,
          params: null,
          response,
        });
      }
    } else if (entry.direction === 'server_to_client' && frame.method) {
      if (liveNotificationMethods.has(frame.method)) {
        liveNotificationCounts[frame.method] = (liveNotificationCounts[frame.method] || 0) + 1;
      } else {
        notifications.push({ direction: 'server_to_client', notification: summarizeNotification(frame) });
      }
    }
  }
  return {
    exchanges,
    notifications: notifications.sort((left, right) => JSON.stringify(left).localeCompare(JSON.stringify(right))),
    liveNotificationCounts,
  };
}

function indexAllowlist(allowlist) {
  return new Map((allowlist.entries || []).map((entry) => [entry.id, entry]));
}

function isAllowlistedDifference(diff, allowlistById) {
  const stdioError = diff.stdio?.response?.error;
  const stdioErrorData = stdioError?.data || {};
  if (
    allowlistById.has('m18-stdio-auth-unavailable-auth-bound-methods')
    && diff.kind === 'rpc_exchange_mismatch'
    && diff.websocket?.method === diff.stdio?.method
    && stdioAuthUnavailableMethods.has(diff.stdio?.method)
    && diff.websocket?.response?.ok === true
    && diff.stdio?.response?.ok === false
    && stdioError?.code === stdioAuthUnavailableShape.code
    && stdioErrorData.kind === stdioAuthUnavailableShape.kind
    && stdioErrorData.recoverable === stdioAuthUnavailableShape.recoverable
    && stdioErrorData.recovery === stdioAuthUnavailableShape.recovery
    && stdioError?.message === `${diff.stdio.method}: authenticated user identity required`
  ) {
    return true;
  }
  return false;
}

function isExpectedRuntimeUnavailable(probe) {
  return probe?.error?.code === -32602 && probe?.error?.data?.kind === 'runtime_unavailable';
}

function normalizeScenarioResult(result) {
  const probes = {};
  for (const [name, probe] of Object.entries(result.probes || {})) {
    if (!authContextProbeNames.has(name)) probes[name] = probe;
  }
  return {
    supportedMethods: paritySupportedMethods(result.supportedMethods),
    profileError: summarizeCapture(result.profileError),
    profileCreate: summarizeResult('profile/local/create', result.profileCreate),
    opened: summarizeResult('session/open', result.opened),
    status: summarizeResult('session/status/read', result.status),
    codexToolStatus: scrub(result.codexToolStatus),
    reconnectStatus: summarizeResult('session/status/read', result.reconnectStatus),
    runtimePolicyStamp: scrub(result.runtimePolicyStamp),
    probes: scrub(probes),
    replayTurn: scrub(result.replayTurn),
    toolTurn: scrub(result.toolTurn),
    codexP0Turn: scrub(result.codexP0Turn),
    approvalTurn: scrub(result.approvalTurn),
    interruptTurn: scrub(result.interruptTurn),
  };
}

function checkedMethodsForResult(result) {
  const checked = new Set(requiredScenarioMethods);
  for (const probe of Object.values(result.probes || {})) {
    if (probe?.method) checked.add(probe.method);
  }
  const supported = new Set(result.supportedMethods || []);
  for (const method of result.unsupportedMethods || []) {
    if (!supported.has(method)) checked.add(method);
  }
  return [...checked].sort();
}

function routeCoverage(routeInventory, wsResult, stdioResult) {
  const inventory = routeInventoryMethods(routeInventory).sort();
  const wsChecked = checkedMethodsForResult(wsResult);
  const stdioChecked = checkedMethodsForResult(stdioResult);
  const checkedByBoth = inventory.filter(
    (method) => wsChecked.includes(method) && stdioChecked.includes(method),
  );
  return {
    inventoryMethods: inventory.length,
    checkedMethods: checkedByBoth.length,
    checkedMethodNames: checkedByBoth,
    websocketOnly: inventory.filter((method) => wsChecked.includes(method) && !stdioChecked.includes(method)),
    stdioOnly: inventory.filter((method) => stdioChecked.includes(method) && !wsChecked.includes(method)),
    missing: inventory.filter((method) => !wsChecked.includes(method) || !stdioChecked.includes(method)),
  };
}

function diffNormalized(wsNorm, stdioNorm, wsResult, stdioResult, allowlist, routeInventory) {
  const allowlistById = indexAllowlist(allowlist);
  const coverage = routeCoverage(routeInventory, wsResult, stdioResult);
  const differences = [];
  if (coverage.missing.length || coverage.websocketOnly.length || coverage.stdioOnly.length) {
    differences.push({ kind: 'route_coverage_gap', coverage });
  }
  const maxExchanges = Math.max(wsNorm.exchanges.length, stdioNorm.exchanges.length);
  for (let i = 0; i < maxExchanges; i += 1) {
    const left = wsNorm.exchanges[i] || null;
    const right = stdioNorm.exchanges[i] || null;
    if (JSON.stringify(left) !== JSON.stringify(right)) {
      differences.push({ kind: 'rpc_exchange_mismatch', index: i, websocket: left, stdio: right });
    }
  }
  if (JSON.stringify(wsNorm.notifications) !== JSON.stringify(stdioNorm.notifications)) {
    differences.push({
      kind: 'notification_mismatch',
      websocket: wsNorm.notifications,
      stdio: stdioNorm.notifications,
    });
  }

  const wsScenario = normalizeScenarioResult(wsResult);
  const stdioScenario = normalizeScenarioResult(stdioResult);
  if (JSON.stringify(wsScenario) !== JSON.stringify(stdioScenario)) {
    differences.push({
      kind: 'scenario_result_mismatch',
      websocket: wsScenario,
      stdio: stdioScenario,
    });
  }

  for (const [transport, result] of [['stdio', stdioResult], ['websocket', wsResult]]) {
    const advertised = new Set(result.supportedMethods || []);
    for (const probe of [result.probes?.routerSetMode, result.probes?.routerGetMetrics]) {
      if (isExpectedRuntimeUnavailable(probe)) continue;
      if (probe && advertised.has(probe.method) && probe.ok === false) {
        differences.push({
          kind: `${transport}_advertised_method_failed`,
          transport,
          method: probe.method,
          error: probe.error,
        });
      }
    }
  }

  const allowed = [];
  const unexpected = [];
  for (const diff of differences) {
    if (isAllowlistedDifference(diff, allowlistById)) allowed.push(diff);
    else unexpected.push(diff);
  }
  return {
    ok: unexpected.length === 0,
    runRoot,
    routeInventory: path.relative(repoRoot, routeInventoryPath),
    allowlist: path.relative(repoRoot, allowlistPath),
    checkedScenarioMethods: coverage.checkedMethodNames,
    routeCoverage: coverage,
    comparedEvents: {
      websocket: {
        rpcExchanges: wsNorm.exchanges.length,
        stableNotifications: wsNorm.notifications.length,
        liveNotifications: wsNorm.liveNotificationCounts,
      },
      stdio: {
        rpcExchanges: stdioNorm.exchanges.length,
        stableNotifications: stdioNorm.notifications.length,
        liveNotifications: stdioNorm.liveNotificationCounts,
      },
    },
    allowlistedDifferences: allowed,
    unexpectedDifferences: unexpected,
  };
}

function toolNamesFromStatus(statusRaw) {
  return (statusRaw?.tools || [])
    .map((entry) => entry.name)
    .filter(Boolean)
    .sort();
}

async function main() {
  const routeInventory = readJson(routeInventoryPath);
  const allowlist = readJson(allowlistPath);
  const backingStoreSeed = scenarioMode === 'backing-store'
    ? seedBackingStores()
    : {
      schema: 'octos-m18-backing-store-seed-v1',
      issue: 'octos#1032',
      mode: scenarioMode,
      seeded: false,
      reason: 'headless mode preserves the expected -32140 sessions-unavailable parity case',
    };
  writeJson(backingStoreSeedPath, backingStoreSeed);
  const port = await getFreePort();
  const baseUrl = `http://127.0.0.1:${port}`;
  let wsServer;
  let wsClient;
  let stdioClient;

  try {
    wsServer = await startWsServer(port);
    await waitForHttp(`${baseUrl}/api/status`);
    wsClient = new WsClient(baseUrl);
    await wsClient.connect();
    const wsResult = await runScenario(wsClient, routeInventory);
    wsClient = wsResult.client;
    await wsClient.close();

    stdioClient = new StdioClient();
    await stdioClient.waitSpawn();
    const stdioResult = await runScenario(stdioClient, routeInventory);
    stdioClient = stdioResult.client;
    await stdioClient.close();

    writeJson(runtimePolicyStampPath, {
      websocket: scrub(wsResult.runtimePolicyStamp),
      stdio: scrub(stdioResult.runtimePolicyStamp),
    });
    if (codexP0Soak) {
      writeJson(toolContractPath, {
        issue: 969,
        scenario: 'm14_codex_p0_tool_parity',
        websocket: scrub(wsResult.codexToolStatusRaw?.coding_tool_contract || null),
        stdio: scrub(stdioResult.codexToolStatusRaw?.coding_tool_contract || null),
      });
      writeJson(toolRegistrySnapshotPath, {
        issue: 969,
        scenario: 'm14_codex_p0_tool_parity',
        websocket: toolNamesFromStatus(wsResult.codexToolStatusRaw),
        stdio: toolNamesFromStatus(stdioResult.codexToolStatusRaw),
      });
      if (!fs.readFileSync(tuiCapturePath, 'utf8').trim()) {
        fs.writeFileSync(tuiCapturePath, '#969 Codex P0 tool parity soak completed over WebSocket and stdio.\n');
      }
    }

    const wsNorm = normalizeTranscript(wsTranscript);
    const stdioNorm = normalizeTranscript(stdioTranscript);
    const diff = {
      ...diffNormalized(wsNorm, stdioNorm, wsResult, stdioResult, allowlist, routeInventory),
      issue: scenarioMode === 'backing-store' ? 'octos#1044' : 'octos#1032',
      mode: scenarioMode,
      host: os.hostname(),
      routeInventoryMethodCount: routeInventory.methods.length,
    };
    writeJson(diffPath, diff);
    console.log(JSON.stringify({
      ok: diff.ok,
      mode: scenarioMode,
      runRoot,
      artifacts: {
        wsTranscript,
        stdioTranscript,
        normalizedDiff: diffPath,
        serverLog,
        runtimePolicyStamp: runtimePolicyStampPath,
        backingStoreSeed: backingStoreSeedPath,
        toolContract: codexP0Soak ? toolContractPath : undefined,
        toolRegistrySnapshot: codexP0Soak ? toolRegistrySnapshotPath : undefined,
        approvalEvents: codexP0Soak ? approvalEventsPath : undefined,
        taskLedger: codexP0Soak ? taskLedgerPath : undefined,
        tuiCapture: codexP0Soak ? tuiCapturePath : undefined,
      },
      allowlistedDifferences: diff.allowlistedDifferences.length,
      unexpectedDifferences: diff.unexpectedDifferences.length,
    }, null, 2));
    if (!diff.ok) process.exitCode = 1;
  } catch (error) {
    const failure = {
      ok: false,
      error: String(error?.stack || error),
      mode: scenarioMode,
      runRoot,
      artifacts: {
        wsTranscript,
        stdioTranscript,
        normalizedDiff: diffPath,
        serverLog,
        runtimePolicyStamp: runtimePolicyStampPath,
        backingStoreSeed: backingStoreSeedPath,
        toolContract: codexP0Soak ? toolContractPath : undefined,
        toolRegistrySnapshot: codexP0Soak ? toolRegistrySnapshotPath : undefined,
        approvalEvents: codexP0Soak ? approvalEventsPath : undefined,
        taskLedger: codexP0Soak ? taskLedgerPath : undefined,
        tuiCapture: codexP0Soak ? tuiCapturePath : undefined,
      },
    };
    writeJson(diffPath, failure);
    console.error(JSON.stringify(failure, null, 2));
    process.exitCode = 1;
  } finally {
    if (wsClient) await wsClient.close();
    if (stdioClient) await stdioClient.close();
    await stopProcess(wsServer);
  }
}

main()
  .then(() => {
    process.exit(process.exitCode || 0);
  })
  .catch((error) => {
    console.error(error);
    process.exit(1);
  });
