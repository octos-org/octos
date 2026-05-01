// slow-then-fast-interleave — the live-thread-interleave pattern (#649).
//
// User Q1 kicks off a deep_research turn that takes ~30 seconds. Before it
// completes, user Q2 sends a fast lookup. The fast turn finishes first.
// LATER, Q1's deep_research result (with a `.md` attachment) arrives —
// but the sticky map has rotated to Q2 by then. Under the bug, Q1's
// research output lands in Q2's bubble.
//
// Ground truth: this is exactly the symptom captured in
// e2e/tests/live-thread-interleave.spec.ts:225-235 ("late background
// result inherited Q2's thread_id from the sticky map"). The fixture is
// the deterministic equivalent.

import type { SseFixture } from '../lib/fixture-types.js';

const SID = 'slow-fast-session';

export const slowThenFastInterleave: SseFixture = {
  name: 'slow-then-fast-interleave',
  description:
    'Slow deep_research Q1 + fast lookup Q2. Q1 result arrives after Q2 sticky has rotated. Buggy reducer routes Q1 research evidence into Q2 bubble.',
  issues: [649, 740],
  session_id: SID,
  events: [
    // ── Q1: slow deep_research ───────────────────────────────────────────
    {
      t: 0,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'slow-Q1',
      cmid: 'slow-Q1',
      text: 'Research the latest M9.5 release notes thoroughly.',
    },
    { t: 100, type: 'turn_started', session_id: SID, turn_id: 'slow-Q1' },
    // Acknowledgment delta, then long silence while research runs.
    {
      t: 500,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'slow-Q1',
      text: 'Researching, this will take a moment...',
    },

    // ── Q2: fast lookup, while Q1 still in flight ────────────────────────
    {
      t: 5000,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'fast-Q2',
      cmid: 'fast-Q2',
      text: 'What is 2+2?',
    },
    { t: 5100, type: 'turn_started', session_id: SID, turn_id: 'fast-Q2' },
    {
      t: 5300,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'fast-Q2',
      text: '4',
    },
    { t: 5400, type: 'turn_completed', session_id: SID, turn_id: 'fast-Q2' },

    // ── Q1: research result arrives (with the canonical .md attachment) ──
    {
      t: 30000,
      type: 'background_result',
      session_id: SID,
      turn_id: 'slow-Q1',
      text: 'Research summary: M9.5 ledger durability + scope-aware approvals shipped this week. See attached.',
      attachments: [
        {
          filename: 'm9-5-research.md',
          path: '/tmp/research/m9-5-research.md',
        },
      ],
    },
    { t: 30100, type: 'turn_completed', session_id: SID, turn_id: 'slow-Q1' },
  ],
  assertions: [
    {
      kind: 'thread_order',
      expected: ['slow-Q1', 'fast-Q2'],
    },
    {
      kind: 'thread_equals',
      turn_id: 'slow-Q1',
      expect: {
        user: 'Research the latest M9.5 release notes thoroughly.',
        asst: 'Researching, this will take a moment...Research summary: M9.5 ledger durability + scope-aware approvals shipped this week. See attached.',
      },
    },
    {
      kind: 'thread_equals',
      turn_id: 'fast-Q2',
      expect: { user: 'What is 2+2?', asst: '4' },
    },
    {
      kind: 'thread_has_attachment',
      turn_id: 'slow-Q1',
      filename: 'm9-5-research.md',
    },
    {
      kind: 'no_misroute',
      allowed_turn_ids: ['slow-Q1', 'fast-Q2'],
    },
  ],
};
