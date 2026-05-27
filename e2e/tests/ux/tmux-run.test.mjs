import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const repoRoot = resolve(__dirname, '..', '..', '..');
const runner = resolve(repoRoot, 'e2e', 'scripts', 'ux-tmux-run.mjs');

function makeEnv(runId) {
  const root = mkdtempSync(join(tmpdir(), 'octos-ux-tmux-run-test-'));
  return {
    env: {
      ...process.env,
      OCTOS_UX_TMUX_RUN_ID: runId,
      OCTOS_UX_TMUX_OUT_ROOT: join(root, 'out'),
      OCTOS_UX_TMUX_RUNTIME_ROOT: join(root, 'runtime'),
    },
    outDir: join(root, 'out', runId, 'stdio-happy-path'),
  };
}

function run(args, env = process.env) {
  return execFileSync(process.execPath, [runner, ...args], {
    cwd: repoRoot,
    env,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function readJson(file) {
  return JSON.parse(readFileSync(file, 'utf8'));
}

test('help documents keep-session and no-validate flags', () => {
  const out = run(['--help']);
  assert.match(out, /--keep-session/);
  assert.match(out, /--no-validate/);
});

test('self-test writes and validates the artifact skeleton', () => {
  const { env, outDir } = makeEnv('ux-run-test-self');
  const out = run(['--self-test', 'stdio-happy-path'], env);
  assert.match(out, /Self-test passed:/);

  const summary = readJson(join(outDir, 'summary.json'));
  const validation = readJson(join(outDir, 'validation.json'));
  const capture = readFileSync(join(outDir, 'tui-capture.txt'), 'utf8');
  const transcript = readFileSync(join(outDir, 'appui-transcript.jsonl'), 'utf8');
  assert.equal(summary.validation_status, 'passed');
  assert.equal(validation.status, 'passed');
  assert.match(capture, /M19_STDIO_HAPPY_PATH_FINAL_LINE/);
  assert.match(capture, /\bComposer\b/);
  assert.match(transcript, /"method":"client_hello"/);
});

test('dry-run can skip automatic validation', () => {
  const { env, outDir } = makeEnv('ux-run-test-no-validate');
  const out = run(['stdio-happy-path', '--dry-run', '--no-validate'], env);
  assert.match(out, /Validation skipped/);

  const summary = readJson(join(outDir, 'summary.json'));
  assert.equal(summary.validation_status, 'skipped');
  assert.equal(summary.validation_skipped, true);
  assert.equal(existsSync(join(outDir, 'validation.json')), false);
});
