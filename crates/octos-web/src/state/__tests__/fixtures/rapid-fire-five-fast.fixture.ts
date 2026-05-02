// rapid-fire-five-fast — port of /tmp/fixture-poc/reducer-test.mjs.
//
// Five users sent ~800ms apart, each turn produces an assistant response.
// In real wave-4 mini3 DOM dumps (the originating evidence for #740) the
// late-arriving deltas of early turns landed in the bubble of whichever
// turn happened to be most recent. The buggy reducer reproduces this; the
// fixed reducer pins each delta to its own turn_id.

import type { SseFixture } from '../lib/fixture-types.js';

const SID = 'rapid-fire-session';

export const rapidFireFiveFast: SseFixture = {
  name: 'rapid-fire-five-fast',
  description:
    'Five users 800ms apart, deltas interleaved with later turn_started events. Buggy reducer misroutes early-turn tails into later bubbles; fixed reducer keeps every answer in its own bubble.',
  issues: [649, 664, 673, 680, 738, 740],
  session_id: SID,
  events: [
    // ── Q1 (cm-A) ────────────────────────────────────────────────────────
    {
      t: 0,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'cm-A',
      cmid: 'cm-A',
      text: '1+1=?',
    },
    { t: 100, type: 'turn_started', session_id: SID, turn_id: 'cm-A' },

    // ── Q2 (cm-B) starts before Q1 has streamed ──────────────────────────
    {
      t: 800,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'cm-B',
      cmid: 'cm-B',
      text: '2+2=?',
    },
    { t: 900, type: 'turn_started', session_id: SID, turn_id: 'cm-B' },

    // ── Q3 (cm-C) — sticky has rotated forward ───────────────────────────
    {
      t: 1600,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'cm-C',
      cmid: 'cm-C',
      text: '3+3=?',
    },
    { t: 1700, type: 'turn_started', session_id: SID, turn_id: 'cm-C' },

    // ── Q1's answer arrives. Sticky=C. Buggy binds to C. ─────────────────
    {
      t: 1800,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'cm-A',
      text: '1+1=2',
    },
    { t: 2000, type: 'turn_completed', session_id: SID, turn_id: 'cm-A' },

    // ── Q4 (cm-D) ────────────────────────────────────────────────────────
    {
      t: 2400,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'cm-D',
      cmid: 'cm-D',
      text: '4+4=?',
    },
    { t: 2500, type: 'turn_started', session_id: SID, turn_id: 'cm-D' },

    // ── Q2's answer arrives. Sticky=D. Buggy binds to D. ─────────────────
    {
      t: 2600,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'cm-B',
      text: '2+2=4',
    },
    { t: 2800, type: 'turn_completed', session_id: SID, turn_id: 'cm-B' },

    // ── Q5 (cm-E) ────────────────────────────────────────────────────────
    {
      t: 3200,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'cm-E',
      cmid: 'cm-E',
      text: '5+5=?',
    },
    { t: 3300, type: 'turn_started', session_id: SID, turn_id: 'cm-E' },

    // ── Q3, Q4, Q5 answers arrive interleaved ────────────────────────────
    {
      t: 3500,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'cm-C',
      text: '3+3=6',
    },
    {
      t: 3550,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'cm-D',
      text: '4+4=8',
    },
    {
      t: 3600,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'cm-E',
      text: '5+5=10',
    },
    { t: 3700, type: 'turn_completed', session_id: SID, turn_id: 'cm-C' },
    { t: 3750, type: 'turn_completed', session_id: SID, turn_id: 'cm-D' },
    { t: 3800, type: 'turn_completed', session_id: SID, turn_id: 'cm-E' },
  ],
  assertions: [
    {
      kind: 'thread_order',
      expected: ['cm-A', 'cm-B', 'cm-C', 'cm-D', 'cm-E'],
    },
    {
      kind: 'thread_equals',
      turn_id: 'cm-A',
      expect: { user: '1+1=?', asst: '1+1=2' },
    },
    {
      kind: 'thread_equals',
      turn_id: 'cm-B',
      expect: { user: '2+2=?', asst: '2+2=4' },
    },
    {
      kind: 'thread_equals',
      turn_id: 'cm-C',
      expect: { user: '3+3=?', asst: '3+3=6' },
    },
    {
      kind: 'thread_equals',
      turn_id: 'cm-D',
      expect: { user: '4+4=?', asst: '4+4=8' },
    },
    {
      kind: 'thread_equals',
      turn_id: 'cm-E',
      expect: { user: '5+5=?', asst: '5+5=10' },
    },
    {
      kind: 'no_misroute',
      allowed_turn_ids: ['cm-A', 'cm-B', 'cm-C', 'cm-D', 'cm-E'],
    },
  ],
};
