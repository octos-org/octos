# Layer 1: SPA reducer fixture tests

This directory is the **Layer 1 testing pyramid** for octos-web's SPA
reducer. It feeds canned UI Protocol v1 SSE event fixtures into the
reducer and asserts thread-graph correctness — in **milliseconds, with no
LLM, no fleet, no Playwright**.

## Why this layer exists

The thread-binding bug class

```
#649 → #664 → #673 → #680 → #738 → #740
```

kept recurring because the only test layers that exercised it ran live:

- **Layer 3**: Playwright soak (`live-thread-interleave.spec.ts`,
  `live-overflow-thread-binding.spec.ts`). Six minutes per scenario, runs
  on the fleet, fragile to LLM-provider quirks.
- **Layer 2**: protocol e2e (`m9-protocol-*.spec.ts`). Faster but still
  spins a real CLI, real WS server, real session.

Neither catches a misroute deterministically before commit. Each
regression in the chain shipped, was caught by soak hours later, then
required a corrective tactical fix that re-introduced a new variant of
the same bug. **Layer 1 closes that loop.**

## What Layer 1 does

A fixture is a JSON-serializable description of a UI Protocol v1 event
stream + the reducer state we expect afterwards. The engine replays the
events into the reducer and asserts.

```
fixture (events + assertions)
       │
       ▼
replay-fixture.ts ─────► Reducer ─────► ReducerState
       │
       ▼
   assertions
       │
       ▼
   pass / fail (with structured diff)
```

The 7 seed fixtures in `fixtures/` are derived from the actual bug
chain — see each file's header comment for the originating issue and the
DOM-misroute pattern they reproduce.

## File layout

```
__tests__/
├── README.md                            ← you are here
├── fixtures.test.ts                     ← Vitest entrypoint; wires every
│                                          fixture against both reducers
├── lib/
│   ├── fixture-types.ts                 ← SseEvent, Assertion, SseFixture
│   ├── replay-fixture.ts                ← replay engine + assertion runner
│   ├── buggy-reducer.ts                 ← placeholder for current SPA
│   │                                      behavior — sticky-map fallback
│   └── fixed-reducer.ts                 ← reference implementation
│                                          PR J must match
└── fixtures/
    ├── rapid-fire-five-fast.fixture.ts          ← #649/#740 base case
    ├── slow-then-fast-interleave.fixture.ts     ← #649 (live-thread-interleave)
    ├── spawn-only-ack-then-result.fixture.ts    ← #738
    ├── m89-recovery-turn.fixture.ts             ← #738 sibling + turn_error
    ├── reload-replay.fixture.ts                 ← reconnect/cursor replay
    ├── tool-retry-collapse.fixture.ts           ← #680 tool-retry collapse
    └── multi-attachment-dedup.fixture.ts        ← multi-file deep_search
```

## How to run

From `crates/octos-web/`:

```bash
npm install
npm test          # one-shot run (CI mode)
npm run test:watch
```

Target: **< 1s total** for all fixtures × both reducers. The replay
engine is fully synchronous; no setTimeout, no real WebSockets, no
filesystem.

## The contract: what each test asserts

Every fixture in `SUITE` (see `fixtures.test.ts`) is checked against
TWO reducers:

### Suite 1: "fails against the current (buggy) reducer"

Proves the fixture has teeth — the bug class it documents is real and
detectable. **This suite passes today** (i.e. each fixture correctly
fails the buggy reducer).

If a fixture were to PASS against the buggy reducer here, that's an
alarm: either the fixture is too weak (false positive) or the bug class
has moved somewhere we don't yet model.

### Suite 2: "passes against the fixed (reference) reducer"

Proves the contract is achievable. The `fixedReducer` is the executable
spec PR J must match. **This suite passes today** (the contract is
expressible).

When PR J ships, the production reducer takes over from `fixed-reducer.ts`
as the suite-2 reducer. **`buggy-reducer.ts` is kept forever** as a
deliberately-broken sentinel: every fixture must continue to detect it
(suite 1 is the negative-coverage gate). If a future "innocent" fixture
change accidentally passes against the buggy sentinel, suite 1 catches
the weakening before it ships.

## Why fixtures INITIALLY appear to "fail" if you only look at Suite 1

This is intentional design — and easy to misread. The fixtures were
designed to FAIL against the current production reducer (the
buggy-reducer placeholder). That failure is the SIGNAL: "yes, this
fixture catches the bug."

We capture that failure as a positive assertion in Suite 1
(`expect(result.pass).toBe(false)`), so the Vitest run shows GREEN —
**because the fixtures correctly detect the bug**.

The real contract is Suite 2: when PR J's typed-`turn_id` reducer
replaces the placeholder, Suite 2 still passes (because the contract is
the same). And on that day Suite 1 is deleted.

## Adding a new fixture

1. Create `fixtures/<my-scenario>.fixture.ts`. Author the events so they
   reproduce the bug pattern (use issue references in the header
   comment); declare assertions for the desired final state.
2. Add it to `SUITE` in `fixtures.test.ts`.
3. Run `npm test`. The new fixture should:
   - **fail** against `buggyReducer` (Suite 1 expects this — a passing
     test means the fixture has teeth)
   - **pass** against `fixedReducer` (Suite 2 expects this — the
     contract must be achievable)
4. If Suite 2 fails: the fixture's assertions are unachievable —
   adjust them.
5. If Suite 1 fails (i.e. the bug fixture passes against buggy): the
   buggy reducer doesn't model the leak that fixture targets. Either
   broaden the placeholder OR confirm the bug has migrated.

## Capture-and-replay (PR I)

PR I adds a `--record-fixture` flag to `e2e/lib/live-browser-helpers.ts`
that writes captured SSE streams + DOM snapshots as JSON. When a soak
run fails, the capture is auto-promoted to
`fixtures/captured/<spec>-<timestamp>.json`. The fixture format here
deliberately mirrors that JSON format, so the import is mechanical.

## CI

`.github/workflows/web-reducer-fixtures.yml` runs on every PR touching
`crates/octos-web/` or `crates/octos-core/src/ui_protocol.rs`. Total
runtime <1s. No flake budget — these are deterministic.

## Cross-references

- Architecture rationale: `/tmp/octos-architecture-FINAL.md` sections A,
  C, and the loophole-audit table.
- PoC: `/tmp/fixture-poc/reducer-test.mjs` (50-line proof, 2ms runtime,
  reproduces wave-4 mini3 misroute pattern).
- UI Protocol v1 spec: `api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md`.
- The bug chain: issues #649, #664, #673, #680, #738, #740, #742.
