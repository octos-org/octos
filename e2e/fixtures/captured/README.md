# Captured live-soak fixtures

This directory holds JSON fixtures that the live e2e harness auto-saves
when running with `OCTOS_CAPTURE_FIXTURE=1` or when a soak test fails.
Each fixture is a snapshot of the SSE event stream + final DOM state from
ONE live spec run, suitable for promoting to the Layer 1 SPA reducer
test corpus.

The capture-and-replay flag is **PR I** in the chat-lifecycle hardening
plan. It feeds the regression corpus that **PR H** consumes (Layer 1
SPA reducer fixtures under
`crates/octos-web/src/state/__tests__/fixtures/`). Together with PR H
it forms the 3-tier promotion workflow:

```
   capture (here)  ─►  triage (you)  ─►  promote  ─►  regression-locked
   live soak run        edit JSON        copy to        Vitest in CI
                        + assertions     Layer 1
```

## How fixtures are auto-captured

Two trigger paths:

1. **Operator-driven**: set `OCTOS_CAPTURE_FIXTURE=1` (or `=true`/`=yes`)
   when invoking Playwright. The harness writes a fixture for every spec
   that calls `attachCapture(page, testInfo)`, regardless of pass/fail.
2. **On failure**: even without the env flag, if a spec fails AND the
   spec called `attachCapture()`, the harness writes the fixture. To
   suppress capture entirely (e.g. constrained CI shards) set
   `OCTOS_CAPTURE_DISABLE=1`.

Both paths write to `e2e/fixtures/captured/<sanitized-test-title>-<iso-timestamp>.json`.
The file is also added as a Playwright attachment to the failing test
report, so it shows up in `playwright-report/`.

### Wiring a spec for capture

```ts
import { attachCapture } from '../lib/capture-replay';
import { sendAndWait, login } from './live-browser-helpers';

test('reproduces overflow-stress thread binding', async ({ page }, testInfo) => {
  const capture = await attachCapture(page, testInfo, {
    description: 'overflow stress, 5 rapid prompts',
  });
  await login(page);
  try {
    await sendAndWait(page, 'first prompt', { capture });
    await sendAndWait(page, 'second prompt', { capture });
    // ... assertions ...
  } catch (err) {
    capture.recordFailure(err);
    throw err;
  } finally {
    await capture.finalize();
  }
});
```

`attachCapture` is a no-op on `OCTOS_CAPTURE_DISABLE=1`; otherwise it
installs a fetch + EventSource tee via `addInitScript` BEFORE
`page.goto()`. Calling `attachCapture` after navigation will not capture
the streams that were opened pre-attach.

### Failure-driven capture timing

Playwright sets `testInfo.status='failed'` only AFTER the test body's
`finally` blocks have run. To handle that race the helper:

1. **Snapshots** the in-page buffer + DOM as soon as `finalize()` is
   called (typically from a `finally` block). The snapshot is held in
   memory.
2. **Defers** the disk write if the capture env flag is off and the
   status is not yet failed.
3. On `page.close` (which fires AFTER status propagation) re-checks
   `testInfo.status` and writes the cached snapshot if the test
   ultimately failed.

Net effect: callers can use the simple `try { ... } finally {
capture.finalize(); }` pattern and still get failure-driven capture
without ever calling `recordFailure(err)` themselves. Calling
`recordFailure(err)` is still useful when you want the assertion
message embedded in the fixture's `assertionFailure` field.

## Fixture format

The on-disk shape is a SUPERSET of PR H's `SseFixture`
(`crates/octos-web/src/state/__tests__/lib/fixture-types.ts`):

```jsonc
{
  "name": "<sanitized-title>",
  "description": "<human description>",
  "captured_at": "2026-04-30T19:12:03.456Z",
  "spec": "live-overflow-stress.spec.ts",
  "base_url": "https://dspfac.octos.ominix.io",
  "session_id": "01HX...",            // best-effort from localStorage
  "events": [                          // PR H-compatible normalized
    { "t": 12, "type": "user_sent", "text": "..." },
    { "t": 415, "type": "message_delta", "text": "Hello" },
    { "t": 1622, "type": "turn_completed" }
  ],
  "raw_events": [                      // lossless wire frames
    {
      "t": 415,
      "url": "/api/chat",
      "source": "fetch-stream",
      "payload": { "type": "token", "text": "Hello" }
    }
  ],
  "finalDom": {
    "user_bubbles": ["..."],
    "assistant_bubbles": ["..."],
    "html_excerpt": "<div ...>"        // first 4KB of message-bubble region
  },
  "assertionFailure": {                // present iff the spec failed
    "message": "expected 1 bubble, got 2",
    "stack": "..."
  }
}
```

### Why two event arrays?

* `events[]` is best-effort normalized to PR H's wire shape — `token`
  ↦ `message_delta`, `done` ↦ `turn_completed`, etc. This is what the
  Layer 1 reducer consumes after promotion.
