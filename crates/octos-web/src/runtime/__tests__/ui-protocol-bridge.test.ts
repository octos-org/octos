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

import { readFileSync } from 'node:fs';
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
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'x' } },
    });
    expect(bridge.metrics.malformed).toBe(1);
  });

  it('rejects envelope with non-integer seq', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 1.5,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'x' } },
    });
    expect(bridge.metrics.malformed).toBe(1);
  });

  it('rejects envelope with seq=0 (per-thread seq is 1-based)', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 0,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'x' } },
    });
    expect(bridge.metrics.malformed).toBe(1);
  });

  it('rejects envelope with unknown payload.type', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 1,
      payload: { type: 'no_such_kind', data: {} },
    });
    expect(bridge.metrics.malformed).toBe(1);
  });

  it('rejects envelope with non-string client_message_id', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
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
      thread_id: 'tA', turn_id: 'turn-A',
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
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 1,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'x' } },
    });
    expect(bridge.metrics.accepted).toBe(0);
    expect(bridge.metrics.malformed).toBe(0);
  });
});

describe('ProjectionEnvelopeBridge hard barrier (spec § 14.6)', () => {
  it('drops envelopes that arrive after turn_terminal on the same thread', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    const seen: unknown[] = [];
    bridge.onEnvelope((env) => seen.push(env));

    // Pre-completion envelopes accepted.
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 1,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'a' } },
    });
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 2,
      payload: { type: 'turn_terminal', data: { outcome: 'completed', token_usage: {} } },
    });
    expect(bridge.metrics.accepted).toBe(2);

    // Post-completion AssistantDelta on the SAME thread — must be dropped.
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 3,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'late' } },
    });
    expect(bridge.metrics.postCompletionDrops).toBe(1);
    expect(bridge.metrics.duplicateCompletedDrops).toBe(0);
    expect(bridge.metrics.accepted).toBe(2);
    expect(seen.length).toBe(2);
  });

  it('counts duplicate turn_terminal under the "duplicate_completed" label', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 1,
      payload: { type: 'turn_terminal', data: { outcome: 'completed', token_usage: {} } },
    });
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 2,
      payload: { type: 'turn_terminal', data: { outcome: 'completed', token_usage: {} } },
    });
    expect(bridge.metrics.duplicateCompletedDrops).toBe(1);
    expect(bridge.metrics.postCompletionDrops).toBe(0);
    expect(bridge.metrics.accepted).toBe(1);
  });

  it('barriers are per-thread — completion on tA does not affect tB', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 1,
      payload: { type: 'turn_terminal', data: { outcome: 'completed', token_usage: {} } },
    });
    // tB is unaffected — accepts envelopes freely.
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tB', turn_id: 'turn-B',
      seq: 1,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'hi' } },
    });
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tB', turn_id: 'turn-B',
      seq: 2,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'more' } },
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
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 1,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'a' } },
    });
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 2,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'b' } },
    });
    // Backward seq — violation, still surfaced.
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 1,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'backward' } },
    });
    expect(bridge.metrics.seqGaps).toBe(1);
    expect(bridge.metrics.accepted).toBe(3);
    expect(seen.length).toBe(3);
  });

  it('counts forward gaps under seqGaps but still surfaces the envelope', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 1,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'a' } },
    });
    // Skip seq=2 — gap.
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 5,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'gap' } },
    });
    expect(bridge.metrics.seqGaps).toBe(1);
    expect(bridge.metrics.accepted).toBe(2);
  });

  it('does not flag the first envelope of a new thread', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tA', turn_id: 'turn-A',
      seq: 1,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'a' } },
    });
    bridge.handle(PROJECTION_ENVELOPE_METHOD, {
      thread_id: 'tB', turn_id: 'turn-B',
      seq: 1,
      payload: { type: 'assistant_delta', data: { assistant_segment_id: 'segment-1', text: 'b' } },
    });
    expect(bridge.metrics.seqGaps).toBe(0);
    expect(bridge.metrics.accepted).toBe(2);
  });
});

