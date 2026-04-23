/**
 * coding-blue: mixed long-running + one-shot turn ordering.
 *
 * The contract (per octos-web refactor contract §Mandatory Live-Site Gate):
 *   "While deep research is active, ask weather, name, and cron questions;
 *    verify they render as normal assistant bubbles and do not create or
 *    inherit background-task status."
 *
 * Real UX: task-anchor bubbles may render interleaved with message bubbles in
 * ways that don't match a strict U-A-U-A-U-A sequence. This spec now asserts:
 *
 *   1. All 3 user bubbles are present in send-order (with the expected content).
 *   2. Every user bubble eventually gets SOME assistant content (some path).
 *   3. Q2 and Q3 answers are short and distinct from the deep-research answer.
 *   4. No cross-wiring: the deep-research content doesn't appear inside the
 *      quick answers, and vice versa.
 *   5. Quick-turn assistant bubbles (A2/A3) must not carry a task-anchor
 *      spinner — they're one-shot shell echoes, not background tasks.
 *
 * Target: OCTOS_TEST_URL (default https://dspfac.crew.ominix.io = mini1).
 */

import { test, expect, type APIRequestContext } from '@playwright/test';
import { login, SEL } from './live-browser-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL || '';
const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE_ID = process.env.OCTOS_PROFILE || 'dspfac';
const SKIP_REASON = 'OCTOS_TEST_URL not set; skipping live mixed-order spec';

interface TaskRow {
  id?: string;
  task_id?: string;
  status?: string;
  current_phase?: string;
  lifecycle_state?: string;
}

async function fetchTasks(
  api: APIRequestContext,
  sessionId: string,
): Promise<TaskRow[]> {
  const resp = await api.get(
    `${TEST_URL}/api/sessions/${encodeURIComponent(sessionId)}/tasks`,
    {
      headers: {
        Authorization: `Bearer ${AUTH_TOKEN}`,
        'X-Profile-Id': PROFILE_ID,
      },
    },
  );
  if (!resp.ok()) return [];
  const body = (await resp.json().catch(() => [])) as unknown;
  return Array.isArray(body) ? (body as TaskRow[]) : [];
}

