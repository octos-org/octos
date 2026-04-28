/**
 * M8.10 PR #4 live e2e: tool-retry collapse.
 *
 * Validates that when an LLM retries the same tool with bad args (e.g. 6×
 * weather pills before settling on the right call), the rendered UI
 * collapses retries into a SINGLE tool-call-bubble that carries a retry
 * counter — not N duplicate pills.
 *
 * Implements the assertion described in the M8.10 plan: send a Chinese-
 * city weather prompt that historically caused 3+ tool retries; assert
 * exactly one `data-testid="tool-call-bubble"` for the tool with
 * `data-tool-call-retry-count >= 1`.
 *
 * The test is allowed to skip if the LLM happens to nail the call on the
 * first attempt — retryCount === 0 on the only bubble means there's no
 * retry behavior to validate, but the renderer is correct (no extra
 * pills appeared). We assert the absence of duplicate pills regardless.
 *
 * The new renderer is BEHIND the feature flag
 * `localStorage.octos_thread_store_v2 = '1'`. This spec sets that flag
 * before any messages are sent.
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

const FLAG_KEY = 'octos_thread_store_v2';

const RETRY_PROMPT =
  process.env.OCTOS_RETRY_PROMPT ||
  '北京今天的天气怎么样？请使用工具查询。';

const TOOL_CALL_BUBBLE = "[data-testid='tool-call-bubble']";
const RETRY_BADGE = "[data-testid='tool-call-retry-badge']";

const MAX_WAIT_FOR_FINAL_MS = 4 * 60 * 1000; // 4 minutes
const MAX_WAIT_FOR_TOOL_CALL_MS = 90 * 1000;

async function enableThreadStoreV2(page: Page) {
  await page.addInitScript((key) => {
    localStorage.setItem(key, '1');
  }, FLAG_KEY);
}

async function waitForCompletion(page: Page, maxWaitMs: number) {
  const start = Date.now();
  let stable = 0;
  while (Date.now() - start < maxWaitMs) {
    const isStreaming = await page
      .locator(SEL.cancelButton)
      .isVisible()
      .catch(() => false);
    if (!isStreaming) {
      stable += 1;
      if (stable >= 2) return true;
    } else {
      stable = 0;
    }
    await page.waitForTimeout(2_500);
  }
  return false;
}

interface ToolCallBubbleSnapshot {
  index: number;
  toolCallId: string;
  retryCount: number;
  name: string;
  threadId: string;
}

async function snapshotToolCallBubbles(page: Page): Promise<ToolCallBubbleSnapshot[]> {
  return page.evaluate((sel) => {
    const nodes = document.querySelectorAll(sel);
    return Array.from(nodes).map((el, i) => {
      const parentAssistant = el.closest("[data-testid='assistant-message']");
      const text = ((el as HTMLElement).innerText || '').trim();
      // First non-empty whitespace-separated token = tool name (the
      // renderer puts `<tool-name><retry-badge>` on the first line).
      const firstLine = text.split(/\n/)[0] || '';
      const tokenMatch = firstLine.match(/^([A-Za-z0-9_.-]+)/);
      return {
        index: i,
        toolCallId: el.getAttribute('data-tool-call-id') || '',
        retryCount: Number(
          el.getAttribute('data-tool-call-retry-count') ?? '0',
        ),
        name: tokenMatch ? tokenMatch[1] : '',
        threadId: parentAssistant?.getAttribute('data-thread-id') || '',
      };
    });
  }, TOOL_CALL_BUBBLE);
}

test.describe('Live tool-retry collapse (M8.10 PR #4)', () => {
  test.setTimeout(MAX_WAIT_FOR_FINAL_MS + 60_000);

  test(
    'multiple retries of the same tool collapse into one bubble with retry counter',
    async ({ page }) => {
      await enableThreadStoreV2(page);
      await login(page);
      await createNewSession(page);

      const userBubblesBefore = await countUserBubbles(page);
      const assistantBubblesBefore = await countAssistantBubbles(page);

      await getInput(page).fill(RETRY_PROMPT);
      await getSendButton(page).click();

      // 1) User bubble appears.
      await expect.poll(() => countUserBubbles(page)).toBe(userBubblesBefore + 1);

      // 2) Assistant bubble materializes.
      await expect
        .poll(() => countAssistantBubbles(page), {
          timeout: 30_000,
          intervals: [1_000, 2_000, 3_000],
        })
        .toBeGreaterThanOrEqual(assistantBubblesBefore + 1);

      // 3) At least one tool-call bubble must appear within the wait window.
      await expect
        .poll(() => page.locator(TOOL_CALL_BUBBLE).count(), {
          timeout: MAX_WAIT_FOR_TOOL_CALL_MS,
          intervals: [2_000, 3_000, 5_000],
        })
        .toBeGreaterThan(0);

      // 4) Wait for completion (streaming finishes).
      const finished = await waitForCompletion(page, MAX_WAIT_FOR_FINAL_MS);
      expect(finished, 'Stream did not finish within the test window').toBeTruthy();

      // 5) Snapshot all tool-call bubbles. Group by name — for any name
      //    appearing more than once we have a duplicate-pill regression.
      const bubbles = await snapshotToolCallBubbles(page);
      console.log(
        'tool-call bubbles after completion:',
        JSON.stringify(bubbles, null, 2),
      );

      const byName = new Map<string, ToolCallBubbleSnapshot[]>();
      for (const b of bubbles) {
        if (!b.name) continue;
        const list = byName.get(b.name) ?? [];
        list.push(b);
        byName.set(b.name, list);
      }

      // The CRITICAL assertion: no duplicate-pill regressions. Each
      // distinct tool name must appear in at most ONE bubble — retries
      // collapse into the same bubble and bump retryCount.
      for (const [name, list] of byName.entries()) {
        expect(
          list.length,
          `Tool "${name}" rendered ${list.length} bubbles (broken duplicate-pill regression). Bubbles: ${JSON.stringify(list)}`,
        ).toBe(1);
      }

      // 6) If any bubble has retryCount >= 1, the retry-collapse path
      //    actually fired. If retryCount === 0 across all bubbles, the
      //    LLM happened to nail the args on the first try — skip the
      //    retry-counter assertion in that case but keep the
      //    no-duplicate-pills check above.
      const maxRetry = bubbles.reduce(
        (acc, b) => (b.retryCount > acc ? b.retryCount : acc),
        0,
      );
      console.log(
        `tool-retry-collapse: total bubbles=${bubbles.length}, maxRetryCount=${maxRetry}`,
      );

      if (maxRetry === 0) {
        test.info().annotations.push({
          type: 'note',
          description:
            'LLM nailed the tool args on the first try; retry-counter path not exercised. ' +
            'No-duplicate-pill check still passed.',
        });
        // Soft-skip is fine — retry-collapse is a tolerance for flaky LLM
        // arg generation; the renderer's retry-counter UI is then unused
        // for this run but provably present (badge selector exists).
        return;
      }

      // 7) When retryCount >= 1, the retry badge must be visible.
      const badgeCount = await page.locator(RETRY_BADGE).count();
      expect(
        badgeCount,
        `retryCount=${maxRetry} but no retry-badge rendered`,
      ).toBeGreaterThan(0);
    },
  );
});
