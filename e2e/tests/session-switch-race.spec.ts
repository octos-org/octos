/**
 * Suite 6 — Session-switch race.
 *
 * Create session A, start a long-running task. Before the task completes,
 * create session B. Fire a quick prompt in B, wait for its answer. Switch
 * back to session A — the task-anchor should still be present, the
 * task-store state should belong to A not B. Assert no cross-session
 * task bleed (Review B B-007 vicinity).
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/session-switch-race.spec.ts
 */
import { expect, test } from '@playwright/test';

import {
  SEL,
  countAssistantBubbles,
  createNewSession,
  getInput,
  getSendButton,
  login,
  sendAndWait,
} from './live-browser-helpers';
import {
  buildEchoShellPrompt,
  getActiveSessionIdOrNull,
} from './matrix-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL;

const LONG_TASK_PROMPT = [
  'Do a deep research on Rust async runtime landscape in 2026 via',
  'run_pipeline in background mode. Do not ask for confirmation,',
  'start the pipeline directly and return "Deep research is running in',
  'the background." as the immediate acknowledgment.',
].join(' ');

test.describe('Suite 6 session-switch-race', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(480_000);

  test('switching to a second session does not bleed task state from the first', async ({
    page,
  }) => {
    await login(page);

    // Session A — start a long background task.
    await createNewSession(page);
    const sessionAId = await getActiveSessionIdOrNull(page);
    expect(sessionAId, 'expected to resolve session A id').toBeTruthy();

    await getInput(page).fill(LONG_TASK_PROMPT);
    await getSendButton(page).click();
    await page.waitForSelector("[data-testid='task-anchor-message']", {
      timeout: 120_000,
      state: 'attached',
    });

    const sessionAAnchorCount = await page
      .locator("[data-testid='task-anchor-message']")
      .count();
    expect(sessionAAnchorCount).toBeGreaterThanOrEqual(1);

    // Session B — create and ask a quick question.
    await createNewSession(page);
    await page.waitForTimeout(2_000);
    const sessionBId = await getActiveSessionIdOrNull(page);
    expect(sessionBId).toBeTruthy();
    expect(sessionBId).not.toBe(sessionAId);

    // Session B's chat view should start empty — no task anchors from A.
    const bAnchorCount = await page
      .locator("[data-testid='task-anchor-message']")
      .count();
    expect(bAnchorCount, 'session B must not show session A task anchors').toBe(0);

    const markerB = `SW-B-${Date.now()}`;
    const bResult = await sendAndWait(page, buildEchoShellPrompt(markerB), {
      label: 'sw-b',
      maxWait: 120_000,
    });
    expect(bResult.responseText).toContain(markerB);

    // Session B still must not show A's task anchor.
    const bAnchorCountAfter = await page
      .locator("[data-testid='task-anchor-message']")
      .count();
    expect(bAnchorCountAfter).toBe(0);

    // Switch back to session A via the sidebar.
    const aSwitchButton = page.locator(
      `[data-session-id="${sessionAId}"] [data-testid='session-switch-button']`,
    );
    await aSwitchButton.waitFor({ state: 'visible', timeout: 15_000 });
    await aSwitchButton.click();
    await page.waitForTimeout(3_000);

    // Session A's task anchor must still be rendered.
    const aAnchorCount = await page
      .locator("[data-testid='task-anchor-message']")
      .count();
    expect(aAnchorCount).toBeGreaterThanOrEqual(1);

    // And the markerB text from session B must not leak into session A.
    const aAssistantTexts = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);
    const combined = aAssistantTexts.join('\n');
    expect(combined).not.toContain(markerB);
    expect(await countAssistantBubbles(page)).toBeGreaterThanOrEqual(1);

    // Confirm we actually landed back on session A.
    const activeAfter = await getActiveSessionIdOrNull(page);
    if (sessionAId && activeAfter) {
      expect(activeAfter).toBe(sessionAId);
    }
  });
});
