// UI Protocol v1 — M9-γ canonical projection envelope BRIDGE
// (UPCR-2026-014, spec § 14).
//
// This module is the canonical CLIENT consumer for the M9-γ
// `projection/envelope` notification surface. Server emit is in
// `crates/octos-cli/src/api/ui_protocol*.rs` (Rust); this bridge is the
// matching TypeScript decode + invariant-check layer.
//
// Once the server-emit + per-connection live filter cutover lands
// (this PR), a WS connection that negotiated `projection.envelope.v1`
// receives ONLY `projection/envelope` notifications for the events
// that surface had legacy analogs (`message/delta`, `message/persisted`,
// `tool/*`, `turn/completed`, `file/attached`). Legacy clients keep
// seeing those notifications; the per-connection mutual exclusion is
// enforced server-side in `live_event_passes_capability_filter`.
//
// The bridge:
//   1. Recognises `projection/envelope` notifications on the WS.
//   2. Validates the wire payload against the existing TS `Envelope`
//      type at `ui-protocol-types.ts:219-229`. Malformed envelopes are
//      rejected (logged + counted in `bridge_malformed_total`).
//      NOTE (feat(envelope-wire-routing)): the wire now also carries
//      `session_id` (+ optional `topic`) FLATTENED alongside the bare
//      `Envelope` fields for multi-session routing (spec § 14.1). This
//      bridge reads only `thread_id`/`seq`/`payload`/`client_message_id`
//      and IGNORES the extra routing keys — the web SPA holds a single
//      session per connection, so they are not needed here. The cast to
//      `Envelope` is intentionally tolerant of the extra keys.
//   3. Enforces the hard barrier from spec § 14.6 — once a
//      `turn_completed` envelope arrives for `thread_id` T, any
//      subsequent envelope on the same thread is DROPPED and the drop
//      is counted in `bridge_post_completion_drop_total` (kind label
//      `"duplicate_completed"` vs `"post_completion"`, matching the
//      server-side metric).
//   4. Asserts strict per-thread `seq` monotonicity. A gap or a
//      backward `seq` is logged and counted in
//      `bridge_seq_gap_total`; the bridge keeps emitting (the
//      projection is the source of truth for what to do with gaps —
//      typically rehydrate via cursor).
//   5. Provides a typed callback API (`onEnvelope`) the projection
//      function subscribes to.
//
// Spec: `api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md` § 14.

import type { Envelope, ThreadId, Seq } from './ui-protocol-types.js';
import { UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1 } from './ui-protocol-types.js';

/** Method literal for the projection-envelope notification.
 *  Mirrors `methods::PROJECTION_ENVELOPE` in the Rust types. */
export const PROJECTION_ENVELOPE_METHOD = 'projection/envelope';

/** Per-bridge invariant counters surfaced for ops monitoring. The
 *  fields mirror the server-side metric labels so an operator sees
 *  matching numbers on both ends. */
export interface BridgeMetrics {
  /** Envelopes accepted by the bridge — payload type-valid, seq
   *  monotonic, hard barrier not tripped. */
  accepted: number;
  /** Wire payload failed shape validation (missing `thread_id` /
   *  `seq` / `payload`, unknown `payload.type`, etc.). */
  malformed: number;
  /** Hard-barrier drops: post-completion envelopes on a closed thread.
   *  Mirrors `octos_projection_post_completion_drop_total{kind="post_completion"}`. */
  postCompletionDrops: number;
  /** Hard-barrier drops: duplicate `turn_completed` on a closed thread.
   *  Mirrors `octos_projection_post_completion_drop_total{kind="duplicate_completed"}`. */
  duplicateCompletedDrops: number;
  /** Seq monotonicity violations (gap or backward seq). The bridge
   *  still surfaces the envelope; the projection decides whether to
   *  rehydrate. */
  seqGaps: number;
}

/** Callback shape for accepted envelopes. The projection function
 *  subscribes via `bridge.onEnvelope(cb)`. */
export type EnvelopeListener = (envelope: Envelope) => void;

/** Per-thread state for hard-barrier + monotonic-seq enforcement. */
interface ThreadState {
  highestSeq: Seq;
  completed: boolean;
}

/** Optional logger interface — defaults to `console`. Unit tests pass
 *  a quieter logger to avoid polluting test output. */
export interface BridgeLogger {
  warn(message: string, context?: unknown): void;
}

const defaultLogger: BridgeLogger = {
  // eslint-disable-next-line no-console
  warn: (msg, ctx) => console.warn(msg, ctx),
};

/** The bridge surface. One instance per WS connection. Callers
 *  feed each notification through `bridge.handle(method, params)` and
 *  subscribe to validated envelopes via `bridge.onEnvelope(cb)`. */
export class ProjectionEnvelopeBridge {
  private readonly listeners: EnvelopeListener[] = [];
  private readonly threads = new Map<ThreadId, ThreadState>();
  private readonly logger: BridgeLogger;
  readonly metrics: BridgeMetrics = {
    accepted: 0,
    malformed: 0,
    postCompletionDrops: 0,
    duplicateCompletedDrops: 0,
    seqGaps: 0,
  };

  constructor(logger: BridgeLogger = defaultLogger) {
    this.logger = logger;
  }

  /** Subscribe a callback that fires for every envelope the bridge
   *  accepts after hard-barrier + shape validation. */
  onEnvelope(listener: EnvelopeListener): void {
    this.listeners.push(listener);
  }

