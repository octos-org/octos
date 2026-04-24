/**
 * Suite 1 — Queue-mode matrix (4/5): interrupt.
 *
 * Contract: Q2 aborts Q1 immediately. Q1's assistant bubble should carry
 * an interrupted/cancelled marker (content or status); Q2 proceeds fresh
 * and its marker lands on the final bubble.
 *
 * NOTE: The interrupt marker format is still in flight (FA-11 through
 * M7.9). We verify the structural contract now — Q2 wins the final
 * answer — and mark the strict "interrupted label" check fixme.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/queue-mode-interrupt.spec.ts
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

test.describe('Suite 1 queue-mode-interrupt', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(360_000);

  test.afterEach(async ({ page }) => {
    await resetQueueMode(page);
  });

  test('interrupt aborts Q1 and lands Q2 as the final answer', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    const { badgeText, feedbackText } = await setQueueMode(page, 'interrupt');
    const ack = `${feedbackText}\n${badgeText}`.toLowerCase();
    expect(ack).toMatch(/interrupt|queue/);

    const markerA = `INTERRUPT-Q1-${Date.now()}`;
    const markerB = `INTERRUPT-Q2-${Date.now() + 1}`;

    // Q1 is deliberately long so Q2's interrupt actually needs to preempt.
    const q1 = [
      `${buildEchoShellPrompt(markerA)}`,
      'After echoing, run `sleep 30` from the repo root so the turn stays live.',
    ].join(' ');

    await fireRapidPrompts(page, [q1, buildEchoShellPrompt(markerB)], 1_500);

    await waitForAllAssistantsContent(page, 1, 180_000);
    await page.waitForTimeout(3_000);

    const { user, assistant } = await countBubbles(page);
    expect(user).toBe(2);
    expect(assistant).toBeGreaterThanOrEqual(1);

    const assistantTexts = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);
    const lastText = assistantTexts[assistantTexts.length - 1] || '';

    // The final bubble must contain Q2's marker — that's the hard
    // contract of interrupt mode.
    expect(lastText).toContain(markerB);

    // Note: a stricter "earliest assistant bubble is labelled as
    // cancelled/interrupted" check is intentionally NOT asserted here.
    // The UI label format is still settling (M7.9 track); once finalized
    // the next iteration of this spec will add a per-bubble status check
    // via data-message-status=cancelled or similar.

    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    expect(streaming).toBe(false);
  });
});
