// UI Protocol v1 — M9-γ canonical projection envelope (UPCR-2026-014).
//
// This file is the TypeScript counterpart of
// `crates/octos-core/src/ui_protocol.rs` for the M9-γ projection
// envelope. The two MUST stay byte-aligned: the Rust enum uses
// `serde(tag = "type", content = "data", rename_all = "snake_case")`,
// so every wire JSON value here round-trips through `serde_json` on
// the server side and through this discriminated union on the client.
//
// Spec: `api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md` § 14
// "M9-γ Envelope".
// ADR: `docs/M9-GAMMA-SERVER-PROJECTION-ADR.md`.
//
// **Hard barrier**: per the ADR, `turn_completed` is the terminal
// payload for a `thread_id`. No further `assistant_*`/`tool_*`
// payloads on the same thread are valid after it.
//
// This module is type-only — it has zero runtime cost. The M9-γ-2
// projection function will import these types and the M9-γ-3 cutover
// will route the `ThreadStore` ingest path through them.

// ── Identifier aliases ───────────────────────────────────────────────────
//
// All wire strings; the projection treats them as opaque. They mirror
// the Rust newtypes in `octos-core` but are NOT the same as the legacy
// fixture-types in `crates/octos-web/src/state/__tests__/lib/fixture-types.ts`,
// which carry `turn_id`. Identity in M9-γ collapses to `seq` — there
// is no `turn_id` on the envelope.

/** Multi-turn cluster identity — the chat thread this envelope projects
 *  into. All envelopes for one logical conversation share a `thread_id`. */
export type ThreadId = string;

/** Server-assigned UUID of a durable message row. Stable across replays.
 *  Mirrors `MessageMeta.message_id` in the Rust types. */
export type MessageId = string;

/** Client optimism + idempotency token. Web client mints; UUIDv7 in prod.
 *  ONLY the optimistic <GhostBubble> overlay consults this — the
 *  projection itself never does. */
export type ClientMessageId = string;

/** RFC 3339 timestamp string (e.g. "2026-05-09T18:30:01Z"). */
export type IsoTimestamp = string;

/** Sub-typed numeric for the strict per-thread server ordering. */
export type Seq = number;

// ── Token usage ──────────────────────────────────────────────────────────

/** Token usage carried on `turn_completed` envelopes. Mirrors
 *  `EnvelopeTokenUsage` in the Rust types. All fields default to zero
 *  and are omitted on the wire when zero (serde
 *  `skip_serializing_if = "is_zero_u64"`). */
export interface EnvelopeTokenUsage {
  input_tokens?: number;
  output_tokens?: number;
  reasoning_tokens?: number;
  cache_read_tokens?: number;
  cache_write_tokens?: number;
}

// ── Message metadata ─────────────────────────────────────────────────────

/** Metadata carried on `assistant_persisted` envelopes. Mirrors
 *  `MessageMeta` in the Rust types. */
export interface MessageMeta {
  /** Server-assigned UUID of the durable row. */
  message_id: MessageId;
  /** Wall-clock RFC 3339 commit time. */
  persisted_at: IsoTimestamp;
  /** File attachments persisted with the message — typically a single
   *  `.md` / `.mp3` / `.pptx` artefact. Empty for assistant rows that
   *  carry only text. Omitted on the wire when empty. */
  media?: string[];
}

// ── Tool end status ──────────────────────────────────────────────────────

/** Status carried on `tool_end` payloads. Mirrors `EnvelopeToolEndStatus`
 *  (snake_case wire form) in the Rust types. */
export type EnvelopeToolEndStatus = 'complete' | 'error';

// ── Payload variants (sealed tagged union) ───────────────────────────────
//
// The Rust enum uses `serde(tag = "type", content = "data", rename_all =
// "snake_case")`, so each variant on the wire looks like:
//
//   { "type": "assistant_delta", "data": { "text": "…" } }
//
// The TS shape mirrors this exactly. Adding a new variant is a wire
// contract change and requires a follow-up UPCR.