describe('canonical v2 contract', () => {
  it.each(['completed', 'errored', 'interrupted', 'rate_limited'])('closes a thread on %s', outcome => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    const base = { thread_id: 't', turn_id: 'turn' };
    bridge.handle(PROJECTION_ENVELOPE_METHOD, { ...base, seq: 1,
      payload: { type: 'turn_terminal', data: { outcome } } });
    bridge.handle(PROJECTION_ENVELOPE_METHOD, { ...base, seq: 2,
      payload: { type: 'reasoning_delta', data: { text: 'late' } } });
    expect(bridge.metrics.accepted).toBe(1);
    expect(bridge.metrics.postCompletionDrops).toBe(1);
  });

  it('delivers a late child result after its parent closes, and closes only the child', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    const seen: unknown[] = [];
    bridge.onEnvelope(value => seen.push(value));
    bridge.handle(PROJECTION_ENVELOPE_METHOD, { thread_id: 'parent', turn_id: 'parent', seq: 1,
      payload: { type: 'turn_terminal', data: { outcome: 'completed' } } });
    const child = { thread_id: 'child', turn_id: 'child', seq: 1,
      payload: { type: 'background/spawn_complete', data: {
        parent_turn_id: 'parent', task_id: 'task', content: 'report', message_id: 'row',
        source: 'background', persisted_at: '2026-10-07T00:00:00Z',
      } } };
    bridge.handle(PROJECTION_ENVELOPE_METHOD, child);
    bridge.handle(PROJECTION_ENVELOPE_METHOD, { ...child, seq: 2 });
    expect(seen).toHaveLength(2);
    expect(bridge.metrics.duplicateCompletedDrops).toBe(1);
    expect(bridge.metrics.malformed).toBe(0);
  });

  it('keeps terminals isolated across sessions and topics', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    const base = { thread_id: 't', turn_id: 'turn' };
    bridge.handle(PROJECTION_ENVELOPE_METHOD, { ...base, session_id: 'a', topic: 'one', seq: 1,
      payload: { type: 'turn_terminal', data: { outcome: 'completed' } } });
    for (const [session_id, topic] of [['b', 'one'], ['a', 'two']]) {
      bridge.handle(PROJECTION_ENVELOPE_METHOD, { ...base, session_id, topic, seq: 1,
        payload: { type: 'reasoning_delta', data: { text: 'hello' } } });
    }
    expect(bridge.metrics.accepted).toBe(3);
    expect(bridge.metrics.postCompletionDrops).toBe(0);
  });

  it.each([
    { type: 'assistant_delta', data: { text: 'missing segment' } },
    { type: 'assistant_persisted', data: { text: 'missing metadata', assistant_segment_id: 's' } },
    { type: 'file_attached', data: { path: '/x', mime: 'text/plain', size_bytes: 1 } },
    { type: 'turn_terminal', data: { outcome: 'unknown' } },
    { type: 'turn_terminal', data: { outcome: 'errored', error: { code: 'oops' } } },
    { type: 'background/spawn_complete', data: { content: 'missing parent and task' } },
    { type: 'tool_end', data: { tool_call_id: 't', status: 'complete', duration_ms: -1 } },
    { type: 'user_message', data: { text: 'hello', files: null } },
    { type: 'turn_completed', data: {} },
  ])('rejects malformed or retired payload $type', payload => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    bridge.handle(PROJECTION_ENVELOPE_METHOD, { thread_id: 't', turn_id: 'turn', seq: 1, payload });
    expect(bridge.metrics.malformed).toBe(1);
    expect(bridge.metrics.accepted).toBe(0);
  });

  it('requires turn identity and preserves cursor/routing and additive fields', () => {
    const bridge = new ProjectionEnvelopeBridge(silentLogger());
    const seen: unknown[] = [];
    bridge.onEnvelope(value => seen.push(value));
    const env = { thread_id: 't', seq: 1, session_id: 'a#one', topic: 'one',
      cursor: { stream: 'a#one', seq: 18 }, future_field: true,
      payload: { type: 'reasoning_delta', data: { text: 'hello' } } };
    bridge.handle(PROJECTION_ENVELOPE_METHOD, env);
    const valid = { ...env, turn_id: 'turn' };
    bridge.handle(PROJECTION_ENVELOPE_METHOD, valid);
    expect(bridge.metrics.malformed).toBe(1);
    expect(seen).toEqual([valid]);
  });
});

function publishedProjectionExamples(spec: string): unknown[] {
  // Match Rust's fixture reader, including Git's Windows CRLF checkout mode.
  spec = spec.replace(/\r\n/g, '\n');
  const section = spec.split('## 14. Canonical v2 Projection Envelope\n')[1]?.split('\n## 15.')[0];
  expect(section).toBeDefined();
  return [...section!.matchAll(/```json\n([\s\S]*?)\n```/g)].map(match => JSON.parse(match[1]));
}

it.each(['\n', '\r\n'])('decodes every published canonical v2 spec example with %j line endings', lineEnding => {
  const spec = readFileSync(new URL('../../../../../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md', import.meta.url), 'utf8')
    .replace(/\r\n/g, '\n').replace(/\n/g, lineEnding);
  const examples = publishedProjectionExamples(spec);
  expect(examples).toHaveLength(11);
  const bridge = new ProjectionEnvelopeBridge(silentLogger());
  const seen: unknown[] = [];
  bridge.onEnvelope(value => seen.push(value));
  examples.forEach((example, index) => {
    const envelope = typeof example === 'object' && example !== null && 'payload' in example ? example : {
      thread_id: `example-${index}`, turn_id: `turn-${index}`, seq: 1, payload: example,
    };
    bridge.handle(PROJECTION_ENVELOPE_METHOD, envelope);
    expect(seen.at(-1)).toEqual(envelope);
  });
  expect(bridge.metrics.malformed).toBe(0);
  expect(bridge.metrics.accepted).toBe(11);
});
