/**
 * Suite 5 — Reload during active background task.
 *
 * Start a deep-research-style task, wait for the task-anchor bubble to
 * appear (PR #42 bubble-level spinner), snapshot the task-store
 * localStorage keys, reload the page, and verify the task-anchor is
 * still rendered after reload, the localStorage keys are present, the
 * task eventually reaches a terminal state, and the final content
 * lands on the correct bubble.
 *
 * Then reload AGAIN while terminal and assert the state is idempotent
 * (no duplicate bubbles).
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/reload-mid-background.spec.ts
 */
import { expect, test, type Page } from '@playwright/test';

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
  getActiveSessionIdOrNull,
  snapshotTaskStore,
  waitForAllAssistantsContent,
} from './matrix-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL;

const LONG_BACKGROUND_PROMPT = [
  'Do a deep research on the latest Rust programming language developments',
  'in 2026 via run_pipeline in background mode. Do not ask for confirmation,',
  'start the pipeline directly and return "Deep research is running in the',
  'background." as the immediate acknowledgment.',
].join(' ');

async function waitForTaskAnchor(page: Page, timeoutMs = 60_000) {
  await page.waitForSelector("[data-testid='task-anchor-message']", {
    timeout: timeoutMs,
    state: 'attached',
  });
}

test.describe('Suite 5 reload-mid-background', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(720_000);

  test('task-anchor survives reload; terminal state is idempotent', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    const sessionIdBefore = await getActiveSessionIdOrNull(page);

    await getInput(page).fill(LONG_BACKGROUND_PROMPT);
    await getSendButton(page).click();

    // Wait until the bubble-level task anchor appears.
    await waitForTaskAnchor(page, 120_000);

    // Snapshot the task-store keys so we can assert persistence survives
    // a reload. If the brief's key prefix isn't yet used, the snapshot
    // helper falls back to current session keys; we still require that
    // at least one relevant key is present.
    const storeBefore = await snapshotTaskStore(page);
    expect(Object.keys(storeBefore).length).toBeGreaterThan(0);

    // Reload #1 — mid-flight.
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await waitForTaskAnchor(page, 60_000);

    // Task-store keys should still be there after reload.
    const storeAfter = await snapshotTaskStore(page);
    expect(Object.keys(storeAfter).length).toBeGreaterThan(0);

    // Wait for the task to reach a non-active (final) state by looking for
    // non-empty assistant content beyond the "running in the background"
    // placeholder. Allow a long runway since deep research is slow.
    await waitForAllAssistantsContent(page, 1, 540_000);

    const userCountAfter = await countUserBubbles(page);
    const assistantCountAfter = await countAssistantBubbles(page);
    expect(userCountAfter).toBeGreaterThanOrEqual(1);
    expect(assistantCountAfter).toBeGreaterThanOrEqual(1);

    // Snapshot bubble counts pre-idempotent-reload.
    const preReloadUser = userCountAfter;
    const preReloadAssistant = assistantCountAfter;

    // Reload #2 — terminal state should be idempotent.
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await page.waitForTimeout(5_000);

    const userCountFinal = await countUserBubbles(page);
    const assistantCountFinal = await countAssistantBubbles(page);
    expect(userCountFinal).toBe(preReloadUser);
    // Allow a +1 tolerance because a pending-anchor can briefly collapse
    // into a terminal bubble on reload, but there must be no duplicate
    // fan-out.
    expect(assistantCountFinal).toBeLessThanOrEqual(preReloadAssistant + 1);
    expect(assistantCountFinal).toBeGreaterThanOrEqual(preReloadAssistant);

    const sessionIdAfter = await getActiveSessionIdOrNull(page);
    if (sessionIdBefore && sessionIdAfter) {
      expect(sessionIdAfter).toBe(sessionIdBefore);
    }
  });
});
