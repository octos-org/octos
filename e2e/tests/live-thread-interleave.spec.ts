/**
 * M8.10 PR #4 live e2e: thread-by-cmid interleave.
 *
 * Validates that when a slow question and a fast question are sent in
 * quick succession, the rendered DOM pairs each question with its own
 * answer, regardless of which one finishes first on the wire.
 *
 * Old (broken) flat-list rendering would interleave: e.g. user1, user2,
 * assistant1's progress events split across both bubbles, then assistant2,
 * then assistant1's final text — pairing breaks. The new thread-by-cmid
 * renderer (octos-web PR #4 / octos-org/octos#627) anchors each response
 * to its origin user message via `responseToClientMessageId`.
 *
 * The new renderer is BEHIND the feature flag
 * `localStorage.octos_thread_store_v2 = '1'`. This spec sets that flag
 * before any messages are sent.
 *
 * Required env:
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io
 *   OCTOS_AUTH_TOKEN=octos-admin-2026
 *   OCTOS_PROFILE=dspfac
 *
 * NEVER point at mini5 — that host is reserved for coding-green tests.
 */

import { expect, test, type Page } from '@playwright/test';

import {
  SEL,
  countAssistantBubbles,
  countUserBubbles,
  createNewSession,
  getInput,
  getSendButton,
  login,
} from './live-browser-helpers';

const SLOW_PROMPT =
  process.env.OCTOS_INTERLEAVE_SLOW_PROMPT ||
  'Use deep research to find the latest news about Rust language. ' +
    "Run the pipeline directly, don't ask. One paragraph.";
const FAST_PROMPT =
  process.env.OCTOS_INTERLEAVE_FAST_PROMPT || '1+1 等于几？只回答数字。';

// Marker used to detect the actual deep_research RESULT (not the
// spawn-ack). The slow prompt asks for "latest news about Rust
// language" — any real research output will reference "Rust" by name
// (it's the literal subject). The Chinese ack
// "深度研究已在后台启动…" contains zero Latin tokens, so this regex
// cannot match the ack. Word boundary keeps it from matching incidental
// substrings (e.g. "trust" or "robust"). We deliberately use a tight
// single-word marker rather than the looser
// `/rust|news|research|pipeline|update/i` that the original
// `FAST_HINT_RE`-mirrored constant suggested, because any English ack
// from a future workflow ("Deep research started…") would match
// `research` and re-introduce the false-pass.
const SLOW_HINT_RE = /\brust\b/i;
const FAST_HINT_RE = /\b2\b|二|两/;

const FLAG_KEY = 'octos_thread_store_v2';

const SLOW_MAX_WAIT_MS = 6 * 60 * 1000; // 6 minutes
const FAST_MAX_WAIT_MS = 90 * 1000; // 90s
const SEND_GAP_MS = 4_000; // ≤10s window between slow send and fast send

async function enableThreadStoreV2(page: Page) {
  await page.addInitScript((key) => {
    localStorage.setItem(key, '1');
  }, FLAG_KEY);
}

async function getOrderedBubbles(page: Page) {
  return page.evaluate(() => {
    const nodes = document.querySelectorAll(
      "[data-testid='user-message'], [data-testid='assistant-message']",
    );
    return Array.from(nodes).map((el, i) => ({
      index: i,
      role:
        el.getAttribute('data-testid') === 'user-message' ? 'user' : 'assistant',
      threadId: el.getAttribute('data-thread-id') || '',
      text: ((el as HTMLElement).innerText || '').trim().slice(0, 400),
    }));
  });
}

