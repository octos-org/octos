#!/usr/bin/env node

// M15 LoopRuntime live-soak. Closes acceptance bullet 5 (live soak) for
// #977 and provides #1023 (M17-E) loop-spawn coverage.
//
// Scenario:
//   1. Open an AppUI session.
//   2. Create a `fixed_interval` loop at the minimum interval the runtime
//      accepts (`LOOP_MIN_INTERVAL_SECONDS` in
//      crates/octos-cli/src/api/agent_orchestrator.rs — currently 60s).
//   3. Wait for ≥3 scheduled fires to drain through the master
//      continuation queue, observed via `task/updated` AppUI frames.
//   4. Call `loop/fire_now` once explicitly to confirm manual firing.
//   5. Capture each fire's assistant reply text (truncated to 200 chars).
//   6. Optionally probe a `self_paced` loop variant by prompting the model
//      to emit `<<loop-next-in: 60s>>`. Verify the loop re-fires after the
//      parsed delay.
//
// Run: DEEPSEEK_API_KEY=sk-... node e2e/scripts/m15-loop-runtime-stdio-soak.mjs
//
// All evidence is written under e2e/test-results-m15-loop-runtime-stdio/
// <UTC timestamp>/. Files are passed through `redactSecrets()` before
// they hit disk.

import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';

const repoRoot = path.resolve(import.meta.dirname, '..', '..');
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..+/, 'Z');
const runRoot = path.resolve(
  process.env.OCTOS_M15_LOOP_SOAK_DIR
    || path.join(repoRoot, 'e2e', 'test-results-m15-loop-runtime-stdio', stamp),
);
const dataDir = path.join(runRoot, 'data');
const workspace = path.join(runRoot, 'workspace');
const octosBin = process.env.OCTOS_BIN || path.join(repoRoot, 'target', 'debug', 'octos');
const profileId = process.env.OCTOS_M15_LOOP_PROFILE || 'm15-loop';
const sessionId =
  process.env.OCTOS_M15_LOOP_SESSION || `${profileId}:local:m15-loop-runtime-${stamp}`;
// LOOP_MIN_INTERVAL_SECONDS = 60 — 3 scheduled fires take ≥180s; allow
// generous slack for model latency. Self-paced variant adds ~120s.
const timeoutMs = Number(process.env.OCTOS_M15_LOOP_TIMEOUT_MS || 600_000);
const loopIntervalSeconds = Number(process.env.OCTOS_M15_LOOP_INTERVAL_SECONDS || 60);
const targetScheduledFires = Number(process.env.OCTOS_M15_LOOP_SCHEDULED_FIRES || 3);
const selfPacedEnabled = (process.env.OCTOS_M15_LOOP_SELF_PACED || 'true').toLowerCase() !== 'false';
const providerFamily = process.env.OCTOS_M15_LOOP_PROVIDER || 'deepseek';
const modelId = process.env.OCTOS_M15_LOOP_MODEL || 'deepseek-chat';
const providerKey =
  process.env.OCTOS_M15_LOOP_API_KEY
  || process.env.OCTOS_M15_NATIVE_API_KEY
  || process.env.OCTOS_M16_NATIVE_API_KEY
  || process.env.DEEPSEEK_API_KEY
  || '';

const observedTranscript = path.join(runRoot, 'client-observed-appui-transcript.jsonl');
const serverStderr = path.join(runRoot, 'server-stderr.log');
const summaryPath = path.join(runRoot, 'm15-loop-runtime-stdio-soak-summary.json');

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

function scanForRawSecrets() {
  // Best-effort guard that the redactor caught everything. We mirror the
  // m15 script's contract: refuse to claim a clean run if any file under
  // runRoot still contains a raw `sk-…` substring.
  const offenders = [];
  function walk(dir) {
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(full);
      } else if (entry.isFile()) {
        try {
          const text = fs.readFileSync(full, 'utf8');
          if (/\bsk-[A-Za-z0-9_-]{16,}\b/.test(text)) {
            offenders.push(path.relative(runRoot, full));
          }
        } catch {
          // ignore binary / unreadable files
        }
      }
    }
  }
  walk(runRoot);
  return offenders;
}

