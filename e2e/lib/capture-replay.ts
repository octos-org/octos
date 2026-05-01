// Capture-and-replay infrastructure for live e2e soak runs (PR I).
//
// Goal: when a live spec fails (or any time `OCTOS_CAPTURE_FIXTURE=1` is set)
// auto-save the SSE event stream + final DOM state + assertion failure to
// `e2e/fixtures/captured/<test-name>-<timestamp>.json`. Captured fixtures
// can then be promoted to Layer 1 (`crates/octos-web/src/state/__tests__/
// fixtures/captured/`) via `scripts/promote-captured-fixture.sh` to lock in
// the regression.
//
// The captured JSON is a SUPERSET of the PR H `SseFixture` format:
//   {
//     name: string,
//     description: string,
//     captured_at: ISO timestamp,
//     spec: spec file basename,
//     base_url: string,
//     session_id?: string,
//     events: SseEvent[],            // best-effort normalized to PR H shape
//     raw_events: RawSseEvent[],     // exact wire frames (lossless)
//     finalDom: { user_bubbles, assistant_bubbles, html_excerpt },
//     assertionFailure?: { message, stack? },
//   }
//
// The two reasons we write BOTH `events` (normalized) and `raw_events`
// (lossless wire frames):
//
//   1. Layer 1 (PR H) consumes `events` after a quick promotion-time edit
//      that fixes turn_id / cmid bindings (today the live server emits
//      `{type: "token"}` whereas PR H's reducer expects `message_delta`).
//      The `events` array gives the promoter a head start.
//
//   2. `raw_events` is the source of truth. If the normalization in
//      `normalizeFrame()` misses something, the promoter can still
//      reconstruct the canonical fixture from raw_events.
//
// Implementation strategy:
//
//  * Page-side `addInitScript` monkey-patches `fetch` to tee streaming
//    `/api/chat` responses into `window.__octosCaptureBuffer`. The SPA sees
//    an unchanged response; we get a side-channel copy. This works for
//    chunked-encoding SSE (which is what `static/app.js` uses) AND for
//    classic `text/event-stream` responses if the page ever switches.
//
//  * `EventSource` is also wrapped (LogPanel etc. uses native EventSource).
//    Best-effort; main coverage is `/api/chat`.
//
//  * Capture overhead: one extra TransformStream + one in-page array push
//    per chunk. Negligible vs. the network and rendering work the page is
//    already doing. Soak runs should see <2% wall-clock overhead.
//
// IMPORTANT: this module touches NO production code. Everything happens
// inside the Playwright page context via `addInitScript`. If the harness
// is removed, the production SPA behaves identically.

import * as fs from 'fs';
import * as path from 'path';
import type { Page, TestInfo } from '@playwright/test';

// ── Captured fixture shape ───────────────────────────────────────────────

/** Raw SSE frame as it crossed the wire — lossless. */
export interface RawSseEvent {
  /** Wall-clock ms since capture started. */
  t: number;
  /** Origin URL (the streaming endpoint, e.g. `/api/chat`). */
  url: string;
  /** Source channel: `fetch-stream` | `eventsource` | `dom-marker`. */
  source: 'fetch-stream' | 'eventsource' | 'dom-marker';
  /** Parsed JSON payload from `data:` line, OR `{__raw: "..."}` if unparseable. */
  payload: unknown;
}

/** PR H-compatible normalized event. The shape mirrors `SseEvent` from
 *  `crates/octos-web/src/state/__tests__/lib/fixture-types.ts`. We can't
 *  always faithfully populate every field at capture time (e.g. `turn_id`
 *  may be missing from older `{type: "token"}` frames); the promoter
 *  fills in gaps. Type is `unknown` rather than the strict PR H union
 *  because raw captures may carry frames PR H doesn't model yet. */
export type NormalizedSseEvent = {
  t: number;
  type: string;
  [k: string]: unknown;
};

/** Final DOM state at capture finalization. */
export interface CapturedDom {
  user_bubbles: string[];
  assistant_bubbles: string[];
  /** First N chars of the message-bubble container HTML for triage. */
  html_excerpt: string;
}

export interface CapturedFixture {
  name: string;
  description: string;
  captured_at: string;
  spec: string;
  base_url: string;
  session_id?: string;
  events: NormalizedSseEvent[];
  raw_events: RawSseEvent[];
  finalDom: CapturedDom;
  assertionFailure?: { message: string; stack?: string };
}

