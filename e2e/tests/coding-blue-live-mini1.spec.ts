/**
 * Live smoke: hit mini1's deployed coding-blue bundle end-to-end in a browser.
 *
 * Target: dspfac.crew.ominix.io (= mini1). Uses the deployed coding-blue bundle
 * at the site root.
 *
 * Asserts:
 *   1. App loads and chat input is visible after login.
 *   2. A single user prompt gets a non-empty assistant response.
 *   3. Two rapid prompts both produce assistant responses (in-order case).
 *
 * This is the regression guard running against real infrastructure, not a
 * mocked runtime. Gated by OCTOS_TEST_URL so it's a no-op in CI without
 * an explicit target.
 *
 * Env:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io
 *   OCTOS_AUTH_TOKEN=octos-admin-2026
 *   OCTOS_PROFILE=dspfac
 *
 * Run:
 *   cd e2e && OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *     npx playwright test tests/coding-blue-live-mini1.spec.ts --project=chromium
 */

import { test, expect } from '@playwright/test';
import { login, SEL } from './live-browser-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL || '';
const SKIP_REASON = 'OCTOS_TEST_URL not set; skipping live mini1 smoke';

test.describe('coding-blue live smoke (mini1)', () => {
  test.skip(() => !TEST_URL, SKIP_REASON);
  test.slow();
  test.setTimeout(300_000);

  test('renders chat UI and single prompt gets a response', async ({ page }) => {
    await login(page);

    // Fresh session via new-chat.
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    const input = page.locator(SEL.chatInput);
    const send = page.locator(SEL.sendButton);

    await input.fill('Answer in one short sentence: What is 17 plus 25?');
    await send.click();

    // Assistant bubble appears and has non-empty content within 90s.
    const assistant = page.locator(SEL.assistantMessage).first();
    await expect(assistant).toBeVisible({ timeout: 90_000 });

    await expect
      .poll(async () => (await assistant.innerText()).trim().length, {
        timeout: 60_000,
        intervals: [2_000],
      })
      .toBeGreaterThan(0);

    const text = (await assistant.innerText()).trim();
    expect(text.length).toBeGreaterThan(0);
    // Very lax sanity — don't pin to model-specific wording.
    const hasExpectedAnswer = text.includes('42') || /forty[\s-]?two/i.test(text);
    if (!hasExpectedAnswer) {
      console.warn(
        `coding-blue-live: assistant answered but without "42": ${text.slice(0, 120)}`,
      );
    }
  });

  test('two rapid prompts in-order both get responses (regression guard)', async ({
    page,
  }) => {
    await login(page);

    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    // Force a brand-new session so we don't inherit history.
    const newChat = page.locator(SEL.newChatButton);
    if (await newChat.isVisible().catch(() => false)) {
      await newChat.click();
      await page.waitForSelector(SEL.chatInput);
    }

    const input = page.locator(SEL.chatInput);
    const send = page.locator(SEL.sendButton);

    // Q1
    await input.fill(
      'Answer in one short sentence: list top 3 programming languages in 2026.',
    );
    await send.click();

    // Q2 ~500ms later, without waiting for Q1 to finish.
    await page.waitForTimeout(500);
    await input.fill(
      'Answer in one short sentence: list top 3 databases in 2026.',
    );
    await send.click();

    // Both user bubbles up immediately.
    await expect(page.locator(SEL.userMessage)).toHaveCount(2, {
      timeout: 20_000,
    });

    // Both assistants land within 3 minutes (real LLM latency, may be slow).
    await expect(page.locator(SEL.assistantMessage)).toHaveCount(2, {
      timeout: 180_000,
    });

    // Both non-empty.
    const bubbles = page.locator(SEL.assistantMessage);
    await expect
      .poll(async () => (await bubbles.nth(0).innerText()).trim().length, {
        timeout: 30_000,
        intervals: [2_000],
      })
      .toBeGreaterThan(0);
    await expect
      .poll(async () => (await bubbles.nth(1).innerText()).trim().length, {
        timeout: 30_000,
        intervals: [2_000],
      })
      .toBeGreaterThan(0);

    const t1 = (await bubbles.nth(0).innerText()).trim();
    const t2 = (await bubbles.nth(1).innerText()).trim();
    expect(t1).not.toBe(t2);
    expect(t1.length).toBeGreaterThan(10);
    expect(t2.length).toBeGreaterThan(10);
  });
});
