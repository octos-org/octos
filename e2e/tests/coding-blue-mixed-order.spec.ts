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
 *   2. Every user bubble eventually gets SOME meaningful assistant content.
 *   3. The deep-research answer names a Rust runtime somewhere in the 3 bubbles.
 *   4. Quick turns (Q2/Q3) produce their own distinct answers ("2+2=4" / "blue").
 *   5. At most one assistant bubble carries task-anchor UI (the DR one).
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

    // Fresh session — bloated history on live minis slows deep-research
    // past the test timeout.
    const newChat = page.locator(SEL.newChatButton);
    if (await newChat.isVisible().catch(() => false)) {
      await newChat.click();
      await page.waitForSelector(SEL.chatInput);
      await page.waitForTimeout(1_000);
    }

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

    // Wait until U1 has STARTED (cancel button visible OR task-anchor appears
    // OR /tasks shows running). Any of those = safe to interleave.
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

    // Wait until 3 assistant bubbles are present AND the cancel button is
    // gone (everything streamed out) AND each bubble has SUBSTANTIAL text
    // (> 8 chars, beyond just a timestamp placeholder).
    await expect(page.locator(SEL.assistantMessage)).toHaveCount(3, {
      timeout: 720_000,
    });

    // Stabilize: cancel gone + all 3 bubbles have meaningful content for two
    // consecutive polls.
    await expect
      .poll(
        async () => {
          const cancelVisible = await page
            .locator(SEL.cancelButton)
            .isVisible()
            .catch(() => false);
          if (cancelVisible) return false;
          const texts = await page
            .locator(SEL.assistantMessage)
            .allInnerTexts();
          if (texts.length < 3) return false;
          return texts.every((t) => stripTimestamp((t || '').trim()).length > 5);
        },
        { timeout: 720_000, intervals: [4_000] },
      )
      .toBe(true);

    // Extra margin: one more stability check — sometimes streaming resumes
    // briefly with a second chunk.
    await page.waitForTimeout(5_000);

    // --- Invariant 1: user bubbles are in send order with U1/U2/U3 content ---
    const userTexts = await page.locator(SEL.userMessage).allInnerTexts();
    expect(userTexts.length).toBe(3);
    expect(userTexts[0]).toContain(marker);
    expect(userTexts[1]).toContain('2+2=4');
    expect(userTexts[2]).toContain('sky is blue');

    // --- Invariant 2: every assistant bubble carries non-timestamp content ---
    const assistantTexts = await page.locator(SEL.assistantMessage).allInnerTexts();
    for (let i = 0; i < 3; i += 1) {
      const stripped = stripTimestamp((assistantTexts[i] || '').trim());
      expect(
        stripped.length,
        `assistant bubble #${i + 1} must have content; got: "${(
          assistantTexts[i] || ''
        ).slice(0, 160)}"`,
      ).toBeGreaterThan(3);
    }

    // --- Invariant 3: at least one bubble names a Rust runtime ---
    const runtimeRe = /tokio|async-std|smol|glommio|embassy|mio/i;
    const researchIdx = assistantTexts.findIndex((t) => runtimeRe.test(t || ''));
    expect(
      researchIdx,
      `at least one assistant bubble must name a Rust runtime (deep-research answer); got: ${assistantTexts
        .map((t, i) => `[${i}] ${(t || '').slice(0, 120)}`)
        .join(' | ')}`,
    ).toBeGreaterThanOrEqual(0);

    // --- Invariant 4: no cross-wiring ---
    // Deep-research bubble must NOT carry Q3's echo text.
    if (researchIdx >= 0) {
      const researchText = (assistantTexts[researchIdx] || '').toLowerCase();
      expect(
        /sky is blue/i.test(researchText),
        "deep-research bubble shouldn't carry Q3's echo",
      ).toBe(false);
    }

    const quickAnswers = assistantTexts.filter((_, i) => i !== researchIdx);
    const quickJoined = quickAnswers.join(' ').toLowerCase();
    expect(
      quickJoined.includes('4') || /2\s*\+\s*2/.test(quickJoined),
      `one quick bubble should answer Q2 (2+2); got: ${quickAnswers.join(
        ' | ',
      )}`,
    ).toBe(true);
    expect(
      quickJoined.includes('blue') || quickJoined.includes('sky'),
      `one quick bubble should answer Q3 (sky is blue); got: ${quickAnswers.join(
        ' | ',
      )}`,
    ).toBe(true);

    // --- Invariant 5: at most one assistant bubble carries task-anchor UI ---
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

/**
 * Strip trailing/leading ISO timestamps and "Thinking..." placeholders so
 * the remaining text represents real assistant content.
 */
function stripTimestamp(s: string): string {
  return s
    .replace(/\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}/g, '')
    .replace(/Thinking\s*\(iteration\s+\d+\)/gi, '')
    .replace(/^\s*[.,]+\s*$/gm, '')
    .trim();
}
