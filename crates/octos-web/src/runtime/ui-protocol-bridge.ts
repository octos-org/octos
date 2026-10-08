// Canonical OUP v2 projection bridge (API spec §14).
// Receives flattened EnvelopeV2 notification params, validates their payload,
// preserves session/topic routing, and enforces foreground and child terminals.
// Live delivery does not require negotiation. The v2 feature token requests
// additional hydrate replay fields. Unknown additive fields are preserved.

import type { Envelope, Seq } from './ui-protocol-types.js';
import { UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2, isPayloadV2, isRecord } from './ui-protocol-types.js';

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
  /** Hard-barrier drops: duplicate `terminal` on a closed thread.
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
  private readonly threads = new Map<string, ThreadState>();
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
    // Thread ids are scoped by the routed session. A shared connection may
    // carry different sessions with the same thread id.
    const threadKey = JSON.stringify([envelope.session_id ?? '', envelope.topic ?? null, envelope.thread_id]);
    const terminal = envelope.payload.type === 'turn_terminal'
      || envelope.payload.type === 'background/spawn_complete';
    const state = this.threads.get(threadKey) ?? {
      highestSeq: 0,
      completed: false,
    };

    // Hard-barrier enforcement (spec § 14.6).
    if (state.completed) {
      if (terminal) {
        this.metrics.duplicateCompletedDrops += 1;
        this.logger.warn(
          'projection.envelope.bridge: duplicate terminal on closed thread (dropped)',
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
    if (terminal) {
      state.completed = true;
    }
    this.threads.set(threadKey, state);
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
    if (!isRecord(params)) {
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
    if (typeof seq !== 'number' || !Number.isFinite(seq) || seq < 1 || !Number.isSafeInteger(seq)) {
      this.bumpMalformed('missing or non-positive integer seq');
      return null;
    }
    if (typeof candidate['turn_id'] !== 'string' || !candidate['turn_id']) {
      this.bumpMalformed('missing or empty turn_id');
      return null;
    }
    if (!isPayloadV2(payload)) {
      this.bumpMalformed('invalid v2 payload');
      return null;
    }
    const type = payload.type;
    const cursor = candidate['cursor'];
    if (cursor != null && (!isRecord(cursor) || typeof cursor.stream !== 'string'
      || typeof cursor.seq !== 'number' || !Number.isSafeInteger(cursor.seq) || cursor.seq < 0)) {
      this.bumpMalformed('invalid cursor');
      return null;
    }
    if ((candidate['session_id'] !== undefined && typeof candidate['session_id'] !== 'string')
      || (candidate['topic'] != null && typeof candidate['topic'] !== 'string')) {
      this.bumpMalformed('invalid routing');
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
    return { ...candidate, thread_id: threadId, turn_id: candidate['turn_id'], seq, payload };
  }

  private bumpMalformed(reason: string): void {
    this.metrics.malformed += 1;
    this.logger.warn(`projection.envelope.bridge: malformed envelope (${reason})`);
  }
}

/** Request this feature for the additional hydrate replay fields. It is not
 * required to receive live canonical v2 notifications. Kept as an API alias. */
export const REQUIRED_FEATURE = UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2;
