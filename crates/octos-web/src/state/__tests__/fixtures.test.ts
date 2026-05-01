// Layer 1 fixture tests — Vitest entry point.
//
// Two test suites per fixture (both kept FOREVER — see README for
// rationale):
//
//   1. "fails against current buggy reducer" — proves the fixture has
//      teeth. The buggy reducer is a deliberately-broken sentinel
//      modeling the production SPA's sticky-map fallback. If a fixture
//      starts to PASS against it, that's a signal the fixture has been
//      weakened (or the bug class moved).
//
//   2. "passes against fixed reducer" — proves the contract is
//      achievable and the fixture is well-formed. The fixed reducer is
//      the executable spec PR J's production reducer must match.
//
// The fixture suite list is deliberately exhaustive — adding a new
// fixture means adding two entries here. We keep it explicit (no
// glob/auto-import) so a new fixture can't be added that ONLY documents
// the bug without proving the fix is achievable.

import { describe, expect, it } from 'vitest';

import type { SseFixture } from './lib/fixture-types.js';
import {
  formatReplayResult,
  replayFixture,
} from './lib/replay-fixture.js';
import { buggyReducer } from './lib/buggy-reducer.js';
import { fixedReducer } from './lib/fixed-reducer.js';

import { rapidFireFiveFast } from './fixtures/rapid-fire-five-fast.fixture.js';
import { slowThenFastInterleave } from './fixtures/slow-then-fast-interleave.fixture.js';
import { spawnOnlyAckThenResult } from './fixtures/spawn-only-ack-then-result.fixture.js';
import { m89RecoveryTurn } from './fixtures/m89-recovery-turn.fixture.js';
import { reloadReplay } from './fixtures/reload-replay.fixture.js';
import { toolRetryCollapse } from './fixtures/tool-retry-collapse.fixture.js';
import { multiAttachmentDedup } from './fixtures/multi-attachment-dedup.fixture.js';

const SUITE: SseFixture[] = [
  rapidFireFiveFast,
  slowThenFastInterleave,
  spawnOnlyAckThenResult,
  m89RecoveryTurn,
  reloadReplay,
  toolRetryCollapse,
  multiAttachmentDedup,
];

describe('Layer 1: fixtures fail against the current (buggy) reducer', () => {
  // The buggy reducer is a deliberately-broken sentinel that models
  // LEAK 2 from the loophole-audit table: every delta binds to the
  // most-recent sticky thread, ignoring event.turn_id. It STAYS in the
  // codebase forever (even after PR J ships the production reducer) so
  // this negative-coverage gate continues to catch fixture weakening.
  //
  // If any of these tests reports "fixture passed against buggy
  // reducer", either the fixture has been weakened (false positive) or
  // the bug class has moved somewhere we don't yet model. Both cases
  // need investigation before the fixture is trusted.
  for (const fixture of SUITE) {
    it(`${fixture.name} fails against buggy reducer (proves teeth)`, () => {
      const result = replayFixture(fixture, buggyReducer);
      expect(
        result.pass,
        `Fixture "${fixture.name}" unexpectedly passed against the buggy reducer.\n${formatReplayResult(result)}`,
      ).toBe(false);
      // Sanity: buggy reducer must produce at LEAST one assertion failure
      // (otherwise the fixture has no teeth).
      expect(result.failures.length).toBeGreaterThan(0);
    });
  }
});

describe('Layer 1: fixtures pass against the fixed (reference) reducer', () => {
  // The fixed reducer is the executable spec PR J must match. If a
  // fixture fails here, the fixture is malformed (asserts the
  // unachievable) — not a real bug indicator.
  for (const fixture of SUITE) {
    it(`${fixture.name} passes against fixed reducer (contract is achievable)`, () => {
      const result = replayFixture(fixture, fixedReducer);
      expect(
        result.pass,
        `Fixture "${fixture.name}" failed against the FIXED reducer — the fixture is malformed (assertions unachievable).\n${formatReplayResult(result)}`,
      ).toBe(true);
    });
  }
});

describe('Layer 1: replay engine determinism', () => {
  // Replay the same fixture twice; assert byte-identical state. If the
  // engine grew nondeterminism (e.g. iteration order over a Set leaking)
  // we want this to fail loudly.
  it('produces identical final state across two replays of the same fixture', () => {
    const a = replayFixture(rapidFireFiveFast, fixedReducer);
    const b = replayFixture(rapidFireFiveFast, fixedReducer);
    expect(a.final_state.thread_order).toEqual(b.final_state.thread_order);
    const aT = Array.from(a.final_state.threads.entries());
    const bT = Array.from(b.final_state.threads.entries());
    expect(aT).toEqual(bT);
  });

  // The engine sorts events by `t` (ties → original index). Asserts the
  // sort is stable in both directions.
  it('orders events by t, tie-broken by authored index', () => {
    const fixture: SseFixture = {
      name: 'sort-stability',
      description: 'two events at same t maintain authored order',
      events: [
        // Authored: A (asst tail) before B (user). After sort: still A, B.
        {
          t: 100,
          type: 'message_delta',
          session_id: 's',
          turn_id: 'A',
          text: 'a-tail',
        },
        {
          t: 100,
          type: 'user_sent',
          session_id: 's',
          turn_id: 'B',
          cmid: 'B',
          text: 'b-question',
        },
      ],
      assertions: [],
    };
    const result = replayFixture(fixture, fixedReducer);
    // The buggy reducer would have rejected the delta (no turn_id A
    // bubble exists yet); the fixed reducer also rejects it but the order
    // of rejection events is what we assert.
    expect(result.final_state.rejected.length).toBe(1);
    expect(result.final_state.rejected[0]!.event.type).toBe('message_delta');
  });
});
