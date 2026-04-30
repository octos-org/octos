/**
 * M8.10 follow-up (#649) live e2e: overflow thread-binding under 3-user race.
 *
 * Regression for the production trace observed on mini3 (2026-04-29,
 * session `web-1777402538752-zn7jfr`):
 *
 *   05:55:36 user      tid=A  深度搜索一下中国的探月工程 clep
 *   05:55:50 user      tid=B  今日股市如何
 *   05:55:55 user      tid=C  你有哪些内置语音
 *   05:56:03 tool      tid=C  # Deep Research: 今日股市行情 ... ← BUG
 *   05:56:14 assistant tid=C  搜索未找到 ... 2026 年 4 月 29 日 ... ← BUG
 *
 * Pre-fix: a long-running spawn_only / background task started in turn A
 * inherits whatever the api_channel sticky map currently holds when it
 * finalises — which after a fast 3-user follow-up is turn C's thread_id.
 * The web client then renders A's late result under C's bubble; A's user
 * bubble appears with NO assistant response.
 *
 * Post-fix (this PR): the BackgroundResultPayload carries
 * `originating_thread_id` snapshotted at spawn time, and
 * `deliver_background_notification` stamps it onto the OutboundMessage
 * metadata so api_channel resolves the thread via the explicit-metadata
 * path, NOT the sticky-map fallback.
 *
 * What this spec drives:
 *  - 3 user messages within a ≤5s window: Q1 (slow / spawning),
 *    Q2 (fast — stocks-style), Q3 (fast — voices-style).
 *  - Wait for ALL responses + tool outputs to land.
 *  - Assert each user's bubble has its own assistant response paired
 *    correctly via `data-thread-id`.
 *  - Assert NO bubble is empty / orphaned.
 *
 * Required env:
 *   OCTOS_TEST_URL=https://dspfac.octos.ominix.io
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

// Q1 is the SLOW / SPAWNING question. We use a deep_research-style
// prompt so the agent hands the work off to a background subagent —
// that's the path #649 fixes.
const Q1_PROMPT =
  process.env.OCTOS_OVERFLOW_Q1_PROMPT ||
  '深度搜索一下中国的探月工程 CLEP，给我一份简短的研究报告。直接执行流程，不要确认。';

// Q2 fires DURING Q1's processing window. A fast factual question —
// triggers the speculative-overflow path on the session actor.
const Q2_PROMPT =
  process.env.OCTOS_OVERFLOW_Q2_PROMPT || '今日股市如何？只需一句概要。';

// Q3 fires immediately after Q2. Another fast factual — drives the
// sticky map to rotate to a third value before Q1's background result
// finalises.
const Q3_PROMPT =
  process.env.OCTOS_OVERFLOW_Q3_PROMPT || '你有哪些内置语音？只列出名称。';

const FLAG_KEY = 'octos_thread_store_v2';

const Q_GAP_MS = 1_500; // ≤5s total window: 1.5s × 2 ≈ 3s
const SLOW_MAX_WAIT_MS = 8 * 60 * 1000; // 8 minutes — deep_research can be slow
const STABLE_OBSERVATIONS = 3;

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
      text: ((el as HTMLElement).innerText || '').trim().slice(0, 600),
    }));
  });
}

async function waitForAllFinished(
  page: Page,
  expectedAssistantCount: number,
  maxWaitMs: number,
) {
  const start = Date.now();
  let lastFilled = 0;
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
        return text.length > 4;
      }).length;
    }, SEL.assistantMessage);

    if (filled >= expectedAssistantCount && !isStreaming) {
      stable += 1;
      if (stable >= STABLE_OBSERVATIONS) return filled;
    } else {
      stable = 0;
    }

    if (filled !== lastFilled) {
      const elapsed = ((Date.now() - start) / 1000).toFixed(0);
      console.log(
        `  [overflow-binding] ${elapsed}s: filled=${filled}/${expectedAssistantCount} streaming=${isStreaming}`,
      );
      lastFilled = filled;
    }
    await page.waitForTimeout(3_000);
  }
  return lastFilled;
}

test.describe('Live overflow thread binding (M8.10 follow-up #649)', () => {
  test.setTimeout(SLOW_MAX_WAIT_MS + 60_000);

  test('3-user fast follow-up keeps each bubble paired with its own assistant', async ({
    page,
  }) => {
    await enableThreadStoreV2(page);
    await login(page);
    await createNewSession(page);

    const userBubblesBefore = await countUserBubbles(page);
    const assistantBubblesBefore = await countAssistantBubbles(page);

    // Q1 — slow / spawning.
    await getInput(page).fill(Q1_PROMPT);
    await getSendButton(page).click();
    await expect.poll(() => countUserBubbles(page)).toBe(userBubblesBefore + 1);
    console.log('[overflow-binding] Q1 sent (slow / spawning)');

    // Q2 — fast follow-up DURING Q1's processing.
    await page.waitForTimeout(Q_GAP_MS);
    await getInput(page).fill(Q2_PROMPT);
    await getSendButton(page).click();
    await expect.poll(() => countUserBubbles(page)).toBe(userBubblesBefore + 2);
    console.log('[overflow-binding] Q2 sent (fast — stocks-style)');

    // Q3 — second fast follow-up. Now the sticky map has rotated A→B→C.
    await page.waitForTimeout(Q_GAP_MS);
    await getInput(page).fill(Q3_PROMPT);
    await getSendButton(page).click();
    await expect.poll(() => countUserBubbles(page)).toBe(userBubblesBefore + 3);
    console.log('[overflow-binding] Q3 sent (fast — voices-style)');

    // Wait for ALL three threads to finalise. Q1 may take minutes
    // because deep_research runs a background subagent; Q2 and Q3
    // finish quickly. The slow Q1 is the one whose late result tests
    // the fix.
    const assistantsExpected = assistantBubblesBefore + 3;
    const filled = await waitForAllFinished(
      page,
      assistantsExpected,
      SLOW_MAX_WAIT_MS,
    );
    expect(
      filled,
      `Only ${filled}/${assistantsExpected} assistant bubbles completed within ${SLOW_MAX_WAIT_MS}ms`,
    ).toBeGreaterThanOrEqual(assistantsExpected);

    // Pull the bubble order from the DOM and assert pairing.
    const bubbles = await getOrderedBubbles(page);
    const newBubbles = bubbles.slice(
      userBubblesBefore + assistantBubblesBefore,
    );
    console.log('new bubbles after 3 Qs:', JSON.stringify(newBubbles, null, 2));

    // Identify the three user-bubble indices in render order.
    const userIndices = newBubbles
      .map((b, i) => (b.role === 'user' ? i : -1))
      .filter((i) => i >= 0);
    expect(
      userIndices,
      `Expected exactly 3 user bubbles in the new region, got ${userIndices.length}; bubbles: ${JSON.stringify(newBubbles)}`,
    ).toHaveLength(3);

    // Each user bubble must have ≥1 NON-EMPTY assistant bubble paired
    // with it. "Paired" = appears AFTER this user bubble and BEFORE
    // the next user bubble (or end of list).
    for (let k = 0; k < userIndices.length; k++) {
      const userIdx = userIndices[k];
      const nextIdx =
        k + 1 < userIndices.length ? userIndices[k + 1] : newBubbles.length;
      const between = newBubbles.slice(userIdx + 1, nextIdx);
      const filledAssistants = between.filter(
        (b) => b.role === 'assistant' && b.text.trim().length > 0,
      );
      expect(
        filledAssistants.length,
        `User bubble #${k + 1} (text="${newBubbles[userIdx].text.slice(0, 80)}") has no paired non-empty assistant bubble. Bubbles between this user and the next: ${JSON.stringify(between)}`,
      ).toBeGreaterThan(0);
    }

    // Per-thread thread_id assertion: each user bubble's thread_id (if
    // emitted) must MATCH the thread_id of every assistant bubble paired
    // with it. The bug pre-fix is that turn A's late background result
    // gets stamped with turn C's thread_id, so its assistant bubbles end
    // up under turn C — different `data-thread-id` from turn A's user
    // bubble. The renderer skips threads whose user bubble has no
    // matching assistant data-thread-id, leaving turn A's bubble empty
    // (the production symptom).
    for (let k = 0; k < userIndices.length; k++) {
      const userIdx = userIndices[k];
      const userTid = newBubbles[userIdx].threadId;
      if (!userTid) {
        // Older renderer / no thread_id surfaced — skip this check.
        continue;
      }
      const nextIdx =
        k + 1 < userIndices.length ? userIndices[k + 1] : newBubbles.length;
      const between = newBubbles.slice(userIdx + 1, nextIdx);
      const assistants = between.filter((b) => b.role === 'assistant');
      const matchingAssistant = assistants.find((b) => b.threadId === userTid);
      expect(
        matchingAssistant,
        `User #${k + 1} (thread_id="${userTid}", text="${newBubbles[userIdx].text.slice(0, 80)}") has NO assistant bubble with matching thread_id. Adjacent assistants: ${JSON.stringify(assistants)}. This is the #649 bug shape: late background result inherited the wrong thread_id from the sticky map.`,
      ).toBeDefined();
    }

    // No "orphaned" assistant bubble: every assistant bubble in the new
    // region must come AFTER one of our three user bubbles. (A bubble
    // appearing BEFORE the first user index would be an orphan from
    // a prior session bleed; a bubble whose thread_id matches none of
    // the three user thread_ids would be a misrouted bubble.)
    const userThreadIds = userIndices
      .map((i) => newBubbles[i].threadId)
      .filter((t) => t && t.length > 0);
    if (userThreadIds.length === userIndices.length) {
      // All user bubbles surfaced thread_ids — we can do strong routing.
      const assistantBubblesNew = newBubbles.filter(
        (b) => b.role === 'assistant' && b.text.trim().length > 0,
      );
      for (const ab of assistantBubblesNew) {
        if (!ab.threadId) continue; // older builds may not surface tid
        expect(
          userThreadIds,
          `Assistant bubble (thread_id="${ab.threadId}", text="${ab.text.slice(0, 80)}") routes to a thread that has NO user bubble in this turn. This is exactly the #649 misrouting symptom. New user thread_ids: ${JSON.stringify(userThreadIds)}`,
        ).toContain(ab.threadId);
      }
    }
  });
});