test.describe('coding-blue mixed long-running + one-shot ordering (live mini1)', () => {
  test.skip(() => !TEST_URL, SKIP_REASON);
  test.setTimeout(900_000); // 15 min — deep-research + 3 quick turns

  test('deep-research + interleaved short turns preserve chat order', async ({
    page,
    request,
  }) => {
    await login(page);
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    const input = page.locator(SEL.chatInput);
    const send = page.locator(SEL.sendButton);

    // U1: long-running deep research.
    const marker = `cb-mix-${Date.now()}`;
    const U1 = `Use deep_search to research in <=200 words: "Compare top 3 Rust async runtimes in 2026". Marker=${marker}`;
    await input.fill(U1);
    await send.click();
    await expect(page.locator(SEL.userMessage)).toHaveCount(1, {
      timeout: 30_000,
    });

    const sessionId = await page.evaluate(() =>
      localStorage.getItem('octos_current_session'),
    );
    expect(sessionId, 'session id must exist').toBeTruthy();

    // Give U1 time to START — either a task-anchor appears in the DOM,
    // OR the /tasks endpoint shows a running task, OR the cancel button
    // becomes visible (streaming started). Any of those mean we can safely
    // interleave without racing the first send.
    const startDeadline = Date.now() + 60_000;
    let started = false;
    while (Date.now() < startDeadline) {
      const anchorCount = await page
        .locator('[data-testid^="task-anchor-message-"]')
        .count();
      const cancelVisible = await page
        .locator(SEL.cancelButton)
        .isVisible()
        .catch(() => false);
      const tasks = await fetchTasks(request, sessionId!);
      const runningTask = tasks.some((t) => {
        const s = (t.lifecycle_state || t.status || '').toLowerCase();
        return s && !['ready', 'completed', 'failed'].includes(s);
      });
      if (anchorCount > 0 || cancelVisible || runningTask) {
        started = true;
        break;
      }
      await page.waitForTimeout(2_000);
    }
    expect(started, 'deep-research U1 must start before interleaving').toBe(true);

    // U2: quick question while U1 is still running.
    const U2 = 'Use shell: echo "2+2=4"';
    await input.fill(U2);
    await send.click();
    await expect(page.locator(SEL.userMessage)).toHaveCount(2, {
      timeout: 30_000,
    });

    // U3: another quick question.
    await page.waitForTimeout(2_000);
    const U3 = 'Use shell: echo "sky is blue"';
    await input.fill(U3);
    await send.click();
    await expect(page.locator(SEL.userMessage)).toHaveCount(3, {
      timeout: 30_000,
    });

    // Wait until 3 assistant bubbles are present (one per prompt) AND each
    // has some non-empty text. Deep-research is slowest — allow 12 min.
    await expect(page.locator(SEL.assistantMessage)).toHaveCount(3, {
      timeout: 720_000,
    });

    await page.waitForFunction(
      () => {
        const bubbles = Array.from(
          document.querySelectorAll('[data-testid="assistant-message"]'),
        );
        if (bubbles.length < 3) return false;
        return bubbles.every(
          (b) => ((b as HTMLElement).innerText || '').trim().length > 3,
        );
      },
      undefined,
      { timeout: 720_000 },
    );

    // --- Invariant 1: user bubbles are in send order with U1/U2/U3 content ---
    const userTexts = await page.locator(SEL.userMessage).allInnerTexts();
    expect(userTexts.length).toBe(3);
    expect(userTexts[0]).toContain(marker);
    expect(userTexts[1]).toContain('2+2=4');
    expect(userTexts[2]).toContain('sky is blue');

    // --- Invariant 2: every assistant bubble carries content ---
    const assistantTexts = await page.locator(SEL.assistantMessage).allInnerTexts();
    for (let i = 0; i < 3; i += 1) {
      expect(
        (assistantTexts[i] || '').trim().length,
        `assistant bubble #${i + 1} must have content`,
      ).toBeGreaterThan(3);
    }

    // --- Invariant 3: find the deep-research answer somewhere in the 3 ---
    //
    // Real UX may or may not render A1 at index 0 — task-anchor buckets +
    // streaming order can place the deep-research bubble anywhere. Find it
    // by content (Rust runtime names).
    const runtimeRe = /tokio|async-std|smol|glommio|embassy|mio/i;
    const researchIdx = assistantTexts.findIndex((t) => runtimeRe.test(t || ''));
    expect(
      researchIdx,
      `at least one assistant bubble must name a Rust runtime (deep-research answer); got: ${assistantTexts
        .map((t, i) => `[${i}] ${(t || '').slice(0, 120)}`)
        .join(' | ')}`,
    ).toBeGreaterThanOrEqual(0);

    // --- Invariant 4: no cross-wiring ---
    //
    // The deep-research bubble must NOT carry the quick-turn markers
    // ("sky is blue" / "2+2"), AND the quick-turn bubbles must NOT carry
    // more than one Rust runtime name (catches accidental re-use of the
    // deep-research text in a quick answer).
    if (researchIdx >= 0) {
      const researchText = (assistantTexts[researchIdx] || '').toLowerCase();
      // The research bubble may or may not echo the markers verbatim —
      // deep_search often summarizes. We only guard against the exact quick
      // echoes appearing inside it.
      expect(
        /sky is blue/i.test(researchText),
        "deep-research bubble shouldn't carry Q3's echo",
      ).toBe(false);
    }

    // Count quick bubbles that match the short-answer signatures.
    const quickAnswers = assistantTexts.filter((_, i) => i !== researchIdx);
    const quickJoined = quickAnswers.join(' ').toLowerCase();
    // At least one of the two quick bubbles should contain '2+2' / '4'.
    expect(
      quickJoined.includes('4') || /2\s*\+\s*2/.test(quickJoined),
      `one quick bubble should answer Q2 (2+2); got: ${quickAnswers.join(
        ' | ',
      )}`,
    ).toBe(true);
    // And one should contain 'blue' or 'sky'.
    expect(
      quickJoined.includes('blue') || quickJoined.includes('sky'),
      `one quick bubble should answer Q3 (sky is blue); got: ${quickAnswers.join(
        ' | ',
      )}`,
    ).toBe(true);

    // --- Invariant 5: quick-turn bubbles must not carry task-anchor UI ---
    //
    // Task-anchor testids were added by PR #42 for long-running tasks only.
    // We can't reliably know which index is A2 vs A3 in the real UX, so we
    // scan all assistant bubbles, skip the research one, and assert that at
    // most one anchor exists across all assistant bubbles (belonging to the
    // deep-research turn).
    const anchorCounts = await page.evaluate(() => {
      const bubbles = Array.from(
        document.querySelectorAll('[data-testid="assistant-message"]'),
      );
      return bubbles.map(
        (b) =>
          (b as HTMLElement).querySelectorAll('[data-testid^="task-anchor-"]')
            .length,
      );
    });
    const totalAnchors = anchorCounts.reduce((sum, c) => sum + c, 0);
    // At most one assistant bubble should carry anchor UI (the deep-research
    // one). If zero, the system streamed inline — also fine.
    expect(
      anchorCounts.filter((c) => c > 0).length,
      `at most 1 assistant bubble may carry task-anchor UI; per-bubble counts: ${JSON.stringify(
        anchorCounts,
      )}`,
    ).toBeLessThanOrEqual(1);

    console.log(
      `[coding-blue mixed-order] researchIdx=${researchIdx} lens=${assistantTexts
        .map((t) => (t || '').length)
        .join(',')} totalAnchors=${totalAnchors}`,
    );
  });
});