* `raw_events[]` preserves the exact wire frame, URL, and source channel
  (`fetch-stream` | `eventsource` | `dom-marker`). If the normalization
  miss something — or if the wire format changes — the promoter can
  still rebuild the canonical fixture from raw frames.

The capture today does NOT mint synthetic `turn_id`/`cmid` for events
the server didn't already carry them on. The promoter fills those gaps
during triage (see below). Once the live server emits canonical
typed envelopes (post-PR G + PR J), captures can flow straight into PR
H without manual editing.

## Promoting to Layer 1

```bash
./e2e/scripts/promote-captured-fixture.sh \
    e2e/fixtures/captured/live-overflow-stress-2026-04-30_19-12-03-456.json \
    overflow-stress-thread-binding
# -> crates/octos-web/src/state/__tests__/fixtures/captured/overflow-stress-thread-binding.fixture.json
```

The script:

* Validates that the source is a JSON file with a non-empty `events[]`
  or `raw_events[]`.
* Disallows path-injection in the target name.
* **Refuses to overwrite existing promoted fixtures by default.** Manual
  triage edits (assertions, turn_id, cmid) are valuable — silent
  overwrites destroy them. Pass `--force` to overwrite, or `--dry-run`
  to preview.

### Triage checklist (manual edits after promotion)

Before committing a promoted fixture, edit it to:

1. **Fill in `turn_id` / `cmid` / `session_id`** on events that lack
   them. Use the `raw_events` order + the `finalDom.user_bubbles`
   order to correlate. Most of the time you can mint deterministic
   stable values (e.g. `turn-1`, `turn-2`) — the reducer treats them
   as opaque strings.
2. **Add an `assertions` array** describing the bug class this fixture
   catches. See `fixture-types.ts::Assertion` for the discriminated
   union (`thread_equals` / `no_misroute` / `thread_order` /
   `no_orphans` / `thread_has_attachment`).
3. **Trim `raw_events`** if the file is bloated by keepalives or
   noise. Keep the wire frames that actually matter for the
   regression.
4. **Drop `finalDom.html_excerpt`** if it contains anything that
   shouldn't land in source control (cookies, tokens). The default
   excerpt is the message-bubble region only, but double-check.
5. **Write a one-line `description`** that names the bug class the
   fixture pins down — that string surfaces in CI failure messages.

A typical promoted fixture is **5–30 KB**. Captures larger than 100KB
usually mean the spec ran for many minutes; consider trimming.

## Capture overhead

* **Page-side**: one extra `TransformStream` tee per streaming
  response, plus an array push per SSE frame. The tee is gated by
  `Content-Type` (`text/event-stream`, `application/x-ndjson`, or
  unset/`text/plain` for chunked streams) — JSON polling responses on
  the same URL prefix short-circuit before any decoding work happens.
* **Default URL filter**: `/api/chat` only. Override
  `streamingPaths` if a custom streaming endpoint should be
  intercepted.
* **Buffer cap**: 10 000 frames per capture (override via
  `OCTOS_CAPTURE_MAX_FRAMES`). Beyond that, frames are dropped and a
  single `capture_truncated` marker is appended.
* **Wall-clock impact on soak runs**: under 2% in practice. Captures
  only flush on test end; in-flight tests pay only the tee.
* **Disk**: bounded by the 4KB DOM excerpt + ~200 bytes per event.
  Three-minute soak runs typically produce 8-30KB JSON files (the
  PR I demo run produced 8.9KB / 9 events).

If a long-running spec produces oversize captures, set
`OCTOS_CAPTURE_DISABLE=1` for that shard or trim `raw_events` at
promotion time.

### Replace vs. delta semantics

The static SPA emits two append-shaped frames AND one full-replace
frame:

* `{type: "token"}` / `{type: "delta"}` — append; mapped to
  `message_delta` in the normalized `events[]`.
* `{type: "replace"}` — full content overwrite; mapped to
  `message_replace` (NOT `message_delta`). PR H's reducer must grow a
  dedicated handler before fixtures with `message_replace` can replay
  cleanly. The promoter should reject (or rewrite) such fixtures
  until that lands. The `raw_events[]` array always preserves the
  original `replace` frame so the promoter can decide.

## Limitations

* `turn_id` synthesis is not done at capture time. Today's live server
  emits legacy `{type: "token"}` frames with no turn id. The promoter
  has to mint stable ids during triage. PR G + PR J will close this
  gap by emitting canonical typed envelopes; once that lands, captures
  can promote without edits.
* WebSocket flows (M9 protocol) are NOT captured by this helper. They
  should be captured via the dedicated `m9-ws-client.ts` recording
  path — that's a separate workflow scoped to the M9 protocol tests.
* The capture is page-scoped. If a spec opens multiple browser pages
  (e.g. multi-tab), each page needs its own `attachCapture` call.
