// multi-attachment-dedup — multi-file deep_search result + dedup.
//
// User asks a research question; the deep_search returns 3 .md files in a
// single `background_result`. Then the user retries the same query
// (different turn_id, identical attachments). Each turn must own its own
// 3-file attachment list — the second turn must NOT inherit the first
// turn's files, and neither turn must drop a duplicate filename across
// turns (a separate failure mode where the SPA's "global attachment
// seen" set incorrectly dedupes across bubbles).

import type { SseFixture } from '../lib/fixture-types.js';

const SID = 'multi-attachment-session';

export const multiAttachmentDedup: SseFixture = {
  name: 'multi-attachment-dedup',
  description:
    'Two background_result events on different turns, each with three .md attachments (same filenames). Each turn must own its own list, in order; no cross-turn dedup.',
  issues: [738],
  session_id: SID,
  events: [
    // ── Turn 1: kicks off a slow research ───────────────────────────────
    {
      t: 0,
      type: 'user_sent',
      session_id: SID,
      turn_id: 't1',
      cmid: 't1',
      text: 'First research query',
    },
    { t: 100, type: 'turn_started', session_id: SID, turn_id: 't1' },
    {
      t: 500,
      type: 'message_delta',
      session_id: SID,
      turn_id: 't1',
      text: 'Researching first query...',
    },
    { t: 600, type: 'turn_completed', session_id: SID, turn_id: 't1' },

    // ── Turn 2: starts a second research BEFORE turn 1's bg result ──────
    // This rotates sticky to t2. Now turn 1's background_result, when it
    // arrives, must STILL bind to t1 — under the buggy reducer it lands
    // in t2 (along with t2's own attachments — three filenames doubled
    // up — exactly the dedup-failure symptom).
    {
      t: 1000,
      type: 'user_sent',
      session_id: SID,
      turn_id: 't2',
      cmid: 't2',
      text: 'Second research query (same shape)',
    },
    { t: 1100, type: 'turn_started', session_id: SID, turn_id: 't2' },
    {
      t: 1500,
      type: 'message_delta',
      session_id: SID,
      turn_id: 't2',
      text: 'Researching second query...',
    },
    { t: 1600, type: 'turn_completed', session_id: SID, turn_id: 't2' },

    // ── Turn 1's background result arrives. Sticky=t2. Must bind to t1. ──
    {
      t: 30000,
      type: 'background_result',
      session_id: SID,
      turn_id: 't1',
      text: 'Done.',
      attachments: [
        { filename: 'summary.md', path: '/tmp/r1/summary.md' },
        { filename: 'details.md', path: '/tmp/r1/details.md' },
        { filename: 'sources.md', path: '/tmp/r1/sources.md' },
      ],
    },

    // ── Turn 2's background result arrives. ─────────────────────────────
    // Same filenames, different paths. Each turn must own its own list.
    {
      t: 31000,
      type: 'background_result',
      session_id: SID,
      turn_id: 't2',
      text: 'Done.',
      attachments: [
        { filename: 'summary.md', path: '/tmp/r2/summary.md' },
        { filename: 'details.md', path: '/tmp/r2/details.md' },
        { filename: 'sources.md', path: '/tmp/r2/sources.md' },
      ],
    },
  ],
  assertions: [
    {
      kind: 'thread_order',
      expected: ['t1', 't2'],
    },
    // Each turn owns its own list, in order — INCLUDING path. Same
    // filenames across turns; only path discriminates. (A reducer that
    // attaches the right filenames but the wrong paths must still
    // fail.)
    {
      kind: 'thread_attachments_equal',
      turn_id: 't1',
      attachments: [
        { filename: 'summary.md', path: '/tmp/r1/summary.md' },
        { filename: 'details.md', path: '/tmp/r1/details.md' },
        { filename: 'sources.md', path: '/tmp/r1/sources.md' },
      ],
    },
    {
      kind: 'thread_attachments_equal',
      turn_id: 't2',
      attachments: [
        { filename: 'summary.md', path: '/tmp/r2/summary.md' },
        { filename: 'details.md', path: '/tmp/r2/details.md' },
        { filename: 'sources.md', path: '/tmp/r2/sources.md' },
      ],
    },
    {
      kind: 'no_misroute',
      allowed_turn_ids: ['t1', 't2'],
    },
  ],
};