/**
 * Wait until both Q1 (slow) and Q2 (fast) have finalised.
 *
 * NOTE — #649 hardening: counting "filled" bubbles by `text.length > 8`
 * alone is not enough, because the slow deep_research path emits a
 * spawn-ack ("深度研究已在后台启动…") within ~1-3 s of the send. That
 * ack satisfies the length threshold and would let this poll declare
 * victory while the actual research RESULT — the late background-
 * completion that #649 misroutes — has not arrived yet. The pairing
 * assertions then run on an already-paired-by-cmid ack and false-pass.
 *
 * Fix: require Q1's paired bubble (the first assistant bubble after
 * the first user bubble in the new region) to contain a content marker
 * (`slowMarker`) before we accept the count as final. `SLOW_HINT_RE`
 * (`/\brust\b/i`) matches the actual research output and does NOT
 * match the Chinese ack — so this poll only releases once the
 * deep_research RESULT lands, exercising the late-binding code path
 * the #649 fix targets.
 *
 * Early-fail diagnostic: while polling, we also look at Q2's paired
 * bubble. If the slow marker shows up THERE before showing up under
 * Q1, that's the smoking-gun #649 misroute (late background result
 * inherited Q2's thread_id). We throw immediately with a clear
 * diagnostic instead of letting the test silently time out.
 */
async function waitForBothFinished(
  page: Page,
  expectedAssistantCount: number,
  maxWaitMs: number,
  baseUserBubbles: number,
  baseAssistantBubbles: number,
  slowMarker: RegExp,
) {
  const start = Date.now();
  let lastFilled = 0;
  let lastSlowMatched = false;
  let stable = 0;
  while (Date.now() - start < maxWaitMs) {
    const isStreaming = await page
      .locator(SEL.cancelButton)
      .isVisible()
      .catch(() => false);
    const filled = await page.evaluate((sel) => {
      const bubbles = document.querySelectorAll(sel);
      return Array.from(bubbles).filter((el) => {
        const text = ((el as HTMLElement).innerText || '').trim();
        return text.length > 8;
      }).length;
    }, SEL.assistantMessage);

    // Inspect Q1's vs Q2's paired bubbles. Q1's assistants live in the
    // new region between the first user bubble and the second user
    // bubble; Q2's assistants live after the second user bubble.
    const regionTexts = await page.evaluate(
      ({ baseUsers, baseAssistants }) => {
        const nodes = document.querySelectorAll(
          "[data-testid='user-message'], [data-testid='assistant-message']",
        );
        const all = Array.from(nodes);
        const newRegion = all.slice(baseUsers + baseAssistants);
        const firstUserIdx = newRegion.findIndex(
          (el) => el.getAttribute('data-testid') === 'user-message',
        );
        if (firstUserIdx < 0) return { slow: '', fast: '' };
        const secondUserRel = newRegion
          .slice(firstUserIdx + 1)
          .findIndex((el) => el.getAttribute('data-testid') === 'user-message');
        const slowEnd =
          secondUserRel < 0
            ? newRegion.length
            : firstUserIdx + 1 + secondUserRel;
        const slowSlice = newRegion.slice(firstUserIdx + 1, slowEnd);
        const fastSlice =
          secondUserRel < 0 ? [] : newRegion.slice(slowEnd + 1);
        const collect = (arr: Element[]) =>
          arr
            .filter(
              (el) => el.getAttribute('data-testid') === 'assistant-message',
            )
            .map((el) => ((el as HTMLElement).innerText || '').trim())
            .join('\n');
        return { slow: collect(slowSlice), fast: collect(fastSlice) };
      },
      { baseUsers: baseUserBubbles, baseAssistants: baseAssistantBubbles },
    );
    const slowMatched = slowMarker.test(regionTexts.slow);
    const fastHasSlowMarker = slowMarker.test(regionTexts.fast);

    // Smoking-gun: late deep_research result landed under Q2's bubble
    // instead of Q1's. Fail fast with full diagnostics rather than
    // burning the rest of the 6-min budget.
    if (fastHasSlowMarker && !slowMatched) {
      throw new Error(
        `BROKEN PAIRING (#649 misroute): slow-Q research-content marker (${slowMarker}) appeared under Q2's bubble while Q1's bubble has no such marker. This is exactly the #649 symptom: late background result inherited Q2's thread_id from the sticky map.\nQ1 region text: ${JSON.stringify(regionTexts.slow.slice(0, 400))}\nQ2 region text: ${JSON.stringify(regionTexts.fast.slice(0, 400))}`,
      );
    }

    if (filled >= expectedAssistantCount && !isStreaming && slowMatched) {
      stable += 1;
      if (stable >= 2) return filled;
    } else {
      stable = 0;
    }

    if (filled !== lastFilled || slowMatched !== lastSlowMatched) {
      const elapsed = ((Date.now() - start) / 1000).toFixed(0);
      console.log(
        `  [interleave] ${elapsed}s: filled=${filled}/${expectedAssistantCount} streaming=${isStreaming} slowMatched=${slowMatched} slowSnippet=${JSON.stringify(regionTexts.slow.slice(0, 120))}`,
      );
      lastFilled = filled;
      lastSlowMatched = slowMatched;
    }
    await page.waitForTimeout(3_000);
  }
  return lastFilled;
}

