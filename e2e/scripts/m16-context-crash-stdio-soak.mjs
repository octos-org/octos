#!/usr/bin/env node

import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';

const repoRoot = path.resolve(import.meta.dirname, '..', '..');
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..+/, 'Z');
const runRoot = path.resolve(
  process.env.OCTOS_M16_CONTEXT_CRASH_DIR
    || path.join(repoRoot, 'e2e', 'test-results-m16-context-crash-stdio', stamp),
);
const dataDir = path.join(runRoot, 'data');
const workspace = path.join(runRoot, 'workspace');
const octosBin = process.env.OCTOS_BIN || path.join(repoRoot, 'target', 'debug', 'octos');
const profileId = process.env.OCTOS_M16_CONTEXT_CRASH_PROFILE || 'm16-context';
const sessionId =
  process.env.OCTOS_M16_CONTEXT_CRASH_SESSION
  || `${profileId}:local:m16-context-crash-${stamp}`;
const timeoutMs = Number(process.env.OCTOS_M16_CONTEXT_CRASH_TIMEOUT_MS || 60_000);
const postRestartTurns = Number(process.env.OCTOS_M16_CONTEXT_CRASH_POST_TURNS || 0);
const pressureRepeat = Number(process.env.OCTOS_M16_CONTEXT_CRASH_PRESSURE_REPEAT || 200);
const crashResponseDelayMs = Number(process.env.OCTOS_M16_CONTEXT_CRASH_RESPONSE_DELAY_MS || 15_000);
const crashDelayAfterRequests = Number(process.env.OCTOS_M16_CONTEXT_CRASH_DELAY_AFTER_REQUESTS || 2);
const finalMarker = 'M16_CONTEXT_CRASH_FINAL_LINE';

assert(Number.isInteger(postRestartTurns) && postRestartTurns >= 0, 'post-restart turns must be a non-negative integer');
assert(Number.isInteger(pressureRepeat) && pressureRepeat >= 1, 'pressure repeat must be a positive integer');

fs.mkdirSync(workspace, { recursive: true });
fs.writeFileSync(
  path.join(workspace, 'context_crash_fixture.txt'),
  'Context crash fixture workspace file.\n',
);

function appendJsonl(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.appendFileSync(file, `${JSON.stringify(value)}\n`);
}

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    let body = '';
    req.setEncoding('utf8');
    req.on('data', (chunk) => {
      body += chunk;
    });
    req.on('end', () => resolve(body));
    req.on('error', reject);
  });
}

