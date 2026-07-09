import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const repoRoot = resolve(__dirname, '..', '..', '..');
const validator = resolve(repoRoot, 'e2e', 'scripts', 'ux-tmux-validate.mjs');

const requiredArtifacts = [
  'scenario.json',
  'summary.json',
  'launch-command.txt',
  'terminal-size.json',
  'input-replay.log',
  'appui-transcript.jsonl',
  'server.log',
  'tui-capture.txt',
  'runtime-policy-stamp.json',
  'validation.json',
];

const requiredWsMethods = [
  'client_hello',
  'config/capabilities/list',
  'profile/local/create',
  'permission/profile/list',
  'permission/profile/set',
  'session/open',
  'session/status/read',
  'tool/status/list',
  'turn/start',
  'session/snapshot',
];

function writeJson(file, value) {
  writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`, 'utf8');
}

function writeJsonl(file, rows) {
  writeFileSync(file, `${rows.map((row) => JSON.stringify(row)).join('\n')}\n`, 'utf8');
}

function artifactSummary(dir, names) {
  return Object.fromEntries(names.map((name) => [name, { path: join(dir, name), exists: true, bytes: 1 }]));
}

function appuiRows({ terminal = 'legacy' } = {}) {
  const methods = [
    'config/capabilities/list',
    'profile/local/create',
    'permission/profile/list',
    'permission/profile/set',
    'session/open',
    'session/status/read',
    'tool/status/list',
  ];
  const rows = [];
  methods.forEach((method, index) => {
    const id = `req-${index}`;
    rows.push({ direction: 'client_to_server', frame: { jsonrpc: '2.0', id, method, params: {} } });
    rows.push({ direction: 'server_to_client', frame: { jsonrpc: '2.0', id, result: { ok: true } } });
  });
  if (terminal === 'envelope') {
    rows.push({
      direction: 'server_to_client',
      frame: {
        jsonrpc: '2.0',
        method: 'projection/envelope',
        params: {
          thread_id: 'ux-backpressure-turn',
          payload: { type: 'turn_completed', data: { token_usage: {} } },
        },
      },
    });
  } else {
    rows.push({
      direction: 'server_to_client',
      frame: { jsonrpc: '2.0', method: 'turn/completed', params: { session_id: 'ux-backpressure-session' } },
    });
  }
  return rows;
}

function makeArtifactDir({ trueDropCoverage, terminal = 'legacy' }) {
  const dir = mkdtempSync(join(tmpdir(), 'octos-backpressure-validate-'));
  mkdirSync(dir, { recursive: true });

  writeJson(join(dir, 'scenario.json'), {
    schema: 'octos.ux.scenario.v1',
    artifact_abi: 'octos.ux.artifacts.v1',
    id: 'dropped-completion-backpressure',
    scenario_id: 'dropped-completion-backpressure',
    session_id: 'ux-backpressure-session',
    required_artifacts: requiredArtifacts,
  });
  writeJson(join(dir, 'summary.json'), {
    schema: 'octos.ux.summary.v1',
    status: 'passed',
    mode: 'run',
    placeholder_artifacts: false,
    real_tmux_launched: true,
    artifacts: artifactSummary(dir, [
      ...requiredArtifacts,
      'tui-capture-replay-lossy.txt',
      'tui-capture-backpressure-final.txt',
      'tui-capture-backpressure-post-recovery.txt',
      'notification-log.jsonl',
      'backpressure-report.json',
      'websocket-transcript.jsonl',
    ]),
  });
  writeFileSync(join(dir, 'launch-command.txt'), 'octos-tui --mode protocol\n', 'utf8');
  writeFileSync(join(dir, 'input-replay.log'), 'line trigger backpressure\n', 'utf8');
  writeFileSync(
    join(dir, 'server.log'),
    trueDropCoverage
      ? [
        'forced turn/completed writer channel full fixture; aborting connection',
        'lifecycle notification not delivered; entry remains in ledger as delivery_failed',
        'writer channel full for lifecycle frame turn/completed',
      ].join('\n')
      : 'server completed without lifecycle drop text\n',
    'utf8',
  );
  writeFileSync(join(dir, 'tui-capture.txt'), 'Chat ready\nComposer\n state Idle\n', 'utf8');
  writeFileSync(join(dir, 'tui-capture-replay-lossy.txt'), 'Replay lossy: dropped durable notifications\nComposer\n state Idle\n', 'utf8');
  writeFileSync(join(dir, 'tui-capture-backpressure-final.txt'), 'Recovered after backpressure\nComposer\n state Idle\n', 'utf8');
  writeFileSync(join(dir, 'tui-capture-backpressure-post-recovery.txt'), 'OK\nDone\nComposer\n state Idle\n', 'utf8');
  writeJson(join(dir, 'terminal-size.json'), {
    schema: 'octos.ux.terminal_size.v1',
    cols: 100,
    rows: 30,
  });
  writeJson(join(dir, 'runtime-policy-stamp.json'), {
    schema: 'octos.ux.runtime_policy_stamp.v1',
    stamp: {},
  });
  writeJsonl(join(dir, 'appui-transcript.jsonl'), appuiRows({ terminal }));
  writeJsonl(join(dir, 'notification-log.jsonl'), [
    {
      direction: 'server_to_client',
      frame: {
        jsonrpc: '2.0',
        method: 'protocol/replay_lossy',
        params: { session_id: 'ux-backpressure-session', dropped_count: 1 },
      },
    },
  ]);
  writeJsonl(join(dir, 'websocket-transcript.jsonl'), requiredWsMethods.map((method, index) => ({
    direction: 'client_to_server',
    frame: { jsonrpc: '2.0', id: `ws-${index}`, method, params: {} },
  })));
  writeJson(join(dir, 'backpressure-report.json'), {
    schema: 'octos.ux.backpressure_report.v1',
    scenario_id: 'dropped-completion-backpressure',
    coverage: trueDropCoverage
      ? {
        true_dropped_turn_completed: true,
        writer_channel_turn_completed_drop: true,
      }
      : 'fixture-backed protocol/replay_lossy recovery; does not force a real dropped turn/completed writer-channel failure',
    session_id: 'ux-backpressure-session',
    replay_lossy: { dropped_count: 1 },
    terminal: terminal === 'envelope'
      ? {
        method: 'projection/envelope',
        legacy_equivalent: 'turn/completed',
        payload_type: 'turn_completed',
        thread_id: 'ux-backpressure-turn',
        params: {
          thread_id: 'ux-backpressure-turn',
          payload: { type: 'turn_completed', data: { token_usage: {} } },
        },
      }
      : { method: 'turn/completed', params: {} },
    forced_terminal_drop: {
      server_log_detected: trueDropCoverage,
    },
    snapshot: { session_id: 'ux-backpressure-session' },
  });
  return dir;
}

function runValidator(dir) {
  return execFileSync(process.execPath, [validator, dir], {
    cwd: repoRoot,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

test('fixture-only replay-lossy report does not satisfy dropped-completion contract', () => {
  const dir = makeArtifactDir({ trueDropCoverage: false });
  assert.throws(
    () => runValidator(dir),
    (error) => {
      assert.equal(error.status, 1);
      const validation = JSON.parse(readFileSync(join(dir, 'validation.json'), 'utf8'));
      const failure = validation.failures.find((entry) => entry.id === 'dropped_completion_backpressure_contract');
      assert.ok(failure);
      assert.match(failure.detail, /true dropped turn\/completed writer-channel coverage/);
      return true;
    },
  );
});

test('true terminal-drop coverage marker satisfies dropped-completion contract', () => {
  const dir = makeArtifactDir({ trueDropCoverage: true });
  const out = runValidator(dir);
  const validation = JSON.parse(out);
  const check = validation.checks.find((entry) => entry.id === 'dropped_completion_backpressure_contract');
  assert.equal(validation.status, 'passed');
  assert.equal(check.status, 'passed');
  assert.match(check.detail, /true dropped turn\/completed backpressure coverage/);
});


test('projection envelope terminal coverage satisfies dropped-completion contract', () => {
  const dir = makeArtifactDir({ trueDropCoverage: true, terminal: 'envelope' });
  const out = runValidator(dir);
  const validation = JSON.parse(out);
  const check = validation.checks.find((entry) => entry.id === 'dropped_completion_backpressure_contract');
  assert.equal(validation.status, 'passed');
  assert.equal(check.status, 'passed');
  assert.match(check.detail, /true dropped turn\/completed backpressure coverage/);
});