// ── Public API ───────────────────────────────────────────────────────────

export interface CaptureOptions {
  /** Force-capture even when env flag not set. Default false. */
  force?: boolean;
  /** Override output dir. Default `e2e/fixtures/captured/`. */
  outputDir?: string;
  /** Human description; defaults to test title. */
  description?: string;
  /** Streaming endpoints to capture. Match by URL substring. Default
   *  matches `/api/chat`, `/api/sessions/`, `/api/tasks/`. */
  streamingPaths?: string[];
}

export interface CaptureHandle {
  /** Returns true iff capture is enabled for this run (env or `force`). */
  enabled: boolean;
  /** Record a synthetic marker so the promoter can correlate user input
   *  with the resulting SSE frames. */
  recordUserSent(text: string): Promise<void>;
  /** Record an assertion failure that should be embedded in the fixture. */
  recordFailure(err: unknown): void;
  /** Drain the in-page buffer + DOM and write the JSON file. Idempotent.
   *  Returns the path written, or null if capture is disabled / nothing to
   *  flush / writing was skipped because the test passed and capture is
   *  on-failure-only. */
  finalize(opts?: { reason?: string }): Promise<string | null>;
}

/** Returns true iff the env opts in to capture. */
export function captureEnabled(force = false): boolean {
  if (force) return true;
  const v = process.env.OCTOS_CAPTURE_FIXTURE;
  return v === '1' || v === 'true' || v === 'yes';
}

/**
 * Attach a capture session to a Playwright page. Must be called BEFORE
 * `page.goto()` because it relies on `addInitScript` to monkey-patch
 * `fetch` and `EventSource` before any SPA code runs.
 *
 * Behaviour:
 *
 *  - If capture is disabled (`OCTOS_CAPTURE_FIXTURE` unset and `force` not
 *    passed) AND the test does not fail, the returned handle is a no-op
 *    skeleton: it records nothing, finalize returns null. This keeps the
 *    overhead of the helper trivial when capture isn't wanted.
 *
 *  - If capture is enabled, the init script is injected and ALL streaming
 *    responses to matching URLs are teed into `window.__octosCaptureBuffer`.
 *
 *  - On `finalize()` we drain the in-page buffer, snapshot the DOM, and
 *    write `<outputDir>/<safe-test-name>-<iso-timestamp>.json`.
 *
 *  - If `testInfo.status === 'failed'` at finalize time, we ALWAYS write
 *    even when the env flag is off — that's the "auto-save on soak failure"
 *    contract. To enable that path the helper itself runs in capture mode
 *    (so the in-page buffer exists); we just check the failure status at
 *    flush time. To suppress capture entirely (e.g. headless smoke runs
 *    that explicitly don't want disk writes) set `OCTOS_CAPTURE_DISABLE=1`.
 */
