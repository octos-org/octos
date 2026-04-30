/**
 * Live realtime status gate — UI surfaces long-running pipeline progress.
 *
 * Validates the deep_research / run_pipeline UI surface end-to-end through
 * the chat web client. The deployed UI is **timeline-only**: a populated
 * `<ul data-testid='tool-call-runtime-timeline'>` rendered inside the
 * `tool-call-bubble` of the run_pipeline tool-call. No NodeCard tree, no
 * `[role='status']` aria-live surface — those never shipped (see task
 * #651 — NodeCard product decision still deferred).
 *
 * Contract (against today's React tree):
 *   1. Trigger a deep_research run via run_pipeline.
 *   2. Find the assistant message added by THIS turn (skip pre-existing
 *      bubbles) and locate its `tool-call-bubble` whose visible name is
 *      `run_pipeline`. Other tools may emit timelines too — we don't
 *      accept their timelines as proof the pipeline UI works.
 *   3. The run_pipeline bubble's `tool-call-runtime-timeline <ul>` must
 *      contain at least one line that matches a pipeline-executor full-
 *      line format (anchored regex, see PIPELINE_LINE_RE).
 *   4. The run_pipeline bubble's `data-tool-call-id` must match a row
 *      in `/api/sessions/<sid>/tasks` whose `tool_name === 'run_pipeline'`
 *      AND whose `started_at` is >= the moment we sent the prompt
 *      (rejects stale tasks from prior turns / sessions).
 *
 * Why this is tight enough to catch SSE-pipeline regressions:
 *   * If the SSE bridge silently dropped tool_progress frames, the
 *     timeline `<ul>` would never render (empty `tc.progress` array
 *     hides the `<ul>` — see chat-thread.tsx:162). Step 3 fails.
 *   * If progress frames arrived but with wrong/missing tool_call_id,
 *     they'd attach to a different bubble or float orphaned. The
 *     run_pipeline bubble would have an empty timeline. Step 3 fails.
 *   * If the executor stopped emitting structured progress text, the
 *     timeline could populate from a leak/echo source but no line
 *     would match the anchored full-line regex. Step 3 fails.
 *   * If a generic supervised tool (e.g. `fm_tts: running`) leaked
 *     into the timeline, the regex's anchored, dot-strict shapes
 *     reject it. Step 3 fails.
 *   * If the cross-check fell through to a stale `run_pipeline` row
 *     from a previous session/turn, the `started_at` baseline rejects
 *     it. Step 4 fails.
 *
 * When NodeCard ships (task #651), re-add the tree assertion as an
 * ADDITIONAL gate alongside this timeline check — don't replace it.
 *
 * Run from /Users/yuechen/home/octos/e2e:
 *
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *     npx playwright test tests/live-realtime-status.spec.ts --workers=1
 */

import { expect, test } from '@playwright/test';

import {
  countAssistantBubbles,
  countUserBubbles,
  createNewSession,
  getEffectiveAdminToken,
  getInput,
  getSendButton,
  login,
} from './live-browser-helpers';

const BASE = process.env.OCTOS_TEST_URL || 'https://dspfac.bot.ominix.io';
const PROFILE = process.env.OCTOS_PROFILE || 'dspfac';

if (BASE.includes('dspfac.ocean.ominix.io')) {
  throw new Error('live-realtime-status refuses to run against mini5; pick mini1/2/3/4 instead.');
}

const PROMPT =
  'Use run_pipeline with deep_research to investigate the latest Bitcoin ' +
  'price news. Use 3 sources. One short paragraph synthesis is enough. ' +
  'Run the pipeline directly.';

const ASSISTANT_MESSAGE = "[data-testid='assistant-message']";
const TOOL_CALL_BUBBLE = "[data-testid='tool-call-bubble']";
const TIMELINE = "[data-testid='tool-call-runtime-timeline']";

