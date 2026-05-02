// m89-recovery-turn — #738 sibling: recovery-turn output binding.
//
// First child turn fires (e.g. an LLM call that fails with a 503 mid-tool-
// use). The agent's recovery loop schedules a NEW turn (with a fresh
// recovery_turn_id) to retry the operation. The recovery's eventual
// successful output must land in the ORIGINATING bubble (the user message
// that started everything) — NOT in a fresh bubble for recovery_turn_id,
// and NOT in whatever happens to be the most recent user message.
//
// This is the #738-sibling scenario: when a child fails and a recovery
// fires, the result-binding has to inherit from the originating turn or
// the user sees an answer "for nothing" — disconnected from the question
// they asked.

import type { SseFixture } from '../lib/fixture-types.js';

const SID = 'm89-recovery-session';

export const m89RecoveryTurn: SseFixture = {
  name: 'm89-recovery-turn',
  description:
    'First child turn fails, recovery turn fires, result lands. Recovery output must inherit originating_turn_id, not bind to a fresh bubble.',
  issues: [738],
  session_id: SID,
  events: [
    // ── Originating user ─────────────────────────────────────────────────
    {
      t: 0,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'orig-turn',
      cmid: 'orig-turn',
      text: 'Summarize the changelog for the last 5 releases.',
    },
    { t: 100, type: 'turn_started', session_id: SID, turn_id: 'orig-turn' },

    // ── Initial attempt: starts streaming ───────────────────────────────
    {
      t: 1000,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'orig-turn',
      text: 'Reading changelog...',
    },

    // ── Unrelated user sends in the gap ──────────────────────────────────
    // Critical for the misroute: the gap-turn rotates sticky FORWARD
    // before the originating turn errors. Under the buggy reducer the
    // turn_error then binds to gap-turn, not orig-turn. Without this
    // ordering the bug is invisible.
    {
      t: 2000,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'gap-turn',
      cmid: 'gap-turn',
      text: 'And what is the weather?',
    },
    { t: 2100, type: 'turn_started', session_id: SID, turn_id: 'gap-turn' },
    {
      t: 2300,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'gap-turn',
      text: 'Sunny, 21C.',
    },
    { t: 2400, type: 'turn_completed', session_id: SID, turn_id: 'gap-turn' },

    // ── orig-turn errors AFTER sticky rotated to gap-turn ───────────────
    // turn_error must STILL bind to orig-turn (the originating bubble).
    {
      t: 3000,
      type: 'turn_error',
      session_id: SID,
      turn_id: 'orig-turn',
      code: 'upstream_503',
      message: 'LLM provider returned 503; recovery scheduled.',
    },

    // ── Recovery turn fires, succeeds, output streams ────────────────────
    // The recovery_turn_id is a FRESH server-side id ('recovery-1') but
    // the result MUST be rendered under 'orig-turn'. The reducer never
    // creates a bubble for 'recovery-1'.
    {
      t: 8000,
      type: 'recovery_turn',
      session_id: SID,
      recovery_turn_id: 'recovery-1',
      originating_turn_id: 'orig-turn',
      text: 'Recovered: the last 5 releases were 1.4 through 1.8 — bug fixes, the new ledger format, and skill manifests.',
    },
  ],
  assertions: [
    // The recovery turn never creates its own bubble.
    {
      kind: 'thread_order',
      expected: ['orig-turn', 'gap-turn'],
    },
    {
      kind: 'thread_equals',
      turn_id: 'orig-turn',
      expect: {
        user: 'Summarize the changelog for the last 5 releases.',
        asst: 'Reading changelog...Recovered: the last 5 releases were 1.4 through 1.8 — bug fixes, the new ledger format, and skill manifests.',
      },
    },
    {
      kind: 'thread_equals',
      turn_id: 'gap-turn',
      expect: {
        user: 'And what is the weather?',
        asst: 'Sunny, 21C.',
      },
    },
    // The turn_error must land in the ORIGINATING bubble, not in
    // whichever bubble is sticky at the time of the error.
    {
      kind: 'thread_has_error',
      turn_id: 'orig-turn',
      code: 'upstream_503',
    },
    {
      kind: 'no_misroute',
      allowed_turn_ids: ['orig-turn', 'gap-turn'],
    },
  ],
};
