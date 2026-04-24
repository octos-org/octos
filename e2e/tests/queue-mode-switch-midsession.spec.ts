/**
 * Suite 8 — Queue-mode switching mid-session.
 *
 * Start in followup, fire Q1. While Q1 is running, send `/queue
 * speculative`. Fire Q2 — new mode applies. Q1's assistant bubble must
 * be unchanged; Q2 follows speculative semantics. Send `/queue followup`
 * again and Q3 is followup.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/queue-mode-switch-midsession.spec.ts
 */
import { expect, test } from '@playwright/test';

import {
  SEL,
  countAssistantBubbles,
  countUserBubbles,
  createNewSession,
  getInput,
  getSendButton,
  login,
} from './live-browser-helpers';
import {
  buildEchoShellPrompt,
  resetQueueMode,
  setQueueMode,
  waitForAllAssistantsContent,
} from './matrix-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL;

test.describe('Suite 8 queue-mode-switch-midsession', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(420_000);

  test.afterEach(async ({ page }) => {
    await resetQueueMode(page);
  });

  test('mode switches mid-session apply to subsequent turns only', async ({
    page,
  }) => {
    // Mid-session switch behavior around speculative still depends on
    // FA-12 + M7.9. Gated fixme until those land.
    test.fixme(
      true,
      'Mid-session queue mode switching pending FA-12 + M7.9 landing',
    );

    await login(page);
    await createNewSession(page);

    // Q1 under followup.
    await setQueueMode(page, 'followup');
    const marker1 = `SW-Q1-${Date.now()}`;
    await getInput(page).fill(buildEchoShellPrompt(marker1));
    await getSendButton(page).click();

    // Wait for Q1's assistant bubble to materialize, then snapshot it.
    await waitForAllAssistantsContent(page, 1, 180_000);
    const q1Texts = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);
    const q1Snapshot = q1Texts.find((text) => text.includes(marker1)) || '';
    expect(q1Snapshot).toContain(marker1);

    // Switch to speculative mid-session.
    const { badgeText: badgeSpec } = await setQueueMode(page, 'speculative');
    expect(badgeSpec.toLowerCase()).toMatch(/spec|queue/);

    // Q2 under speculative.
    const marker2 = `SW-Q2-${Date.now() + 1}`;
    await getInput(page).fill(buildEchoShellPrompt(marker2));
    await getSendButton(page).click();
    await waitForAllAssistantsContent(page, 2, 180_000);

    // Q1's text must not have changed when Q2 arrived.
    const midTexts = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);
    const q1Still = midTexts.find((text) => text.includes(marker1)) || '';
    expect(q1Still).toContain(marker1);
    expect(q1Still).not.toContain(marker2);

    // Switch back to followup for Q3.
    const { badgeText: badgeFu } = await setQueueMode(page, 'followup');
    expect(badgeFu.toLowerCase()).toMatch(/followup|queue/);

    const marker3 = `SW-Q3-${Date.now() + 2}`;
    await getInput(page).fill(buildEchoShellPrompt(marker3));
    await getSendButton(page).click();
    await waitForAllAssistantsContent(page, 3, 180_000);

    const user = await countUserBubbles(page);
    const assistant = await countAssistantBubbles(page);
    expect(user).toBe(3);
    expect(assistant).toBeGreaterThanOrEqual(3);

    const finalTexts = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);
    const combined = finalTexts.join('\n');
    expect(combined).toContain(marker1);
    expect(combined).toContain(marker2);
    expect(combined).toContain(marker3);

    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    expect(streaming).toBe(false);
  });
});
