// reload-replay — drop the WS, reconnect, replay from cursor.
//
// User sends two messages, both stream successfully. Connection drops.
// Server replays the same events from cursor on reconnect (since the
// client's last-known cursor was earlier). The final state MUST be
// identical to what we'd have seen with no drop — same bubbles, same
// text, same attachment placement.
//
// Under the buggy reducer the SPA wipes its DOM on reconnect (it calls
// loadHistory and rebuilds from REST), and the rebuild path itself has
// thread-binding inconsistencies — so the post-reconnect graph is
// observably different. Under the fixed reducer the typed-turn_id keys
// make every replay event idempotent, so the graph is preserved.

import type { SseFixture } from '../lib/fixture-types.js';

const SID = 'reload-replay-session';

export const reloadReplay: SseFixture = {
  name: 'reload-replay',
  description:
    'WebSocket drops mid-session, reconnect replays events from cursor. Thread graph must be identical pre- and post-disconnect.',
  issues: [],
  session_id: SID,
  events: [
    // ── Two complete turns before the drop ───────────────────────────────
    {
      t: 0,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'turn-1',
      cmid: 'turn-1',
      text: 'First question.',
    },
    { t: 100, type: 'turn_started', session_id: SID, turn_id: 'turn-1' },
    {
      t: 500,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'turn-1',
      text: 'First answer.',
    },
    { t: 600, type: 'turn_completed', session_id: SID, turn_id: 'turn-1' },

    {
      t: 1000,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'turn-2',
      cmid: 'turn-2',
      text: 'Second question.',
    },
    { t: 1100, type: 'turn_started', session_id: SID, turn_id: 'turn-2' },
    {
      t: 1500,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'turn-2',
      text: 'Second answer.',
    },
    { t: 1600, type: 'turn_completed', session_id: SID, turn_id: 'turn-2' },

    // ── Connection drops ─────────────────────────────────────────────────
    { t: 2000, type: 'connection_drop' },

    // ── Reconnect; server replays from cursor (idempotent) ──────────────
    // After connection_resume the engine feeds the SAME events again. A
    // typed-turn_id reducer treats them as idempotent (same key, same
    // text — overwrites with identical content). The graph survives.
    { t: 3000, type: 'connection_resume', cursor: 'cursor-before-drop' },

    // Replayed events from cursor
    {
      t: 3001,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'turn-1',
      cmid: 'turn-1',
      text: 'First question.',
    },
    { t: 3002, type: 'turn_started', session_id: SID, turn_id: 'turn-1' },
    {
      t: 3003,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'turn-1',
      text: 'First answer.',
    },
    { t: 3004, type: 'turn_completed', session_id: SID, turn_id: 'turn-1' },
    {
      t: 3005,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'turn-2',
      cmid: 'turn-2',
      text: 'Second question.',
    },
    { t: 3006, type: 'turn_started', session_id: SID, turn_id: 'turn-2' },
    {
      t: 3007,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'turn-2',
      text: 'Second answer.',
    },
    { t: 3008, type: 'turn_completed', session_id: SID, turn_id: 'turn-2' },
  ],
  assertions: [
    // Graph identical to what a no-drop run would produce.
    {
      kind: 'thread_order',
      expected: ['turn-1', 'turn-2'],
    },
    {
      kind: 'thread_equals',
      turn_id: 'turn-1',
      expect: { user: 'First question.', asst: 'First answer.' },
    },
    {
      kind: 'thread_equals',
      turn_id: 'turn-2',
      expect: { user: 'Second question.', asst: 'Second answer.' },
    },
    {
      kind: 'no_misroute',
      allowed_turn_ids: ['turn-1', 'turn-2'],
    },
  ],
};
