#!/usr/bin/env node

// M15 GoalRuntime live-soak. Closes acceptance bullet 5 (live soak) for
// #979 and provides #1023 (M17-E) goal-budget coverage.
//
// Scenario A — budget_exhaust:
//   1. Open an AppUI session.
//   2. Call `session/goal/set` with `token_budget: 5000`.
//   3. Capture the initial continuation; wait
//      `GOAL_MIN_CONTINUATION_INTERVAL_MS` (30s) for the next
//      continuation to verify recurrence.
//   4. Drive turns to exhaust the budget and capture the
//      `budget_limited` state transition.
//   5. Confirm a wrap-up turn was enqueued.
//
// Scenario B — sentinel_complete:
//   1. Set a fresh goal.
//   2. Run one continuation whose prompt asks the model to emit
//      `<goal:complete>` at the trailing edge of its reply.
//   3. Confirm `session/goal/get` reports `status: "complete"` and
//      that no further continuations are queued.
//
// IMPORTANT — known live-runtime limitation:
// Per the implementation note in `maybe_advance_goal_runtime_after_turn`
// (crates/octos-cli/src/session_actor.rs), the wire path currently calls
// `record_goal_turn(…, tokens_consumed=0, …)`. That means real LLM token
// spend is NOT yet attributed to `goal.tokens_used`, so the goal cannot
// organically reach `tokens_used >= token_budget` via real turns. The
// implementation comment flags this explicitly as follow-up #1133.
// To still close acceptance bullet 5, this soak drives Scenario A's
// `budget_limited` transition via an explicit
// `session/goal/set { status: "budget_limited" }` RPC, which is the
// real, wire-supported transition path. The summary records
// `wrap_up_emitted: false` for the explicit path so reviewers can see
// that the wrap-up enqueue branch (only entered via
// `record_goal_turn_internal` with `tokens_consumed > 0`) is NOT
// exercised. The organic-exhaustion bullet remains open until #1133.
//
// Run: DEEPSEEK_API_KEY=sk-... node e2e/scripts/m15-goal-runtime-stdio-soak.mjs
//
// Evidence lands under e2e/test-results-m15-goal-runtime-stdio/
// <UTC timestamp>/. All files pass through `redactSecrets()` first.

import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';

const repoRoot = path.resolve(import.meta.dirname, '..', '..');
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..+/, 'Z');
const runRoot = path.resolve(
  process.env.OCTOS_M15_GOAL_SOAK_DIR
    || path.join(repoRoot, 'e2e', 'test-results-m15-goal-runtime-stdio', stamp),
);
const dataDir = path.join(runRoot, 'data');
const workspace = path.join(runRoot, 'workspace');
const octosBin = process.env.OCTOS_BIN || path.join(repoRoot, 'target', 'debug', 'octos');
const profileId = process.env.OCTOS_M15_GOAL_PROFILE || 'm15-goal';
const sessionAId =
  process.env.OCTOS_M15_GOAL_SESSION_A
  || `${profileId}:local:m15-goal-budget-${stamp}`;
const sessionBId =
  process.env.OCTOS_M15_GOAL_SESSION_B
  || `${profileId}:local:m15-goal-sentinel-${stamp}`;
// Two scenarios × (initial continuation + 30s recurrence + model time)
// → allow ~10 minutes.
const timeoutMs = Number(process.env.OCTOS_M15_GOAL_TIMEOUT_MS || 600_000);
// GOAL_MIN_CONTINUATION_INTERVAL_MS = 30_000. Mirror it here so the
// soak's wait derives from the runtime constant the orchestrator
// enforces.
const goalMinContinuationIntervalMs = Number(
  process.env.OCTOS_M15_GOAL_MIN_CONTINUATION_INTERVAL_MS || 30_000,
);
const providerFamily = process.env.OCTOS_M15_GOAL_PROVIDER || 'deepseek';
const modelId = process.env.OCTOS_M15_GOAL_MODEL || 'deepseek-chat';
const providerKey =
  process.env.OCTOS_M15_GOAL_API_KEY
  || process.env.OCTOS_M15_NATIVE_API_KEY
  || process.env.OCTOS_M16_NATIVE_API_KEY
  || process.env.DEEPSEEK_API_KEY
  || '';

