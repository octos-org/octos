// Unit tests for the M9-γ projection-envelope bridge
// (UPCR-2026-014, spec § 14.6).
//
// Coverage:
//   - Malformed envelope rejection (missing fields, wrong types,
//     unknown payload.type).
//   - Hard-barrier post-completion drop (with metric labels matching
//     the server-side `octos_projection_post_completion_drop_total`
//     counter).
//   - Strict per-thread seq monotonicity (gap / backward seq
//     violations surface AND bump `seqGaps`).
//   - Listener fan-out for accepted envelopes.

import { describe, expect, it, vi } from 'vitest';

import {
  PROJECTION_ENVELOPE_METHOD,
  ProjectionEnvelopeBridge,
  type BridgeLogger,
} from '../ui-protocol-bridge.js';

function silentLogger(): BridgeLogger {
  return { warn: vi.fn() };
}

describe('ProjectionEnvelopeBridge.decodeAndValidate', () => {
  it('rejects non-object params', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, null);
    bridge.handle(PROJECTION_ENVELOPE_METHOD, 'string');
    bridge.handle(PROJECTION_ENVELOPE_METHOD, 42);
    expect(bridge.metrics.malformed).toBe(3);
    expect(bridge.metrics.accepted).toBe(0);
  });

  it('rejects envelope missing thread_id', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      seq: 1,
      payload: { type: 'assistant_delta', data: { text: 'x' } },
    });
    expect(bridge.metrics.malformed).toBe(1);
  });

  it('rejects envelope with non-integer seq', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 1.5,
      payload: { type: 'assistant_delta', data: { text: 'x' } },
    });
    expect(bridge.metrics.malformed).toBe(1);
  });

  it('rejects envelope with seq=0 (per-thread seq is 1-based)', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 0,
      payload: { type: 'assistant_delta', data: { text: 'x' } },
    });
    expect(bridge.metrics.malformed).toBe(1);
  });

  it('rejects envelope with unknown payload.type', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 1,
      payload: { type: 'no_such_kind', data: {} },
    });
    expect(bridge.metrics.malformed).toBe(1);
  });

  it('rejects envelope with non-string client_message_id', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 1,
      client_message_id: 12345,
      payload: { type: 'user_message', data: { text: 'hi' } },
    });
    expect(bridge.metrics.malformed).toBe(1);
  });

  it('accepts a well-formed envelope and routes to listeners', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    const seen: unknown[] = [];
    bridge.onEnvelope((env) => seen.push(env));
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 1,
      client_message_id: 'cmid-1',
      payload: { type: 'user_message', data: { text: 'hi' } },
    });
    expect(bridge.metrics.accepted).toBe(1);
    expect(bridge.metrics.malformed).toBe(0);
    expect(seen.length).toBe(1);
  });

  it('ignores methods other than projection/envelope', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle('message/delta', {
      thread_id: 'tA',
      seq: 1,
      payload: { type: 'assistant_delta', data: { text: 'x' } },
    });
    expect(bridge.metrics.accepted).toBe(0);
    expect(bridge.metrics.malformed).toBe(0);
  });
});

describe('ProjectionEnvelopeBridge hard barrier (spec § 14.6)', () => {
  it('drops envelopes that arrive after turn_completed on the same thread', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    const seen: unknown[] = [];
    bridge.onEnvelope((env) => seen.push(env));

    // Pre-completion envelopes accepted.
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 1,
      payload: { type: 'assistant_delta', data: { text: 'a' } },
    });
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 2,
      payload: { type: 'turn_completed', data: { token_usage: {} } },
    });
    expect(bridge.metrics.accepted).toBe(2);

    // Post-completion AssistantDelta on the SAME thread — must be dropped.
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 3,
      payload: { type: 'assistant_delta', data: { text: 'late' } },
    });
    expect(bridge.metrics.postCompletionDrops).toBe(1);
    expect(bridge.metrics.duplicateCompletedDrops).toBe(0);
    expect(bridge.metrics.accepted).toBe(2);
    expect(seen.length).toBe(2);
  });

  it('counts duplicate turn_completed under the "duplicate_completed" label', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 1,
      payload: { type: 'turn_completed', data: { token_usage: {} } },
    });
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 2,
      payload: { type: 'turn_completed', data: { token_usage: {} } },
    });
    expect(bridge.metrics.duplicateCompletedDrops).toBe(1);
    expect(bridge.metrics.postCompletionDrops).toBe(0);
    expect(bridge.metrics.accepted).toBe(1);
  });

  it('barriers are per-thread — completion on tA does not affect tB', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 1,
      payload: { type: 'turn_completed', data: { token_usage: {} } },
    });
    // tB is unaffected — accepts envelopes freely.
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tB',
      seq: 1,
      payload: { type: 'assistant_delta', data: { text: 'hi' } },
    });
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tB',
      seq: 2,
      payload: { type: 'assistant_delta', data: { text: 'more' } },
    });
    expect(bridge.metrics.accepted).toBe(3);
    expect(bridge.metrics.postCompletionDrops).toBe(0);
  });
});

describe('ProjectionEnvelopeBridge seq monotonicity assertion', () => {
  it('counts non-monotonic seqs under seqGaps but still surfaces the envelope', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    const seen: unknown[] = [];
    bridge.onEnvelope((env) => seen.push(env));

    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 1,
      payload: { type: 'assistant_delta', data: { text: 'a' } },
    });
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 2,
      payload: { type: 'assistant_delta', data: { text: 'b' } },
    });
    // Backward seq — violation, still surfaced.
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 1,
      payload: { type: 'assistant_delta', data: { text: 'backward' } },
    });
    expect(bridge.metrics.seqGaps).toBe(1);
    expect(bridge.metrics.accepted).toBe(3);
    expect(seen.length).toBe(3);
  });

  it('counts forward gaps under seqGaps but still surfaces the envelope', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 1,
      payload: { type: 'assistant_delta', data: { text: 'a' } },
    });
    // Skip seq=2 — gap.
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 5,
      payload: { type: 'assistant_delta', data: { text: 'gap' } },
    });
    expect(bridge.metrics.seqGaps).toBe(1);
    expect(bridge.metrics.accepted).toBe(2);
  });

  it('does not flag the first envelope of a new thread', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA',
      seq: 1,
      payload: { type: 'assistant_delta', data: { text: 'a' } },
    });
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tB',
      seq: 1,
      payload: { type: 'assistant_delta', data: { text: 'b' } },
    });
    expect(bridge.metrics.seqGaps).toBe(0);
    expect(bridge.metrics.accepted).toBe(2);
  });
});
