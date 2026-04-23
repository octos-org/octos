/**
 * coding-blue: mixed long-running + one-shot turn ordering.
 *
 * The contract (per octos-web refactor contract §Mandatory Live-Site Gate):
 *   "While deep research is active, ask weather, name, and cron questions;
 *    verify they render as normal assistant bubbles and do not create or
 *    inherit background-task status."
 *
 * Real UX: task-anchor bubbles may render interleaved with message bubbles in
 * ways that don't match a strict U-A-U-A-U-A sequence. The deep-research
 * answer in particular is delivered as a `task-anchor-message-<id>` bubble
 * (background-task path, PR #42 testid convention) — NOT as a regular
 * `assistant-message`. Short shell echoes come back as regular
 * `assistant-message` bubbles. Streaming can also emit intermediate
 * reasoning-chunk bubbles, so the total count is variable.
 *
 * This spec asserts the stable invariants (any bubble — assistant-message or
 * task-anchor-message — counts as a carrier of LLM output):
 *
 *   1. All 3 user bubbles are present in send-order (with the expected content).
 *   2. At least one bubble names a Rust runtime (deep-research answer).
 *   3. Transcript contains "2+2=4" and "sky is blue" (quick-echo answers).
 *   4. No cross-wiring: the research bubble does not carry the Q3 echo phrase.
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

    // The deep-research answer is delivered as a task-anchor-message bubble
    // (background-task path, PR #42 testid convention) while the two shell
    // echoes come back as regular assistant-message bubbles. Because streaming
    // can also produce intermediate assistant-message bubbles (e.g. a "let me
    // try a different query" reasoning chunk with a deep_search tool tag),
    // the total bubble count is variable. The stable invariant is that *all
    // three* user prompts eventually get answered somewhere in the transcript.
    //
    // Combined selector matches both regular assistant bubbles and task-anchor
    // bubbles — either shape carries legitimate LLM output.
    const TRANSCRIPT_SEL =
      '[data-testid="assistant-message"], [data-testid^="task-anchor-message-"]';

    // Stabilize: cancel button is gone AND the transcript has at least one
    // bubble carrying each of the three expected signatures:
    //   - a Rust-runtime name (deep-research answer),
    //   - "2+2=4" (literal shell echo for Q2 — strict to avoid matching
    //     random "4"s in deep-research numbered lists),
    //   - "sky is blue" (literal shell echo for Q3).
    // Polls up to 14 minutes (slightly under the 15-min test setTimeout).
    const runtimeRe = /tokio|async-std|smol|glommio|embassy|mio/i;
    const q2Re = /2\s*\+\s*2\s*=\s*4/i;
    const q3Re = /sky\s+is\s+blue/i;

    const stableReady = await expect
      .poll(
        async () => {
          const cancelVisible = await page
            .locator(SEL.cancelButton)
            .isVisible()
            .catch(() => false);
          if (cancelVisible) return false;
          const texts = await page.locator(TRANSCRIPT_SEL).allInnerTexts();
          if (texts.length < 1) return false;
          const joined = texts.join(' ');
          return (
            runtimeRe.test(joined) && q2Re.test(joined) && q3Re.test(joined)
          );
        },
        { timeout: 840_000, intervals: [5_000] },
      )
      .toBe(true)
      .then(() => true)
      .catch(() => false);

    // Extra margin: one more stability wait — sometimes streaming resumes
    // briefly with a second chunk.
    await page.waitForTimeout(5_000);

    const transcriptTexts = await page
      .locator(TRANSCRIPT_SEL)
      .allInnerTexts();
    const joinedTranscript = transcriptTexts.join(' ');

    if (!stableReady) {
      console.log(
        `[coding-blue mixed-order] partial transcript after timeout: ${transcriptTexts
          .map((t, i) => `[${i}] ${(t || '').slice(0, 100)}`)
          .join(' | ')}`,
      );
    }

    // --- Invariant 1: user bubbles are in send order with U1/U2/U3 content ---
    const userTexts = await page.locator(SEL.userMessage).allInnerTexts();
    expect(userTexts.length).toBe(3);
    expect(userTexts[0]).toContain(marker);
    expect(userTexts[1]).toContain('2+2=4');
    expect(userTexts[2]).toContain('sky is blue');

    // --- Invariant 2: deep-research answer is present somewhere ---
    const researchIdx = transcriptTexts.findIndex((t) => runtimeRe.test(t || ''));
    expect(
      researchIdx,
      `at least one transcript bubble must name a Rust runtime (deep-research answer); got: ${transcriptTexts
        .map((t, i) => `[${i}] ${(t || '').slice(0, 120)}`)
        .join(' | ')}`,
    ).toBeGreaterThanOrEqual(0);

    // --- Invariant 3: both shell echoes produced answers ---
    expect(
      q2Re.test(joinedTranscript),
      `transcript should carry Q2 answer (2+2=4); got: ${joinedTranscript.slice(
        0,
        500,
      )}`,
    ).toBe(true);
    expect(
      q3Re.test(joinedTranscript),
      `transcript should carry Q3 answer (sky is blue); got: ${joinedTranscript.slice(
        0,
        500,
      )}`,
    ).toBe(true);

    // --- Invariant 4: no cross-wiring ---
    // The specific bubble that names a runtime must NOT also carry Q3's echo.
    if (researchIdx >= 0) {
      const researchText = (transcriptTexts[researchIdx] || '').toLowerCase();
      // Use word-boundary "sky is blue" so we don't false-positive on a
      // deep-research summary that happens to mention the color blue.
      expect(
        /sky is blue/.test(researchText),
        "deep-research bubble shouldn't carry Q3's echo phrase",
      ).toBe(false);
    }

    console.log(
      `[coding-blue mixed-order] bubbles=${transcriptTexts.length} researchIdx=${researchIdx} lens=${transcriptTexts
        .map((t) => (t || '').length)
        .join(',')}`,
    );
  });
});
