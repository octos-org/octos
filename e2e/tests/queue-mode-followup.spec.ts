/**
 * Suite 1 — Queue-mode matrix (1/5): followup.
 *
 * Contract: Q2 waits for Q1 to finish. Strict interleave in the DOM is
 * user -> assistant -> user -> assistant. Both user prompts land, both
 * assistant bubbles land, both markers appear in the assistant output.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/queue-mode-followup.spec.ts
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
  expectAssistantsContainAll,
  fireRapidPrompts,
  resetQueueMode,
  setQueueMode,
  waitForAllAssistantsContent,
} from './matrix-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL;

test.describe('Suite 1 queue-mode-followup', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(360_000);

  test.afterEach(async ({ page }) => {
    await resetQueueMode(page);
  });

  test('followup preserves strict user->assistant->user->assistant interleave', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    const { badgeText, feedbackText } = await setQueueMode(page, 'followup');
    const ack = `${feedbackText}\n${badgeText}`.toLowerCase();
    expect(ack).toMatch(/followup|queue/);

    const markerA = `FOLLOWUP-Q1-${Date.now()}`;
    const markerB = `FOLLOWUP-Q2-${Date.now() + 1}`;
    await fireRapidPrompts(
      page,
      [buildEchoShellPrompt(markerA), buildEchoShellPrompt(markerB)],
      500,
    );

    const filled = await waitForAllAssistantsContent(page, 2, 240_000);
    expect(filled).toBeGreaterThanOrEqual(2);

    const { user, assistant } = await countBubbles(page);
    expect(user).toBe(2);
    expect(assistant).toBeGreaterThanOrEqual(2);

    // Strict ordering: at the start of the thread, user #0 must come before
    // user #1 and at least one assistant should sit between them (followup
    // serializes the turns).
    const roles = await page.evaluate(() => {
      const nodes = document.querySelectorAll(
        "[data-testid='user-message'], [data-testid='assistant-message']",
      );
      return Array.from(nodes).map((node) =>
        node.getAttribute('data-testid')?.includes('user') ? 'user' : 'assistant',
      );
    });

    const userIdx = roles
      .map((role, idx) => (role === 'user' ? idx : -1))
      .filter((idx) => idx >= 0);
    expect(userIdx.length).toBe(2);
    expect(userIdx[1] - userIdx[0]).toBeGreaterThanOrEqual(2);

    // Both markers must appear in assistant output somewhere.
    await expectAssistantsContainAll(page, [markerA, markerB]);

    // Streaming must be finished by now.
    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    expect(streaming).toBe(false);
  });
});
