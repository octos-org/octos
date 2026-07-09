// Fixture tests for the restart-reconnect visible contract in
// e2e/scripts/ux-tmux-validate.mjs, including the reconnect-events.jsonl
// artifact that e2e/matrix/octos-ux.toml declares for the lane.
// Run with: `node --test e2e/tests/ux/restart-validate.test.mjs`

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const repoRoot = resolve(__dirname, '..', '..', '..');
const validator = resolve(repoRoot, 'e2e', 'scripts', 'ux-tmux-validate.mjs');

const SESSION_ID = 'ux-restart-session';
const PROFILE_ID = 'coding';
const ENDPOINT = 'ws://127.0.0.1:0/ws';

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

const restartArtifacts = [
  'tui-capture-pre-restart.txt',
  'tui-capture-post-reconnect.txt',
  'pre-restart-snapshot.json',
  'post-reconnect-snapshot.json',
  'websocket-transcript.jsonl',
  'reconnect-events.jsonl',
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
  'session/snapshot',
  'session/hydrate',
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

function appuiRows() {
  const followUps = [
    'profile/local/create',
    'permission/profile/list',
    'permission/profile/set',
    'session/open',
    'session/status/read',
    'tool/status/list',
  ];
  const rows = [
    {
      direction: 'client_to_server',
      frame: { jsonrpc: '2.0', id: 'req-capabilities', method: 'config/capabilities/list', params: {} },
    },
    {
      direction: 'server_to_client',
      frame: {
        jsonrpc: '2.0',
        id: 'req-capabilities',
        result: {
          capabilities: {
            supported_methods: [...followUps, ...requiredWsMethods],
          },
        },
      },
    },
  ];
  followUps.forEach((method, index) => {
    const id = `req-${index}`;
    rows.push({ direction: 'client_to_server', frame: { jsonrpc: '2.0', id, method, params: {} } });
    rows.push({ direction: 'server_to_client', frame: { jsonrpc: '2.0', id, result: { ok: true } } });
  });
  rows.push({
    direction: 'server_to_client',
    frame: { jsonrpc: '2.0', method: 'turn/completed', params: { session_id: SESSION_ID } },
  });
  return rows;
}

function reconnectEventRows() {
  return [
    {
      ts: '2026-07-09T00:00:00.000Z',
      schema: 'octos.ux.restart_reconnect.reconnect_event.v1',
      phase: 'pre',
      event: 'connected',
      endpoint: ENDPOINT,
      session_id: SESSION_ID,
      profile_id: PROFILE_ID,
      cursor_seq: 4,
    },
    {
      ts: '2026-07-09T00:01:00.000Z',
      schema: 'octos.ux.restart_reconnect.reconnect_event.v1',
      phase: 'post',
      event: 'reconnected',
      endpoint: ENDPOINT,
      session_id: SESSION_ID,
      profile_id: PROFILE_ID,
      cursor_seq: 9,
    },
  ];
}

function snapshotValue(phase, seq) {
  return {
    schema: 'octos.ux.restart_reconnect.snapshot.v1',
    generated_at: '2026-07-09T00:00:00.000Z',
    phase,
    endpoint: ENDPOINT,
    session_id: SESSION_ID,
    profile_id: PROFILE_ID,
    cursor: { seq },
  };
}

function makeArtifactDir() {
  const dir = mkdtempSync(join(tmpdir(), 'octos-restart-validate-'));
  mkdirSync(dir, { recursive: true });

  writeJson(join(dir, 'scenario.json'), {
    schema: 'octos.ux.scenario.v1',
    artifact_abi: 'octos.ux.artifacts.v1',
    id: 'restart-reconnect',
    scenario_id: 'restart-reconnect',
    session_id: SESSION_ID,
    profile_id: PROFILE_ID,
    required_artifacts: requiredArtifacts,
  });
  writeJson(join(dir, 'summary.json'), {
    schema: 'octos.ux.summary.v1',
    status: 'passed',
    mode: 'run',
    placeholder_artifacts: false,
    real_tmux_launched: true,
    artifacts: artifactSummary(dir, [...requiredArtifacts, ...restartArtifacts]),
  });
  writeFileSync(join(dir, 'launch-command.txt'), 'octos-tui --mode protocol\n', 'utf8');
  writeFileSync(join(dir, 'input-replay.log'), 'line before restart prompt\n', 'utf8');
  writeFileSync(join(dir, 'server.log'), 'serve listening; restart fixture\n', 'utf8');
  writeFileSync(join(dir, 'tui-capture.txt'), 'Chat ready\n› \nComposer\n state Idle\n', 'utf8');
  writeFileSync(
    join(dir, 'tui-capture-pre-restart.txt'),
    'Before backend restart, answer briefly so the reconnect fixture has visible session state.\nComposer\n state Idle\n',
    'utf8',
  );
  writeFileSync(
    join(dir, 'tui-capture-post-reconnect.txt'),
    'Backend connection reconnected\nM19_RESTART_RECONNECT_FINAL_LINE\nComposer\n state Idle\n',
    'utf8',
  );
  writeJson(join(dir, 'terminal-size.json'), {
    schema: 'octos.ux.terminal_size.v1',
    cols: 100,
    rows: 30,
  });
  writeJson(join(dir, 'runtime-policy-stamp.json'), {
    schema: 'octos.ux.runtime_policy_stamp.v1',
    stamp: {},
  });
  writeJsonl(join(dir, 'appui-transcript.jsonl'), appuiRows());
  writeJsonl(join(dir, 'websocket-transcript.jsonl'), requiredWsMethods.map((method, index) => ({
    direction: 'client_to_server',
    frame: { jsonrpc: '2.0', id: `ws-${index}`, method, params: {} },
  })));
  writeJson(join(dir, 'pre-restart-snapshot.json'), snapshotValue('pre', 4));
  writeJson(join(dir, 'post-reconnect-snapshot.json'), snapshotValue('post', 9));
  writeJsonl(join(dir, 'reconnect-events.jsonl'), reconnectEventRows());
  return dir;
}

function runValidator(dir) {
  return execFileSync(process.execPath, [validator, dir], {
    cwd: repoRoot,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function expectContractFailure(dir, detailPattern) {
  assert.throws(
    () => runValidator(dir),
    (error) => {
      assert.equal(error.status, 1);
      const validation = JSON.parse(readFileSync(join(dir, 'validation.json'), 'utf8'));
      const failure = validation.failures.find((entry) => entry.id === 'restart_reconnect_visible_contract');
      assert.ok(failure, 'expected restart_reconnect_visible_contract failure');
      assert.match(failure.detail, detailPattern);
      return true;
    },
  );
}

test('restart fixture with reconnect events satisfies the restart contract', () => {
  const dir = makeArtifactDir();
  const out = runValidator(dir);
  const validation = JSON.parse(out);
  const check = validation.checks.find((entry) => entry.id === 'restart_reconnect_visible_contract');
  assert.equal(validation.status, 'passed');
  assert.equal(check.status, 'passed');
  assert.match(check.detail, /reconnect events/);
  assert.ok(check.evidence.includes('reconnect-events.jsonl'));
});

test('missing reconnect-events.jsonl fails the restart contract', () => {
  const dir = makeArtifactDir();
  rmSync(join(dir, 'reconnect-events.jsonl'));
  expectContractFailure(dir, /reconnect-events\.jsonl/);
});

test('reconnect events without a post-restart event fail the restart contract', () => {
  const dir = makeArtifactDir();
  writeJsonl(join(dir, 'reconnect-events.jsonl'), [reconnectEventRows()[0]]);
  expectContractFailure(dir, /missing a post-restart reconnect event/);
});

test('reconnect events for a different session fail the restart contract', () => {
  const dir = makeArtifactDir();
  writeJsonl(
    join(dir, 'reconnect-events.jsonl'),
    reconnectEventRows().map((row) => ({ ...row, session_id: 'ux-some-other-session' })),
  );
  expectContractFailure(dir, /does not match pre-restart snapshot session_id/);
});
