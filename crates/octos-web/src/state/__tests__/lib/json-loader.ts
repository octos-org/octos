// JSON fixture loader + normalizer.
//
// PR I (capture-and-replay flag in `e2e/lib/live-browser-helpers.ts`)
// captures live SSE streams as raw UI Protocol notifications. Those
// notifications arrive as `{"jsonrpc": "2.0", "method": "...", "params": {...}}`
// envelopes, NOT as the flat `{type, ...}` form the in-process replay
// engine consumes. This loader bridges the two:
//
//   1. Parses a JSON fixture file (either flat-form or wrapped-form).
//   2. Validates required fields and types.
//   3. Normalizes wrapped JSON-RPC notifications into the flat form.
//   4. Returns a typed `SseFixture` ready to feed to `replayFixture`.
//
// Wrapped-form input shape (what PR I will write):
//
//   {
//     "name": "captured-2026-04-30-1234",
//     "description": "...",
//     "session_id": "live-session-abc",
//     "events": [
//       {
//         "t": 0,
//         "envelope": {
//           "jsonrpc": "2.0",
//           "method": "message/delta",
//           "params": {
//             "session_id": "...",
//             "turn_id": "...",
//             "text": "..."
//           }
//         }
//       },
//       ...
//     ],
//     "assertions": [...]
//   }
//
// Flat-form input is the same as the TS fixture files in `fixtures/`.
//
// On any validation failure we throw with a clear path-prefixed message
// so the operator promoting a captured fixture sees exactly what was
// malformed.

import type {
  Assertion,
  SseEvent,
  SseFixture,
} from './fixture-types.js';

// ── Wire method → flat-event type mapping ────────────────────────────────
//
// Mirrors the UI Protocol `method` strings in
// `crates/octos-core/src/ui_protocol.rs::ui_protocol_methods`.

const METHOD_TO_TYPE: Record<string, string> = {
  'turn/started': 'turn_started',
  'turn/completed': 'turn_completed',
  'turn/error': 'turn_error',
  'message/delta': 'message_delta',
  'tool/started': 'tool_started',
  'tool/progress': 'tool_progress',
  'tool/completed': 'tool_completed',
  // Custom (no UI Protocol equivalent yet — both PR H-internal):
  'user/sent': 'user_sent',
  'background/result': 'background_result',
  'recovery/turn': 'recovery_turn',
  'connection/drop': 'connection_drop',
  'connection/resume': 'connection_resume',
};

// UI Protocol notifications the reducer doesn't yet model but that are
// valid wire messages a captured stream may include. We accept them and
// drop them so a raw capture from PR I doesn't have to be hand-edited.
// Mirrors the constants in `crates/octos-core/src/ui_protocol.rs`
// (`ui_protocol_methods` module — `task/updated`, `task/output/delta`,
// `progress/updated`, `warning`, `approval/*`, `session/*`, etc.).
const IGNORED_METHODS: Set<string> = new Set([
  'session/open',
  'session/opened',
  'session/closed',
  'session/hydrate',
  'task/updated',
  'task/output/delta',
  'progress/updated',
  'warning',
  'protocol/replay_lossy',
  'approval/requested',
  'approval/decided',
  'approval/cancelled',
  'approval/auto_resolved',
  'approval/respond',
  'turn/interrupt',
  'capability/announce',
  'thread/graph/get',
  'turn/state/get',
  'message/persisted',
]);

/** Loose JSON value type — keep TS happy without importing a JSON schema lib. */
type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

function isObject(v: unknown): v is Record<string, JsonValue> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function require_(obj: Record<string, JsonValue>, key: string, path: string): JsonValue {
  if (!(key in obj)) {
    throw new Error(`fixture validation: missing required field "${path}.${key}"`);
  }
  return obj[key]!;
}

function requireString(v: JsonValue, path: string): string {
  if (typeof v !== 'string') {
    throw new Error(`fixture validation: expected string at "${path}", got ${typeof v}`);
  }
  return v;
}

function requireNumber(v: JsonValue, path: string): number {
  if (typeof v !== 'number' || !Number.isFinite(v)) {
    throw new Error(`fixture validation: expected finite number at "${path}", got ${JSON.stringify(v)}`);
  }
  return v;
}

/** Sentinel returned by `normalizeEvent` when the input was a known
 *  ignored UI Protocol notification. Callers filter these out. */
