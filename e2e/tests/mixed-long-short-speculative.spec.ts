/**
 * Suite 3 — Long-running + one-shot mix (speculative queue mode).
 *
 * Same scenario as mixed-long-short-followup but speculative: the deep
 * research runs async while Q2/Q3 quick echoes fire concurrently.
 * Post-FA-12 all 3 must land content.
 *
 * Currently expected to fail pre-FA-12 — marked fixme until coding-blue-r1
 * deploys FA-12 + M7.9 speculative fix to the target mini.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/mixed-long-short-speculative.spec.ts
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

const LONG_RESEARCH_PROMPT = [
  'Use shell tool only.',
  'If shell is not already active, call activate_tools with exactly ["shell"] once.',
  'Run `echo RESEARCH-START && for i in 1 2 3 4 5; do sleep 2; echo tick-$i; done && echo RESEARCH-DONE`',
  'then return a short summary mentioning RESEARCH-DONE.',
  'Do not start background work.',
].join(' ');

test.describe('Suite 3 mixed-long-short speculative', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(540_000);

  test.afterEach(async ({ page }) => {
    await resetQueueMode(page);
  });

  test('speculative runs long research concurrent with two quick echoes', async ({
    page,
  }) => {
    // Concurrent delivery of A/B/C under speculative is the exact FA-11
    // shape: three independent streams each need their own final bubble.
    // Remove fixme when FA-12 + M7.9 land on mini1.
    test.fixme(
      true,
      'Concurrent mixed delivery pending FA-12 + M7.9 landing on target mini',
    );

    await login(page);
    await createNewSession(page);
    await setQueueMode(page, 'speculative');

    const markerB = `MIXSP-B-${Date.now()}`;
    const markerC = `MIXSP-C-${Date.now() + 1}`;
    const prompts = [
      LONG_RESEARCH_PROMPT,
      buildEchoShellPrompt(markerB),
      buildEchoShellPrompt(markerC),
    ];

    await fireRapidPrompts(page, prompts, 300);

    const filled = await waitForAllAssistantsContent(page, 3, 480_000);
    expect(filled).toBeGreaterThanOrEqual(3);

    const { user, assistant } = await countBubbles(page);
    expect(user).toBe(3);
    expect(assistant).toBeGreaterThanOrEqual(3);

    const assistantTexts = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);

    // All three markers must exist in distinct bubbles; speculative is
    // allowed to deliver them out of order but never merged.
    const findIdx = (needle: string) =>
      assistantTexts.findIndex((text) => text.includes(needle));

    const aIdx = findIdx('RESEARCH-DONE');
    const bIdx = findIdx(markerB);
    const cIdx = findIdx(markerC);

    expect(aIdx, 'RESEARCH-DONE missing').toBeGreaterThanOrEqual(0);
    expect(bIdx, `marker ${markerB} missing`).toBeGreaterThanOrEqual(0);
    expect(cIdx, `marker ${markerC} missing`).toBeGreaterThanOrEqual(0);
    expect(new Set([aIdx, bIdx, cIdx]).size).toBe(3);

    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    expect(streaming).toBe(false);
  });
});