export async function attachCapture(
  page: Page,
  testInfo: TestInfo,
  opts: CaptureOptions = {},
): Promise<CaptureHandle> {
  const disabled = process.env.OCTOS_CAPTURE_DISABLE === '1';
  if (disabled) {
    return noopHandle();
  }

  // We always install the init script when capture isn't explicitly
  // disabled — that way we can still flush a fixture when the test fails,
  // even if the operator forgot `OCTOS_CAPTURE_FIXTURE=1`. The page-side
  // overhead of an idle TransformStream tee is negligible because we
  // ALSO gate by `Content-Type: text/event-stream` (or chunked transfer)
  // before starting to decode — see the matchUrl + content-type filter
  // inside the init script.
  //
  // Default narrowed to `/api/chat` only (per codex review): the broader
  // `/api/sessions/` and `/api/tasks/` paths include polling endpoints
  // that return JSON, not SSE, and teeing those needlessly burns CPU.
  // Override `streamingPaths` if you have a custom streaming endpoint.
  const streamingPaths = opts.streamingPaths ?? ['/api/chat'];

  // Hard cap on the page-side buffer to bound disk usage from a runaway
  // soak run. Configurable via env. Beyond this we drop frames silently
  // (with a single warning marker) so that one bad soak run doesn't
  // produce a 500MB JSON file.
  const maxFrames = Number(process.env.OCTOS_CAPTURE_MAX_FRAMES || 10_000);

  await page.addInitScript(
    ({ paths, maxFrames }) => {
      // Browser context — `window` and `Response`/`fetch` are global.
      type WireFrame = {
        t: number;
        url: string;
        source: string;
        payload: unknown;
      };
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const w = window as any;
      if (w.__octosCaptureBuffer) return; // idempotent
      const startedAt = Date.now();
      const buffer: WireFrame[] = [];
      w.__octosCaptureBuffer = buffer;
      w.__octosCaptureStartedAt = startedAt;

      const matchUrl = (u: string): boolean => {
        try {
          const lower = u.toLowerCase();
          return paths.some((p: string) => lower.includes(p.toLowerCase()));
        } catch {
          return false;
        }
      };

      let droppedOnce = false;
      const pushFrame = (frame: WireFrame) => {
        if (buffer.length >= maxFrames) {
          if (!droppedOnce) {
            droppedOnce = true;
            buffer.push({
              t: Date.now() - startedAt,
              url: 'capture-internal',
              source: 'dom-marker',
              payload: {
                type: 'capture_truncated',
                reason: 'max_frames_reached',
                limit: maxFrames,
              },
            });
          }
          return;
        }
        buffer.push(frame);
      };

      const parseAndPush = (
        urlStr: string,
        source: WireFrame['source'],
        chunkText: string,
        carry: { buf: string },
      ) => {
        carry.buf += chunkText;
        const lines = carry.buf.split('\n');
        carry.buf = lines.pop() || '';
        for (const raw of lines) {
          const line = raw.replace(/\r$/, '');
          if (!line.startsWith('data:')) continue;
          const json = line.slice(5).trim();
          if (!json) continue;
          let payload: unknown;
          try {
            payload = JSON.parse(json);
          } catch {
            payload = { __raw: json };
          }
          pushFrame({
            t: Date.now() - startedAt,
            url: urlStr,
            source,
            payload,
          });
        }
      };

      // Patch fetch to tee streaming /api/chat (and similar) responses.
      const origFetch = w.fetch.bind(w);
      w.fetch = async function patchedFetch(
        input: RequestInfo | URL,
        init?: RequestInit,
      ): Promise<Response> {
        const urlStr =
          typeof input === 'string'
            ? input
            : input instanceof URL
              ? input.toString()
              : (input as Request).url;
        const resp: Response = await origFetch(input, init);
        if (!matchUrl(urlStr) || !resp.body) return resp;

        // Only tee true streams. Static JSON responses on the same path
        // (e.g. polling endpoints) carry `application/json` and get short
        // circuited here so we don't burn CPU decoding chunks of fully
        // buffered JSON. SSE responses set `text/event-stream`; chunked
        // streams without a content-type are also accepted (they're how
        // the static SPA's `/api/chat` returns its frames).
        const ct = (resp.headers.get('content-type') || '').toLowerCase();
        const isStream =
          ct.includes('text/event-stream') ||
          ct.includes('application/x-ndjson') ||
          ct === '' ||
          ct.startsWith('text/plain'); // some daemons stream plain
        if (!isStream) return resp;

        // Tee the body. One branch goes back to the SPA; the other runs
        // through a parser that pushes to `buffer`. We construct a
        // synthetic Response so the SPA continues to work normally.
        const [a, b] = resp.body.tee();
        const carry = { buf: '' };
        const decoder = new TextDecoder();
        (async () => {
          const reader = b.getReader();
          for (;;) {
            try {
              const { value, done } = await reader.read();
              if (done) {
                if (carry.buf) parseAndPush(urlStr, 'fetch-stream', '\n', carry);
                break;
              }
              parseAndPush(
                urlStr,
                'fetch-stream',
                decoder.decode(value, { stream: true }),
                carry,
              );
            } catch {
              break;
            }
          }
        })();

        const cloned = new Response(a, {
          status: resp.status,
          statusText: resp.statusText,
          headers: resp.headers,
        });
        return cloned;
      };

      // Patch EventSource (LogPanel etc.). Best-effort.
      const OrigES = w.EventSource;
      if (typeof OrigES === 'function') {
        const Patched = function (this: unknown, url: string, cfg?: unknown) {
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          const es = new (OrigES as any)(url, cfg);
          if (matchUrl(url)) {
            es.addEventListener('message', (ev: MessageEvent) => {
              let payload: unknown = ev.data;
              if (typeof ev.data === 'string') {
                try {
                  payload = JSON.parse(ev.data);
                } catch {
                  payload = { __raw: ev.data };
                }
              }
              pushFrame({
                t: Date.now() - startedAt,
                url,
                source: 'eventsource',
                payload,
              });
            });
          }
          return es;
        } as unknown as typeof EventSource;
        Patched.prototype = OrigES.prototype;
        // Inherit static constants without tripping the readonly types of
        // the EventSource interface — addInitScript runs in the browser
        // context where this assignment is legal even though the Node
        // typings forbid it. Use index access via `as any` to dodge.
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const PA = Patched as any;
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const OA = OrigES as any;
        PA.CONNECTING = OA.CONNECTING;
        PA.OPEN = OA.OPEN;
        PA.CLOSED = OA.CLOSED;
        w.EventSource = Patched;
      }

      // Marker hook — the harness can call this from page.evaluate to
      // splice synthetic events (e.g. `user_sent`) into the timeline.
      w.__octosCaptureMark = (payload: unknown) => {
        pushFrame({
          t: Date.now() - startedAt,
          url: 'dom-marker',
          source: 'dom-marker',
          payload,
        });
      };
    },
    { paths: streamingPaths, maxFrames },
  );

  let assertionFailure: { message: string; stack?: string } | undefined;
  let finalized = false;
  let wroteToDisk = false;
  // Snapshot taken at finalize() invocation. Held so that if the
  // operator's `finally` block finalizes BEFORE Playwright marks
  // `testInfo.status='failed'`, the page-close listener can still
  // re-evaluate the failure status and write the file using this
  // already-drained snapshot. Without this, the test body's own
  // try/finally would race against Playwright's own status propagation
  // and we'd emit a "passing" verdict on tests that actually fail.
  let snapshot:
    | { events: RawSseEvent[]; dom: CapturedDom; sessionId?: string }
    | null = null;

  const handle: CaptureHandle = {
    enabled: true,

    async recordUserSent(text: string) {
      try {
        await page.evaluate(
          (t) => {
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            const fn = (window as any).__octosCaptureMark;
            if (typeof fn === 'function') {
              fn({ type: 'user_sent', text: t });
            }
          },
          text,
        );
      } catch {
        // Page may have closed; ignore.
      }
    },

    recordFailure(err: unknown) {
      if (err instanceof Error) {
        assertionFailure = { message: err.message, stack: err.stack };
      } else {
        assertionFailure = { message: String(err) };
      }
    },

    async finalize(finalizeOpts: { reason?: string } = {}): Promise<string | null> {
      if (finalized) return null;
      finalized = true;

      const force = !!opts.force;
      const onEnv = captureEnabled(force);
      // Drain UNCONDITIONALLY. The decision to keep-or-drop happens
      // after — but we must do the page.evaluate while the page is
      // still alive. Otherwise the page-close fallback can't recover
      // anything (the page is gone by the time it fires).
      let drained: { events: RawSseEvent[]; dom: CapturedDom; sessionId?: string } = {
        events: [],
        dom: { user_bubbles: [], assistant_bubbles: [], html_excerpt: '' },
      };
      try {
        drained = await page.evaluate(() => {
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          const w = window as any;
          const buffer = (w.__octosCaptureBuffer || []) as Array<{
            t: number;
            url: string;
            source: 'fetch-stream' | 'eventsource' | 'dom-marker';
            payload: unknown;
          }>;
          const userBubbles = Array.from(
            document.querySelectorAll("[data-testid='user-message']"),
          ).map((n) => (n.textContent || '').trim());
          const asstBubbles = Array.from(
            document.querySelectorAll("[data-testid='assistant-message']"),
          ).map((n) => (n.textContent || '').trim());
          // Triage excerpt — first 4KB of the messages-region HTML.
          const region =
            document.querySelector("[data-testid='messages-region']") ||
            document.querySelector('main') ||
            document.body;
          const html = (region?.innerHTML || '').slice(0, 4096);
          // The static SPA stores the active session id under
          // `octos_current_session` (see `crates/octos-cli/static/app.js`).
          // The richer dashboard SPA may use other keys; we probe both
          // shapes so the helper works against whichever bundle the
          // target host is serving today.
          const sessionId =
            localStorage.getItem('octos_current_session') ||
            localStorage.getItem('selected_session') ||
            localStorage.getItem('current_session') ||
            undefined;
          return {
            events: buffer,
            dom: {
              user_bubbles: userBubbles,
              assistant_bubbles: asstBubbles,
              html_excerpt: html,
            },
            sessionId: sessionId || undefined,
          };
        });
      } catch (err) {
        // Page closed before we could drain. Fall back to whatever
        // earlier snapshot we have (may be empty).
        // eslint-disable-next-line no-console
        console.warn(
          `[capture-replay] could not drain page buffer: ${(err as Error).message}`,
        );
        if (snapshot) drained = snapshot;
      }
      // Cache for any later page-close hook.
      snapshot = drained;

      // Decide whether to write. We deliberately re-read `testInfo.status`
      // here AFTER the drain — because Playwright sets status to 'failed'
      // only as the test body returns, finalize() invoked from a user
      // `finally` runs before that point. In that case we DEFER the
      // write to the page-close hook (which fires AFTER status
      // propagation) using the snapshot we just captured.
      const onFailure = testInfo.status === 'failed' || !!assertionFailure;
      if (!onEnv && !onFailure) {
        // Mark the snapshot as a deferred-write candidate. The
        // page-close hook below will re-check status and flush if the
        // test ended up failing. Stays no-op if the test passes.
        return null;
      }

      const fixture: CapturedFixture = {
        name: sanitizeName(testInfo.title),
        description:
          opts.description ||
          `${onFailure ? 'failed' : 'captured'} live run: ${testInfo.title}` +
            (finalizeOpts.reason ? ` (${finalizeOpts.reason})` : ''),
        captured_at: new Date().toISOString(),
        spec: path.basename(testInfo.file || 'unknown.spec.ts'),
        base_url: process.env.OCTOS_TEST_URL || 'http://localhost:3000',
        session_id: drained.sessionId,
        events: drained.events.map(normalizeFrame),
        raw_events: drained.events,
        finalDom: drained.dom,
        assertionFailure,
      };

      const outDir =
        opts.outputDir || path.resolve(__dirname, '..', 'fixtures', 'captured');
      try {
        fs.mkdirSync(outDir, { recursive: true });
      } catch {
        // best effort
      }

      const stamp = new Date()
        .toISOString()
        .replace(/[:.]/g, '-')
        .replace(/T/, '_')
        .replace(/Z$/, '');
      const filename = `${sanitizeName(testInfo.title)}-${stamp}.json`;
      const fullPath = path.join(outDir, filename);
      fs.writeFileSync(fullPath, JSON.stringify(fixture, null, 2), 'utf-8');
      wroteToDisk = true;

      // eslint-disable-next-line no-console
      console.log(
        `[capture-replay] wrote ${fixture.events.length} events + ${fixture.raw_events.length} raw frames -> ${fullPath}`,
      );
      try {
        testInfo.attachments.push({
          name: 'captured-fixture.json',
          path: fullPath,
          contentType: 'application/json',
        });
      } catch {
        // Older Playwright versions may not allow direct push; ignore.
      }
      return fullPath;
    },
  };

  // Auto-finalize on page close. By the time Playwright tears the page
  // down it has already updated `testInfo.status`, so a deferred snapshot
  // (cached above by the user-driven finalize) can now be written if the
  // test failed. If finalize() was never called by the user, this is
  // also where the very first drain happens — but the page is already
  // gone, so the drain is best-effort.
  page.once('close', () => {
    (async () => {
      if (wroteToDisk) return; // user-driven finalize already wrote
      const onEnv = captureEnabled(!!opts.force);
      const onFailure =
        testInfo.status === 'failed' ||
        testInfo.status === 'timedOut' ||
        !!assertionFailure;
      if (!onEnv && !onFailure) return;
      if (snapshot) {
        // Deferred-write path — snapshot was drained earlier.
        try {
          writeSnapshotToDisk(testInfo, opts, snapshot, assertionFailure, 'page-closed');
        } catch (err) {
          // eslint-disable-next-line no-console
          console.warn(
            `[capture-replay] deferred write failed: ${(err as Error).message}`,
          );
        }
        return;
      }
      // No snapshot taken — try one last drain; will fail if the page
      // is fully gone, but cheap to attempt.
      try {
        finalized = false;
        await handle.finalize({ reason: 'page-closed' });
      } catch {
        /* page is gone */
      }
    })().catch(() => {});
  });

  return handle;
}

