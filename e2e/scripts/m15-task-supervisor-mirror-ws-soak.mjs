#!/usr/bin/env node

import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import WebSocket from 'ws';

const repoRoot = path.resolve(import.meta.dirname, '..', '..');
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..+/, 'Z');
const runRoot = path.resolve(
  process.env.OCTOS_M15_TASK_SUPERVISOR_MIRROR_WS_DIR
    || process.env.OCTOS_M15_TASK_SUPERVISOR_MIRROR_DIR
    || path.join(repoRoot, 'e2e', 'test-results-m15-task-supervisor-mirror-ws', stamp),
);
const dataDir = path.join(runRoot, 'data');
const workspace = path.join(runRoot, 'workspace');
const octosBin = process.env.OCTOS_BIN || path.join(repoRoot, 'target', 'debug', 'octos');
const authToken = process.env.OCTOS_M15_TASK_SUPERVISOR_AUTH_TOKEN
  || `m15-task-mirror-${crypto.randomBytes(8).toString('hex')}`;
const sessionId = process.env.OCTOS_M15_TASK_SUPERVISOR_SESSION
  || `api:m15-task-mirror-ws-${stamp}`;
const profileId = process.env.OCTOS_M15_TASK_SUPERVISOR_PROFILE || 'm15-task-mirror-ws';
const timeoutMs = Number(process.env.OCTOS_M15_TASK_SUPERVISOR_TIMEOUT_MS || 45_000);

const transcriptPath = path.join(runRoot, 'client-observed-appui-ws-transcript.jsonl');
const serverLog = path.join(runRoot, 'server.log');
const summaryPath = path.join(runRoot, 'm15-task-supervisor-mirror-ws-summary.json');

const requestedUiFeatures = [
  'approval.typed.v1',
  'pane.snapshots.v1',
  'session.workspace_cwd.v1',
  'harness.task_control.v1',
  'state.session_hydrate.v1',
  'state.thread_graph.v1',
  'state.turn_state_get.v1',
  'event.message_persisted.v1',
  'event.spawn_complete.v1',
  'auxiliary.rest_to_ws.v1',
  'coding.autonomy.v1',
  'coding.agent_control.v1',
  'coding.goal_runtime.v1',
  'coding.loop_runtime.v1',
  'review.start.v1',
  'context.lifecycle.v1',
  'permission.profile.v1',
  'runtime.policy_stamp.v1',
  'profile.local_create.v1',
];

const requiredMethods = [
  'profile/local/create',
  'session/open',
  'turn/start',
  'agent/list',
  'agent/status/read',
  'task/output/read',
];
const requiredNotifications = ['agent/updated', 'task/updated'];
const requiredCodingFeatures = [
  'harness.task_control.v1',
  'coding.autonomy.v1',
  'coding.agent_control.v1',
];

fs.mkdirSync(workspace, { recursive: true });
fs.closeSync(fs.openSync(transcriptPath, 'a'));
fs.closeSync(fs.openSync(serverLog, 'a'));

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