  /** Feed a single JSON-RPC notification through the bridge. Methods
   *  other than `projection/envelope` are silently ignored — the
   *  caller may safely feed every WS frame. */
  handle(method: string, params: unknown): void {
    if (method !== PROJECTION_ENVELOPE_METHOD) {
      return;
    }
    const envelope = this.decodeAndValidate(params);
    if (!envelope) {
      return;
    }
    const state = this.threads.get(envelope.thread_id) ?? {
      highestSeq: 0,
      completed: false,
    };

    // Hard-barrier enforcement (spec § 14.6).
    if (state.completed) {
      const isTurnCompleted = envelope.payload.type === 'turn_completed';
      if (isTurnCompleted) {
        this.metrics.duplicateCompletedDrops += 1;
        this.logger.warn(
          'projection.envelope.bridge: duplicate turn_completed on closed thread (dropped)',
          { thread_id: envelope.thread_id, seq: envelope.seq },
        );
      } else {
        this.metrics.postCompletionDrops += 1;
        this.logger.warn(
          'projection.envelope.bridge: post-completion envelope (dropped)',
          {
            thread_id: envelope.thread_id,
            seq: envelope.seq,
            type: envelope.payload.type,
          },
        );
      }
      return;
    }

    // Seq monotonicity check (spec § 14.1: strictly monotonic; gaps
    // are an error and trigger rehydration). We log and count the
    // violation but still surface the envelope so the projection can
    // decide whether to ignore the gap or kick off a rehydrate.
    if (envelope.seq <= state.highestSeq) {
      this.metrics.seqGaps += 1;
      this.logger.warn(
        'projection.envelope.bridge: non-monotonic seq (still surfaced)',
        {
          thread_id: envelope.thread_id,
          seq: envelope.seq,
          highest_seq: state.highestSeq,
        },
      );
    } else if (envelope.seq !== state.highestSeq + 1 && state.highestSeq !== 0) {
      // Forward gap — also a violation under § 14.1, but the bridge
      // surfaces the envelope and lets the projection rehydrate.
      this.metrics.seqGaps += 1;
      this.logger.warn(
        'projection.envelope.bridge: seq gap (still surfaced)',
        {
          thread_id: envelope.thread_id,
          seq: envelope.seq,
          expected: state.highestSeq + 1,
        },
      );
    }

    if (envelope.seq > state.highestSeq) {
      state.highestSeq = envelope.seq;
    }
    if (envelope.payload.type === 'turn_completed') {
      state.completed = true;
    }
    this.threads.set(envelope.thread_id, state);
    this.metrics.accepted += 1;
    for (const listener of this.listeners) {
      try {
        listener(envelope);
      } catch (err) {
        this.logger.warn('projection.envelope.bridge: listener threw', err);
      }
    }
  }

  /** Validate the JSON-RPC `params` payload against the wire schema
   *  for the envelope. Returns the typed envelope on success or
   *  `null` on shape failure (the malformed counter is bumped). */
  private decodeAndValidate(params: unknown): Envelope | null {
    if (!params || typeof params !== 'object') {
      this.bumpMalformed('non-object params');
      return null;
    }
    const candidate = params as Record<string, unknown>;
    const threadId = candidate['thread_id'];
    const seq = candidate['seq'];
    const payload = candidate['payload'];
    if (typeof threadId !== 'string' || threadId.length === 0) {
      this.bumpMalformed('missing or empty thread_id');
      return null;
    }
    if (typeof seq !== 'number' || !Number.isFinite(seq) || seq < 1 || !Number.isInteger(seq)) {
      this.bumpMalformed('missing or non-positive integer seq');
      return null;
    }
    if (!payload || typeof payload !== 'object') {
      this.bumpMalformed('missing payload');
      return null;
    }
    const payloadObj = payload as Record<string, unknown>;
    const type = payloadObj['type'];
    const data = payloadObj['data'];
    if (typeof type !== 'string') {
      this.bumpMalformed('missing payload.type');
      return null;
    }
    if (!data || typeof data !== 'object') {
      this.bumpMalformed('missing payload.data');
      return null;
    }
    const knownTypes = [
      'user_message',
      'assistant_delta',
      'assistant_persisted',
      'tool_start',
      'tool_progress',
      'tool_end',
      'file_attached',
      'turn_completed',
    ];
    if (!knownTypes.includes(type)) {
      this.bumpMalformed(`unknown payload.type: ${type}`);
      return null;
    }
    const clientMessageId = candidate['client_message_id'];
    if (
      clientMessageId !== undefined &&
      clientMessageId !== null &&
      typeof clientMessageId !== 'string'
    ) {
      this.bumpMalformed('client_message_id must be string when present');
      return null;
    }
    // Per spec § 14.1: `client_message_id` is ONLY populated on
    // `user_message` envelopes. A server emitting it on any other
    // variant is a wire contract violation — we surface but log.
    if (clientMessageId && type !== 'user_message') {
      this.logger.warn(
        'projection.envelope.bridge: client_message_id present on non-user_message variant (wire contract violation)',
        { thread_id: threadId, seq, type },
      );
    }
    return params as Envelope;
  }

  private bumpMalformed(reason: string): void {
    this.metrics.malformed += 1;
    this.logger.warn(`projection.envelope.bridge: malformed envelope (${reason})`);
  }
}

/** Convenience: the wire feature flag the bridge expects to have been
 *  negotiated at session/open. Re-exported from `ui-protocol-types`
 *  for caller ergonomics — passing this string into a `session/open`
 *  request's `X-Octos-Ui-Features` opts the connection into the M9-γ
 *  cutover. */
export const REQUIRED_FEATURE = UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1;