function writeSnapshotToDisk(
  testInfo: TestInfo,
  opts: CaptureOptions,
  drained: { events: RawSseEvent[]; dom: CapturedDom; sessionId?: string },
  assertionFailure: { message: string; stack?: string } | undefined,
  reason: string,
): string {
  const onFailure =
    testInfo.status === 'failed' ||
    testInfo.status === 'timedOut' ||
    !!assertionFailure;
  const fixture: CapturedFixture = {
    name: sanitizeName(testInfo.title),
    description:
      opts.description ||
      `${onFailure ? 'failed' : 'captured'} live run: ${testInfo.title} (${reason})`,
    captured_at: new Date().toISOString(),
    spec: path.basename(testInfo.file || 'unknown.spec.ts'),
    base_url: process.env.OCTOS_TEST_URL || 'http://localhost:3000',
    session_id: drained.sessionId,
    events: drained.events.map(normalizeFrame),
    raw_events: drained.events,
    finalDom: drained.dom,
    assertionFailure,
  };
  const outDir =
    opts.outputDir || path.resolve(__dirname, '..', 'fixtures', 'captured');
  fs.mkdirSync(outDir, { recursive: true });
  const stamp = new Date()
    .toISOString()
    .replace(/[:.]/g, '-')
    .replace(/T/, '_')
    .replace(/Z$/, '');
  const filename = `${sanitizeName(testInfo.title)}-${stamp}.json`;
  const fullPath = path.join(outDir, filename);
  fs.writeFileSync(fullPath, JSON.stringify(fixture, null, 2), 'utf-8');
  // eslint-disable-next-line no-console
  console.log(
    `[capture-replay] (deferred) wrote ${fixture.events.length} events + ${fixture.raw_events.length} raw frames -> ${fullPath}`,
  );
  return fullPath;
}

