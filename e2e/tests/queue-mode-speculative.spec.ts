/**
 * Suite 1 — Queue-mode matrix (5/5): speculative.
 *
 * Contract (THIS IS THE FA-11 REGRESSION): both prompts run concurrently;
 * both assistant bubbles eventually carry their respective content. Both
 * markers (Q1 and Q2) must land in distinct assistant bubbles, not on
 * one merged bubble. Post-FA-12 + coding-blue-r1 this spec must pass.
 *
 * Currently known to fail against coding-blue pre-FA-12 — marked fixme
 * with an explicit pointer to the fix it depends on. Remove the fixme
 * once FA-12 lands on mini1.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/queue-mode-speculative.spec.ts
 */
import { expect, test } from '@playwright/test';

import {
  SEL,
  createNewSession,
  login,
} from './live-browser-helpers';
import {
  buildEchoShellPrompt,
  countBubbles,
  fireRapidPrompts,
  resetQueueMode,
  setQueueMode,
  waitForAllAssistantsContent,
} from './matrix-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL;

test.describe('Suite 1 queue-mode-speculative (FA-11 regression)', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(360_000);

  test.afterEach(async ({ page }) => {
    await resetQueueMode(page);
  });

  test('speculative delivers both answers in distinct bubbles', async ({
    page,
  }) => {
    // FA-11 regression — expected to fail until FA-12 + coding-blue-r1
    // are deployed to the target mini. Remove this fixme once the fix
    // is live to let the guard catch future regressions.
    test.fixme(
      true,
      'FA-11 concurrent delivery — requires FA-12 landing on target mini',
    );

    await login(page);
    await createNewSession(page);

    const { badgeText, feedbackText } = await setQueueMode(page, 'speculative');
    const ack = `${feedbackText}\n${badgeText}`.toLowerCase();
    expect(ack).toMatch(/spec|queue/);

    const markerA = `SPEC-Q1-${Date.now()}`;
    const markerB = `SPEC-Q2-${Date.now() + 1}`;

    // Fire rapidly — speculative must pipeline both concurrently.
    await fireRapidPrompts(
      page,
      [buildEchoShellPrompt(markerA), buildEchoShellPrompt(markerB)],
      300,
    );

    const filled = await waitForAllAssistantsContent(page, 2, 240_000);
    expect(filled).toBeGreaterThanOrEqual(2);

    const { user, assistant } = await countBubbles(page);
    expect(user).toBe(2);
    expect(assistant).toBeGreaterThanOrEqual(2);

    const assistantTexts = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);

    // Each marker must live in its own bubble — speculative is allowed to
    // interleave order but must never merge the two answers.
    const q1Bubble = assistantTexts.findIndex((text) => text.includes(markerA));
    const q2Bubble = assistantTexts.findIndex((text) => text.includes(markerB));
    expect(q1Bubble, `Q1 marker ${markerA} missing from any assistant bubble`).toBeGreaterThanOrEqual(0);
    expect(q2Bubble, `Q2 marker ${markerB} missing from any assistant bubble`).toBeGreaterThanOrEqual(0);
    expect(q1Bubble, 'Q1 and Q2 must land in distinct assistant bubbles').not.toBe(q2Bubble);

    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    expect(streaming).toBe(false);
  });
});