function wait(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
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
        req.on('timeout', () => req.destroy(new Error('timeout')));
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

class WsAppUiClient {
  constructor(baseUrl) {
    const featureQuery = requestedUiFeatures
      .map((feature) => `ui_feature=${encodeURIComponent(feature)}`)
      .join('&');
    this.baseUrl = baseUrl;
    this.url = baseUrl
      .replace(/^http:/, 'ws:')
      .replace(/^https:/, 'wss:')
      .replace(/\/$/, '')
      .concat(`/api/ui-protocol/ws?${featureQuery}`);
    this.nextSeq = 0;
    this.pending = new Map();
    this.notifications = [];
    this.agentUpdated = [];
    this.taskUpdated = [];
    this.turnCompleted = null;
    this.turnErrored = null;
    this.closed = true;
    this.ws = null;
  }

  nextId() {
    this.nextSeq += 1;
    return `m15-task-mirror-ws-${this.nextSeq}-${crypto.randomBytes(3).toString('hex')}`;
  }

  observeFrame(direction, frame) {
    appendJsonl(transcriptPath, { direction, frame });
    if (direction !== 'server_to_client') return;

    if (frame && Object.prototype.hasOwnProperty.call(frame, 'id') && frame.id != null) {
      const pending = this.pending.get(String(frame.id));
      if (!pending) return;
      this.pending.delete(String(frame.id));
      if (frame.error) {
        pending.reject(new RpcFailure(`RPC ${pending.method} failed`, frame.error));
      } else {
        pending.resolve(frame.result);
      }
      return;
    }

    if (!frame?.method) return;
    this.notifications.push(frame);
    const params = frame.params || {};
    if (frame.method === 'agent/updated') {
      this.agentUpdated.push(params);
    } else if (frame.method === 'task/updated') {
      this.taskUpdated.push(params);
    } else if (frame.method === 'turn/completed') {
      this.turnCompleted = params;
    } else if (frame.method === 'turn/error') {
      this.turnErrored = params;
    }
  }

  rejectAllPending(error) {
    for (const [, pending] of this.pending) {
      pending.reject(error);
    }
    this.pending.clear();
  }

  async connect() {
    this.closed = false;
    this.ws = new WebSocket(this.url, {
      headers: {
        Authorization: `Bearer ${authToken}`,
        'X-Octos-Ui-Features': requestedUiFeatures.join(','),
      },
    });
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`WS connect timeout to ${this.url}`)), 10_000);
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
          appendJsonl(transcriptPath, {
            direction: 'server_to_client_non_json',
            line: data.toString(),
          });
          return;
        }
        this.observeFrame('server_to_client', frame);
      });
      this.ws.on('close', (code, reason) => {
        this.closed = true;
        this.rejectAllPending(new TransportFailure('WS closed before response', {
          code,
          reason: reason?.toString(),
        }));
      });
    });
  }

  request(method, params = {}, rpcTimeoutMs = 15_000) {
    const id = this.nextId();
    const frame = { jsonrpc: '2.0', id, method, params };
    appendJsonl(transcriptPath, { direction: 'client_to_server', frame });
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`RPC timeout for ${method}`));
      }, rpcTimeoutMs);
      this.pending.set(id, {
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
      try {
        if (this.closed || !this.ws) throw new TransportFailure(`WS is closed before ${method}`);
        this.ws.send(JSON.stringify(frame));
      } catch (error) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(error);
      }
    });
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
    await this.connect();
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
    dataDir,
    '--cwd',
    workspace,
  ], {
    cwd: repoRoot,
    env: {
      ...process.env,
      OCTOS_M9_PROTOCOL_FIXTURES: '1',
      RUST_BACKTRACE: process.env.RUST_BACKTRACE || '1',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  child.stdout.on('data', (chunk) => appendText(serverLog, `[stdout] ${chunk.toString()}`));
  child.stderr.on('data', (chunk) => appendText(serverLog, `[stderr] ${chunk.toString()}`));
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('octos serve did not spawn')), 10_000);
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

async function waitFor(predicate, description) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await wait(50);
  }
  throw new Error(`Timed out waiting for ${description}`);
}

function mirroredAgent(params) {
  const agent = params?.agent;
  if (!agent) return false;
  const backendKind = String(agent.backend_kind || '');
  return (backendKind === 'spawn_child_session' || backendKind.startsWith('task_supervisor:'))
    && String(agent.session_id || '') === sessionId;
}

function capabilitiesPayload(value) {
  return value?.capabilities || value?.opened?.capabilities || value || {};
}

function assertIncludesAll(actual, expected, label) {
  const actualSet = new Set(actual || []);
  for (const item of expected) {
    assert(actualSet.has(item), `missing ${label} ${item}`);
  }
}

async function negotiate(client) {
  const hello = await client.request('client_hello', {
    transport: 'websocket',
    client: { name: 'm15-task-supervisor-mirror-ws-soak' },
    supported_features: requestedUiFeatures,
  });
  assert(hello?.type === 'server_hello', 'client_hello did not return server_hello');

  const capabilities = await client.request('config/capabilities/list');
  const advertised = capabilitiesPayload(capabilities);
  assertIncludesAll(advertised.supported_methods, requiredMethods, 'AppUI method');
  assertIncludesAll(advertised.supported_notifications, requiredNotifications, 'AppUI notification');
  assertIncludesAll(advertised.supported_features, requiredCodingFeatures, 'negotiated feature');
  return { hello, capabilities, advertised };
}

