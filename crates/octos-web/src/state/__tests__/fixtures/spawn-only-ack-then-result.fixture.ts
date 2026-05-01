// spawn-only-ack-then-result — issue #738 canonical scenario.
//
// User asks a deep_search question. The agent immediately replies with an
// "ack" message ("running deep_search...") and then a spawn_only background
// task fires. Two MINUTES later the result lands with a `.md` attachment.
// In the meantime no other user has sent — so this is the simplest possible
// case. The bug, even here, is that the current SPA has no concept of
// background_result -> originating turn binding; the result appends to the
// most recent assistant bubble.
//
// We add a SECOND user message between the ack and the result to make the
// misroute observable: under the buggy reducer the `.md` attachment lands
// in user2's bubble; under the fixed reducer it lands in user1's bubble
// where the ack was.

import type { SseFixture } from '../lib/fixture-types.js';

const SID = 'spawn-only-session';

export const spawnOnlyAckThenResult: SseFixture = {
  name: 'spawn-only-ack-then-result',
  description:
    'Deep_search ack at t=2s, second user at t=10s, deep_search result with .md attachment at t=180s. Result must land in originating turn, not in second-user bubble. Issue #738.',
  issues: [738],
  session_id: SID,
  events: [
    // ── Originating user message ─────────────────────────────────────────
    {
      t: 0,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'orig-turn',
      cmid: 'orig-turn',
      text: 'Deep-search the latest octos release notes.',
    },
    { t: 100, type: 'turn_started', session_id: SID, turn_id: 'orig-turn' },

    // ── Ack message: agent says it's running deep_search ────────────────
    {
      t: 2000,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'orig-turn',
      text: 'Running deep_search in background, will deliver results shortly.',
    },
    { t: 2100, type: 'turn_completed', session_id: SID, turn_id: 'orig-turn' },

    // ── Second user, while deep_search still running ────────────────────
    {
      t: 10000,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'second-turn',
      cmid: 'second-turn',
      text: 'While that runs: what is the capital of France?',
    },
    { t: 10100, type: 'turn_started', session_id: SID, turn_id: 'second-turn' },
    {
      t: 10500,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'second-turn',
      text: 'Paris.',
    },
    { t: 10600, type: 'turn_completed', session_id: SID, turn_id: 'second-turn' },

    // ── Deep_search result lands. Sticky=second-turn. ──────────────────
    // The fixed reducer must bind this to orig-turn (per event.turn_id).
    // The buggy reducer binds to sticky and the .md ends up under
    // 'second-turn'.
    {
      t: 180000,
      type: 'background_result',
      session_id: SID,
      turn_id: 'orig-turn',
      text: 'Deep_search complete: see attached release notes.',
      attachments: [
        {
          filename: 'release-notes.md',
          path: '/tmp/skill-output/release-notes.md',
        },
      ],
    },
  ],
  assertions: [
    {
      kind: 'thread_order',
      expected: ['orig-turn', 'second-turn'],
    },
    {
      kind: 'thread_equals',
      turn_id: 'orig-turn',
      expect: {
        user: 'Deep-search the latest octos release notes.',
        asst: 'Running deep_search in background, will deliver results shortly.Deep_search complete: see attached release notes.',
      },
    },
    {
      kind: 'thread_equals',
      turn_id: 'second-turn',
      expect: {
        user: 'While that runs: what is the capital of France?',
        asst: 'Paris.',
      },
    },
    {
      kind: 'thread_has_attachment',
      turn_id: 'orig-turn',
      filename: 'release-notes.md',
    },
    {
      kind: 'no_misroute',
      allowed_turn_ids: ['orig-turn', 'second-turn'],
    },
  ],
};
