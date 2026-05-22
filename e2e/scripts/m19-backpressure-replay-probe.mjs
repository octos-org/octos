#!/usr/bin/env node

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import WebSocket from 'ws';

const artifactDir = path.resolve(process.env.OCTOS_M19_BACKPRESSURE_ARTIFACT_DIR || process.cwd());
const endpoint = process.env.OCTOS_M19_BACKPRESSURE_WS_ENDPOINT;
const authToken = process.env.OCTOS_M19_BACKPRESSURE_AUTH_TOKEN;
const profileId = process.env.OCTOS_M19_BACKPRESSURE_PROFILE_ID || 'coding';
const sessionId = process.env.OCTOS_M19_BACKPRESSURE_SESSION_ID;
const workspace = process.env.OCTOS_M19_BACKPRESSURE_WORKSPACE || process.cwd();
const prompt = process.env.OCTOS_M19_BACKPRESSURE_PROMPT
  || 'M9 replay-lossy fixture for M18 reconnect-style replay.';

function positiveIntegerEnv(name, fallback) {
  const raw = process.env[name];
  if (!raw) return fallback;
  const value = Number(raw);
  return Number.isInteger(value) && value > 0 ? value : fallback;
}

const timeoutMs = positiveIntegerEnv('OCTOS_M19_BACKPRESSURE_TIMEOUT_MS', 15_000);
const appuiTranscript = path.join(artifactDir, 'appui-transcript.jsonl');
const websocketTranscript = path.join(artifactDir, 'websocket-transcript.jsonl');
const notificationLog = path.join(artifactDir, 'notification-log.jsonl');
const reportPath = path.join(artifactDir, 'backpressure-report.json');

