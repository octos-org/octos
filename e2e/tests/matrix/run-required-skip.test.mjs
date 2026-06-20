import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const repoRoot = path.resolve(__dirname, '..', '..', '..');
const cli = path.join(repoRoot, 'e2e', 'matrix', 'run.mjs');

function freshDir(label) {
  return fs.mkdtempSync(path.join(os.tmpdir(), `m22-required-skip-${label}-`));
}

function writeManifest(dir, { required }) {
  const manifest = path.join(dir, 'onboarding.toml');
  fs.writeFileSync(
    manifest,
    [
      '[pack]',
      'name = "onboarding"',
      'contract = "UPCR-2026-018"',
      'issue = "1056"',
      '',
      '[[scenarios]]',
      'name = "release-placeholder"',
      'tier = "release"',
      'transport = "stdio"',
      'description = "placeholder release lane"',
      `required = ${required ? 'true' : 'false'}`,
      'validators = ["no_otp_emitted"]',
      'artifacts = ["rpc-transcript.jsonl"]',
      'skip_reason = "not wired yet"',
      '',
    ].join('\n'),
  );
  return manifest;
}

function runMatrix({ manifest, outDir }) {
  return execFileSync(
    process.execPath,
    [cli, '--pack', 'onboarding', '--tier', 'release', '--manifest', manifest],
    {
      cwd: repoRoot,
      env: {
        ...process.env,
        OCTOS_BIN: process.execPath,
        OCTOS_MATRIX_DIR: outDir,
      },
      encoding: 'utf8',
    },
  );
}

test('required skipped release scenario fails the matrix run', () => {
  const dir = freshDir('required');
  const manifest = writeManifest(dir, { required: true });
  const outDir = path.join(dir, 'out');

  assert.throws(
    () => runMatrix({ manifest, outDir }),
    (error) => {
      assert.equal(error.status, 1);
      return true;
    },
  );

  const summary = JSON.parse(fs.readFileSync(path.join(outDir, 'summary.json'), 'utf8'));
  assert.equal(summary.ok, false);
  assert.equal(summary.counts.failed, 1);
  assert.equal(summary.counts.skipped, 0);
  assert.equal(summary.scenarios[0].status, 'failed');
  assert.equal(summary.scenarios[0].required, true);
  assert.equal(summary.scenarios[0].failure_reason, 'required scenario cannot be skipped');
});

test('optional skipped release scenario remains a skipped audit row', () => {
  const dir = freshDir('optional');
  const manifest = writeManifest(dir, { required: false });
  const outDir = path.join(dir, 'out');

  const stdout = runMatrix({ manifest, outDir });
  const parsed = JSON.parse(stdout.slice(stdout.indexOf('{')));

  assert.equal(parsed.ok, true);
  assert.equal(parsed.counts.failed, 0);
  assert.equal(parsed.counts.skipped, 1);
  assert.equal(parsed.scenarios[0].status, 'skipped');
  assert.equal(parsed.scenarios[0].required, false);
});
