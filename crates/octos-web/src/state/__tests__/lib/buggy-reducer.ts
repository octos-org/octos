// Current (buggy) SPA reducer — placeholder for what PR J replaces.
//
// This reducer models the production SPA's actual thread-binding behavior
// today. The bug class (#649 → #664 → #673 → #680 → #738 → #740) is rooted
// in TWO leaks documented in /tmp/octos-architecture-FINAL.md (the
// "loophole-audit" table referenced from sections A and C):
//
//   LEAK 1 — Server-side sticky-map fallback. When the server's
//     `api_channel.rs` receives a stream-forwarded delta whose envelope
//     does not carry an explicit thread_id, it falls back to "last
//     thread_id seen for this channel". Under interleave the sticky map
//     has rotated past the originating turn — the client receives the
//     delta stamped with the WRONG thread_id.
//
//   LEAK 2 — Client-side reducer fallback. The current SPA at
//     crates/octos-cli/static/app.js does not read thread_id off the wire
//     at all (the SSE stream is treated as a raw token feed). Every delta
//     is appended to whichever assistant bubble was most recently created.
//     If a later user sends Q2 mid-stream, Q1's tail tokens land in Q2's
//     bubble.
//
// This reducer reproduces LEAK 2 (and approximates LEAK 1's effect from
// the client's vantage). PR J ships the typed-turn_id reducer that
// replaces this file. When PR J lands, the 5 seed fixtures will pass —
// today they all FAIL deterministically, which is the contract.
//
// THE SHAPE OF THE BUG
// ────────────────────
// On `user_sent`, we record (turn_id → bubble) and we update `sticky` to
// the new turn_id. On `message_delta`, we IGNORE event.turn_id and append
// to `sticky`. On `turn_started`, we update sticky too (mimicking the
// server's "active turn" advance). This is what produces the wave-4 mini3
// misroute pattern: a late delta for Q1 hits a sticky that has rotated
// past Q1, so Q1's text lands in Q3's bubble.
//
// For `background_result` (issue #738), the bug is even simpler: the
// current SPA doesn't have a separate handler — background results show
// up as a fresh assistant message at the END of the conversation,
// detached from their originating turn. We model that as "binds to
// sticky" too.

import type { Reducer, ReducerState } from './replay-fixture.js';
import type { SseEvent, TurnId } from './fixture-types.js';

/** State internal to the buggy reducer — the sticky-map. Stored alongside
 *  the public ReducerState via a side map keyed by state object identity.
 *  We use a WeakMap so each fixture run has its own sticky without us
 *  having to widen the public ReducerState type. */
const stickyByState: WeakMap<ReducerState, { sticky: TurnId | null }> =
  new WeakMap();

function getSticky(state: ReducerState): TurnId | null {
  return stickyByState.get(state)?.sticky ?? null;
}

function setSticky(state: ReducerState, turn_id: TurnId | null): void {
  stickyByState.set(state, { sticky: turn_id });
}

/** Mutates state in-place and returns it. The replay engine treats reducer
 *  return as canonical, so we keep the contract clean. */
function ensureThread(state: ReducerState, turn_id: TurnId): void {
  if (!state.threads.has(turn_id)) {
    state.threads.set(turn_id, { turn_id, user: '', asst: '' });
    state.thread_order.push(turn_id);
  }
}