async function ensureLocalProfile(client) {
  try {
    return await client.request('profile/local/create', {
      name: 'M15 Task Supervisor Mirror WS',
      username: profileId,
      email: `${profileId}@example.test`,
    });
  } catch (error) {
    if (error instanceof RpcFailure) {
      const kind = error.error?.data?.kind || error.error?.data?.code;
      if (['profile_exists', 'already_exists', 'conflict'].includes(String(kind))) {
        return { reused: true, error: error.error };
      }
    }
    throw error;
  }
}

async function readMirroredState(client, agentId, taskId, terminalAgent) {
  const agentProfileId = terminalAgent.profile_id || profileId;
  const agentList = await client.request('agent/list', {
    session_id: sessionId,
    profile_id: agentProfileId,
  });
  const listed = (agentList?.agents || []).find((agent) => agent.agent_id === agentId);
  assert(listed, `agent/list missing mirrored agent ${agentId}`);
  assert(
    listed.backend_kind === terminalAgent.backend_kind,
    'agent/list backend_kind mismatch',
  );
  assert(listed.status === terminalAgent.status, 'agent/list terminal status mismatch');

  const agentStatus = await client.request('agent/status/read', {
    agent_id: agentId,
    session_id: sessionId,
    profile_id: agentProfileId,
  });
  assert(
    agentStatus?.agent?.backend_kind === terminalAgent.backend_kind,
    'agent/status/read backend_kind mismatch',
  );
  assert(
    agentStatus?.agent?.status === terminalAgent.status,
    'agent/status/read terminal status mismatch',
  );

  const taskOutput = await client.request('task/output/read', {
    session_id: sessionId,
    task_id: taskId,
    cursor: { offset: 0 },
  });
  const text = String(taskOutput?.text || '');
  for (const expected of [
    'fixture output line one',
    'fixture output line two',
    'fixture output line three',
  ]) {
    assert(text.includes(expected), `task/output/read missing ${expected}`);
  }

  return { agentList, listed, agentStatus, taskOutput, agentProfileId };
}

async function hydrateIfSupported(client, advertised, turnCompletedCursor) {
  const supportsHydrate = Boolean(
    (advertised.supported_methods || []).includes('session/hydrate')
      && (advertised.supported_features || []).includes('state.session_hydrate.v1'),
  );
  if (!supportsHydrate) {
    return { supported: false, result: null };
  }

  const result = await client.request('session/hydrate', {
    session_id: sessionId,
    include: ['messages', 'threads', 'turns', 'pending_approvals'],
  });
  assert(result?.session_id === sessionId, 'session/hydrate returned wrong session_id');
  assert(result?.cursor?.stream === sessionId, 'session/hydrate returned wrong cursor stream');
  if (turnCompletedCursor?.seq != null) {
    assert(
      Number(result.cursor.seq) >= Number(turnCompletedCursor.seq),
      'session/hydrate cursor did not reach the completed turn cursor',
    );
  }
  return { supported: true, result };
}

