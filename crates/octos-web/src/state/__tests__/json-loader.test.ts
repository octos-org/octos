// Tests for the JSON fixture loader. The contract:
//
//   1. Flat-form fixtures (the TS shape) load through unchanged.
//   2. Wrapped-form fixtures (UI Protocol JSON-RPC envelopes) normalize
//      to the flat shape; the replay engine sees them as if they were
//      authored as TS.
//   3. Validation errors include a path so a fixture-promotion operator
//      can locate the bad field instantly.

import { describe, expect, it } from 'vitest';

import { loadFixtureJson, normalizeEvent } from './lib/json-loader.js';
import { replayFixture } from './lib/replay-fixture.js';
import { fixedReducer } from './lib/fixed-reducer.js';

describe('Layer 1: JSON fixture loader', () => {
  it('loads flat-form fixtures (round-trips with replay engine)', () => {
    const flat = {
      name: 'flat-form-test',
      description: 'simple round-trip',
      session_id: 's',
      events: [
        {
          t: 0,
          type: 'user_sent',
          session_id: 's',
          turn_id: 'A',
          cmid: 'A',
          text: 'q',
        },
        { t: 100, type: 'turn_started', session_id: 's', turn_id: 'A' },
        {
          t: 200,
          type: 'message_delta',
          session_id: 's',
          turn_id: 'A',
          text: 'a',
        },
      ],
      assertions: [
        {
          kind: 'thread_equals',
          turn_id: 'A',
          expect: { user: 'q', asst: 'a' },
        },
      ],
    };
    const fixture = loadFixtureJson(flat);
    const result = replayFixture(fixture, fixedReducer);
    expect(result.pass).toBe(true);
  });

  it('normalizes wrapped-form (UI Protocol method/params) to flat', () => {
    const wrapped = {
      name: 'wrapped-form-test',
      description: 'capture-and-replay shape',
      session_id: 's',
      events: [
        // user_sent is PR H-internal — uses our 'user/sent' method name.
        {
          t: 0,
          envelope: {
            jsonrpc: '2.0',
            method: 'user/sent',
            params: {
              session_id: 's',
              turn_id: 'A',
              cmid: 'A',
              text: 'q',
            },
          },
        },
        {
          t: 100,
          envelope: {
            jsonrpc: '2.0',
            method: 'turn/started',
            params: { session_id: 's', turn_id: 'A' },
          },
        },
        {
          t: 200,
          envelope: {
            jsonrpc: '2.0',
            method: 'message/delta',
            params: { session_id: 's', turn_id: 'A', text: 'a' },
          },
        },
      ],
      assertions: [
        {
          kind: 'thread_equals',
          turn_id: 'A',
          expect: { user: 'q', asst: 'a' },
        },
      ],
    };
    const fixture = loadFixtureJson(wrapped);
    expect(fixture.events[0]!.type).toBe('user_sent');
    expect(fixture.events[1]!.type).toBe('turn_started');
    expect(fixture.events[2]!.type).toBe('message_delta');
    const result = replayFixture(fixture, fixedReducer);
    expect(result.pass).toBe(true);
  });

  it('rejects unknown UI Protocol methods with a clear message', () => {
    expect(() =>
      normalizeEvent(
        {
          t: 0,
          envelope: {
            jsonrpc: '2.0',
            method: 'turn/teleport',
            params: {},
          },
        },
        'fixture.events[0]',
      ),
    ).toThrow(/unknown UI Protocol method "turn\/teleport"/);
  });

  it('rejects malformed top-level fixture with a path', () => {
    expect(() => loadFixtureJson({})).toThrow(/missing required field "fixture\.name"/);
    expect(() => loadFixtureJson({ name: 1 })).toThrow(/expected string at "fixture\.name"/);
    expect(() =>
      loadFixtureJson({ name: 'x', description: 'y', events: 'not-array', assertions: [] }),
    ).toThrow(/"fixture\.events" is not an array/);
  });

  it('rejects events with neither type nor envelope', () => {
    expect(() => normalizeEvent({ t: 0 }, 'fixture.events[0]')).toThrow(
      /neither "type" nor "envelope"/,
    );
  });

  it('rejects events with both type and envelope (ambiguous)', () => {
    expect(() =>
      normalizeEvent(
        {
          t: 0,
          type: 'message_delta',
          envelope: { jsonrpc: '2.0', method: 'message/delta', params: {} },
        },
        'fixture.events[0]',
      ),
    ).toThrow(/has BOTH "type" and "envelope"/);
  });

  it('drops known ignored UI Protocol notifications without error', () => {
    // progress/updated, task/updated, etc. are valid wire messages but
    // not modeled by this layer. A capture from PR I might include
    // them; the loader silently skips them so promotion is mechanical.
    const fixture = loadFixtureJson({
      name: 'with-ignored',
      description: 'capture contains a progress/updated that the reducer ignores',
      session_id: 's',
      events: [
        {
          t: 0,
          envelope: {
            jsonrpc: '2.0',
            method: 'user/sent',
            params: { session_id: 's', turn_id: 'A', cmid: 'A', text: 'q' },
          },
        },
        {
          t: 50,
          envelope: {
            jsonrpc: '2.0',
            method: 'progress/updated',
            params: { session_id: 's', progress: 0.42 },
          },
        },
        {
          t: 100,
          envelope: {
            jsonrpc: '2.0',
            method: 'turn/started',
            params: { session_id: 's', turn_id: 'A' },
          },
        },
      ],
      assertions: [],
    });
    // The progress/updated event was dropped — fixture has only 2 events.
    expect(fixture.events).toHaveLength(2);
    expect(fixture.events[0]!.type).toBe('user_sent');
    expect(fixture.events[1]!.type).toBe('turn_started');
  });
});