interface AssistantDeltaPayload {
  type: 'assistant_delta';
  data: { text: string };
}

interface AssistantPersistedPayload {
  type: 'assistant_persisted';
  data: { text: string; meta: MessageMeta };
}

interface ToolStartPayload {
  type: 'tool_start';
  data: { tool_call_id: string; name: string };
}

interface ToolProgressPayload {
  type: 'tool_progress';
  data: { tool_call_id: string; message: string };
}

interface ToolEndPayload {
  type: 'tool_end';
  data: {
    tool_call_id: string;
    status: EnvelopeToolEndStatus;
    /** Set iff `status === 'error'`. Omitted on the wire when null. */
    error?: string;
  };
}

interface FileAttachedPayload {
  type: 'file_attached';
  data: { path: string; mime: string; size_bytes: number };
}

interface TurnCompletedPayload {
  type: 'turn_completed';
  data: { token_usage: EnvelopeTokenUsage };
}

/** Sealed tagged union of payloads carried by the M9-γ projection
 *  envelope. The discriminator is `type`; payload data lives under
 *  `data`. Variant names are snake_case to match the wire / Rust shape. */
export type Payload =
  | AssistantDeltaPayload
  | AssistantPersistedPayload
  | ToolStartPayload
  | ToolProgressPayload
  | ToolEndPayload
  | FileAttachedPayload
  | TurnCompletedPayload;

// ── Envelope ─────────────────────────────────────────────────────────────

/** Canonical M9-γ projection envelope.
 *
 *  Per UPCR-2026-014 and the M9-γ ADR, this is the single shape the
 *  web client's deterministic projection consumes. The committed
 *  envelope log is `Envelope[]` indexed by `(thread_id, seq)`; the
 *  projection is a pure function from that log to `ChatViewModel`.
 *
 *  Identity collapses to `seq` — the only key the projection cares
 *  about. `client_message_id` lives ONLY on user-message-rooted
 *  envelopes so the optimistic `<GhostBubble>` overlay can match its
 *  server reflection and unmount; the projection itself NEVER consults
 *  it. */
export interface Envelope {
  thread_id: ThreadId;
  seq: Seq;
  /** Present on user-message-rooted envelopes (the optimistic
   *  `<GhostBubble>` overlay matches its server reflection here).
   *  Absent on internal events (assistant deltas, tool events,
   *  turn_completed). The projection MUST NOT consult this field. */
  client_message_id?: ClientMessageId;
  payload: Payload;
}

// ── Capability feature flag ──────────────────────────────────────────────

/** Wire-form capability flag for UPCR-2026-014. Servers advertise it via
 *  `UiProtocolCapabilities.supported_features`; clients request it via
 *  the `X-Octos-Ui-Features` header. Mirrors
 *  `UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1` in the Rust types. */
export const UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1 = 'projection.envelope.v1';

// ── Type guards (optional ergonomic helpers) ─────────────────────────────
//
// The projection function will switch on `envelope.payload.type` and
// rely on TS's discriminated-union narrowing. These helpers exist for
// callers that need a runtime check (e.g. a debug overlay rendering a
// raw envelope).

export function isAssistantDelta(p: Payload): p is AssistantDeltaPayload {
  return p.type === 'assistant_delta';
}

export function isAssistantPersisted(p: Payload): p is AssistantPersistedPayload {
  return p.type === 'assistant_persisted';
}

export function isToolStart(p: Payload): p is ToolStartPayload {
  return p.type === 'tool_start';
}

export function isToolProgress(p: Payload): p is ToolProgressPayload {
  return p.type === 'tool_progress';
}

export function isToolEnd(p: Payload): p is ToolEndPayload {
  return p.type === 'tool_end';
}

export function isFileAttached(p: Payload): p is FileAttachedPayload {
  return p.type === 'file_attached';
}

export function isTurnCompleted(p: Payload): p is TurnCompletedPayload {
  return p.type === 'turn_completed';
}
