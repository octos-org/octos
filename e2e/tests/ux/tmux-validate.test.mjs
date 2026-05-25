// Unit tests for the M19-D UX tmux artifact validator.
// Run with: `node --test e2e/tests/ux/tmux-validate.test.mjs`

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import {
  cpSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const REPO_ROOT = resolve(__dirname, '..', '..', '..');
const VALIDATOR = resolve(REPO_ROOT, 'e2e', 'scripts', 'ux-tmux-validate.mjs');
const FIXTURE = resolve(REPO_ROOT, 'e2e', 'fixtures', 'ux-artifact-self-test');

const REQUIRED_CHECK_IDS = [
  'artifact_abi',
  'appui_transcript_parseable',
  'capabilities_before_followups',
  'no_unadvertised_methods_called',
  'rendered_screen_no_known_bug_patterns',
  'final_answer_visible',
  'composer_usable',
  'secret_redaction',
];

function copyFixture() {
  const dir = mkdtempSync(join(tmpdir(), 'ux-tmux-validate-'));
  cpSync(FIXTURE, dir, { recursive: true });
  return dir;
}

function readJson(file) {
  return JSON.parse(readFileSync(file, 'utf8'));
}

function writeJson(file, value) {
  writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function runValidator(dir) {
  try {
    const stdout = execFileSync(process.execPath, [VALIDATOR, dir], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return {
      status: 0,
      result: JSON.parse(stdout),
    };
  } catch (error) {
    assert.ok(error.stdout, error.stderr?.toString() || error.message);
    return {
      status: error.status,
      result: JSON.parse(error.stdout.toString()),
    };
  }
}

function withFixture(fn) {
  const dir = copyFixture();
  try {
    return fn(dir);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

function failureIds(result) {
  return new Set(result.failures.map((failure) => failure.id));
}

function makeRunFixture(dir, marker = 'M19_FINAL_MARKER') {
  const scenario = readJson(join(dir, 'scenario.json'));
  scenario.final_marker = marker;
  writeJson(join(dir, 'scenario.json'), scenario);

  const summary = readJson(join(dir, 'summary.json'));
  summary.mode = 'run';
  summary.status = 'passed';
  summary.placeholder_artifacts = false;
  summary.real_tmux_launched = true;
  summary.final_marker = marker;
  writeJson(join(dir, 'summary.json'), summary);
}

test('self-test fixture passes and exposes the M19-D validator ids', () => withFixture((dir) => {
  const { status, result } = runValidator(dir);
  assert.equal(status, 0);
  assert.equal(result.status, 'passed');
  assert.equal(result.layout_snapshot?.status, 'available');
  assert.ok(
    result.layout_snapshot.regions.some((region) => region.name === 'composer'),
    'layout snapshot should include the composer region',
  );
  const checkIds = new Set(result.checks.map((check) => check.id));
  for (const id of REQUIRED_CHECK_IDS) {
    assert.ok(checkIds.has(id), `expected check id ${id}`);
  }
}));

test('capabilities_before_followups fails when session opens before capability evidence', () => withFixture((dir) => {
  writeFileSync(
    join(dir, 'appui-transcript.jsonl'),
    [
      JSON.stringify({
        direction: 'client_to_server',
        frame: { jsonrpc: '2.0', id: 'bad-1', method: 'session/open', params: {} },
      }),
      JSON.stringify({
        direction: 'server_to_client',
        frame: { jsonrpc: '2.0', id: 'bad-1', result: { opened: true } },
      }),
      '',
    ].join('\n'),
  );
  const { status, result } = runValidator(dir);
  assert.equal(status, 1);
  assert.ok(failureIds(result).has('capabilities_before_followups'));
}));

test('no_unadvertised_methods_called fails client RPCs outside supported_methods', () => withFixture((dir) => {
  writeFileSync(
    join(dir, 'appui-transcript.jsonl'),
    [
      JSON.stringify({
        direction: 'client_to_server',
        frame: { jsonrpc: '2.0', id: 'caps-1', method: 'config/capabilities/list', params: {} },
      }),
      JSON.stringify({
        direction: 'server_to_client',
        frame: {
          jsonrpc: '2.0',
          id: 'caps-1',
          result: { capabilities: { supported_methods: ['session/open'] } },
        },
      }),
      JSON.stringify({
        direction: 'client_to_server',
        frame: { jsonrpc: '2.0', id: 'bad-2', method: 'turn/start', params: {} },
      }),
      JSON.stringify({
        direction: 'server_to_client',
        frame: { jsonrpc: '2.0', id: 'bad-2', result: { ok: true } },
      }),
      '',
    ].join('\n'),
  );
  const { status, result } = runValidator(dir);
  assert.equal(status, 1);
  assert.ok(failureIds(result).has('no_unadvertised_methods_called'));
}));

test('final_answer_visible fails passed real runs without the declared marker', () => withFixture((dir) => {
  makeRunFixture(dir, 'M19_FINAL_MARKER');
  const { status, result } = runValidator(dir);
  assert.equal(status, 1);
  assert.ok(failureIds(result).has('final_answer_visible'));
}));

test('composer_usable fails when the composer row is hidden', () => withFixture((dir) => {
  writeFileSync(
    join(dir, 'tui-capture.txt'),
    [
      'Octos TUI',
      '',
      'Messages',
      'Assistant: Self-test turn completed.',
      '',
      'state Ready',
      '',
    ].join('\n'),
  );
  const { status, result } = runValidator(dir);
  assert.equal(status, 1);
  assert.ok(failureIds(result).has('composer_usable'));
}));

test('rendered_screen_no_known_bug_patterns fails known rendered regressions', () => withFixture((dir) => {
  writeFileSync(
    join(dir, 'tui-capture.txt'),
    [
      'Octos TUI',
      '',
      'Messages',
      '[x] Point me at a project path',
      '',
      'Composer',
      '>',
      '',
      'state Ready',
      '',
    ].join('\n'),
  );
  const { status, result } = runValidator(dir);
  assert.equal(status, 1);
  assert.ok(failureIds(result).has('rendered_screen_no_known_bug_patterns'));
}));

test('secret_redaction fails retained artifacts containing common token shapes', () => withFixture((dir) => {
  writeFileSync(
    join(dir, 'server.log'),
    'leaked token: sk-ant-abcdefghijklmnopqrstuvwxyz123456\n',
  );
  const { status, result } = runValidator(dir);
  assert.equal(status, 1);
  assert.ok(failureIds(result).has('secret_redaction'));
}));
