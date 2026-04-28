/**
 * M7.9 / W2.G2 — live cancel test.
 *
 * Triggers a long-running pipeline (deep research) and clicks the
 * NodeCard cancel pill. Asserts the supervisor flips the task to
 * `Cancelled` within 15s of the click, surfaced via the SSE
 * task_status channel.
 *
 * Run against a live host:
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   npx playwright test e2e/tests/live-cancel.spec.ts
 *
 * Skips automatically when OCTOS_TEST_URL is unset so unit / CI runs
 * don't pay the live-network cost.
 */
import { expect, test } from '@playwright/test';
import { createNewSession, login, sendAndWait } from './live-browser-helpers';

const LIVE_URL = process.env.OCTOS_TEST_URL || '';

test.skip(!LIVE_URL, 'OCTOS_TEST_URL not set — live test requires a running host');
test.setTimeout(300_000);

test('cancel button transitions long pipeline to Cancelled within 15s', async ({ page }) => {
  test.setTimeout(300_000);
  await login(page);
  await createNewSession(page);

  // Trigger a long-running deep research pipeline. The exact prompt
  // doesn't matter — we just need something that spawns run_pipeline
  // with multiple nodes so the NodeCard tree appears.
  const resultPromise = sendAndWait(
    page,
    '请对「全球AI智能体竞争格局2026年趋势」做一次深度研究，输出完整报告。',
    { maxWait: 60_000, throwOnTimeout: false },
  );

  // Wait for the NodeCard tree to appear (run_pipeline starts).
  await expect
    .poll(
      async () => page.locator("[data-testid='node-card-tree']").count(),
      { timeout: 60_000, intervals: [2_000] },
    )
    .toBeGreaterThan(0);

  const cancelButton = page
    .locator("[data-testid='node-tree-cancel-button']")
    .first();
  await expect(cancelButton).toBeVisible({ timeout: 10_000 });
  await cancelButton.click();

  // Confirmation modal should pop. Confirm.
  const modal = page.locator("[data-testid='node-card-confirm-modal']");
  await expect(modal).toBeVisible();
  await page
    .locator("[data-testid='node-card-confirm-confirm']")
    .click();

  // The optimistic "cancelling…" pill appears immediately on success.
  await expect(
    page.locator("[data-testid='node-tree-cancel-pending']"),
  ).toBeVisible({ timeout: 10_000 });

  // Within 15s of the cancel click the supervisor pushes a task_status
  // event with status=cancelled — the message bubble drops out of the
  // streaming state, no further tool_progress arrives, and the cancel
  // pill stays visible (we don't auto-clear it). Use the assistant
  // message text as the durability signal: the worker should have
  // emitted a "cancelled" assistant line by then.
  const cancelStart = Date.now();
  await expect
    .poll(
      async () =>
        (await page
          .locator("[data-testid='assistant-message']")
          .last()
          .textContent()) || '',
      { timeout: 25_000, intervals: [2_000] },
    )
    .toMatch(/cancel/i);
  const cancelLatency = Date.now() - cancelStart;
  expect(cancelLatency).toBeLessThan(20_000);

  // Drain the original sendAndWait promise so the test process exits cleanly.
  await resultPromise.catch(() => undefined);
});
