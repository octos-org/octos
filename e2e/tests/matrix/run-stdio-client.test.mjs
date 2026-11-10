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

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { StdioClient } from "../../matrix/run.mjs";

function freshTmp(label) {
  return fs.mkdtempSync(path.join(os.tmpdir(), `m22-stdio-client-${label}-`));
}

test("rpc() on a backend that exits with pending requests fails those requests too", async () => {
  const tmp = freshTmp("pending");
  const transcriptLog = path.join(tmp, "transcript.jsonl");
  const stderrLog = path.join(tmp, "stderr.log");

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
  const pending = client.rpc("any/method", {});

  // Allow exit to land.
  await client.exited;

  // The pending promise must resolve (not hang, not reject by
  // timeout) with a typed backend_exited error. Clear the watchdog
  // when the race completes so the test process exits promptly.
  let hangTimer;
  const watchdog = new Promise((_, reject) => {
    hangTimer = setTimeout(
      () => reject(new Error("hung waiting for rejection")),
      4_000,
    );
  });
  try {
    const frame = await Promise.race([pending, watchdog]);
    assert.ok(frame.error, "frame must carry an error");
    assert.equal(frame.error.data?.kind, "backend_exited");
  } finally {
    clearTimeout(hangTimer);
  }
});
