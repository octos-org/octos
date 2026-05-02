// Layer 1 SPA reducer fixture format.
//
// The fixture format mirrors the typed UI Protocol v1 envelope (see
// `crates/octos-core/src/ui_protocol.rs::TurnStartedEvent`,
// `MessageDeltaEvent`, etc.). Every event MUST carry an explicit `turn_id` —
// that is the contract this layer enforces. The replay engine feeds events
// into the reducer in `t` order; assertions run against the final state.
//
// Two design notes:
//
// 1. `SseFixture` is JSON-serializable end-to-end. PR I (capture-and-replay
//    flag in the live harness) writes captures as JSON; this layer accepts
//    them without modification. The TS files in fixtures/ are the
//    hand-authored seeds; future captured fixtures will be `*.json`.
//
// 2. `Assertion` is a discriminated union, not a function. That keeps
//    fixtures fully declarative (so they round-trip through JSON) and lets
//    the replay engine produce structured failure reports — which is
//    important when a Layer 2 capture-replay run wants to file the JSON
//    directly into a regression test without compilation.

// ── Identifiers (mirror the Rust newtypes) ───────────────────────────────
//
// All three are wire strings. The reducer treats them as opaque. The point
// of the typed identity work in PR A is that they are NOT interchangeable
// at the Rust layer; here we only assert that the wire form is consistent.

/** Server protocol identity. UUIDv7 string in production. */
export type TurnId = string;

/** Render grouping (which DOM bubble). Currently equals the originating
 *  user message's turn_id; PR J makes this explicit. */
export type ThreadId = string;

/** Client optimism + idempotency token. Web client mints. UUIDv7 in prod. */
export type ClientMessageId = string;

/** Session identity. */
export type SessionKey = string;

// ── SSE / UI Protocol event payloads ─────────────────────────────────────
//
// These are the SHAPES the SPA reducer consumes. They mirror the Rust
// `UiNotification` variants in `octos-core/src/ui_protocol.rs`. The shape
// names match the wire `"type"` (snake_case as serialized by the server).
//
// Every event carries `turn_id` — even `user_sent`, which is the client's
// own optimistic synthesis at send time. The reducer NEVER fabricates a
// turn_id from "current sticky" or "latest message"; if `turn_id` is
// missing on an event, that event is invalid and the reducer rejects it.

interface SseEventBase {
  /** Logical time in ms from fixture origin. Used by the replay engine to
   *  order events; not passed to the reducer (the reducer is stateless wrt
   *  wall clock). */
  t: number;
  /** Wire `"type"` discriminant. */
  type: string;
}

/** Optimistic user message, minted client-side. Carries the cmid the
 *  client generated and the turn_id the server later confirms via
 *  `turn/started`. In live capture, cmid arrives first; turn_id is filled
 *  in when the server responds. For fixtures, both are pre-stamped. */
export interface UserSentEvent extends SseEventBase {
  type: 'user_sent';
  session_id: SessionKey;
  turn_id: TurnId;
  cmid: ClientMessageId;
  text: string;
}

/** Server confirms a turn has started. Mirrors `TurnStartedEvent`. */
export interface TurnStartedEvent extends SseEventBase {
  type: 'turn_started';
  session_id: SessionKey;
  turn_id: TurnId;
}

/** Streaming assistant text. Mirrors `MessageDeltaEvent`. */
export interface MessageDeltaEvent extends SseEventBase {
  type: 'message_delta';
  session_id: SessionKey;
  turn_id: TurnId;
  text: string;
}

/** Server confirms a turn has completed. Mirrors `TurnCompletedEvent`. */
export interface TurnCompletedEvent extends SseEventBase {
  type: 'turn_completed';
  session_id: SessionKey;
  turn_id: TurnId;
}

/** Server reports a turn errored. Mirrors `TurnErrorEvent`. */
export interface TurnErrorEvent extends SseEventBase {
  type: 'turn_error';
  session_id: SessionKey;
  turn_id: TurnId;
  code: string;
  message: string;
}

/** Tool invocation begun. Mirrors `ToolStartedEvent`. */
export interface ToolStartedEvent extends SseEventBase {
  type: 'tool_started';
  session_id: SessionKey;
  turn_id: TurnId;
  tool_call_id: string;
  tool_name: string;
}

/** Tool progress update. Mirrors `ToolProgressEvent`. */
export interface ToolProgressEvent extends SseEventBase {
  type: 'tool_progress';
  session_id: SessionKey;
  turn_id: TurnId;
  tool_call_id: string;
  message?: string;
}

/** Tool completed (success or failure). Mirrors `ToolCompletedEvent`.
 *  When `success === false`, the agent typically issues a retry —
 *  modeled by emitting a fresh `tool_started` with a NEW `tool_call_id`
 *  bound to the SAME `turn_id`. Issue #680 / "tool retry collapse" was
 *  the bug where the retry's output bound to whatever turn was sticky
 *  by the time the retry fired. */
export interface ToolCompletedEvent extends SseEventBase {
  type: 'tool_completed';
  session_id: SessionKey;
  turn_id: TurnId;
  tool_call_id: string;
  tool_name: string;
  success: boolean;
  output_preview?: string;
}

/** Background spawn-only result delivery. Carries the originating turn_id
 *  so the result lands in the right bubble even when the live sticky map
 *  has rotated forward. Issue #738. */
export interface BackgroundResultEvent extends SseEventBase {
  type: 'background_result';
  session_id: SessionKey;
  turn_id: TurnId;
  text: string;
  /** Optional file attachments (e.g. `.md` reports from deep_search). */
  attachments?: Array<{ filename: string; path: string }>;
}

