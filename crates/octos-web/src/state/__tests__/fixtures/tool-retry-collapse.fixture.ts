// tool-retry-collapse — issue #680.
//
// User Q1 invokes a flaky tool (e.g. web_fetch). The tool errors. The
// agent retries with a fresh tool_call_id but bound to the SAME turn_id.
// Between the failed-call and the retry-fire, user Q2 has sent a fast
// lookup. The retry's tool_started + tool_completed must bind to Q1's
// bubble — NOT to Q2's, even though sticky has rotated.
//
// The buggy reducer routes the retry under whichever bubble is sticky
// when the retry fires (Q2's). Q1's bubble shows a failed tool with no
// retry; Q2's bubble shows a successful tool call out of nowhere.

import type { SseFixture } from '../lib/fixture-types.js';

const SID = 'tool-retry-session';

export const toolRetryCollapse: SseFixture = {
  name: 'tool-retry-collapse',
  description:
    'Failed tool call retried under same turn_id after sticky rotates to Q2. Retry must bind to originating turn (Q1).',
  issues: [680],
  session_id: SID,
  events: [
    // ── Q1: invokes flaky tool ───────────────────────────────────────────
    {
      t: 0,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'turn-1',
      cmid: 'turn-1',
      text: 'Fetch the contents of https://example.com',
    },
    { t: 100, type: 'turn_started', session_id: SID, turn_id: 'turn-1' },
    {
      t: 500,
      type: 'tool_started',
      session_id: SID,
      turn_id: 'turn-1',
      tool_call_id: 'tc-1a',
      tool_name: 'web_fetch',
    },
    {
      t: 1500,
      type: 'tool_completed',
      session_id: SID,
      turn_id: 'turn-1',
      tool_call_id: 'tc-1a',
      tool_name: 'web_fetch',
      success: false,
      output_preview: 'connection reset',
    },
    // The agent has decided to retry — but in the meantime user Q2 sends.

    // ── Q2: fast lookup ──────────────────────────────────────────────────
    {
      t: 2000,
      type: 'user_sent',
      session_id: SID,
      turn_id: 'turn-2',
      cmid: 'turn-2',
      text: 'What is 2+2?',
    },
    { t: 2100, type: 'turn_started', session_id: SID, turn_id: 'turn-2' },
    {
      t: 2300,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'turn-2',
      text: '4',
    },
    { t: 2400, type: 'turn_completed', session_id: SID, turn_id: 'turn-2' },

    // ── Q1: retry fires; sticky=turn-2. Retry MUST bind to turn-1. ──────
    {
      t: 3000,
      type: 'tool_started',
      session_id: SID,
      turn_id: 'turn-1',
      tool_call_id: 'tc-1b',
      tool_name: 'web_fetch',
    },
    {
      t: 3500,
      type: 'tool_completed',
      session_id: SID,
      turn_id: 'turn-1',
      tool_call_id: 'tc-1b',
      tool_name: 'web_fetch',
      success: true,
      output_preview: '<html>...</html>',
    },
    {
      t: 3700,
      type: 'message_delta',
      session_id: SID,
      turn_id: 'turn-1',
      text: 'Fetched: <html>...</html>',
    },
    { t: 3800, type: 'turn_completed', session_id: SID, turn_id: 'turn-1' },
  ],
  assertions: [
    {
      kind: 'thread_order',
      expected: ['turn-1', 'turn-2'],
    },
    {
      kind: 'thread_equals',
      turn_id: 'turn-1',
      expect: {
        user: 'Fetch the contents of https://example.com',
        asst: 'Fetched: <html>...</html>',
      },
    },
    {
      kind: 'thread_equals',
      turn_id: 'turn-2',
      expect: { user: 'What is 2+2?', asst: '4' },
    },
    // Both tool calls (initial + retry) must end up under turn-1.
    {
      kind: 'thread_has_tool_call',
      turn_id: 'turn-1',
      tool_call_id: 'tc-1a',
      tool_name: 'web_fetch',
      success: false,
    },
    {
      kind: 'thread_has_tool_call',
      turn_id: 'turn-1',
      tool_call_id: 'tc-1b',
      tool_name: 'web_fetch',
      success: true,
    },
    {
      kind: 'no_misroute',
      allowed_turn_ids: ['turn-1', 'turn-2'],
    },
  ],
};
