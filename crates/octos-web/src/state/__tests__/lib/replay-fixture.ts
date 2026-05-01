// Layer 1 SPA reducer fixture replay engine.
//
// Takes an `SseFixture` and a `Reducer`, advances the reducer event-by-event
// in `t` order, then runs every assertion against the final state. Returns
// a structured result so the Vitest test wrapper can convert it to an
// `expect(...)` assertion that produces a clean diff.
//
// Two intentional design properties:
//
// 1. The engine is COMPLETELY DETERMINISTIC. No setTimeout, no Promises,
//    no Date.now(). Events are processed synchronously in the order their
//    `t` field implies (ties broken by array index — fixtures must be
//    authored stable). This is what gets us <1s total CI runtime.
//
// 2. The engine is ALLOCATION-SAFE. We never mutate the fixture; the
//    reducer's state object is passed by reference but the events array is
//    iterated read-only. That means a captured fixture loaded from JSON
//    can be replayed against multiple reducers (current buggy + future
//    fixed) and the diff is meaningful.

import type {
  Assertion,
  SseEvent,
  SseFixture,
  ThreadBubble,
  TurnId,
} from './fixture-types.js';

// ── Reducer contract ─────────────────────────────────────────────────────
//
// The SPA reducer the fixtures test against. Thread-keyed Map of bubbles +
// connection state (so reload-replay can assert that reconnection
// preserves the graph). PR J ships the production version of this; the
// current placeholder is `buggy-reducer.ts`.

export interface ReducerState {
  /** Render order — first appearance of a turn_id wins. The order is
   *  asserted by `kind: 'thread_order'`. */
  thread_order: TurnId[];
  /** Per-thread render content. */
  threads: Map<TurnId, ThreadBubble>;
  /** True between `connection_drop` and `connection_resume`. The reducer
   *  may queue events while disconnected; the engine ALWAYS feeds them
   *  through anyway, modelling a server that replays-from-cursor on
   *  resume. */
  connected: boolean;
  /** Events the reducer rejected (bad turn_id, missing fields, etc.).
   *  Surfaced in failure reports so a fixture author can see why a
   *  delta got dropped. */
  rejected: Array<{ event: SseEvent; reason: string }>;
}

export function emptyState(): ReducerState {
  return {
    thread_order: [],
    threads: new Map(),
    connected: true,
    rejected: [],
  };
}

/** A reducer implementation. Pure function; no I/O. Receives the previous
 *  state and the next event; returns the next state. The engine never
 *  shares state across fixtures — each replay starts from `emptyState()`. */
export type Reducer = (state: ReducerState, event: SseEvent) => ReducerState;

// ── Replay loop ──────────────────────────────────────────────────────────

/** Result of replaying a fixture. `pass` is the verdict; `failures` lists
 *  every assertion that didn't hold. `final_state` is included so a CI
 *  log can show what the reducer actually produced. */
export interface ReplayResult {
  fixture_name: string;
  pass: boolean;
  failures: AssertionFailure[];
  final_state: ReducerState;
  events_processed: number;
  duration_ms: number;
}

export interface AssertionFailure {
  assertion: Assertion;
  message: string;
}

/** Sort events by `t`, ties broken by original index — guarantees that two
 *  events at the same logical timestamp keep their authored order. */
function sortEvents(events: SseEvent[]): SseEvent[] {
  const indexed = events.map((ev, idx) => ({ ev, idx }));
  indexed.sort((a, b) => {
    if (a.ev.t !== b.ev.t) return a.ev.t - b.ev.t;
    return a.idx - b.idx;
  });
  return indexed.map((x) => x.ev);
}

export function replayFixture(
  fixture: SseFixture,
  reducer: Reducer,
): ReplayResult {
  const t0 =
    typeof performance !== 'undefined' && typeof performance.now === 'function'
      ? performance.now()
      : Date.now();

  let state = emptyState();
  const ordered = sortEvents(fixture.events);

  for (const event of ordered) {
    state = reducer(state, event);
  }

  const t1 =
    typeof performance !== 'undefined' && typeof performance.now === 'function'
      ? performance.now()
      : Date.now();

  const failures = runAssertions(state, fixture.assertions, ordered);

  return {
    fixture_name: fixture.name,
    pass: failures.length === 0,
    failures,
    final_state: state,
    events_processed: ordered.length,
    duration_ms: Math.max(0, t1 - t0),
  };
}