// Pipeline progress entries the executor emits via `report_progress`
// (see crates/octos-pipeline/src/executor.rs). The runtime timeline
// strips a leading `[info]/[debug]/[warn]/[error]` tag in chat-thread.tsx
// (lines 169–171), so we match the post-strip surface text.
//
// Each alternative below is anchored to the FULL LINE (^...$, no
// substrings) and pinned to the literal punctuation the executor uses.
// This rejects accidental matches against generic supervised-task
// progress like `fm_tts: running` (no trailing dots) or
// `tts: completed` (different verb).
//
// Executor line shapes covered:
//   "Pipeline 'deep_research' started (5 nodes)"     -> EXEC line 800
//   "Pipeline 'deep_research' complete (12s)"        -> EXEC line 1559
//   "search [llm]: 'rust async' done (2/3, 1s)"      -> EXEC line 988
//   "synthesize: planning sub-tasks..."              -> EXEC line 1114
//   "synthesize: 4 workers running (a, b, c, d)"     -> EXEC line 1235
//   "synthesize: 'worker_a' done (2/4, 1s)"          -> EXEC line 1289
//   "synthesize: done (4 workers, 12s)"              -> EXEC line 1313
//   "step: running..."                               -> EXEC line 1414
//   "step: done (12s)"                               -> EXEC line 1494
const PIPELINE_LINE_RE = new RegExp(
  [
    // "Pipeline '<id>' started (<n> nodes)"
    String.raw`^Pipeline '[^']+' started \(\d+ nodes?\)$`,
    // "Pipeline '<id>' complete (<secs>s)"
    String.raw`^Pipeline '[^']+' complete \(\d+(?:\.\d+)?s\)$`,
    // "<label>[ [model]]: '<target>' done (<n>/<m>, <secs>s)"
    String.raw`^[\w.-]+(?: \[[^\]]+\])?: '[^']+' done \(\d+/\d+, \d+(?:\.\d+)?s\)$`,
    // "<label>: planning sub-tasks..."
    String.raw`^[\w.-]+: planning sub-tasks\.\.\.$`,
    // "<label>: <n> workers running (...)"
    String.raw`^[\w.-]+: \d+ workers running \(.+\)$`,
    // "<label>: running..." (executor uses three dots)
    String.raw`^[\w.-]+: running\.\.\.$`,
    // "<label>: done (<secs>s)" or "<label>: done (<n> workers, <secs>s)"
    // — pipeline executor only emits these two forms (EXEC 1313, 1494).
    String.raw`^[\w.-]+: done \((?:\d+ workers?, )?\d+(?:\.\d+)?s\)$`,
  ].join('|'),
);

interface BackgroundTaskRow {
  id?: string;
  tool_name?: string;
  tool_call_id?: string;
  parent_session_key?: string | null;
  status?: string;
  lifecycle_state?: string;
  // started_at is an ISO timestamp string (RFC3339) on this API.
  started_at?: string | null;
}

async function getTasks(
  sessionId: string,
  token: string,
): Promise<BackgroundTaskRow[]> {
  const resp = await fetch(
    `${BASE}/api/sessions/${encodeURIComponent(sessionId)}/tasks`,
    {
      headers: {
        Authorization: `Bearer ${token}`,
        'X-Profile-Id': PROFILE,
      },
    },
  );
  if (!resp.ok) return [];
  return (await resp.json().catch(() => [])) as BackgroundTaskRow[];
}

function parseStartedAtMs(value: string | null | undefined): number | null {
  if (!value) return null;
  const ts = Date.parse(value);
  return Number.isFinite(ts) ? ts : null;
}