const requestedFeatures = [
  'approval.typed.v1',
  'pane.snapshots.v1',
  'session.workspace_cwd.v1',
  'harness.task_control.v1',
  'state.session_hydrate.v1',
  'state.thread_graph.v1',
  'state.turn_state_get.v1',
  'event.message_persisted.v1',
  'event.spawn_complete.v1',
  'projection.envelope.v1',
  'auxiliary.rest_to_ws.v1',
];

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function appendJsonl(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.appendFileSync(file, `${JSON.stringify({ ts: new Date().toISOString(), ...value })}\n`);
}

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`, 'utf8');
}

class RpcFailure extends Error {
  constructor(message, error) {
    super(message);
    this.name = 'RpcFailure';
    this.error = error;
  }
}

class WsProbeClient {
  constructor() {
    const featureQuery = requestedFeatures
      .map((feature) => `ui_feature=${encodeURIComponent(feature)}`)
      .join('&');
    this.url = endpoint.includes('?') ? `${endpoint}&${featureQuery}` : `${endpoint}?${featureQuery}`;
    this.pending = new Map();
    this.notifications = [];
    this.nextSeq = 0;
    this.closed = false;
    this.ws = null;
  }

  nextId() {
    this.nextSeq += 1;
    return `m19-backpressure-${this.nextSeq}-${crypto.randomUUID()}`;
  }

  record(direction, frame) {
    appendJsonl(websocketTranscript, { direction, frame });
    appendJsonl(appuiTranscript, { direction, frame });
    if (frame?.method && !Object.prototype.hasOwnProperty.call(frame, 'id')) {
      appendJsonl(notificationLog, { direction, frame });
    }
  }

  async connect() {
    this.ws = new WebSocket(this.url, {
      headers: authToken ? { Authorization: `Bearer ${authToken}` } : {},
    });
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`connect timeout to ${this.url}`)), timeoutMs);
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
          appendJsonl(websocketTranscript, {
            direction: 'server_to_client_non_json',
            line: data.toString(),
          });
          return;
        }
        this.record('server_to_client', frame);
        if (Object.prototype.hasOwnProperty.call(frame, 'id') && frame.id != null) {
          const pending = this.pending.get(String(frame.id));
          if (!pending) return;
          this.pending.delete(String(frame.id));
          if (frame.error) {
            pending.reject(new RpcFailure(`${pending.method} failed`, frame.error));
          } else {
            pending.resolve(frame.result);
          }
          return;
        }
        if (frame.method) this.notifications.push(frame);
      });
      this.ws.on('close', () => {
        this.closed = true;
        for (const [, pending] of this.pending) {
          pending.reject(new Error(`WebSocket closed before ${pending.method} response`));
        }
        this.pending.clear();
      });
    });
  }

  request(method, params = {}) {
    const id = this.nextId();
    const frame = { jsonrpc: '2.0', id, method, params };
    this.record('client_to_server', frame);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`RPC timeout for ${method}`));
      }, timeoutMs);
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
        if (this.closed) throw new Error(`WebSocket closed before ${method}`);
        this.ws.send(JSON.stringify(frame));
      } catch (error) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(error);
      }
    });
  }

  waitForNotification(method, predicate = () => true) {
    const existing = this.notifications.find((frame) => frame.method === method && predicate(frame));
    if (existing) return Promise.resolve(existing);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        cleanup();
        reject(new Error(`timeout waiting for ${method}`));
      }, timeoutMs);
      const onMessage = (data) => {
        let frame;
        try {
          frame = JSON.parse(data.toString());
        } catch {
          return;
        }
        if (frame.method === method && predicate(frame)) {
          cleanup();
          resolve(frame);
        }
      };
      const cleanup = () => {
        clearTimeout(timer);
        this.ws.off('message', onMessage);
      };
      this.ws.on('message', onMessage);
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
}

async function ensureProfile(client) {
  try {
    return await client.request('profile/local/create', {
      name: profileId,
      username: profileId,
      email: `${profileId}@example.invalid`,
    });
  } catch (error) {
    if (error instanceof RpcFailure) {
      const kind = error.error?.data?.kind || error.error?.data?.code;
      if (['profile_exists', 'already_exists', 'conflict', 'profile_local_collision'].includes(String(kind))) {
        return { reused: true, error: error.error };
      }
    }
    throw error;
  }
}

async function main() {
  assert(endpoint, 'OCTOS_M19_BACKPRESSURE_WS_ENDPOINT is required');
  assert(authToken, 'OCTOS_M19_BACKPRESSURE_AUTH_TOKEN is required');
  assert(sessionId, 'OCTOS_M19_BACKPRESSURE_SESSION_ID is required');
  fs.rmSync(appuiTranscript, { force: true });
  fs.rmSync(websocketTranscript, { force: true });
  fs.rmSync(notificationLog, { force: true });

  const client = new WsProbeClient();
  try {
    await client.connect();
    const hello = await client.request('client_hello', {
      client: { name: 'm19-backpressure-replay-probe' },
      supported_features: requestedFeatures,
    });
    const capabilities = await client.request('config/capabilities/list');
    const supportedMethods = capabilities?.capabilities?.supported_methods || [];
    for (const method of [
      'profile/local/create',
      'permission/profile/list',
      'permission/profile/set',
      'session/open',
      'session/status/read',
      'session/snapshot',
      'tool/status/list',
      'turn/start',
    ]) {
      assert(supportedMethods.includes(method), `server did not advertise ${method}`);
    }

    const profile = await ensureProfile(client);
    const permissionsBefore = await client.request('permission/profile/list', {
      session_id: sessionId,
      profile_id: profileId,
    });
    const permissionSet = await client.request('permission/profile/set', {
      session_id: sessionId,
      profile_id: profileId,
      update: { mode: 'workspace_write', network: 'deny', approval_policy: 'on-request' },
    });
    const opened = await client.request('session/open', {
      session_id: sessionId,
      profile_id: profileId,
      cwd: workspace,
    });
    const status = await client.request('session/status/read', {
      session_id: sessionId,
      profile_id: profileId,
    });
    const tools = await client.request('tool/status/list', {
      session_id: sessionId,
      profile_id: profileId,
    });

    const turnId = crypto.randomUUID();
    const before = client.notifications.length;
    const accepted = await client.request('turn/start', {
      session_id: sessionId,
      profile_id: profileId,
      turn_id: turnId,
      input: [{ kind: 'text', text: prompt }],
    });
    assert(accepted?.accepted === true, 'replay-lossy turn/start was not accepted');
    const replayLossy = await client.waitForNotification(
      'protocol/replay_lossy',
      (frame) => frame.params?.session_id === sessionId,
    );
    const terminal = await Promise.race([
      client.waitForNotification('turn/completed', (frame) => frame.params?.turn_id === turnId),
      client.waitForNotification('turn/error', (frame) => frame.params?.turn_id === turnId),
    ]);
    const snapshot = await client.request('session/snapshot', {
      session_id: sessionId,
      profile_id: profileId,
    });
    const notifications = client.notifications.slice(before);

    writeJson(reportPath, {
      schema: 'octos.ux.backpressure_report.v1',
      generated_at: new Date().toISOString(),
      scenario_id: 'dropped-completion-backpressure',
      coverage: 'fixture-backed protocol/replay_lossy recovery; does not force a real dropped turn/completed writer-channel failure',
      endpoint,
      session_id: sessionId,
      profile_id: profileId,
      workspace,
      hello,
      capabilities: {
        supported_features: capabilities?.capabilities?.supported_features || [],
        supported_methods: supportedMethods,
      },
      profile,
      permissions_before: permissionsBefore,
      permission_set: permissionSet,
      opened,
      status,
      tools,
      turn_id: turnId,
      prompt,
      replay_lossy: {
        dropped_count: replayLossy.params?.dropped_count,
        has_last_durable_cursor: Boolean(replayLossy.params?.last_durable_cursor),
        params: replayLossy.params || {},
      },
      terminal: {
        method: terminal.method,
        params: terminal.params || {},
      },
      observed_methods: notifications.map((frame) => frame.method),
      snapshot,
    });
  } finally {
    await client.close();
  }
}

main().catch((error) => {
  console.error(error instanceof Error ? error.stack || error.message : String(error));
  process.exitCode = 1;
});
