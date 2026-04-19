/**
 * Phase 3 coding hard-case acceptance scaffolding.
 *
 * These cases are intentionally marked fixme for now. They define the live
 * operator-facing proofs we want once the coding/debugging loop runtime grows
 * beyond workflow demos.
 *
 * Target cases:
 * - repo edit yields a bounded, reviewable diff
 * - failing test is repaired in the same session
 * - child-session fanout/join stays bounded for coding work
 * - long idle resume preserves the same coding turn
 * - concurrent coding sessions stay isolated under load
 *
 * Run listing only:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   npx playwright test tests/coding-hardcases.spec.ts --list
 */
import { test } from '@playwright/test';

import {
  createNewSession,
  login,
  sendAndWait,
  countAssistantBubbles,
  countUserBubbles,
} from './live-browser-helpers';

test.setTimeout(600_000);

test.describe('Phase 3 coding hard cases', () => {
  test('repo edit task writes a bounded diff and exposes reviewable output', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    const marker = `phase3-${Date.now()}`;
    const prompt = [
      'Use shell tool only.',
      'If shell is not already active, activate it first.',
      `Create a temporary git repo inside the current workspace at ./${marker}.`,
      'Stay inside the current workspace; do not use /tmp or any other absolute temp directory.',
      'Inside it, create notes.txt with exactly two lines: alpha and beta.',
      'Run git add notes.txt so the file is tracked before editing it.',
      'Make exactly one edit: change beta to gamma.',
      'Then run git diff -- notes.txt.',
      'Return only the unified diff, nothing else.',
      'Do not start background work.',
    ].join(' ');

    const result = await sendAndWait(page, prompt, {
      maxWait: 180_000,
      label: 'bounded-diff',
    });

    const response = result.responseText;
    if (!response) {
      throw new Error('Expected a reviewable diff response, got empty assistant output');
    }

    const userBubbles = await countUserBubbles(page);
    const assistantBubbles = await countAssistantBubbles(page);

    test.expect(userBubbles).toBe(1);
    test.expect(assistantBubbles).toBe(1);
    test.expect(response).toContain('diff --git');
    test.expect(response).toContain('notes.txt');
    test.expect(response).toContain('-beta');
    test.expect(response).toContain('+gamma');
    test.expect(response.length).toBeLessThan(4_000);
  });

  test.fixme('failing test is repaired without starting a second ghost turn', async ({ page }) => {
    await login(page);
    await createNewSession(page);
    await sendAndWait(page, 'TODO: seed failing test fixture and ask for targeted repair');
  });

  test.fixme('coding fanout creates bounded child sessions and joins them cleanly', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);
    await sendAndWait(page, 'TODO: trigger bounded child-session coding fanout');
  });

  test.fixme('long idle resume keeps the same coding turn after reconnect', async ({ page }) => {
    await login(page);
    await createNewSession(page);
    await sendAndWait(page, 'TODO: start long coding task, idle, reload, and verify turn merge');
  });

  test.fixme('concurrent coding sessions remain isolated under load', async ({ browser }) => {
    const pageA = await browser.newPage();
    const pageB = await browser.newPage();
    await login(pageA);
    await login(pageB);
    await createNewSession(pageA);
    await createNewSession(pageB);
    await sendAndWait(pageA, 'TODO: concurrent coding case A');
    await sendAndWait(pageB, 'TODO: concurrent coding case B');
  });
});