const IGNORE = Symbol('ignored-notification');

/** Take an event in either flat or wrapped form and return the flat form
 *  the replay engine consumes. Wrapped events go through method
 *  normalization. Returns `IGNORE` if the wrapped form is a known
 *  ignored notification (e.g. `progress/updated`). */
export function normalizeEvent(
  input: unknown,
  path: string,
): SseEvent | typeof IGNORE {
  if (!isObject(input)) {
    throw new Error(`fixture validation: event at "${path}" is not an object`);
  }
  const t = requireNumber(require_(input, 't', path), `${path}.t`);

  const hasType = 'type' in input;
  const hasEnvelope = 'envelope' in input;

  // Reject ambiguous events. A captured fixture might enrich an envelope
  // with extra metadata, but a top-level `type` next to `envelope` makes
  // the discriminant unclear and earlier silently leaked envelope fields
  // into the flat output. Fail-closed.
  if (hasType && hasEnvelope) {
    throw new Error(
      `fixture validation: event at "${path}" has BOTH "type" and "envelope" — ambiguous; choose one`,
    );
  }

  // Wrapped form first (preferred when the capture is raw UI Protocol).
  if (hasEnvelope) {
    const env = input.envelope;
    if (!isObject(env)) {
      throw new Error(`fixture validation: "${path}.envelope" is not an object`);
    }
    const method = requireString(
      require_(env, 'method', `${path}.envelope`),
      `${path}.envelope.method`,
    );
    if (IGNORED_METHODS.has(method)) {
      return IGNORE;
    }
    const flatType = METHOD_TO_TYPE[method];
    if (!flatType) {
      throw new Error(
        `fixture validation: unknown UI Protocol method "${method}" at "${path}.envelope.method" (known: ${Object.keys(METHOD_TO_TYPE).join(', ')}; ignored: ${[...IGNORED_METHODS].join(', ')})`,
      );
    }
    const params = isObject(env.params) ? env.params : {};
    return { ...params, t, type: flatType } as unknown as SseEvent;
  }

  // Flat form.
  if (hasType) {
    return { ...input, t } as unknown as SseEvent;
  }

  throw new Error(`fixture validation: event at "${path}" has neither "type" nor "envelope"`);
}

export const IGNORED_EVENT = IGNORE;

/** Load and validate a fixture from a JSON-decoded object. Caller is
 *  responsible for `JSON.parse` so this function works in environments
 *  without `fs` (e.g. browser-side fixture inspection). */
export function loadFixtureJson(raw: unknown): SseFixture {
  if (!isObject(raw)) {
    throw new Error('fixture validation: top-level value is not an object');
  }
  const name = requireString(require_(raw, 'name', 'fixture'), 'fixture.name');
  const description = requireString(
    require_(raw, 'description', 'fixture'),
    'fixture.description',
  );

  const eventsRaw = require_(raw, 'events', 'fixture');
  if (!Array.isArray(eventsRaw)) {
    throw new Error('fixture validation: "fixture.events" is not an array');
  }
  const events: SseEvent[] = [];
  for (let idx = 0; idx < eventsRaw.length; idx++) {
    const normalized = normalizeEvent(eventsRaw[idx], `fixture.events[${idx}]`);
    if (normalized === IGNORE) continue; // dropped: known no-op notification
    events.push(normalized);
  }

  const assertionsRaw = require_(raw, 'assertions', 'fixture');
  if (!Array.isArray(assertionsRaw)) {
    throw new Error('fixture validation: "fixture.assertions" is not an array');
  }
  const assertions: Assertion[] = assertionsRaw.map((a, idx) => {
    if (!isObject(a)) {
      throw new Error(`fixture validation: fixture.assertions[${idx}] is not an object`);
    }
    if (!('kind' in a) || typeof a.kind !== 'string') {
      throw new Error(`fixture validation: fixture.assertions[${idx}].kind missing or not a string`);
    }
    return a as unknown as Assertion;
  });

  const fixture: SseFixture = {
    name,
    description,
    events,
    assertions,
  };
  if ('session_id' in raw && typeof raw.session_id === 'string') {
    fixture.session_id = raw.session_id;
  }
  if ('issues' in raw && Array.isArray(raw.issues)) {
    fixture.issues = raw.issues.filter(
      (n): n is number => typeof n === 'number',
    );
  }
  return fixture;
}