/** Recovery-turn fired after a child failure. Mirrors M8.9 recovery flow.
 *  The result MUST inherit the originating turn_id, NOT the recovery's
 *  fresh turn_id. */
export interface RecoveryTurnEvent extends SseEventBase {
  type: 'recovery_turn';
  session_id: SessionKey;
  /** The new turn_id the server allocated for the recovery turn. */
  recovery_turn_id: TurnId;
  /** The originating turn_id whose bubble the recovery's output belongs in. */
  originating_turn_id: TurnId;
  text: string;
}

/** Connection drop / reconnect signal. Used by the reload-replay fixture
 *  to assert that the post-reconnect graph equals the pre-disconnect graph. */
export interface ConnectionDropEvent extends SseEventBase {
  type: 'connection_drop';
}

/** Reconnect; the engine resumes feeding events after this marker. The
 *  reducer is preserved across the gap; only the transport reset. */
export interface ConnectionResumeEvent extends SseEventBase {
  type: 'connection_resume';
  /** Cursor the server replays from. */
  cursor?: string;
}

/** Discriminated union of every event the replay engine recognizes. */
export type SseEvent =
  | UserSentEvent
  | TurnStartedEvent
  | MessageDeltaEvent
  | TurnCompletedEvent
  | TurnErrorEvent
  | ToolStartedEvent
  | ToolProgressEvent
  | ToolCompletedEvent
  | BackgroundResultEvent
  | RecoveryTurnEvent
  | ConnectionDropEvent
  | ConnectionResumeEvent;

// ── Assertions (declarative, JSON-serializable) ──────────────────────────

/** A tool call rendered inside a thread bubble. */
export interface ToolCallRecord {
  tool_call_id: string;
  tool_name: string;
  /** Final outcome — undefined while still in flight. */
  success?: boolean;
  output_preview?: string;
}

/** Error state attached to a thread bubble (set by `turn_error`). */
export interface ThreadError {
  code: string;
  message: string;
}

/** A single thread bubble in the rendered chat. The reducer's output
 *  state.threads must contain one of these per turn_id. */
export interface ThreadBubble {
  turn_id: TurnId;
  user: string;
  asst: string;
  attachments?: Array<{ filename: string; path: string }>;
  tool_calls?: ToolCallRecord[];
  /** Set when a `turn_error` arrived for this turn. */
  error?: ThreadError;
}

/** Discriminated assertion union. `kind` selects the check; the engine
 *  reports `message` on failure. */
export type Assertion =
  /** The thread for `turn_id` exists and matches the given expectation. */
  | {
      kind: 'thread_equals';
      turn_id: TurnId;
      expect: { user?: string; asst?: string };
    }
  /** No assistant text, attachment, or delta has bound to a turn_id other
   *  than the listed allowed_turn_ids. Catches misroute bugs (#649/#740). */
  | {
      kind: 'no_misroute';
      allowed_turn_ids: TurnId[];
    }
  /** The threads in final state, in display order, exactly equal `expected`.
   *  Order matters because the bug class includes off-by-one bubble
   *  ordering. */
  | {
      kind: 'thread_order';
      expected: TurnId[];
    }
  /** Every event with a `turn_id` was bound to a thread known to the
   *  reducer. Catches "ghost-thread" bugs where a delta arrives for a turn
   *  the client never saw start. */
  | {
      kind: 'no_orphans';
    }
  /** A specific turn_id's bubble carries the named attachment. Used by
   *  spawn_only fixtures (#738) to assert .md result delivery. */
  | {
      kind: 'thread_has_attachment';
      turn_id: TurnId;
      filename: string;
    }
  /** A turn_id's bubble carries EXACTLY this list of attachments,
   *  order-sensitive. Each entry can be either a bare filename string
   *  (filename-only check) or a `{filename, path}` pair (both must
   *  match). Use the pair form when paths discriminate (e.g. two
   *  turns produce same-named attachments from different temp dirs).
   *  Catches multi-attachment dedup bugs and out-of-order rendering. */
  | {
      kind: 'thread_attachments_equal';
      turn_id: TurnId;
      /** Backwards-compatible: keep `filenames` for fixtures that only
       *  care about the names. New fixtures should prefer
       *  `attachments`. */
      filenames?: string[];
      attachments?: Array<{ filename: string; path: string }>;
    }
  /** A turn_id's bubble has the given tool call recorded with the given
   *  outcome. Used by tool-retry-collapse (#680) to assert the retry's
   *  output binds to the originating turn even after sticky rotates. */
  | {
      kind: 'thread_has_tool_call';
      turn_id: TurnId;
      tool_call_id: string;
      tool_name?: string;
      success?: boolean;
    }
  /** A turn_id's bubble carries a turn_error with the given code. */
  | {
      kind: 'thread_has_error';
      turn_id: TurnId;
      code: string;
    };

// ── The fixture container ────────────────────────────────────────────────

/** A complete fixture: events to replay and assertions to run against the
 *  final reducer state. */
export interface SseFixture {
  /** Stable name; used in failure messages and CI logs. */
  name: string;
  /** Human-readable description of the bug class this catches. */
  description: string;
  /** Optional GitHub issue references documenting the originating bug(s). */
  issues?: number[];
  /** Session id used by every event. Defaults to `'fixture-session'`. */
  session_id?: SessionKey;
  /** Ordered (by `t`) event stream. */
  events: SseEvent[];
  /** Assertions evaluated against the final state. */
  assertions: Assertion[];
}