async function main() {
  const port = await getFreePort();
  const baseUrl = `http://127.0.0.1:${port}`;
  let server;
  let client;

  try {
    server = await startWsServer(port);
    await waitForHttp(`${baseUrl}/api/status`);

    client = new WsAppUiClient(baseUrl);
    await client.connect();
    const negotiated = await negotiate(client);
    const profileCreate = await ensureLocalProfile(client);

    const opened = await client.request('session/open', {
      session_id: sessionId,
      profile_id: profileId,
    });
    const turnId = crypto.randomUUID();
    await client.request('turn/start', {
      session_id: sessionId,
      turn_id: turnId,
      input: [
        {
          kind: 'text',
          text: 'M9 task output fixture: create deterministic background task output and mirror it into agent supervision.',
        },
      ],
    });

    await waitFor(() => client.turnCompleted || client.turnErrored, 'turn terminal event');
    assert(!client.turnErrored, `turn errored: ${JSON.stringify(client.turnErrored)}`);
    await waitFor(
      () => client.agentUpdated.some(mirroredAgent),
      'mirrored TaskSupervisor agent/updated notification',
    );

    const mirroredUpdates = client.agentUpdated.filter(mirroredAgent);
    const terminalUpdate = mirroredUpdates.find((params) =>
      ['completed', 'failed', 'interrupted'].includes(String(params?.agent?.status || '')),
    );
    assert(terminalUpdate, 'missing terminal mirrored agent update');
    const terminalAgent = terminalUpdate.agent;
    const agentId = terminalAgent.agent_id;
    assert(String(agentId || '').startsWith('task-'), `unexpected mirrored agent id ${agentId}`);

    const taskId = terminalAgent.task_id;
    assert(taskId, 'mirrored agent did not expose the source task_id');
    const terminalTaskUpdate = client.taskUpdated.find((params) =>
      params?.task_id === taskId && ['completed', 'failed', 'interrupted'].includes(String(params?.state || '')),
    );
    assert(terminalTaskUpdate, `missing terminal task/updated notification for ${taskId}`);

    const liveRead = await readMirroredState(client, agentId, taskId, terminalAgent);
    const completedCursor = client.turnCompleted?.cursor || null;

    await client.reconnect();
    const reopened = await client.request('session/open', {
      session_id: sessionId,
      profile_id: profileId,
      after: completedCursor,
    });
    const hydration = await hydrateIfSupported(client, negotiated.advertised, completedCursor);
    const reconnectRead = await readMirroredState(client, agentId, taskId, terminalAgent);

    const summary = {
      ok: true,
      runRoot,
      dataDir,
      workspace,
      baseUrl,
      sessionId,
      profileId,
      profileCreate,
      agentProfileId: liveRead.agentProfileId,
      turnId,
      negotiated: {
        requestedFeatures: requestedUiFeatures,
        supportedFeatures: negotiated.advertised.supported_features || [],
        supportedMethods: negotiated.advertised.supported_methods || [],
        supportedNotifications: negotiated.advertised.supported_notifications || [],
      },
      notifications: client.notifications.length,
      agentUpdated: client.agentUpdated.length,
      taskUpdated: client.taskUpdated.length,
      mirroredUpdates: mirroredUpdates.map((params) => ({
        agentId: params.agent.agent_id,
        backendKind: params.agent.backend_kind,
        status: params.agent.status,
        lastTask: params.agent.last_task,
      })),
      taskUpdates: client.taskUpdated.map((params) => ({
        taskId: params.task_id,
        state: params.state,
        runtimeDetail: params.runtime_detail,
      })),
      listedAgent: {
        agentId: liveRead.listed.agent_id,
        backendKind: liveRead.listed.backend_kind,
        status: liveRead.listed.status,
      },
      reconnectListedAgent: {
        agentId: reconnectRead.listed.agent_id,
        backendKind: reconnectRead.listed.backend_kind,
        status: reconnectRead.listed.status,
      },
      taskOutputBytes: liveRead.taskOutput.text.length,
      reconnectTaskOutputBytes: reconnectRead.taskOutput.text.length,
      hydration: {
        supported: hydration.supported,
        cursor: hydration.result?.cursor || null,
        messages: hydration.result?.messages?.length ?? null,
        threads: hydration.result?.threads?.length ?? null,
        turns: hydration.result?.turns?.length ?? null,
      },
      openedCursor: opened?.opened?.cursor || null,
      reopenedCursor: reopened?.opened?.cursor || null,
      artifacts: {
        transcript: transcriptPath,
        serverLog,
        summary: summaryPath,
      },
      host: os.hostname(),
    };
    writeJson(summaryPath, summary);
    console.log(JSON.stringify(summary, null, 2));
  } catch (error) {
    const failure = {
      ok: false,
      error: String(error?.stack || error),
      runRoot,
      dataDir,
      workspace,
      sessionId,
      profileId,
      notifications: client?.notifications?.length || 0,
      agentUpdated: client?.agentUpdated?.length || 0,
      taskUpdated: client?.taskUpdated?.length || 0,
      turnCompleted: client?.turnCompleted || null,
      turnErrored: client?.turnErrored || null,
      artifacts: {
        transcript: transcriptPath,
        serverLog,
        summary: summaryPath,
      },
    };
    writeJson(summaryPath, failure);
    console.error(JSON.stringify(failure, null, 2));
    process.exitCode = 1;
  } finally {
    if (client) await client.close();
    await stopProcess(server);
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
