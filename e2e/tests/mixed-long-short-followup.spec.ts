/**
 * Suite 3 — Long-running + one-shot mix (followup queue mode).
 *
 * Migrated from the coding-blue-mixed-order track (FA-10 rewrite). The
 * supervisor runs a deep-research-style Q1, then two quick shell echoes
 * Q2/Q3 behind it. Under followup, they must serialize — all three
 * user bubbles rendered, assistant answers eventually land for each,
 * and markers B/C arrive only after A's bubble exists.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/mixed-long-short-followup.spec.ts
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

test.describe('Suite 3 mixed-long-short followup', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(540_000);

  test.afterEach(async ({ page }) => {
    await resetQueueMode(page);
  });

  test('long research + two quick echoes serialize under followup', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);
    await setQueueMode(page, 'followup');

    const markerB = `MIXFU-B-${Date.now()}`;
    const markerC = `MIXFU-C-${Date.now() + 1}`;
    const prompts = [
      LONG_RESEARCH_PROMPT,
      buildEchoShellPrompt(markerB),
      buildEchoShellPrompt(markerC),
    ];

    await fireRapidPrompts(page, prompts, 500);

    // Up to 8 min for all 3 assistant bubbles to settle under followup.
    const filled = await waitForAllAssistantsContent(page, 3, 480_000);
    expect(filled).toBeGreaterThanOrEqual(3);

    const { user, assistant } = await countBubbles(page);
    expect(user).toBe(3);
    expect(assistant).toBeGreaterThanOrEqual(3);

    // Content-based assertion: RESEARCH-DONE plus both quick markers must
    // appear in the assistant thread.
    const assistantTexts = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);
    const combined = assistantTexts.join('\n');
    expect(combined).toContain('RESEARCH-DONE');
    expect(combined).toContain(markerB);
    expect(combined).toContain(markerC);

    // Ordering: A's research bubble must appear before B/C markers under
    // followup, since followup waits for the long turn first.
    const researchIdx = assistantTexts.findIndex((text) =>
      text.includes('RESEARCH-DONE'),
    );
    const markerBIdx = assistantTexts.findIndex((text) => text.includes(markerB));
    const markerCIdx = assistantTexts.findIndex((text) => text.includes(markerC));
    expect(researchIdx).toBeGreaterThanOrEqual(0);
    expect(markerBIdx).toBeGreaterThan(researchIdx);
    expect(markerCIdx).toBeGreaterThan(researchIdx);

    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    expect(streaming).toBe(false);
  });
});
