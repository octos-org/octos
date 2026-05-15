import { test, expect } from '@playwright/test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

test.describe('M12 solo AppUI soak artifacts', () => {
  test('fixture transport validates no-OTP and permission evidence artifacts', () => {
    const repoRoot = path.resolve(__dirname, '..', '..');
    const probe = path.join(repoRoot, 'scripts', 'm12-solo-appui-probe.mjs');
    const tmpRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'octos-m12-solo-e2e-'));
    const outDir = path.join(tmpRoot, 'artifacts');
    const workspace = path.join(tmpRoot, 'workspace');
    const dataDir = path.join(tmpRoot, 'data');

    try {
      const result = spawnSync(
        process.execPath,
        [
          probe,
          '--transport',
          'fixture',
          '--out-dir',
          outDir,
          '--workspace',
          workspace,
          '--data-dir',
          dataDir,
          '--profile-id',
          'm12solo',
          '--session-id',
          'm12solo:local:e2e#fixture',
          '--strict',
        ],
        { cwd: repoRoot, encoding: 'utf8' },
      );
      expect(result.status, `${result.stdout}\n${result.stderr}`).toBe(0);

      const transcriptPath = path.join(outDir, 'appui-transcript.jsonl');
      const summaryPath = path.join(outDir, 'soak-summary.json');
      const policyPath = path.join(outDir, 'runtime-policy-stamp.json');
      const toolsPath = path.join(outDir, 'tool-registry-snapshot.json');
      const approvalsPath = path.join(outDir, 'approval-events.jsonl');
      const filesystemPath = path.join(outDir, 'filesystem-probe.json');

      for (const file of [
        transcriptPath,
        summaryPath,
        policyPath,
        toolsPath,
        approvalsPath,
        filesystemPath,
      ]) {
        expect(fs.existsSync(file), `missing artifact ${file}`).toBe(true);
      }

      const transcript = fs.readFileSync(transcriptPath, 'utf8');
      expect(transcript).toContain('"method":"profile/local/create"');
      expect(transcript).not.toMatch(/auth\/(send_code|verify)/);

      const summary = JSON.parse(fs.readFileSync(summaryPath, 'utf8'));
      expect(summary.status).toBe('passed');
      expect(summary.no_otp_assertion.ok).toBe(true);
      expect(summary.approval_events.requested).toBe(0);
      expect(summary.cases.map((c: { name: string }) => c.name)).toEqual(
        expect.arrayContaining([
          'workspace-write',
          'approval-never-sandbox-active',
          'danger-full-access-approval-never',
          'tenant-danger-rejection',
        ]),
      );

      const policy = JSON.parse(fs.readFileSync(policyPath, 'utf8'));
      expect(policy.stamp.approval_policy).toBe('never');
      expect(policy.stamp.sandbox_mode).toBe('danger-full-access');
      expect(policy.stamp.filesystem_scope).toBe('host');

      const filesystem = JSON.parse(fs.readFileSync(filesystemPath, 'utf8'));
      expect(filesystem.cases.map((c: { name: string }) => c.name)).toEqual(
        expect.arrayContaining([
          'workspace-write',
          'approval-never-sandbox-active',
          'danger-full-access-approval-never',
        ]),
      );
    } finally {
      if (process.env.OCTOS_M12_SOAK_TEST_KEEP !== '1') {
        fs.rmSync(tmpRoot, { recursive: true, force: true });
      }
    }
  });
});