test.describe(`Realtime status surface (${BASE})`, () => {
  test.setTimeout(360_000);

  test('timeline populates with pipeline progress, anchored to run_pipeline', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    const sessionIdBefore = await page.evaluate(() =>
      localStorage.getItem('octos_current_session'),
    );
    expect(sessionIdBefore, 'expected octos_current_session after createNewSession').toBeTruthy();

    const userBefore = await countUserBubbles(page);
    const assistantBefore = await countAssistantBubbles(page);

    // Baseline pre-existing run_pipeline tasks so a stale row from a
    // prior turn cannot satisfy the cross-check.
    const token = await getEffectiveAdminToken();
    const baselineTaskIds = new Set(
      (await getTasks(sessionIdBefore!, token))
        .filter((row) => row.tool_name === 'run_pipeline')
        .map((row) => row.id ?? row.tool_call_id ?? '')
        .filter(Boolean),
    );
    const sentAtMs = Date.now();

    await getInput(page).fill(PROMPT);
    await getSendButton(page).click();

    // 1. User bubble materializes.
    await expect.poll(() => countUserBubbles(page)).toBe(userBefore + 1);

    // 2. Assistant placeholder bubble within 30s.
    await expect
      .poll(() => countAssistantBubbles(page), { timeout: 30_000 })
      .toBeGreaterThanOrEqual(assistantBefore + 1);

    // The assistant bubble we care about is the FIRST one added by
    // this turn — index `assistantBefore` (0-based).
    const turnAssistant = page.locator(ASSISTANT_MESSAGE).nth(assistantBefore);

    // 3. Tool-call bubble inside the new assistant within 60s.
    await expect
      .poll(() => turnAssistant.locator(TOOL_CALL_BUBBLE).count(), {
        timeout: 60_000,
      })
      .toBeGreaterThan(0);

    // 4. The run_pipeline tool-call-bubble. The bubble structure is
    //    `<div data-testid='tool-call-bubble'><span>{tc.name}{retryBadge?}
    //     </span>{tc.progress.length > 0 && <ul ...>}</div>`
    //    (chat-thread.tsx:151–177 / 611–641). The retry badge, when
    //    present, adds an `×N` suffix to the span text. We DO NOT use
    //    `:has-text("run_pipeline")` (matches any descendant text in
    //    the bubble) — instead we read the FIRST text node of the
    //    bubble's first child span and require it to equal
    //    "run_pipeline" exactly. This rejects planning/echo bubbles
    //    that mention "run_pipeline" in nested progress text. Wait
    //    up to 90s: the agent may run a planning tool first.
    async function locateRunPipelineBubbleId(): Promise<string | null> {
      return turnAssistant.evaluate((root, sel) => {
        const bubbles = root.querySelectorAll(sel) as NodeListOf<HTMLElement>;
        for (const b of Array.from(bubbles)) {
          const span = b.querySelector(':scope > span');
          if (!span) continue;
          // firstChild is the leading text node ({tc.name}) before any
          // nested badge element.
          const first = span.firstChild;
          const label =
            first && first.nodeType === Node.TEXT_NODE
              ? (first.nodeValue ?? '').trim()
              : '';
          if (label === 'run_pipeline') {
            return b.getAttribute('data-tool-call-id');
          }
        }
        return null;
      }, TOOL_CALL_BUBBLE);
    }

    let runPipelineToolCallId: string | null = null;
    await expect
      .poll(async () => {
        runPipelineToolCallId = await locateRunPipelineBubbleId();
        return runPipelineToolCallId;
      }, { timeout: 90_000 })
      .not.toBeNull();
    expect(
      runPipelineToolCallId,
      'run_pipeline tool-call-bubble must be present in this turn',
    ).toBeTruthy();
    const runPipelineBubble = turnAssistant.locator(
      `${TOOL_CALL_BUBBLE}[data-tool-call-id="${runPipelineToolCallId}"]`,
    );

    // 5. Runtime timeline populates inside the run_pipeline bubble
    //    within 180s. The `<ul>` only renders when
    //    `tc.progress.length > 0` (chat-thread.tsx:162) — its mere
    //    presence implies at least one progress frame was delivered,
    //    parsed, and stored against this exact tool_call_id.
    await expect
      .poll(() => runPipelineBubble.locator(TIMELINE).count(), {
        timeout: 180_000,
      })
      .toBeGreaterThan(0);

    // 6. At least one timeline `<li>` text matches a pipeline-format
    //    line. Poll up to 120s after the `<ul>` first appears so
    //    slow-starting executors that emit the "Pipeline X started"
    //    line late still pass.
    let matchingLine: string | undefined;
    let allTimelineText = '';
    await expect
      .poll(
        async () => {
          const lis = await runPipelineBubble
            .locator(`${TIMELINE} > li`)
            .allTextContents();
          allTimelineText = lis.join('\n');
          matchingLine = lis
            .map((l) => l.trim())
            .find((l) => PIPELINE_LINE_RE.test(l));
          return matchingLine !== undefined;
        },
        { timeout: 120_000 },
      )
      .toBe(true);
    console.log(`[realtime] matched_line=${JSON.stringify(matchingLine)}`);
    expect(
      matchingLine,
      `no pipeline-format progress line in run_pipeline timeline. Sample: ${allTimelineText.slice(0, 800)}`,
    ).toBeTruthy();

    // 7. tool_call_id is `runPipelineToolCallId` (already extracted in
    //    step 4 from `data-tool-call-id` on the run_pipeline bubble).
    //    This is the same id the backend SSE channel used.
    const renderedToolCallId = runPipelineToolCallId;
    expect(
      renderedToolCallId,
      'run_pipeline tool-call-bubble must expose data-tool-call-id',
    ).toBeTruthy();
    console.log(`[realtime] rendered_tool_call_id=${renderedToolCallId}`);

    // 8. Cross-check via `/api/sessions/<sid>/tasks`. Require:
    //      - tool_name === 'run_pipeline'
    //      - tool_call_id === renderedToolCallId
    //      - id NOT in pre-send baseline (rejects stale rows)
    //      - started_at parseable AND >= sentAtMs - 5s skew
    //    The skew accounts for clock drift between mini and CI. A
    //    passing cross-check proves the rendered surface corresponds
    //    to a freshly-registered pipeline task in this session.
    const sessionIdNow = await page.evaluate(() =>
      localStorage.getItem('octos_current_session'),
    );
    expect(sessionIdNow, 'octos_current_session should still be set').toBeTruthy();
    expect(sessionIdNow).toBe(sessionIdBefore);

    let matchedTask: BackgroundTaskRow | undefined;
    await expect
      .poll(
        async () => {
          const rows = await getTasks(sessionIdNow!, token);
          matchedTask = rows.find((row) => {
            if (row.tool_name !== 'run_pipeline') return false;
            if (row.tool_call_id !== renderedToolCallId) return false;
            const rowId = row.id ?? row.tool_call_id ?? '';
            if (baselineTaskIds.has(rowId)) return false;
            const startedMs = parseStartedAtMs(row.started_at);
            if (startedMs === null) return false;
            return startedMs >= sentAtMs - 5_000;
          });
          return matchedTask !== undefined;
        },
        { timeout: 60_000, intervals: [2_000, 3_000, 5_000] },
      )
      .toBe(true);
    console.log(
      `[realtime] task_match id=${matchedTask?.id} tool_call_id=${matchedTask?.tool_call_id} started_at=${matchedTask?.started_at}`,
    );

    // 9. Final sanity: only ONE user bubble was added.
    const userAfter = await countUserBubbles(page);
    const assistantAfter = await countAssistantBubbles(page);
    expect(userAfter).toBe(userBefore + 1);
    const newAssistantBubbles = assistantAfter - assistantBefore;
    if (newAssistantBubbles > 1) {
      console.log(
        `[realtime] note: newAssistantBubbles=${newAssistantBubbles} (allowed)`,
      );
    }
  });
});
