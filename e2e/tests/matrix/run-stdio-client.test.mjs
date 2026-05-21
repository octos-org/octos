// Codex P2 follow-up on #1157 (M22 onboarding matrix):
//
// When the spawned `octos serve` process exits before / during an RPC
// (startup crash, panic, wrong binary at OCTOS_BIN), the original
// runner would emit `EPIPE` on `child.stdin` with no handler and
// Node would terminate — leaving no scenario.json or summary.json
// artifact behind. The StdioClient must instead:
//
//   1. Catch the EPIPE error event so Node doesn't crash.
//   2. Reject pending RPCs with a typed `backend_exited` error.
//   3. Reject *future* RPCs with the same typed error (don't write
//      to a closed stdin).
//
// This test exercises the rejection paths using a fake "backend"
// that exits immediately (`node -e "process.exit(7)"`).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import { StdioClient } from '../../matrix/run.mjs';

function freshTmp(label) {
  return fs.mkdtempSync(path.join(os.tmpdir(), `m22-stdio-client-${label}-`));
}

test('rpc() on a backend that already exited resolves to a typed backend_exited error', async () => {
  const tmp = freshTmp('exit');
  const transcriptLog = path.join(tmp, 'transcript.jsonl');
  const stderrLog = path.join(tmp, 'stderr.log');

  const client = new StdioClient({
    // node exits immediately — emulates a binary that crashes on
    // startup or the wrong file at OCTOS_BIN.
    octosBin: process.execPath,
    dataDir: tmp,
    workspace: tmp,
    repoRoot: tmp,
    stderrLog,
    transcriptLog,
    timeoutMs: 2_000,
  });

  // Replace the spawn-launched arguments with a tiny exit-7 inline
  // script. (Node is the binary; the args came from the constructor
  // and were `['serve', '--stdio', ...]`; we override via wait then
  // rpc.) Simplest: wait for the child to exit naturally — but our
  // fake "octos" was launched with `serve --stdio --data-dir ...`,
  // which node will fail-parse and exit non-zero. That is exactly
  // the failure mode codex described.

  // Give the child a chance to spawn then exit.
  await client.exited;
  assert.notEqual(client.exitInfo, null, 'exitInfo must be captured');

  // Now issue an rpc — this is the line that previously wrote to a
  // dead stdin and crashed Node with EPIPE.
  const frame = await client.rpc('any/method', { foo: 1 });
  assert.ok(frame.error, 'frame must be an error response');
  assert.equal(frame.error.data?.kind, 'backend_exited');
  assert.match(frame.error.message, /exited unexpectedly|has exited|stdin write failed/);

  // Transcript must contain the client_to_server attempt so a human
  // operator can see what the runner tried to send.
  const transcript = fs.readFileSync(transcriptLog, 'utf8').trim().split('\n').map(JSON.parse);
  assert.ok(
    transcript.some((row) => row.direction === 'client_to_server' && row.frame.method === 'any/method'),
    'transcript must record the attempted client_to_server frame',
  );
});

test('rpc() on a backend that exits with pending requests fails those requests too', async () => {
  const tmp = freshTmp('pending');
  const transcriptLog = path.join(tmp, 'transcript.jsonl');
  const stderrLog = path.join(tmp, 'stderr.log');

  // Use a short-lived python/sh fallback: a process that holds stdin
  // open briefly then exits. node with `-e` works the same way.
  const client = new StdioClient({
    octosBin: process.execPath,
    dataDir: tmp,
    workspace: tmp,
    repoRoot: tmp,
    stderrLog,
    transcriptLog,
    timeoutMs: 5_000,
  });

  // Don't wait for exit — issue rpc while the child is "alive"
  // (although in this test it actually exits very fast because the
  // args don't parse). We're verifying the race where exit happens
  // AFTER write was attempted: pending must still be rejected.
  const pending = client.rpc('any/method', {});

  // Allow exit to land.
  await client.exited;

  // The pending promise must resolve (not hang, not reject by
  // timeout) with a typed backend_exited error. Clear the watchdog
  // when the race completes so the test process exits promptly.
  let hangTimer;
  const watchdog = new Promise((_, reject) => {
    hangTimer = setTimeout(() => reject(new Error('hung waiting for rejection')), 4_000);
  });
  try {
    const frame = await Promise.race([pending, watchdog]);
    assert.ok(frame.error, 'frame must carry an error');
    assert.equal(frame.error.data?.kind, 'backend_exited');
  } finally {
    clearTimeout(hangTimer);
  }
});