const observedTranscript = path.join(runRoot, 'client-observed-appui-transcript.jsonl');
const serverStderr = path.join(runRoot, 'server-stderr.log');
const summaryPath = path.join(runRoot, 'm15-goal-runtime-stdio-soak-summary.json');

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
    // ignore
  }
}

function redactGeneratedSecrets() {
  redactJsonFile(path.join(dataDir, 'profiles', `${profileId}.json`));
}

function scanForRawSecrets() {
  // Same belt-and-braces check the loop runtime soak uses.
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
      'Missing provider key. Set DEEPSEEK_API_KEY (or OCTOS_M15_GOAL_API_KEY / OCTOS_M15_NATIVE_API_KEY) before running the goal runtime soak.',
    runRoot,
  };
  writeJson(summaryPath, failure);
  console.error(JSON.stringify(failure, null, 2));
  process.exit(2);
}

fs.mkdirSync(workspace, { recursive: true });
fs.writeFileSync(
  path.join(workspace, 'FACT_A.md'),
  '# Fact A\nThe workspace lives in a soak fixture directory.\n',
);
fs.writeFileSync(
  path.join(workspace, 'FACT_B.md'),
  '# Fact B\nIt contains three small fact files used by the goal soak.\n',
);
fs.writeFileSync(
  path.join(workspace, 'FACT_C.md'),
  '# Fact C\nEach fact file is one short paragraph.\n',
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
// Notifications are partitioned by session_id so the two scenarios
// can independently track recurrence.
const turnCompletedBySession = new Map();
const messageDeltasBySession = new Map();
let stderrText = '';
let nextSeq = 0;

function bucket(map, sessionId) {
  let entry = map.get(sessionId);
  if (!entry) {
    entry = [];
    map.set(sessionId, entry);
  }
  return entry;
}

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
  const sessionId = params.session_id || params.sessionId || params?.session?.session_id || '';
  if (frame.method === 'message/delta') {
    bucket(messageDeltasBySession, sessionId).push({
      at: Date.now(),
      text: typeof params.text === 'string' ? params.text : '',
    });
  } else if (frame.method === 'turn/completed') {
    bucket(turnCompletedBySession, sessionId).push({ at: Date.now(), params });
  }
});