// ── Internals ────────────────────────────────────────────────────────────

function noopHandle(): CaptureHandle {
  return {
    enabled: false,
    async recordUserSent() {
      /* no-op */
    },
    recordFailure() {
      /* no-op */
    },
    async finalize() {
      return null;
    },
  };
}

function sanitizeName(s: string): string {
  return s
    .toLowerCase()
    .replace(/[^a-z0-9-_]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 80) || 'capture';
}

/**
 * Best-effort normalization of a captured raw frame into something close
 * to PR H's `SseEvent`. Preserves all fields; renames the wire `type`
 * when we recognize it. Promoter scripts will still need to fill in
 * missing turn_id / cmid / session_id when promoting to Layer 1.
 */
function normalizeFrame(raw: RawSseEvent): NormalizedSseEvent {
  if (raw.source === 'dom-marker') {
    const p = (raw.payload as { type?: string }) || {};
    return { t: raw.t, ...p, type: p.type || 'dom_marker' };
  }
  if (typeof raw.payload !== 'object' || raw.payload === null) {
    return { t: raw.t, type: 'unknown', payload: raw.payload };
  }
  const obj = raw.payload as Record<string, unknown>;
  const type = (obj.type as string) || 'unknown';

  // Map the legacy `static/app.js` SSE shapes to PR H's wire names.
  // The mapping is intentionally non-destructive: we copy through every
  // original field plus rename `type`. The promoter has full info either
  // way (raw_events is preserved).
  //
  // IMPORTANT: `replace` is NOT mapped to `message_delta` — the SPA
  // treats `replace` as a full-content overwrite, whereas `message_delta`
  // is append-only. Collapsing the two would silently corrupt fixture
  // playback for any turn that emitted a `replace` (e.g. tool-progress
  // truncation, artifact preview replacement). `replace` is preserved
  // as `message_replace` so PR H's reducer can grow a dedicated handler
  // (or the promoter can reject the fixture as un-replayable today).
  let mappedType = type;
  if (type === 'token' || type === 'delta') mappedType = 'message_delta';
  else if (type === 'replace') mappedType = 'message_replace';
  else if (type === 'done') mappedType = 'turn_completed';
  else if (type === 'turn_start' || type === 'turn-started')
    mappedType = 'turn_started';
  else if (type === 'background_result' || type === 'bg_result')
    mappedType = 'background_result';

  return { t: raw.t, ...obj, type: mappedType };
}
