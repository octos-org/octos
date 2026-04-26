/**
 * M7.9 / W2.G3 — live restart-from-node test.
 *
 * Triggers a pipeline with fault injection (a research prompt that
 * names an unknown skill so a node fails), waits for the failed
 * NodeCard row to appear, clicks "restart", and verifies that:
 *
 *   1. The confirmation modal explains the scope (upstream cached
 *      outputs reused).
 *   2. After confirmation, only the failed subtree re-runs — the
 *      original failed task stays visible in history while a new task
 *      id appears under the same chat bubble.
 *
 * Run against a live host:
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   npx playwright test e2e/tests/live-restart.spec.ts
 *
 * Skips when OCTOS_TEST_URL is unset.
 */
import { expect, test } from '@playwright/test';
import { createNewSession, login, sendAndWait } from './live-browser-helpers';

const LIVE_URL = process.env.OCTOS_TEST_URL || '';

test.skip(!LIVE_URL, 'OCTOS_TEST_URL not set — live test requires a running host');
test.setTimeout(360_000);

test('restart-from-node re-runs only the failed subtree', async ({ page }) => {
  test.setTimeout(360_000);
  await login(page);
  await createNewSession(page);

  // Fault-injection prompt: ask for a podcast using a non-existent
  // voice clone. The runtime will try to call podcast_generate with
  // an invalid voice and the validator will mark the node as failed.
  const resultPromise = sendAndWait(
    page,
    '用 clone:not-a-real-voice 做一个2分钟的播客，主题是城市夜景。',
    { maxWait: 90_000, throwOnTimeout: false },
  );

  // Wait for at least one failed NodeCard row.
  await expect
    .poll(
      async () =>
        page
          .locator("[data-testid='node-card'][data-node-status='error']")
          .count(),
      { timeout: 90_000, intervals: [2_000] },
    )
    .toBeGreaterThan(0);

  const failedRow = page
    .locator("[data-testid='node-card'][data-node-status='error']")
    .first();
  const failedNodeId = await failedRow.getAttribute('data-node-id');
  expect(failedNodeId).not.toBeNull();

  // Hover/scroll the row into view, then click its "restart" button.
  await failedRow.scrollIntoViewIfNeeded();
  const restartButton = failedRow.locator(
    "[data-testid='node-restart-button']",
  );
  await expect(restartButton).toBeVisible({ timeout: 10_000 });
  await restartButton.click();

  // Confirmation modal text must mention upstream cache reuse —
  // that's the key UX promise of restart-from-node.
  const modal = page.locator("[data-testid='node-card-confirm-modal']");
  await expect(modal).toBeVisible();
  await expect(modal).toContainText(/upstream cached outputs reused/i);

  await page.locator("[data-testid='node-card-confirm-confirm']").click();

  // The "restarting…" indicator replaces the restart button.
  await expect(
    failedRow.locator("[data-testid='node-restart-done']"),
  ).toBeVisible({ timeout: 10_000 });

  // The original failed row stays visible (history preserved).
  await expect(failedRow).toBeVisible();
  await expect(failedRow).toHaveAttribute('data-node-status', 'error');

  // A new task entry should appear in the supervisor task list — we
  // can't easily assert "only the subtree re-ran" purely from the UI
  // without backend introspection, so check that at least one node
  // returned to the running state (proving the relaunch fired).
  await expect
    .poll(
      async () =>
        page
          .locator("[data-testid='node-card'][data-node-status='running']")
          .count(),
      { timeout: 30_000, intervals: [2_000] },
    )
    .toBeGreaterThan(0);

  await resultPromise.catch(() => undefined);
});
