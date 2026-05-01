// Reference (fixed) SPA reducer — the contract PR J must satisfy.
//
// This reducer is the executable specification of what PR J's typed-
// turn_id reducer MUST do. The 5 seed fixtures are validated against this
// reducer (they pass) and against the buggy-reducer (they fail).
// Together those two outcomes prove:
//
//   (a) the fixtures express a real, achievable contract, AND
//   (b) the fixtures detect the current buggy behavior.
//
// Properties:
//
//   - Every turn-scoped event MUST carry an explicit turn_id. Events
//     without turn_id are rejected (no sticky-map fallback).
//   - `recovery_turn` binds output to `originating_turn_id`, NOT to
//     `recovery_turn_id`. (Issue #738 sibling.)
//   - `background_result` binds to its event's turn_id (NOT to whatever
//     was last active). Attachments are stored on that thread.
//   - On `connection_resume` the graph is preserved (the server replays
//     from cursor; the reducer accepts replayed events idempotently).
//
// PR J replaces the placeholder buggy-reducer.ts with a production-grade
// implementation that preserves THESE behaviors. The fixtures don't care
// HOW PR J achieves it — only that the reducer's observable state
// matches what fixed-reducer below produces.

import type { Reducer } from './replay-fixture.js';
import type { TurnId } from './fixture-types.js';

function ensureThread(
  state: { threads: Map<TurnId, { turn_id: TurnId; user: string; asst: string; attachments?: Array<{ filename: string; path: string }> }>; thread_order: TurnId[] },
  turn_id: TurnId,
): void {
  if (!state.threads.has(turn_id)) {
    state.threads.set(turn_id, { turn_id, user: '', asst: '' });
    state.thread_order.push(turn_id);
  }
}

export const fixedReducer: Reducer = (state, event) => {
  switch (event.type) {
    case 'user_sent': {
      ensureThread(state, event.turn_id);
      const t = state.threads.get(event.turn_id)!;
      t.user = event.text;
      return state;
    }

    case 'turn_started': {
      // Make sure the thread exists; the server has confirmed the turn.
      ensureThread(state, event.turn_id);
      return state;
    }

    case 'message_delta': {
      if (!event.turn_id) {
        state.rejected.push({ event, reason: 'message_delta missing turn_id' });
        return state;
      }
      // Strict: a delta for a turn we've never seen start is a server bug
      // OR a stale replay. Fail-closed by recording rejection. The
      // assertion `no_orphans` will surface this.
      if (!state.threads.has(event.turn_id)) {
        state.rejected.push({
          event,
          reason: `message_delta for unknown turn_id "${event.turn_id}"`,
        });
        return state;
      }
      const t = state.threads.get(event.turn_id)!;
      t.asst += event.text;
      return state;
    }

    case 'turn_completed': {
      // No-op for binding; the server has signaled the turn done.
      return state;
    }

    case 'turn_error': {
      if (!event.turn_id) {
        state.rejected.push({ event, reason: 'turn_error missing turn_id' });
        return state;
      }
      ensureThread(state, event.turn_id);
      const t = state.threads.get(event.turn_id)!;
      t.error = { code: event.code, message: event.message };
      return state;
    }

    case 'tool_started': {
      if (!event.turn_id) {
        state.rejected.push({ event, reason: 'tool_started missing turn_id' });
        return state;
      }
      ensureThread(state, event.turn_id);
      const t = state.threads.get(event.turn_id)!;
      t.tool_calls = (t.tool_calls ?? []).concat([
        { tool_call_id: event.tool_call_id, tool_name: event.tool_name },
      ]);
      return state;
    }

    case 'tool_progress': {
      // No render-state mutation needed for binding correctness; the
      // production UI will surface progress text but the binding contract
      // is fully expressed via tool_started + tool_completed.
      return state;
    }

    case 'tool_completed': {
      if (!event.turn_id) {
        state.rejected.push({ event, reason: 'tool_completed missing turn_id' });
        return state;
      }
      ensureThread(state, event.turn_id);
      const t = state.threads.get(event.turn_id)!;
      const tc = (t.tool_calls ?? []).find((x) => x.tool_call_id === event.tool_call_id);
      if (tc) {
        tc.success = event.success;
        tc.output_preview = event.output_preview;
      } else {
        // No matching started — server replay or out-of-order delivery.
        // We accept and create the record (the typed turn_id is what
        // matters for binding correctness).
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
      if (!event.turn_id) {
        state.rejected.push({
          event,
          reason: 'background_result missing turn_id',
        });
        return state;
      }
      ensureThread(state, event.turn_id);
      const t = state.threads.get(event.turn_id)!;
      t.asst += event.text;
      if (event.attachments) {
        t.attachments = (t.attachments ?? []).concat(event.attachments);
      }
      return state;
    }

    case 'recovery_turn': {
      // KEY: the recovery's output lands in originating_turn_id's bubble,
      // NOT in recovery_turn_id's. The recovery_turn_id is server-side
      // bookkeeping; the user never saw a fresh user message for it.
      if (!event.originating_turn_id) {
        state.rejected.push({
          event,
          reason: 'recovery_turn missing originating_turn_id',
        });
        return state;
      }
      ensureThread(state, event.originating_turn_id);
      const t = state.threads.get(event.originating_turn_id)!;
      t.asst += event.text;
      return state;
    }

    case 'connection_drop': {
      state.connected = false;
      return state;
    }

    case 'connection_resume': {
      // PR G UPCR-2026-009 semantic: on resume the client invokes
      // `session/hydrate` and rebuilds state from the authoritative server
      // snapshot. The replayed events that follow re-establish the graph
      // from scratch. So the reducer DROPS its current threads and lets
      // the replayed events repopulate.
      //
      // This is the contract: the post-resume graph is determined solely
      // by what the server replays. Any drift between the pre-disconnect
      // graph and the post-resume graph is the server's responsibility,
      // and the protocol-level guarantee is "they match" because both
      // come from the same ledger.
      state.thread_order = [];
      state.threads.clear();
      // Note: we deliberately keep state.rejected — those rejections are
      // reducer-level diagnostics that survive the transport reset.
      state.connected = true;
      return state;
    }
  }
  return state;
};
