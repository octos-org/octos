// Standalone driver: print every fixture's failure report against the
// buggy reducer. Used for the PR body "demonstrated to fail" evidence
// and for human inspection. Run via tsx:
//
//   npx tsx scripts/print-buggy-output.mjs
//
// (tsx is dev-only; not added to package.json — one-shot tool.)
import { replayFixture, formatReplayResult } from '../src/state/__tests__/lib/replay-fixture.ts';
import { buggyReducer } from '../src/state/__tests__/lib/buggy-reducer.ts';
import { rapidFireFiveFast } from '../src/state/__tests__/fixtures/rapid-fire-five-fast.fixture.ts';
import { slowThenFastInterleave } from '../src/state/__tests__/fixtures/slow-then-fast-interleave.fixture.ts';
import { spawnOnlyAckThenResult } from '../src/state/__tests__/fixtures/spawn-only-ack-then-result.fixture.ts';
import { m89RecoveryTurn } from '../src/state/__tests__/fixtures/m89-recovery-turn.fixture.ts';
import { reloadReplay } from '../src/state/__tests__/fixtures/reload-replay.fixture.ts';
import { toolRetryCollapse } from '../src/state/__tests__/fixtures/tool-retry-collapse.fixture.ts';
import { multiAttachmentDedup } from '../src/state/__tests__/fixtures/multi-attachment-dedup.fixture.ts';

const SUITE = [
  rapidFireFiveFast,
  slowThenFastInterleave,
  spawnOnlyAckThenResult,
  m89RecoveryTurn,
  reloadReplay,
  toolRetryCollapse,
  multiAttachmentDedup,
];

console.log('Layer 1 fixture replay against the BUGGY reducer:\n');
let totalMs = 0;
let failCount = 0;
for (const f of SUITE) {
  const r = replayFixture(f, buggyReducer);
  totalMs += r.duration_ms;
  if (!r.pass) failCount += 1;
  console.log(formatReplayResult(r));
  console.log('');
}
console.log(
  `Summary: ${failCount}/${SUITE.length} fixtures fail against buggy reducer in ${totalMs.toFixed(2)}ms total.`,
);