// ── Assertion runner ─────────────────────────────────────────────────────

function runAssertions(
  state: ReducerState,
  assertions: Assertion[],
  events: SseEvent[],
): AssertionFailure[] {
  const failures: AssertionFailure[] = [];
  for (const a of assertions) {
    const failure = checkAssertion(state, a, events);
    if (failure) failures.push({ assertion: a, message: failure });
  }
  return failures;
}

function checkAssertion(
  state: ReducerState,
  a: Assertion,
  events: SseEvent[],
): string | null {
  switch (a.kind) {
    case 'thread_equals': {
      const t = state.threads.get(a.turn_id);
      if (!t) return `thread for turn_id="${a.turn_id}" missing entirely`;
      if (a.expect.user !== undefined && t.user !== a.expect.user) {
        return `thread "${a.turn_id}" user: want ${JSON.stringify(a.expect.user)}, got ${JSON.stringify(t.user)}`;
      }
      if (a.expect.asst !== undefined && t.asst !== a.expect.asst) {
        return `thread "${a.turn_id}" asst: want ${JSON.stringify(a.expect.asst)}, got ${JSON.stringify(t.asst)}`;
      }
      return null;
    }
    case 'no_misroute': {
      const allowed = new Set(a.allowed_turn_ids);
      // For every turn_id present in the final state's threads, it must
      // be allowed. AND every event's turn_id must have ended up in its
      // own bubble. We reconstruct each turn's expected text from the
      // events, but BOUNDED by `connection_resume` markers — events
      // before a resume are considered "replayed and superseded" by the
      // post-resume tail. (This mirrors the session/hydrate semantic:
      // server replays from cursor; the post-resume events are
      // canonical.)
      //
      // If there is no connection_resume in the stream, we look at every
      // event. If there IS, we restart text accumulation from the
      // resume point.
      let lastResumeIdx = -1;
      for (let i = 0; i < events.length; i++) {
        if (events[i]!.type === 'connection_resume') lastResumeIdx = i;
      }
      const startIdx = lastResumeIdx >= 0 ? lastResumeIdx + 1 : 0;
      const bound: Map<TurnId, string> = new Map();
      for (let i = startIdx; i < events.length; i++) {
        const ev = events[i]!;
        const tid = turnIdOf(ev);
        if (!tid) continue;
        if (!allowed.has(tid)) continue;
        if (ev.type === 'message_delta') {
          bound.set(tid, (bound.get(tid) ?? '') + ev.text);
        } else if (ev.type === 'background_result') {
          bound.set(tid, (bound.get(tid) ?? '') + ev.text);
        } else if (ev.type === 'recovery_turn') {
          // Recovery output should land in the originating bubble.
          const ori = ev.originating_turn_id;
          bound.set(ori, (bound.get(ori) ?? '') + ev.text);
        }
      }
      for (const [tid, expectText] of bound) {
        const got = state.threads.get(tid);
        if (!got) {
          return `misroute: turn_id="${tid}" bubble missing — text "${expectText}" was bound elsewhere`;
        }
        if (!got.asst.includes(expectText) && expectText.length > 0) {
          // Find where the text actually landed.
          let landedIn: TurnId | null = null;
          for (const [otherTid, other] of state.threads) {
            if (otherTid !== tid && other.asst.includes(expectText)) {
              landedIn = otherTid;
              break;
            }
          }
          if (landedIn) {
            return `misroute: text "${expectText}" expected in turn_id="${tid}" but landed in turn_id="${landedIn}"`;
          }
          return `misroute: turn_id="${tid}" bubble does not contain expected text "${expectText}"`;
        }
      }
      return null;
    }
    case 'thread_order': {
      const got = state.thread_order;
      if (got.length !== a.expected.length) {
        return `thread_order length mismatch: want ${a.expected.length} (${JSON.stringify(a.expected)}), got ${got.length} (${JSON.stringify(got)})`;
      }
      for (let i = 0; i < got.length; i++) {
        if (got[i] !== a.expected[i]) {
          return `thread_order[${i}]: want "${a.expected[i]}", got "${got[i]}" (full: ${JSON.stringify(got)})`;
        }
      }
      return null;
    }
    case 'no_orphans': {
      if (state.rejected.length === 0) return null;
      const reasons = state.rejected
        .map(
          (r) =>
            `${r.event.type}@t=${r.event.t}(turn_id=${turnIdOf(r.event) ?? '<none>'}): ${r.reason}`,
        )
        .join('; ');
      return `${state.rejected.length} event(s) rejected as orphaned: ${reasons}`;
    }
    case 'thread_has_attachment': {
      const t = state.threads.get(a.turn_id);
      if (!t) return `thread for turn_id="${a.turn_id}" missing — cannot check attachment "${a.filename}"`;
      const found = (t.attachments ?? []).some(
        (att) => att.filename === a.filename,
      );
      if (!found) {
        // Check if the attachment landed elsewhere — that's the real bug.
        for (const [otherTid, other] of state.threads) {
          if (otherTid === a.turn_id) continue;
          if ((other.attachments ?? []).some((att) => att.filename === a.filename)) {
            return `attachment "${a.filename}" expected in turn_id="${a.turn_id}" but landed in turn_id="${otherTid}"`;
          }
        }
        return `attachment "${a.filename}" missing from turn_id="${a.turn_id}" (and not found in any other bubble)`;
      }
      return null;
    }
    case 'thread_attachments_equal': {
      const t = state.threads.get(a.turn_id);
      if (!t) return `thread for turn_id="${a.turn_id}" missing — cannot check attachments`;
      const gotAtt = t.attachments ?? [];
      // Prefer the typed `attachments` form when given (path-sensitive).
      if (a.attachments) {
        if (gotAtt.length !== a.attachments.length) {
          return `attachments length mismatch for turn_id="${a.turn_id}": want ${JSON.stringify(a.attachments)}, got ${JSON.stringify(gotAtt)}`;
        }
        for (let i = 0; i < gotAtt.length; i++) {
          const want = a.attachments[i]!;
          const got = gotAtt[i]!;
          if (got.filename !== want.filename || got.path !== want.path) {
            return `attachments[${i}] for turn_id="${a.turn_id}": want ${JSON.stringify(want)}, got ${JSON.stringify(got)}`;
          }
        }
        return null;
      }
      // Filename-only check (backwards-compat for simpler fixtures).
      const wantNames = a.filenames ?? [];
      const gotNames = gotAtt.map((x) => x.filename);
      if (gotNames.length !== wantNames.length) {
        return `attachments length mismatch for turn_id="${a.turn_id}": want ${JSON.stringify(wantNames)}, got ${JSON.stringify(gotNames)}`;
      }
      for (let i = 0; i < gotNames.length; i++) {
        if (gotNames[i] !== wantNames[i]) {
          return `attachments[${i}] for turn_id="${a.turn_id}": want "${wantNames[i]}", got "${gotNames[i]}" (full: ${JSON.stringify(gotNames)})`;
        }
      }
      return null;
    }
    case 'thread_has_tool_call': {
      const t = state.threads.get(a.turn_id);
      if (!t) return `thread for turn_id="${a.turn_id}" missing — cannot check tool_call "${a.tool_call_id}"`;
      const tc = (t.tool_calls ?? []).find((x) => x.tool_call_id === a.tool_call_id);
      if (!tc) {
        // Look for the tool call elsewhere — that's the misroute.
        for (const [otherTid, other] of state.threads) {
          if (otherTid === a.turn_id) continue;
          if ((other.tool_calls ?? []).some((x) => x.tool_call_id === a.tool_call_id)) {
            return `tool_call "${a.tool_call_id}" expected in turn_id="${a.turn_id}" but landed in turn_id="${otherTid}"`;
          }
        }
        return `tool_call "${a.tool_call_id}" missing from turn_id="${a.turn_id}"`;
      }
      if (a.tool_name !== undefined && tc.tool_name !== a.tool_name) {
        return `tool_call "${a.tool_call_id}" tool_name: want "${a.tool_name}", got "${tc.tool_name}"`;
      }
      if (a.success !== undefined && tc.success !== a.success) {
        return `tool_call "${a.tool_call_id}" success: want ${a.success}, got ${tc.success}`;
      }
      return null;
    }
    case 'thread_has_error': {
      const t = state.threads.get(a.turn_id);
      if (!t) return `thread for turn_id="${a.turn_id}" missing — cannot check error code "${a.code}"`;
      if (!t.error) {
        // Did the error land elsewhere?
        for (const [otherTid, other] of state.threads) {
          if (otherTid === a.turn_id) continue;
          if (other.error?.code === a.code) {
            return `turn_error code="${a.code}" expected in turn_id="${a.turn_id}" but landed in turn_id="${otherTid}"`;
          }
        }
        return `turn_error code="${a.code}" missing from turn_id="${a.turn_id}"`;
      }
      if (t.error.code !== a.code) {
        return `turn_error in turn_id="${a.turn_id}": want code="${a.code}", got code="${t.error.code}"`;
      }
      return null;
    }
  }
  return null;
}

