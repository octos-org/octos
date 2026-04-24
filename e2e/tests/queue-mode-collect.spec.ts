/**
 * Suite 1 — Queue-mode matrix (2/5): collect.
 *
 * Contract: Q2 is batched with Q1 into one turn. One assistant bubble
 * answers both. The final bubble content must mention both markers.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/queue-mode-collect.spec.ts
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

test.describe('Suite 1 queue-mode-collect', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(360_000);

  test.afterEach(async ({ page }) => {
    await resetQueueMode(page);
  });

  test('collect batches rapid prompts into one assistant turn', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    const { badgeText, feedbackText } = await setQueueMode(page, 'collect');
    const ack = `${feedbackText}\n${badgeText}`.toLowerCase();
    expect(ack).toMatch(/collect|queue/);

    const markerA = `COLLECT-Q1-${Date.now()}`;
    const markerB = `COLLECT-Q2-${Date.now() + 1}`;
    await fireRapidPrompts(
      page,
      [buildEchoShellPrompt(markerA), buildEchoShellPrompt(markerB)],
      500,
    );

    // Wait for at least one rich assistant turn — collect may render 1 or 2
    // bubbles depending on backend fan-out, but the batched response must
    // name both markers.
    await waitForAllAssistantsContent(page, 1, 240_000);

    // Allow an extra grace window in case the backend is still finalizing.
    await page.waitForTimeout(4_000);

    const { user, assistant } = await countBubbles(page);
    expect(user).toBe(2);
    expect(assistant).toBeGreaterThanOrEqual(1);

    // Any assistant bubble in the thread must contain both markers so the
    // batch is provably collected into one answer.
    const assistantTexts = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);
    const batched = assistantTexts.find(
      (text) => text.includes(markerA) && text.includes(markerB),
    );
    expect(batched, 'collect mode must produce one bubble containing both markers').toBeTruthy();

    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    expect(streaming).toBe(false);
  });
});