async function startFakeOpenAiServer() {
  const requests = [];
  const server = http.createServer(async (req, res) => {
    if (req.method !== 'POST' || req.url !== '/v1/chat/completions') {
      res.writeHead(404, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ error: { message: 'not found' } }));
      return;
    }
    const raw = await readBody(req);
    let body;
    try {
      body = JSON.parse(raw);
    } catch {
      body = { raw };
    }
    requests.push({
      at: new Date().toISOString(),
      stream: body.stream === true,
      model: body.model,
      messages: body.messages || [],
    });
    if (crashResponseDelayMs > 0 && requests.length >= crashDelayAfterRequests) {
      await new Promise((resolve) => setTimeout(resolve, crashResponseDelayMs));
      if (res.destroyed) {
        return;
      }
    }

    const content = `Context crash proof response ${requests.length}. ${finalMarker}`;
    if (body.stream === true) {
      res.writeHead(200, {
        'content-type': 'text/event-stream',
        'cache-control': 'no-cache',
        connection: 'keep-alive',
      });
      res.write(
        `data: ${JSON.stringify({
          choices: [{ index: 0, delta: { content }, finish_reason: null }],
          usage: null,
        })}\n\n`,
      );
      res.write(
        `data: ${JSON.stringify({
          choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
          usage: { prompt_tokens: 32, completion_tokens: 9, total_tokens: 41 },
        })}\n\n`,
      );
      res.write('data: [DONE]\n\n');
      res.end();
      return;
    }

    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({
      id: `fake-${requests.length}`,
      object: 'chat.completion',
      choices: [
        {
          index: 0,
          message: { role: 'assistant', content },
          finish_reason: 'stop',
        },
      ],
      usage: { prompt_tokens: 32, completion_tokens: 9, total_tokens: 41 },
    }));
  });
  await new Promise((resolve, reject) => {
    server.listen(0, '127.0.0.1', resolve);
    server.once('error', reject);
  });
  const { port } = server.address();
  return {
    baseUrl: `http://127.0.0.1:${port}/v1`,
    requests,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

class StdioClient {
  constructor(label, extraEnv = {}) {
    this.label = label;
    this.transcript = path.join(runRoot, `${label}-appui-transcript.jsonl`);
    this.stderrPath = path.join(runRoot, `${label}-server-stderr.log`);
    this.pending = new Map();
    this.notifications = [];
    this.messageDeltas = [];
    this.turnCompleted = false;
    this.turnErrored = null;
    this.stderrText = '';
    this.nextSeq = 0;
    this.child = spawn(octosBin, ['serve', '--stdio', '--data-dir', dataDir, '--cwd', workspace], {
      cwd: repoRoot,
      env: {
        ...process.env,
        ...extraEnv,
        OCTOS_CONTEXT_COMPACT_THRESHOLD_TOKENS: '1',
        OCTOS_CONTEXT_COMPACT_KEEP_ITEMS: '4',
        RUST_BACKTRACE: process.env.RUST_BACKTRACE || '1',
      },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    this.child.stderr.on('data', (chunk) => {
      const text = chunk.toString();
      this.stderrText += text;
      fs.appendFileSync(this.stderrPath, text);
    });
    const rl = readline.createInterface({ input: this.child.stdout });
    rl.on('line', (line) => this.onLine(line));
  }

  async waitSpawn() {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`${this.label}: octos serve --stdio did not spawn`)),
        10_000,
      );
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

  onLine(line) {
    let frame;
    try {
      frame = JSON.parse(line);
    } catch {
      appendJsonl(this.transcript, { direction: 'server_to_client_non_json', line });
      return;
    }
    appendJsonl(this.transcript, { direction: 'server_to_client', frame });
    if (frame && Object.prototype.hasOwnProperty.call(frame, 'id') && frame.id != null) {
      const request = this.pending.get(String(frame.id));
      if (request) {
        this.pending.delete(String(frame.id));
        if (frame.error) {
          request.reject(new Error(`${this.label}: RPC ${request.method} failed: ${JSON.stringify(frame.error)}`));
        } else {
          request.resolve(frame.result);
        }
      }
      return;
    }
    if (!frame?.method) return;
    this.notifications.push(frame);
    const params = frame.params || {};
    if (frame.method === 'message/delta') {
      this.messageDeltas.push(String(params.text || ''));
    } else if (frame.method === 'turn/completed') {
      this.turnCompleted = true;
    } else if (frame.method === 'turn/error') {
      this.turnErrored = params;
    }
  }

  rpc(method, params = {}, rpcTimeoutMs = 15_000) {
    const id = `${this.label}-${++this.nextSeq}-${crypto.randomBytes(3).toString('hex')}`;
    const frame = { jsonrpc: '2.0', id, method, params };
    appendJsonl(this.transcript, { direction: 'client_to_server', frame });
    this.child.stdin.write(`${JSON.stringify(frame)}\n`);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${this.label}: RPC timeout for ${method}`));
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
    });
  }

  async waitFor(predicate, description) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error(`${this.label}: timed out waiting for ${description}`);
  }

  async close() {
    try {
      this.child.stdin.end();
    } catch {
      // ignore cleanup failure
    }
    if (!this.child.killed) {
      this.child.kill('SIGTERM');
    }
    await new Promise((resolve) => {
      const timer = setTimeout(() => {
        if (!this.child.killed) this.child.kill('SIGKILL');
        resolve();
      }, 2_000);
      this.child.once('exit', () => {
        clearTimeout(timer);
        resolve();
      });
    });
  }

  async kill(signal = 'SIGKILL') {
    try {
      this.child.kill(signal);
    } catch {
      // ignore cleanup failure
    }
    await new Promise((resolve) => {
      const timer = setTimeout(resolve, 2_000);
      this.child.once('exit', () => {
        clearTimeout(timer);
        resolve();
      });
    });
  }
}

function contextStateOf(result) {
  return result?.context_state || result?.status?.context_state || null;
}

async function openProfileAndSession(client, fakeBaseUrl) {
  const capabilities = await client.rpc('config/capabilities/list');
  const supported = capabilities?.capabilities?.supported_methods || [];
  for (const method of ['profile/local/create', 'profile/llm/upsert', 'session/open', 'turn/start', 'session/status/read']) {
    assert(supported.includes(method), `${client.label}: missing AppUI method ${method}`);
  }
  await client.rpc('profile/local/create', {
    name: 'M16 Context Restart',
    username: profileId,
    email: `${profileId}@example.test`,
  });
  await client.rpc('profile/llm/upsert', {
    profile_id: profileId,
    set_primary: true,
    selection: {
      family_id: 'openai',
      model_id: 'gpt-4o-mini',
      route: {
        route_id: 'local-openai-fixture',
        api_type: 'openai',
        base_url: fakeBaseUrl,
        api_key_env: 'OCTOS_FAKE_OPENAI_KEY',
      },
    },
    api_key: 'test-key',
  });
  return client.rpc('session/open', {
    session_id: sessionId,
    profile_id: profileId,
    cwd: workspace,
  });
}

async function runTurn(client, text) {
  client.turnCompleted = false;
  client.turnErrored = null;
  client.messageDeltas = [];
  const turnId = crypto.randomUUID();
  await client.rpc('turn/start', {
    session_id: sessionId,
    profile_id: profileId,
    turn_id: turnId,
    input: [{ kind: 'text', text }],
  });
  await client.waitFor(
    () => client.turnCompleted || client.turnErrored,
    `turn ${turnId} terminal event`,
  );
  assert(!client.turnErrored, `${client.label}: turn errored: ${JSON.stringify(client.turnErrored)}`);
  assert(
    client.messageDeltas.join('').includes(finalMarker),
    `${client.label}: final marker missing from streamed response`,
  );
  return turnId;
}

async function startTurnAndWaitForModelRequest(client, fake, text, minRequestCount) {
  client.turnCompleted = false;
  client.turnErrored = null;
  client.messageDeltas = [];
  const turnId = crypto.randomUUID();
  await client.rpc('turn/start', {
    session_id: sessionId,
    profile_id: profileId,
    turn_id: turnId,
    input: [{ kind: 'text', text }],
  });
  await client.waitFor(
    () => fake.requests.length >= minRequestCount,
    `model request for crash turn ${turnId}`,
  );
  return turnId;
}

async function main() {
  const fake = await startFakeOpenAiServer();
  let first;
  let second;
  try {
    first = new StdioClient('before-restart');
    await first.waitSpawn();
    const firstOpen = await openProfileAndSession(first, fake.baseUrl);
    const seedTurnId = await runTurn(
      first,
      `Seed committed history before crash injection. ${'seed context history '.repeat(pressureRepeat)}`,
    );
    const seedStatus = await first.rpc('session/status/read', {
      session_id: sessionId,
      profile_id: profileId,
    });
    const seedContext = contextStateOf(seedStatus);
    assert(seedContext, 'seed status missing context_state');
    assert(seedContext.last_compaction_id, 'seed status missing last_compaction_id');
    const crashTurnId = await startTurnAndWaitForModelRequest(
      first,
      fake,
      `Crash after prompt-time context persistence while model is blocked. ${'alpha beta gamma '.repeat(pressureRepeat)}`,
      2,
    );
    const inFlightStatus = await first.rpc('session/status/read', {
      session_id: sessionId,
      profile_id: profileId,
    });
    const inFlightContext = contextStateOf(inFlightStatus);
    assert(inFlightContext, 'in-flight status missing context_state');
    assert(inFlightContext.last_compaction_id, 'in-flight status missing last_compaction_id');
    assert(inFlightContext.transcript_hash, 'in-flight status missing transcript_hash');
    assert(inFlightContext.recovery_state === 'exact', `in-flight recovery_state was ${inFlightContext.recovery_state}`);
    await first.kill('SIGKILL');

    second = new StdioClient('after-restart');
    await second.waitSpawn();
    const secondOpen = await second.rpc('session/open', {
      session_id: sessionId,
      profile_id: profileId,
      cwd: workspace,
    });
    const secondOpenContext = contextStateOf({ context_state: secondOpen?.opened?.context_state });
    const secondInitialStatus = await second.rpc('session/status/read', {
      session_id: sessionId,
      profile_id: profileId,
    });
    const secondInitialContext = contextStateOf(secondInitialStatus);
    assert(secondInitialContext, 'second status missing context_state');
    assert(
      ['exact', 'rebuilt'].includes(secondInitialContext.recovery_state),
      `crash restart must report exact or rebuilt recovery, got ${secondInitialContext.recovery_state}`,
    );
    const exactCrashRecovery =
      secondInitialContext.recovery_state === 'exact'
      && secondInitialContext.last_compaction_id === inFlightContext.last_compaction_id
      && secondInitialContext.transcript_hash === inFlightContext.transcript_hash;
    const rebuiltCrashRecovery =
      secondInitialContext.recovery_state === 'rebuilt'
      && secondInitialContext.transcript_hash !== inFlightContext.transcript_hash;
    assert(
      exactCrashRecovery || rebuiltCrashRecovery,
      'crash restart must either preserve the exact prompt-time context or explicitly report rebuilt recovery',
    );
    const secondTurns = [];
    for (let i = 0; i < Math.max(postRestartTurns, 1); i += 1) {
      const turnId = await runTurn(
        second,
        `Continue after exact context crash recovery, post-restart turn ${i + 1}. ${'delta epsilon zeta '.repeat(pressureRepeat)}`,
      );
      const status = await second.rpc('session/status/read', {
        session_id: sessionId,
        profile_id: profileId,
      });
      const context = contextStateOf(status);
      assert(context, `post-restart turn ${i + 1} status missing context_state`);
      assert(['exact', 'rebuilt'].includes(context.recovery_state), `post-restart turn ${i + 1} recovery_state was ${context.recovery_state}`);
      assert(context.last_compaction_id, `post-restart turn ${i + 1} missing last_compaction_id`);
      secondTurns.push({ turnId, context });
    }
    const secondContext = secondTurns[secondTurns.length - 1].context;

    const contextLedgerDir = path.join(dataDir, 'profiles');
    const contextLedgerFiles = fs.existsSync(dataDir)
      ? Array.from(fs.readdirSync(dataDir, { recursive: true }))
          .filter((name) => String(name).includes('context_ledgers'))
          .map((name) => String(name))
          .sort()
      : [];
    assert(contextLedgerFiles.length > 0, 'no context ledger files found under data dir');

    const summary = {
      ok: true,
      runRoot,
      dataDir,
      workspace,
      sessionId,
      profileId,
      fakeBaseUrl: fake.baseUrl,
      firstOpenContext: contextStateOf({ context_state: firstOpen?.opened?.context_state }),
      secondOpenContext,
      seedTurnId,
      seedContext,
      crashTurnId,
      exactCrashRecovery,
      rebuiltCrashRecovery,
      preRestartTurns: 0,
      postRestartTurns: secondTurns.length,
      pressureRepeat,
      inFlightContext,
      secondInitialContext,
      secondTurns,
      firstContext: inFlightContext,
      secondContext,
      contextLedgerFiles,
      fakeRequestCount: fake.requests.length,
      fakeRequests: fake.requests.map((request) => ({
        at: request.at,
        stream: request.stream,
        model: request.model,
        messageCount: request.messages.length,
        hasConversationSummary: JSON.stringify(request.messages).includes('[Conversation summary]'),
      })),
      notifications: {
        beforeRestart: first.notifications.length,
        afterRestart: second.notifications.length,
      },
      host: os.hostname(),
    };
    writeJson(path.join(runRoot, 'm16-context-crash-stdio-summary.json'), summary);
    console.log(JSON.stringify(summary, null, 2));
  } catch (error) {
    const failure = {
      ok: false,
      error: String(error?.stack || error),
      runRoot,
      sessionId,
      profileId,
      fakeRequests: fake.requests.map((request) => ({
        at: request.at,
        stream: request.stream,
        model: request.model,
        messageCount: request.messages.length,
      })),
      beforeRestart: first
        ? {
            notifications: first.notifications.length,
            turnCompleted: first.turnCompleted,
            turnErrored: first.turnErrored,
            stderrPreview: first.stderrText.split(/\r?\n/).filter(Boolean).slice(-40),
          }
        : null,
      afterRestart: second
        ? {
            notifications: second.notifications.length,
            turnCompleted: second.turnCompleted,
            turnErrored: second.turnErrored,
            stderrPreview: second.stderrText.split(/\r?\n/).filter(Boolean).slice(-40),
          }
        : null,
    };
    writeJson(path.join(runRoot, 'm16-context-crash-stdio-summary.json'), failure);
    console.error(JSON.stringify(failure, null, 2));
    process.exitCode = 1;
  } finally {
    if (first) await first.close();
    if (second) await second.close();
    await fake.close();
  }
}

main();
