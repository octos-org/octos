/**
 * M8.10 stress test: thread_id binding under realistic 3-5 user overflow.
 *
 * Existing live-thread-interleave.spec.ts only sends 2 messages with a
 * fixed 1.5s gap. Real users (issue #649) send 3-5 messages with random
 * gaps and at least one slow tool. The narrow scope kept letting
 * variations of the thread_id binding bug ship to production
 * (#629 -> #635 -> #637 -> #649).
 *
 * Each scenario sends multiple messages within a short window, waits for
 * all responses to land, then verifies that the assistant response BELOW
 * each user bubble in DOM order matches the prompt by content.
 *
 * Required env:
 *   OCTOS_TEST_URL=https://dspfac.octos.ominix.io   (mini3, pre-#649)
 *   OCTOS_AUTH_TOKEN=octos-admin-2026
 *   OCTOS_PROFILE=dspfac
 *
 * Behind the same v2 flag as live-thread-interleave: the new thread-by-cmid
 * renderer is gated by `localStorage.octos_thread_store_v2 = '1'`.
 *
 * NEVER point at mini5 — that host is reserved for coding-green tests.
 *
 * EXPECTED behavior:
 *   - On daemons without #649 fix: at least one scenario (typically
 *     `slow-research-then-fast-questions`) FAILS, because a late-arriving
 *     slow tool result gets bound to the wrong thread.
 *   - After #649 deploys: all scenarios PASS. This becomes a regression
 *     check for the entire 3-5 user overflow class.
 *
 * Tracking: M8.10 follow-up issue #654 (generative property tests).
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

interface ScenarioMessage {
  gap_ms: number;
  text: string;
  expected_in_response: string[];
}

interface Scenario {
  name: string;
  messages: ScenarioMessage[];
  timeout_ms: number;
}

const SCENARIOS: Scenario[] = [
  {
    name: 'slow-research-then-fast-questions',
    messages: [
      {
        gap_ms: 0,
        text:
          'Use deep research to find latest Rust news. Run pipeline directly. One paragraph.',
        expected_in_response: ['rust', 'research', 'language', 'news'],
      },
      {
        gap_ms: 5000,
        text: 'What is 1 + 1?',
        expected_in_response: ['2', '两', '二'],
      },
      {
        gap_ms: 4000,
        text: '你有哪些内置语音',
        expected_in_response: ['vivian', 'serena', 'voice', '语音', 'tts'],
      },
    ],
    timeout_ms: 240_000,
  },
  {
    name: 'three-fast-then-one-slow',
    messages: [
      {
        gap_ms: 0,
        text: '今天日期是多少？',
        expected_in_response: ['2026', 'date', '日期', '月', 'april'],
      },
      {
        gap_ms: 2000,
        text: '北京天气怎么样？',
        expected_in_response: ['beijing', '北京', 'weather', '天气', 'temperature'],
      },
      {
        gap_ms: 1500,
        text: '搜索一下今日Rust相关新闻 (deep search)',
        expected_in_response: ['rust', 'news', '新闻', 'language'],
      },
    ],
    timeout_ms: 300_000,
  },
  {
    name: 'rapid-fire-five-fast',
    messages: [
      { gap_ms: 0, text: '1+1 = ?', expected_in_response: ['2', '两', '二'] },
      { gap_ms: 800, text: '2+2 = ?', expected_in_response: ['4', '四'] },
      { gap_ms: 800, text: '3+3 = ?', expected_in_response: ['6', '六'] },
      { gap_ms: 800, text: '4+4 = ?', expected_in_response: ['8', '八'] },
      { gap_ms: 800, text: '5+5 = ?', expected_in_response: ['10', '十'] },
    ],
    timeout_ms: 180_000,
  },
];

async function enableThreadStoreV2(page: Page) {
  await page.addInitScript((key) => {
    localStorage.setItem(key, '1');
  }, FLAG_KEY);
}

interface OrderedBubble {
  index: number;
  role: 'user' | 'assistant';
  threadId: string;
  text: string;
}

async function getOrderedBubbles(page: Page): Promise<OrderedBubble[]> {
  return page.evaluate(() => {
    const nodes = document.querySelectorAll(
      "[data-testid='user-message'], [data-testid='assistant-message']",
    );
    return Array.from(nodes).map((el, i) => ({
      index: i,
      role:
        el.getAttribute('data-testid') === 'user-message'
          ? ('user' as const)
          : ('assistant' as const),
      threadId: el.getAttribute('data-thread-id') || '',
      text: ((el as HTMLElement).innerText || '').trim(),
    }));
  });
}

async function waitForAllAssistantsFilled(
  page: Page,
  expectedAssistantCount: number,
  maxWaitMs: number,
  label: string,
): Promise<number> {
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
        return text.length > 1;
      }).length;
    }, SEL.assistantMessage);

    if (filled >= expectedAssistantCount && !isStreaming) {
      stable += 1;
      if (stable >= 2) return filled;
    } else {
      stable = 0;
    }

    if (filled !== lastFilled) {
      const elapsed = ((Date.now() - start) / 1000).toFixed(0);
      console.log(
        `  [${label}] ${elapsed}s: filled=${filled}/${expectedAssistantCount} streaming=${isStreaming}`,
      );
      lastFilled = filled;
    }
    await page.waitForTimeout(3_000);
  }
  return lastFilled;
}

function assertPairing(
  scenario: Scenario,
  newBubbles: OrderedBubble[],
): { violations: string[]; debug: string } {
  const violations: string[] = [];

  // Walk the bubble list, picking out user bubbles in order.
  const userIndices: number[] = [];
  for (let i = 0; i < newBubbles.length; i++) {
    if (newBubbles[i].role === 'user') userIndices.push(i);
  }

  if (userIndices.length < scenario.messages.length) {
    violations.push(
      `Expected ${scenario.messages.length} user bubbles, found ${userIndices.length}`,
    );
  }

  // Track thread-id collisions: each user bubble's paired assistant should
  // have a unique thread-id (when the renderer exposes it).
  const seenThreadIds = new Set<string>();

  for (let i = 0; i < scenario.messages.length; i++) {
    const userIdx = userIndices[i];
    if (userIdx === undefined) {
      violations.push(`User bubble ${i} not found in DOM`);
      continue;
    }
    const userBubble = newBubbles[userIdx];
    const userPrompt = scenario.messages[i].text;

    // Look at substrings to handle whitespace / formatting changes.
    const promptShort = userPrompt.slice(0, 12).replace(/\s+/g, '').toLowerCase();
    const userTextNorm = userBubble.text.replace(/\s+/g, '').toLowerCase();
    if (!userTextNorm.includes(promptShort.slice(0, 6))) {
      violations.push(
        `User bubble ${i} text "${userBubble.text.slice(0, 60)}" doesn't contain prompt prefix "${userPrompt.slice(0, 30)}"`,
      );
    }

    // Find the next assistant bubble after this user that has any text.
    // Stop scanning when we hit the next user bubble (that's a different
    // thread).
    const nextUserIdx =
      i + 1 < userIndices.length ? userIndices[i + 1] : newBubbles.length;
    let assistantBubble: OrderedBubble | undefined;
    for (let j = userIdx + 1; j < nextUserIdx; j++) {
      const b = newBubbles[j];
      if (b.role === 'assistant' && b.text.length > 0) {
        assistantBubble = b;
        break;
      }
    }

    if (!assistantBubble) {
      violations.push(
        `User ${i} ("${userPrompt.slice(0, 30)}"): no paired assistant response found between this user and the next user`,
      );
      continue;
    }

    // Track thread-id uniqueness when available.
    if (assistantBubble.threadId) {
      if (seenThreadIds.has(assistantBubble.threadId)) {
        violations.push(
          `User ${i}: assistant thread_id "${assistantBubble.threadId}" already used by an earlier thread (binding collision)`,
        );
      }
      seenThreadIds.add(assistantBubble.threadId);
    }

    // Content-match check: at least one expected marker must appear in the
    // assistant's text. Markers are lowercased for case-insensitive match.
    const assistantTextLower = assistantBubble.text.toLowerCase();
    const markers = scenario.messages[i].expected_in_response;
    const matched = markers.some((mark) =>
      assistantTextLower.includes(mark.toLowerCase()),
    );
    if (!matched) {
      violations.push(
        `User ${i} ("${userPrompt.slice(0, 30)}"): assistant response "${assistantBubble.text.slice(0, 120)}" does not contain any expected marker [${markers.join(', ')}]`,
      );
    }
  }

  const debug = JSON.stringify(
    newBubbles.map((b) => ({
      role: b.role,
      threadId: b.threadId,
      text: b.text.slice(0, 80),
    })),
    null,
    2,
  );
  return { violations, debug };
}

test.describe('M8.10 stress: thread_id binding under realistic 3-5 user overflow', () => {
  test.setTimeout(420_000);

  for (const scenario of SCENARIOS) {
    test(`stress scenario: ${scenario.name}`, async ({ page }) => {
      await enableThreadStoreV2(page);
      await login(page);
      await createNewSession(page);

      const userBefore = await countUserBubbles(page);
      const assistantBefore = await countAssistantBubbles(page);

      // Send every message with the configured gap in front of it.
      for (let i = 0; i < scenario.messages.length; i++) {
        const m = scenario.messages[i];
        if (m.gap_ms > 0) await page.waitForTimeout(m.gap_ms);
        await getInput(page).fill(m.text);
        await getSendButton(page).click();
        await expect
          .poll(() => countUserBubbles(page), { timeout: 30_000 })
          .toBe(userBefore + i + 1);
      }

      // Wait for all assistant bubbles to settle.
      const expectedAssistants = assistantBefore + scenario.messages.length;
      const filled = await waitForAllAssistantsFilled(
        page,
        expectedAssistants,
        scenario.timeout_ms,
        scenario.name,
      );
      expect(
        filled,
        `Only ${filled}/${expectedAssistants} assistant bubbles completed within ${scenario.timeout_ms / 1000}s`,
      ).toBeGreaterThanOrEqual(expectedAssistants);

      // Pull all bubbles from the DOM, slice off pre-existing ones, and
      // verify pairing against expected content per user prompt.
      const allBubbles = await getOrderedBubbles(page);
      const newBubbles = allBubbles.slice(userBefore + assistantBefore);
      const { violations, debug } = assertPairing(scenario, newBubbles);

      if (violations.length > 0) {
        console.log(`[${scenario.name}] DOM dump:`, debug);
      }
      expect(
        violations,
        `Pairing violations in scenario "${scenario.name}":\n  - ${violations.join('\n  - ')}\n\nDOM:\n${debug}`,
      ).toEqual([]);
    });
  }
});
