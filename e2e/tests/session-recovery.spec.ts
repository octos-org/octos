/**
 * Repo-level live browser recovery acceptance coverage.
 *
 * These cases extend the smoke/capability suites by proving:
 * - double reload during an active stream still collapses into one final turn
 * - concurrent browser sessions stay isolated after reload
 * - returning to an earlier session after reload restores the correct history
 *
 * Run against a live deployment:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/session-recovery.spec.ts
 */
import { expect, test, type Browser, type Page } from '@playwright/test';
import {
  SEL,
  countAssistantBubbles,
  countUserBubbles,
  createNewSession,
  getChatThreadText,
  getInput,
  getSendButton,
  login,
  sendAndWait,
} from './live-browser-helpers';

async function resetChat(page: Page) {
  await login(page);
  await createNewSession(page);
}

async function openAuthedChat(browser: Browser) {
  const context = await browser.newContext();
  const page = await context.newPage();
  await resetChat(page);
  return { context, page };
}

async function waitForRecoveredTurn(page: Page, timeoutMs = 240_000) {
  const deadline = Date.now() + timeoutMs;
  let lastAssistantCount = -1;
  let lastText = '';
  let stableCount = 0;

  while (Date.now() < deadline) {
    const assistantCount = await countAssistantBubbles(page);
    const userCount = await countUserBubbles(page);
    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    const text =
      assistantCount > 0
        ? ((await page
            .locator(SEL.assistantMessage)
            .last()
            .textContent()
            .catch(() => '')) || '').trim()
        : '';

    if (userCount === 1 && assistantCount > 0 && !streaming && text) {
      if (assistantCount === lastAssistantCount && text === lastText) {
        stableCount++;
        if (stableCount >= 2) {
          return text;
        }
      } else {
        stableCount = 0;
      }
    } else {
      stableCount = 0;
    }

    lastAssistantCount = assistantCount;
    lastText = text;
    await page.waitForTimeout(2_000);
  }

  throw new Error('Timed out waiting for the recovered turn to settle');
}

test.describe('Live session recovery', () => {
  test.setTimeout(360_000);

  test('reloading twice during an active stream resumes the same turn', async ({
    page,
  }) => {
    await resetChat(page);

    const marker = `RECONNECT-${Date.now()}`;
    await getInput(page).fill(
      `Write a detailed memo about reconnect storms and session recovery directly in chat. Do not use tools or write files. Include ${marker} exactly once near the end and keep the answer long enough to survive a couple of reloads.`,
    );
    await getSendButton(page).click();

    await page.waitForFunction(
      () =>
        document.querySelectorAll("[data-testid='assistant-message']").length > 0 &&
        document.querySelector("[data-testid='cancel-button']") !== null,
      undefined,
      { timeout: 30_000 },
    );

    await page.waitForTimeout(2_500);
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await page.waitForTimeout(1_500);
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });

    const finalText = await waitForRecoveredTurn(page);
    expect(finalText.length).toBeGreaterThan(0);
    expect(await countUserBubbles(page)).toBe(1);
    expect(await countAssistantBubbles(page)).toBeGreaterThanOrEqual(1);
    expect(finalText).toContain(marker);
  });

  test('concurrent live sessions stay isolated after independent reloads', async ({
    browser,
  }) => {
    const first = await openAuthedChat(browser);
    const second = await openAuthedChat(browser);

    try {
      const alpha = `ALPHA-${Date.now()}`;
      const beta = `BRAVO-${Date.now()}`;

      const [alphaResult, betaResult] = await Promise.all([
        sendAndWait(first.page, `Reply with exactly: ${alpha}`, {
          label: 'recovery-alpha',
          maxWait: 60_000,
        }),
        sendAndWait(second.page, `Reply with exactly: ${beta}`, {
          label: 'recovery-beta',
          maxWait: 60_000,
        }),
      ]);

      expect(alphaResult.responseLen).toBeGreaterThan(0);
      expect(betaResult.responseLen).toBeGreaterThan(0);

      await Promise.all([
        first.page.reload({ waitUntil: 'domcontentloaded' }),
        second.page.reload({ waitUntil: 'domcontentloaded' }),
      ]);
      await Promise.all([
        first.page.waitForSelector(SEL.chatInput, { timeout: 15_000 }),
        second.page.waitForSelector(SEL.chatInput, { timeout: 15_000 }),
      ]);
      await Promise.all([
        first.page.waitForTimeout(3_000),
        second.page.waitForTimeout(3_000),
      ]);

      const alphaText = await getChatThreadText(first.page);
      const betaText = await getChatThreadText(second.page);

      expect(alphaText).toContain(alpha);
      expect(alphaText).not.toContain(beta);
      expect(betaText).toContain(beta);
      expect(betaText).not.toContain(alpha);

      expect(await countUserBubbles(first.page)).toBe(1);
      expect(await countAssistantBubbles(first.page)).toBe(1);
      expect(await countUserBubbles(second.page)).toBe(1);
      expect(await countAssistantBubbles(second.page)).toBe(1);
    } finally {
      await Promise.all([first.context.close(), second.context.close()]);
    }
  });

  test('switching back to an earlier session after reload restores its history', async ({
    page,
  }) => {
    await resetChat(page);

    const alpha = `ALPHA-${Date.now()}`;
    const beta = `BRAVO-${Date.now()}`;

    const first = await sendAndWait(page, `Reply with exactly: ${alpha}`, {
      label: 'restore-alpha',
      maxWait: 60_000,
    });
    expect(first.responseLen).toBeGreaterThan(0);
    await page.waitForTimeout(2_000);

    const firstSessionId = await page
      .locator("[data-active='true']")
      .first()
      .getAttribute('data-session-id');
    expect(firstSessionId).toBeTruthy();

    await createNewSession(page);
    const second = await sendAndWait(page, `Reply with exactly: ${beta}`, {
      label: 'restore-beta',
      maxWait: 60_000,
    });
    expect(second.responseLen).toBeGreaterThan(0);
    await page.waitForTimeout(2_000);

    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await page.waitForTimeout(3_000);

    const currentText = await getChatThreadText(page);
    expect(currentText).toContain(beta);
    expect(currentText).not.toContain(alpha);
    expect(await countUserBubbles(page)).toBe(1);
    expect(await countAssistantBubbles(page)).toBe(1);

    const firstSessionButton = page.locator(
      `[data-session-id="${firstSessionId}"] [data-testid="session-switch-button"]`,
    );
    await firstSessionButton.waitFor({ state: 'visible', timeout: 15_000 });
    await firstSessionButton.click();
    await page.waitForTimeout(3_000);

    const restoredText = await getChatThreadText(page);
    expect(restoredText).toContain(alpha);
    expect(restoredText).not.toContain(beta);
    expect(await countUserBubbles(page)).toBe(1);
    expect(await countAssistantBubbles(page)).toBe(1);
  });
});