if (!providerKey) {
  const failure = {
    ok: false,
    error:
      'Missing provider key. Set DEEPSEEK_API_KEY (or OCTOS_M15_LOOP_API_KEY / OCTOS_M15_NATIVE_API_KEY) before running the loop runtime soak.',
    runRoot,
  };
  writeJson(summaryPath, failure);
  console.error(JSON.stringify(failure, null, 2));
  process.exit(2);
}

fs.mkdirSync(workspace, { recursive: true });
// Drop a couple of fixture files so the loop prompt has something to
// observe in the workspace — the model is asked to mention one of these.
fs.writeFileSync(
  path.join(workspace, 'NOTES.md'),
  '# Workspace notes\n\nThis directory is a soak fixture for the M15 LoopRuntime live test.\n',
);
fs.writeFileSync(
  path.join(workspace, 'CHANGELOG.txt'),
  'M15 LoopRuntime soak workspace fixture.\n',
);

const child = spawn(
  octosBin,
  ['serve', '--stdio', '--data-dir', dataDir, '--cwd', workspace],
  {
    cwd: repoRoot,
    env: {
      ...process.env,
      RUST_BACKTRACE: process.env.RUST_BACKTRACE || '1',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  },
);

const pending = new Map();
const notifications = [];
const messageDeltas = [];
const taskUpdated = [];
const turnCompleted = [];
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
    messageDeltas.push({
      at: Date.now(),
      text: typeof params.text === 'string' ? params.text : '',
    });
  } else if (frame.method === 'task/updated' || frame.method === 'agent/updated') {
    // Loop fires surface as `agent/updated` (the LoopFire continuation
    // drives a master continuation, which is observable as an
    // agent-state transition). We accept either to keep this
    // assertion compatible with both projection shapes.
    taskUpdated.push({ at: Date.now(), method: frame.method, params });
  } else if (frame.method === 'turn/completed') {
    turnCompleted.push({ at: Date.now(), params });
  }
});

function rpc(method, params = {}, rpcTimeoutMs = 30_000) {
  const id = `m15-loop-${++nextSeq}-${crypto.randomBytes(3).toString('hex')}`;
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

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(predicate, description, perCheckMs = 500, deadlineMs = timeoutMs) {
  const deadline = Date.now() + deadlineMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await sleep(perCheckMs);
  }
  throw new Error(`Timed out waiting for ${description}`);
}

function snapshotTurnCount() {
  return turnCompleted.length;
}

