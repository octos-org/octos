/**
 * Suite 2 — Rapid consecutive chats.
 *
 * Fire 5 prompts over 3 seconds (500ms apart) under the default followup
 * queue mode. Assert all 5 user bubbles render in send order, at least
 * 3 of 5 get distinct assistant replies within 3 min, and no bubble is
 * empty after all streams close.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/rapid-consecutive-chats.spec.ts
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

test.describe('Suite 2 rapid consecutive chats', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(360_000);

  test.afterEach(async ({ page }) => {
    await resetQueueMode(page);
  });

  test('5 rapid prompts all land as ordered user bubbles with >=3 answers', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);
    await setQueueMode(page, 'followup');

    const markers = [0, 1, 2, 3, 4].map((i) => `RAPID-${i}-${Date.now()}`);
    const prompts = markers.map((marker) => buildEchoShellPrompt(marker));

    await fireRapidPrompts(page, prompts, 500);

    // Wait up to 3 minutes for at least 3 non-empty assistant bubbles.
    const filled = await waitForAllAssistantsContent(page, 3, 180_000);
    expect(filled).toBeGreaterThanOrEqual(3);

    const { user, assistant } = await countBubbles(page);
    expect(user).toBe(5);
    expect(assistant).toBeGreaterThanOrEqual(3);

    // Send order: the five user bubbles must carry the five markers
    // in the order they were sent.
    const userTexts = await page
      .locator(SEL.userMessage)
      .allTextContents()
      .catch(() => []);
    expect(userTexts).toHaveLength(5);
    for (let i = 0; i < markers.length; i += 1) {
      expect(userTexts[i], `user bubble ${i} missing ${markers[i]}`).toContain(markers[i]);
    }

    // No assistant bubble may be empty after streams close. We permit
    // "short" placeholders (e.g. whitespace/emoji-only), but zero-length
    // text is a regression.
    const assistantTexts = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);
    for (const [idx, text] of assistantTexts.entries()) {
      expect(text.trim().length, `assistant bubble ${idx} is empty`).toBeGreaterThan(0);
    }

    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    expect(streaming).toBe(false);
  });
});