test.describe('Live thread interleave (M8.10 PR #4)', () => {
  test.setTimeout(SLOW_MAX_WAIT_MS + FAST_MAX_WAIT_MS + 60_000);

  test('slow Q + fast Q pair correctly with thread-store-v2 flag on', async ({
    page,
  }) => {
    await enableThreadStoreV2(page);
    await login(page);
    await createNewSession(page);

    const userBubblesBefore = await countUserBubbles(page);
    const assistantBubblesBefore = await countAssistantBubbles(page);

    // 1. Send slow question first.
    await getInput(page).fill(SLOW_PROMPT);
    await getSendButton(page).click();
    await expect.poll(() => countUserBubbles(page)).toBe(userBubblesBefore + 1);

    // 2. Wait briefly, then send the fast question. The slow Q may be
    //    in active foreground streaming OR may have already spawned to
    //    background — `deep_search` / `run_pipeline` are spawn_only, so
    //    the foreground SSE turn ends with a "background started" ack
    //    within ~2-3s and the cancel button disappears even though the
    //    real research is still running. We DO NOT gate on
    //    `cancelButton.isVisible()` here: gating on it caused a
    //    pre-existing failure that blocked this spec from ever
    //    reaching the pairing-assertion stage. The threading invariant
    //    we test below holds either way: each user message's responses
    //    (including any later background-completion bubble) bind to
    //    its own thread by clientMessageId / cmid.
    await page.waitForTimeout(SEND_GAP_MS);

    await getInput(page).fill(FAST_PROMPT);
    await getSendButton(page).click();
    await expect.poll(() => countUserBubbles(page)).toBe(userBubblesBefore + 2);

    // 3. Wait until both answers are complete AND Q1's paired bubble
    //    contains the actual research-content marker (not just the
    //    spawn-ack). See `waitForBothFinished` doc comment for the
    //    #649 hardening rationale.
    const assistantsExpected = assistantBubblesBefore + 2;
    const filled = await waitForBothFinished(
      page,
      assistantsExpected,
      SLOW_MAX_WAIT_MS,
      userBubblesBefore,
      assistantBubblesBefore,
      SLOW_HINT_RE,
    );
    expect(
      filled,
      `Only ${filled}/${assistantsExpected} assistant bubbles completed within ${SLOW_MAX_WAIT_MS}ms`,
    ).toBeGreaterThanOrEqual(assistantsExpected);

    // 4. Pull the bubble order from the DOM and assert pairing.
    //    Expected pattern (thread-by-cmid):
    //       [user_slow][assistant_slow ...][user_fast][assistant_fast ...]
    //    The slow thread was sent first, so it appears first in the thread
    //    list (sort key = userMsg.timestamp). Each user is followed by its
    //    own assistant response, regardless of arrival order on the wire.
    const bubbles = await getOrderedBubbles(page);
    const newBubbles = bubbles.slice(
      userBubblesBefore + assistantBubblesBefore,
    );
    console.log('new bubbles after both Qs:', JSON.stringify(newBubbles, null, 2));

    // The first new bubble should be the slow user.
    expect(newBubbles.length).toBeGreaterThanOrEqual(4);
    expect(newBubbles[0].role).toBe('user');

    // Find the indices of user / assistant pairs.
    const userIndices = newBubbles
      .map((b, i) => (b.role === 'user' ? i : -1))
      .filter((i) => i >= 0);
    expect(userIndices).toHaveLength(2);

    // Slow user is at [0]. Its assistant must come BEFORE the fast user.
    const fastUserIdx = userIndices[1];
    expect(fastUserIdx).toBeGreaterThan(0);

    // There must be at least one assistant bubble between slow user and
    // fast user that contains some recognizable slow-prompt content.
    const between = newBubbles.slice(1, fastUserIdx);
    const slowAssistantBetween = between.filter(
      (b) => b.role === 'assistant' && b.text.length > 0,
    );
    expect(
      slowAssistantBetween.length,
      `Expected at least one slow-thread assistant bubble between users; got ${between.length} bubbles between: ${JSON.stringify(between)}`,
    ).toBeGreaterThan(0);

    // After the fast user, there must be the fast assistant.
    const after = newBubbles.slice(fastUserIdx + 1);
    const fastAssistantAfter = after.filter(
      (b) => b.role === 'assistant' && b.text.length > 0,
    );
    expect(
      fastAssistantAfter.length,
      `Expected at least one fast-thread assistant bubble after the fast user; got ${after.length} bubbles after: ${JSON.stringify(after)}`,
    ).toBeGreaterThan(0);

    // Pairing sanity: each thread's assistant bubble carries
    // data-thread-id matching its parent user. The renderer (PR #4) sets
    // data-thread-id on assistant-message elements. If absent (older
    // build), skip this check rather than fail.
    const slowThreadIds = slowAssistantBetween
      .map((b) => b.threadId)
      .filter((tid) => tid.length > 0);
    const fastThreadIds = fastAssistantAfter
      .map((b) => b.threadId)
      .filter((tid) => tid.length > 0);
    if (slowThreadIds.length > 0 && fastThreadIds.length > 0) {
      // The slow thread id and fast thread id must differ — pairing intact.
      expect(slowThreadIds[0]).not.toBe(fastThreadIds[0]);
    }

    // Hard content check (#649 hardening): the slow assistant's text
    // MUST contain a research-content marker — this is what proves the
    // late deep_research RESULT actually attached to Q1's bubble (and
    // not Q2's, which is the #649 misrouting symptom). Without this
    // assertion the spec would false-pass on the spawn-ack alone.
    // `waitForBothFinished` already polls for this marker, so by the
    // time we reach here it should be present; if it's not, the late
    // result either timed out (raise SLOW_MAX_WAIT_MS) or got bound to
    // the wrong bubble (the #649 regression).
    const slowText = slowAssistantBetween.map((b) => b.text).join(' ');
    const fastText = fastAssistantAfter.map((b) => b.text).join(' ');
    expect(
      SLOW_HINT_RE.test(slowText),
      `Slow-Q's paired bubble has no research-content marker. This means the deep_research result either never arrived under Q1's bubble (possibly bound to Q2's bubble — the #649 regression) or the spawn-ack alone is what got rendered. slow="${slowText.slice(0, 400)}" fast="${fastText.slice(0, 200)}"`,
    ).toBe(true);

    // Soft semantic check: the slow assistant's text should NOT include
    // any "1+1=2" content (that's the fast Q's answer). If it does, the
    // wires got crossed.
    if (FAST_HINT_RE.test(slowText) && !SLOW_HINT_RE.test(slowText)) {
      throw new Error(
        `BROKEN PAIRING: slow assistant text contains fast-Q answer marker but no slow-Q content. slow="${slowText.slice(0, 200)}" fast="${fastText.slice(0, 200)}"`,
      );
    }
  });
});