function captureAssistantExcerpt(beforeMessageDeltaCount) {
  // Join everything streamed AFTER the cursor and trim to 200 chars.
  const slice = messageDeltas.slice(beforeMessageDeltaCount);
  const joined = slice.map((entry) => entry.text).join('').trim();
  return joined.slice(0, 200);
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
  for (const method of ['loop/create', 'loop/list', 'loop/fire_now', 'loop/delete']) {
    assert(supported.includes(method), `missing AppUI method ${method}`);
  }

  const created = await rpc('profile/local/create', {
    name: 'M15 Loop Runtime',
    username: profileId,
    email: `${profileId}@example.test`,
  });
  assert(created?.profile_id === profileId, 'local profile was not created');

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
          api_key_env:
            providerFamily === 'deepseek'
              ? 'DEEPSEEK_API_KEY'
              : `${providerFamily.toUpperCase()}_API_KEY`,
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

  // ---- Scenario 1: fixed_interval loop ----
  const fixedLoop = await rpc(
    'loop/create',
    {
      session_id: sessionId,
      profile_id: profileId,
      mode: 'fixed_interval',
      interval_seconds: loopIntervalSeconds,
      prompt:
        'List one short observation about the workspace and stop. Reply in one sentence, ≤ 30 words.',
    },
    30_000,
  );
  assert(
    fixedLoop?.loop_id && /^loop_\d+$/.test(fixedLoop.loop_id),
    `loop/create did not return a loop_id: ${JSON.stringify(fixedLoop)}`,
  );
  const fixedLoopId = fixedLoop.loop_id;

  // Wait for ≥targetScheduledFires turns to complete on this session.
  const scheduledStart = Date.now();
  const scheduledCaptures = [];
  let lastDeltaCursor = messageDeltas.length;
  let lastTurnCursor = snapshotTurnCount();

  const scheduledDeadlineMs =
    targetScheduledFires * loopIntervalSeconds * 1000 + 180_000; // slack
  while (
    scheduledCaptures.length < targetScheduledFires
    && Date.now() - scheduledStart < scheduledDeadlineMs
  ) {
    await waitFor(
      () => snapshotTurnCount() > lastTurnCursor,
      `scheduled fire #${scheduledCaptures.length + 1}`,
      1000,
      scheduledDeadlineMs - (Date.now() - scheduledStart),
    );
    const excerpt = captureAssistantExcerpt(lastDeltaCursor);
    scheduledCaptures.push({
      fire_kind: 'scheduled',
      assistant_excerpt: excerpt,
      at_ms_from_start: Date.now() - scheduledStart,
    });
    lastDeltaCursor = messageDeltas.length;
    lastTurnCursor = snapshotTurnCount();
  }
  assert(
    scheduledCaptures.length >= targetScheduledFires,
    `only observed ${scheduledCaptures.length}/${targetScheduledFires} scheduled fires`,
  );
  for (const capture of scheduledCaptures) {
    assert(
      capture.assistant_excerpt.length > 0,
      `scheduled fire produced empty assistant reply: ${JSON.stringify(capture)}`,
    );
  }

  // ---- loop/fire_now ----
  lastDeltaCursor = messageDeltas.length;
  lastTurnCursor = snapshotTurnCount();
  const fireNowResult = await rpc(
    'loop/fire_now',
    {
      loop_id: fixedLoopId,
      session_id: sessionId,
      profile_id: profileId,
    },
    30_000,
  );
  assert(
    fireNowResult?.ok === true || fireNowResult?.status === 'queued',
    `loop/fire_now did not queue a fire: ${JSON.stringify(fireNowResult)}`,
  );
  await waitFor(
    () => snapshotTurnCount() > lastTurnCursor,
    'fire_now turn',
    1000,
    180_000,
  );
  const fireNowCaptures = [
    {
      fire_kind: 'fire_now',
      assistant_excerpt: captureAssistantExcerpt(lastDeltaCursor),
    },
  ];
  assert(
    fireNowCaptures[0].assistant_excerpt.length > 0,
    'fire_now produced empty assistant reply',
  );

  // ---- Scenario 2 (optional): self_paced loop ----
  const selfPacedCaptures = [];
  let selfPacedLoopId = null;
  if (selfPacedEnabled) {
    try {
      // #1136 codex P2 — delete the fixed-interval loop BEFORE the
      // self-paced probe. Otherwise the fixed loop's next scheduled
      // fire (at the ~60s mark) can complete in the same session
      // around the time the self-paced re-fire is expected, and the
      // turn-count-driven `waitFor` below cannot tell them apart —
      // attributing a fixed-loop reply to the self-paced capture and
      // counting it as a success.
      try {
        await rpc(
          'loop/delete',
          {
            loop_id: fixedLoopId,
            session_id: sessionId,
            profile_id: profileId,
          },
          5_000,
        );
      } catch (deleteErr) {
        console.error(`warning: failed to pre-delete fixed loop before self-paced probe: ${deleteErr.message ?? deleteErr}`);
      }
      const selfPaced = await rpc(
        'loop/create',
        {
          session_id: sessionId,
          profile_id: profileId,
          mode: 'self_paced',
          prompt:
            'Reply with one short workspace observation in ≤ 25 words, then on a new line emit exactly the sentinel `<<loop-next-in: 60s>>` (so the runtime knows when to fire you again). Do not call any tools.',
        },
        30_000,
      );
      assert(selfPaced?.loop_id, 'self_paced loop/create did not return a loop_id');
      selfPacedLoopId = selfPaced.loop_id;

      // Self-paced loops do not auto-fire; the first fire must be
      // explicitly triggered. Subsequent fires depend on the model's
      // `<<loop-next-in: …>>` sentinel.
      for (let i = 0; i < 2; i += 1) {
        lastDeltaCursor = messageDeltas.length;
        lastTurnCursor = snapshotTurnCount();
        if (i === 0) {
          const seed = await rpc(
            'loop/fire_now',
            {
              loop_id: selfPacedLoopId,
              session_id: sessionId,
              profile_id: profileId,
            },
            30_000,
          );
          assert(
            seed?.ok === true || seed?.status === 'queued',
            `self_paced seed fire_now did not queue: ${JSON.stringify(seed)}`,
          );
          await waitFor(
            () => snapshotTurnCount() > lastTurnCursor,
            `self_paced seed turn`,
            1000,
            120_000,
          );
        } else {
          // Wait for the model-driven re-fire (~60s after the prior
          // turn completes, per the emitted sentinel).
          await waitFor(
            () => snapshotTurnCount() > lastTurnCursor,
            `self_paced re-fire after <<loop-next-in: 60s>>`,
            1500,
            180_000,
          );
        }
        const excerpt = captureAssistantExcerpt(lastDeltaCursor);
        selfPacedCaptures.push({
          fire_kind: 'self_paced',
          assistant_excerpt: excerpt,
          sentinel_observed: /<<loop-next-in:\s*\d+\s*[smh]?\s*>>/.test(
            messageDeltas
              .slice(lastDeltaCursor)
              .map((entry) => entry.text)
              .join(''),
          ),
        });
      }
    } catch (error) {
      // Self-paced is best-effort — if the model declines to emit the
      // sentinel, surface it in the summary but do not fail the soak's
      // primary acceptance bullet (fixed_interval + fire_now).
      selfPacedCaptures.push({
        fire_kind: 'self_paced',
        assistant_excerpt: '',
        error: String(error?.message || error),
      });
    }
  }

  // Cleanup — best effort.
  try {
    await rpc('loop/delete', {
      loop_id: fixedLoopId,
      session_id: sessionId,
      profile_id: profileId,
    });
  } catch {
    // ignore
  }
  if (selfPacedLoopId) {
    try {
      await rpc('loop/delete', {
        loop_id: selfPacedLoopId,
        session_id: sessionId,
        profile_id: profileId,
      });
    } catch {
      // ignore
    }
  }

  const captures = [...scheduledCaptures, ...fireNowCaptures, ...selfPacedCaptures];
  const successfulSelfPaced = selfPacedCaptures.filter(
    (capture) => capture.assistant_excerpt && capture.assistant_excerpt.length > 0,
  );
  // Redact known config files BEFORE scanning so the scan reflects the
  // post-soak evidence shape (transcripts + summaries), not the
  // by-design `env_vars.DEEPSEEK_API_KEY` value written by `profile/create`.
  redactGeneratedSecrets();
  const offenders = scanForRawSecrets();

  const ok =
    scheduledCaptures.length >= targetScheduledFires
    && fireNowCaptures.length >= 1
    && captures
      .filter((capture) => capture.fire_kind !== 'self_paced')
      .every((capture) => capture.assistant_excerpt.length > 0)
    // Loop fires drive the MASTER continuation queue and surface as
    // `turn/started` + `turn/completed` (NOT `task/updated` or
    // `agent/updated`). The acceptance signal is therefore turn-level:
    // every scheduled fire + fire_now drives a complete turn.
    && turnCompleted.length >= targetScheduledFires + 1
    && offenders.length === 0;

  const summary = {
    ok,
    loopId: fixedLoopId,
    selfPacedLoopId,
    scheduledFires: scheduledCaptures.length,
    fireNowFires: fireNowCaptures.length,
    selfPacedFires: successfulSelfPaced.length,
    captures,
    modelId,
    secretScanClean: offenders.length === 0,
    secretScanOffenders: offenders,
    runRoot,
    dataDir,
    workspace,
    sessionId,
    profileId,
    providerFamily,
    notifications: notifications.length,
    taskUpdated: taskUpdated.length,
    turnCompleted: turnCompleted.length,
    loopIntervalSeconds,
    stderrPreview: stderrText.split(/\r?\n/).filter(Boolean).slice(-20),
    host: os.hostname(),
  };
  writeJson(summaryPath, summary);
  console.log(JSON.stringify(summary, null, 2));
  // #1136 codex P2: CI runners look at the exit code, not the
  // `ok` field inside summary.json. Without this, a soak that flags
  // `ok: false` (e.g. missing fires, secret-scan offenders) still
  // exits 0 and would be treated as a pass by downstream automation.
  if (!summary.ok) {
    process.exitCode = 1;
  }
}

main()
  .catch((error) => {
    redactGeneratedSecrets();
    const offenders = scanForRawSecrets();
    const failure = {
      ok: false,
      error: String(error?.stack || error),
      runRoot,
      notifications: notifications.length,
      taskUpdated: taskUpdated.length,
      turnCompleted: turnCompleted.length,
      secretScanClean: offenders.length === 0,
      secretScanOffenders: offenders,
      stderrPreview: stderrText.split(/\r?\n/).filter(Boolean).slice(-40),
    };
    writeJson(summaryPath, failure);
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