export const buggyReducer: Reducer = (state, event) => {
  // Carry the sticky pointer forward to the next state object. Since we
  // mutate `state` in place (the engine doesn't require immutability) we
  // can preserve the WeakMap entry on the same object.
  const prevSticky = getSticky(state);
  setSticky(state, prevSticky);

  switch (event.type) {
    case 'user_sent': {
      ensureThread(state, event.turn_id);
      const t = state.threads.get(event.turn_id)!;
      t.user = event.text;
      // BUG (LEAK 2): sticky advances to the most recent user. Later
      // deltas for any earlier turn will misroute.
      setSticky(state, event.turn_id);
      return state;
    }

    case 'turn_started': {
      // Mirrors the server's active-turn advance. In production this is
      // what makes the bug bite even harder: the server's sticky also
      // advances on turn_started, so deltas for the now-non-active turn
      // go to the wrong bubble.
      setSticky(state, event.turn_id);
      return state;
    }

    case 'message_delta': {
      // BUG: ignore event.turn_id; bind to sticky.
      const target = getSticky(state);
      if (target === null) {
        state.rejected.push({ event, reason: 'no sticky thread to bind to' });
        return state;
      }
      ensureThread(state, target);
      const t = state.threads.get(target)!;
      t.asst += event.text;
      return state;
    }

    case 'turn_completed': {
      // No-op for binding. Sticky stays put — which is part of the bug.
      return state;
    }

    case 'turn_error': {
      // BUG: just like message_delta — the current SPA flags whichever
      // bubble is currently sticky as errored, ignoring event.turn_id.
      // The actual originating turn never sees the error indicator.
      const target = getSticky(state);
      if (target === null) {
        state.rejected.push({ event, reason: 'no sticky thread for turn_error' });
        return state;
      }
      ensureThread(state, target);
      const t = state.threads.get(target)!;
      t.error = { code: event.code, message: event.message };
      return state;
    }

    case 'tool_started': {
      // BUG: tool calls render under whatever bubble is sticky. Issue
      // #680 / "tool retry collapse" — when a retry fires after sticky
      // has rotated to a later turn, the retry's tool_started binds
      // there and the originating turn loses the tool indicator.
      const target = getSticky(state);
      if (target === null) {
        state.rejected.push({ event, reason: 'no sticky thread for tool_started' });
        return state;
      }
      ensureThread(state, target);
      const t = state.threads.get(target)!;
      t.tool_calls = (t.tool_calls ?? []).concat([
        { tool_call_id: event.tool_call_id, tool_name: event.tool_name },
      ]);
      return state;
    }

    case 'tool_progress': {
      // No render impact in the current SPA — silently consumed.
      return state;
    }

    case 'tool_completed': {
      // BUG: completion is matched against tool_call_id but the search
      // is scoped to the SAME bubble that received tool_started. If
      // sticky has rotated between started and completed (which is
      // exactly what happens during retry collapse), the completion
      // event creates a fresh tool_calls entry under the new sticky
      // instead of finalizing the original.
      const target = getSticky(state);
      if (target === null) {
        state.rejected.push({ event, reason: 'no sticky thread for tool_completed' });
        return state;
      }
      ensureThread(state, target);
      const t = state.threads.get(target)!;
      const tc = (t.tool_calls ?? []).find((x) => x.tool_call_id === event.tool_call_id);
      if (tc) {
        tc.success = event.success;
        tc.output_preview = event.output_preview;
      } else {
        // Fresh entry under wrong bubble — exactly the misroute symptom.
        t.tool_calls = (t.tool_calls ?? []).concat([
          {
            tool_call_id: event.tool_call_id,
            tool_name: event.tool_name,
            success: event.success,
            output_preview: event.output_preview,
          },
        ]);
      }
      return state;
    }

    case 'background_result': {
      // BUG: like message_delta, the current SPA appends to the most
      // recent assistant bubble. The originating turn_id on the event is
      // ignored. Issue #738.
      const target = getSticky(state);
      if (target === null) {
        state.rejected.push({
          event,
          reason: 'no sticky thread to attach background result to',
        });
        return state;
      }
      ensureThread(state, target);
      const t = state.threads.get(target)!;
      t.asst += event.text;
      if (event.attachments) {
        t.attachments = (t.attachments ?? []).concat(event.attachments);
      }
      return state;
    }

    case 'recovery_turn': {
      // BUG: the current SPA treats the recovery_turn's text as a fresh
      // delta. With sticky pointing at whoever sent most recently, the
      // recovery output binds there — NOT to originating_turn_id.
      const target = getSticky(state);
      if (target === null) {
        state.rejected.push({ event, reason: 'no sticky thread for recovery' });
        return state;
      }
      ensureThread(state, target);
      const t = state.threads.get(target)!;
      t.asst += event.text;
      return state;
    }

    case 'connection_drop': {
      state.connected = false;
      return state;
    }

    case 'connection_resume': {
      // BUG: the current SPA does NOT deduplicate replayed events on
      // reconnect. SSE is treated as a raw token stream; each token is
      // appended to whatever bubble is currently sticky. Replayed
      // user_sent events re-create/touch existing bubbles; replayed
      // message_delta events DOUBLE-APPEND text to the wrong bubble
      // (sticky has rotated forward by then). The visible symptom is
      // duplicated text in the wrong bubble after every reconnect.
      state.connected = true;
      // sticky stays where it was — that's part of the bug.
      return state;
    }
  }

  // Exhaustive switch above; unreachable.
  return state;
};