/** Pull the turn_id out of any event that carries one. Connection signals
 *  return null. */
function turnIdOf(ev: SseEvent): TurnId | null {
  switch (ev.type) {
    case 'user_sent':
    case 'turn_started':
    case 'message_delta':
    case 'turn_completed':
    case 'turn_error':
    case 'tool_started':
    case 'tool_progress':
    case 'tool_completed':
    case 'background_result':
      return ev.turn_id;
    case 'recovery_turn':
      return ev.originating_turn_id;
    case 'connection_drop':
    case 'connection_resume':
      return null;
  }
}

/** Pretty-print a replay result for failure reports. Used by the Vitest
 *  wrappers to attach a readable diff to the assertion error. */
export function formatReplayResult(r: ReplayResult): string {
  const lines: string[] = [];
  lines.push(
    `[${r.pass ? 'PASS' : 'FAIL'}] ${r.fixture_name} (${r.events_processed} events, ${r.duration_ms.toFixed(2)}ms)`,
  );
  if (!r.pass) {
    lines.push(`  failures (${r.failures.length}):`);
    for (const f of r.failures) {
      lines.push(`    - [${f.assertion.kind}] ${f.message}`);
    }
    lines.push(`  final state.thread_order: ${JSON.stringify(r.final_state.thread_order)}`);
    // Iterate via thread_order, not Map insertion, so the output is
    // stable even if some future V8 changes Map iteration semantics.
    // Threads not in thread_order (shouldn't happen, but defensive)
    // get printed at the end.
    const printed = new Set<TurnId>();
    for (const tid of r.final_state.thread_order) {
      const t = r.final_state.threads.get(tid);
      if (!t) continue;
      printed.add(tid);
      const att = t.attachments?.length
        ? ` attachments=${JSON.stringify(t.attachments.map((a) => a.filename))}`
        : '';
      const tc = t.tool_calls?.length
        ? ` tool_calls=${JSON.stringify(t.tool_calls.map((c) => c.tool_call_id))}`
        : '';
      const er = t.error ? ` error=${JSON.stringify(t.error)}` : '';
      lines.push(
        `    thread "${tid}": user=${JSON.stringify(t.user)} asst=${JSON.stringify(t.asst)}${att}${tc}${er}`,
      );
    }
    for (const [tid, t] of r.final_state.threads) {
      if (printed.has(tid)) continue;
      lines.push(`    thread "${tid}" (off-order): user=${JSON.stringify(t.user)} asst=${JSON.stringify(t.asst)}`);
    }
    if (r.final_state.rejected.length > 0) {
      lines.push(`  rejected events: ${r.final_state.rejected.length}`);
      for (const rj of r.final_state.rejected) {
        lines.push(`    - ${rj.event.type}@t=${rj.event.t}: ${rj.reason}`);
      }
    }
  }
  return lines.join('\n');
}