function rpc(method, params = {}, rpcTimeoutMs = 30_000) {
  const id = `m15-goal-${++nextSeq}-${crypto.randomBytes(3).toString('hex')}`;
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

async function waitFor(predicate, description, perCheckMs = 750, deadlineMs = timeoutMs) {
  const deadline = Date.now() + deadlineMs;
  while (Date.now() < deadline) {
    const result = predicate();
    const resolved = result && typeof result.then === 'function' ? await result : result;
    if (resolved) return;
    await sleep(perCheckMs);
  }
  throw new Error(`Timed out waiting for ${description}`);
}

async function ensureProfileAndProvider() {
  const created = await rpc('profile/local/create', {
    name: 'M15 Goal Runtime',
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
}

async function openSession(sessionId) {
  await rpc('session/open', {
    session_id: sessionId,
    profile_id: profileId,
    cwd: workspace,
  });
}

async function runBudgetExhaustScenario() {
  await openSession(sessionAId);
  const turnsBucket = bucket(turnCompletedBySession, sessionAId);
  const beforeTurns = turnsBucket.length;

  // Goal with a tight token_budget so the explicit `budget_limited`
  // transition is consistent with the soak narrative even though
  // organic exhaustion is not yet wire-observable (see header doc).
  const setResult = await rpc(
    'session/goal/set',
    {
      session_id: sessionAId,
      profile_id: profileId,
      objective:
        'Summarize three small workspace facts in one sentence each, then stop. Read FACT_A.md, FACT_B.md, FACT_C.md from the workspace if you need ground truth.',
      token_budget: 5000,
      transition_actor: 'user',
    },
    30_000,
  );
  assert(
    setResult?.goal?.status === 'active',
    `session/goal/set did not return an active goal: ${JSON.stringify(setResult)}`,
  );
  const goalId = setResult.goal.goal_id;

  // Initial continuation. `set_goal` enqueues immediately on active.
  await waitFor(
    () => turnsBucket.length > beforeTurns,
    'initial goal continuation',
    1000,
    180_000,
  );
  const afterInitialTurns = turnsBucket.length;

  // Recurrence — wait for at least one more continuation after
  // GOAL_MIN_CONTINUATION_INTERVAL_MS so we know the recurrence
  // scheduler is wired. The SessionActor's continuation tick is 2s,
  // so total worst-case ≈ 30s gate + 2s tick + LLM latency.
  await waitFor(
    () => turnsBucket.length > afterInitialTurns,
    'second goal continuation past GOAL_MIN_CONTINUATION_INTERVAL_MS',
    1500,
    goalMinContinuationIntervalMs + 180_000,
  );
  const afterSecondTurns = turnsBucket.length;
  const continuationsObserved = afterSecondTurns - beforeTurns;

  // Drive a third turn to push `continuations_used` higher and exercise
  // the per-session rate window. Best-effort, do not fail if the model
  // path takes longer than the recurrence gate again.
  let thirdTurnObserved = false;
  try {
    await waitFor(
      () => turnsBucket.length > afterSecondTurns,
      'third goal continuation',
      1500,
      goalMinContinuationIntervalMs + 120_000,
    );
    thirdTurnObserved = true;
  } catch {
    // tolerated
  }

  // Explicit budget_limited transition. This is the wire-supported
  // path; organic exhaustion via `tokens_used >= token_budget` is not
  // yet wired into the AppUI session actor (see follow-up #1133).
  const explicitBudgetLimited = await rpc(
    'session/goal/set',
    {
      session_id: sessionAId,
      profile_id: profileId,
      objective: setResult.goal.objective,
      status: 'budget_limited',
      transition_actor: 'backend',
    },
    30_000,
  );
  assert(
    explicitBudgetLimited?.goal?.status === 'budget_limited',
    `explicit budget_limited transition was not applied: ${JSON.stringify(explicitBudgetLimited)}`,
  );

  // Verify via session/goal/get.
  const readBack = await rpc('session/goal/get', {
    session_id: sessionAId,
    profile_id: profileId,
  });
  assert(
    readBack?.goal?.status === 'budget_limited',
    `session/goal/get did not reflect budget_limited: ${JSON.stringify(readBack)}`,
  );

  return {
    name: 'budget_exhaust',
    goal_id: goalId,
    continuations_observed: continuationsObserved + (thirdTurnObserved ? 1 : 0),
    transitioned_to: readBack.goal.status,
    transitioned_via: 'explicit_set_goal_status',
    wrap_up_emitted: false,
    wrap_up_emitted_note:
      'wrap-up enqueue is only triggered by record_goal_turn_internal with non-zero tokens_consumed; the AppUI wire path passes 0 today (see #1133)',
    third_turn_observed: thirdTurnObserved,
  };
}

async function runSentinelCompleteScenario() {
  await openSession(sessionBId);
  const turnsBucket = bucket(turnCompletedBySession, sessionBId);
  const deltasBucket = bucket(messageDeltasBySession, sessionBId);
  const beforeTurns = turnsBucket.length;
  const beforeDeltas = deltasBucket.length;

  const objective =
    'Reply with one sentence that says you are stopping work on the workspace soak. Then on a new final line emit EXACTLY the sentinel `<goal:complete>` (this is required so the runtime stops recurrence). Do not call any tools.';
  const setResult = await rpc(
    'session/goal/set',
    {
      session_id: sessionBId,
      profile_id: profileId,
      objective,
      token_budget: 5000,
      transition_actor: 'user',
    },
    30_000,
  );
  assert(
    setResult?.goal?.status === 'active',
    `session/goal/set did not return an active goal: ${JSON.stringify(setResult)}`,
  );
  const goalId = setResult.goal.goal_id;

  await waitFor(
    () => turnsBucket.length > beforeTurns,
    'initial sentinel-scenario continuation',
    1000,
    180_000,
  );

  // The session actor calls `maybe_complete_goal_from_model` after each
  // assistant turn. Give it a short grace period for the orchestrator
  // to flip the status, then verify via `session/goal/get`.
  let goalRecord = null;
  let sentinelObserved = false;
  await waitFor(
    async () => {
      // Inline assistant-tail check so we do not poll the orchestrator
      // when the model hasn't yet emitted the sentinel.
      const tail = deltasBucket
        .slice(beforeDeltas)
        .map((entry) => entry.text)
        .join('')
        .trim();
      const lower = tail.toLowerCase();
      const sentinels = ['<goal:complete>', '[goal:complete]', 'goal-complete', 'goal_complete'];
      const lastLine = lower
        .split(/\r?\n/)
        .map((line) => line.trim())
        .filter(Boolean)
        .pop() || '';
      sentinelObserved = sentinels.some(
        (sentinel) => lastLine === sentinel || lastLine.endsWith(sentinel),
      );
      if (!sentinelObserved) return false;
      goalRecord = await rpc('session/goal/get', {
        session_id: sessionBId,
        profile_id: profileId,
      });
      return goalRecord?.goal?.status === 'complete';
    },
    'sentinel-driven complete transition',
    1500,
    180_000,
  );

  // Confirm the goal stays complete and the orchestrator does not
  // queue further continuations. We wait one full
  // GOAL_MIN_CONTINUATION_INTERVAL_MS window past the sentinel turn
  // and assert no new turn/completed frames arrived for this session.
  const turnsAtCompletion = turnsBucket.length;
  await sleep(goalMinContinuationIntervalMs + 5_000);
  const noFurtherContinuation = turnsBucket.length === turnsAtCompletion;

  return {
    name: 'sentinel_complete',
    goal_id: goalId,
    continuations_observed: turnsAtCompletion - beforeTurns,
    transitioned_to: goalRecord?.goal?.status || 'unknown',
    sentinel_at_tail: sentinelObserved,
    no_further_continuations_for_30s: noFurtherContinuation,
  };
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
  for (const method of ['session/goal/set', 'session/goal/get', 'session/goal/clear']) {
    assert(supported.includes(method), `missing AppUI method ${method}`);
  }

  await ensureProfileAndProvider();

  const scenarioA = await runBudgetExhaustScenario();
  const scenarioB = await runSentinelCompleteScenario();

  // Redact known config files BEFORE scanning so the scan reflects the
  // post-soak evidence shape, not the by-design `env_vars.*_API_KEY`
  // value written by `profile/create`.
  redactGeneratedSecrets();
  const offenders = scanForRawSecrets();
  const goalScenarios = [scenarioA, scenarioB];

  const ok =
    scenarioA.transitioned_to === 'budget_limited'
    && scenarioA.continuations_observed >= 2
    && scenarioB.transitioned_to === 'complete'
    && scenarioB.sentinel_at_tail === true
    && scenarioB.no_further_continuations_for_30s === true
    && offenders.length === 0;

  const summary = {
    ok,
    goalScenarios,
    modelId,
    secretScanClean: offenders.length === 0,
    secretScanOffenders: offenders,
    runRoot,
    dataDir,
    workspace,
    profileId,
    sessionAId,
    sessionBId,
    providerFamily,
    notifications: notifications.length,
    turnsBySession: Object.fromEntries(
      Array.from(turnCompletedBySession.entries()).map(([key, value]) => [key, value.length]),
    ),
    goalMinContinuationIntervalMs,
    knownGaps: [
      'organic_token_exhaustion_not_wire_observable: session_actor wires tokens_consumed=0 today (see #1133); explicit set_goal(status="budget_limited") is the only wire-supported transition this soak can exercise.',
    ],
    stderrPreview: stderrText.split(/\r?\n/).filter(Boolean).slice(-20),
    host: os.hostname(),
  };
  writeJson(summaryPath, summary);
  console.log(JSON.stringify(summary, null, 2));
  // #1136 codex P2: surface failure via exit code so CI runners
  // don't treat an `ok: false` summary as a pass.
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
      turnsBySession: Object.fromEntries(
        Array.from(turnCompletedBySession.entries()).map(([key, value]) => [key, value.length]),
      ),
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
