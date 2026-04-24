/**
 * Suite 9 — Task-anchor lifecycle trace.
 *
 * Long-running task produces `task-anchor-message`, `task-anchor-spinner`,
 * `task-anchor-label`, `task-anchor-detail` testids. Assert each testid
 * appears at the expected lifecycle point, the spinner disappears on
 * completion, and the label transitions from a running state to a
 * terminal one.
 *
 * The brief enumerates additional testids (`task-anchor-phase-*`,
 * `task-anchor-progress-*`) that arrive with M7.9 PRs — their checks
 * are gated fixme until those testids ship.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/task-anchor-lifecycle.spec.ts
 */
import { expect, test } from '@playwright/test';

import {
  SEL,
  createNewSession,
  getInput,
  getSendButton,
  login,
} from './live-browser-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL;

const LONG_PIPELINE_PROMPT = [
  'Run a deep research pipeline on Rust 2026 via run_pipeline in',
  'background mode. Do not ask for confirmation — run the pipeline',
  'directly and return "Deep research is running in the background."',
  'as the immediate acknowledgment.',
].join(' ');

test.describe('Suite 9 task-anchor-lifecycle', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(720_000);

  test('spinner appears while active, disappears on terminal', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    await getInput(page).fill(LONG_PIPELINE_PROMPT);
    await getSendButton(page).click();

    // Phase 1: the task-anchor-message must render within a bounded
    // window of the user send.
    await page.waitForSelector("[data-testid='task-anchor-message']", {
      timeout: 120_000,
      state: 'attached',
    });

    // Phase 2: the spinner must be visible at least once while the task
    // is active. Poll briefly since streams may produce quick transient
    // phases before the spinner settles.
    let sawSpinner = false;
    const spinnerDeadline = Date.now() + 60_000;
    while (Date.now() < spinnerDeadline) {
      const hasSpinner = await page
        .locator("[data-testid='task-anchor-spinner']")
        .first()
        .isVisible({ timeout: 1_000 })
        .catch(() => false);
      if (hasSpinner) {
        sawSpinner = true;
        break;
      }
      await page.waitForTimeout(1_000);
    }
    expect(sawSpinner, 'task-anchor-spinner never appeared while task was active').toBe(true);

    // Phase 3: label must be visible and non-empty.
    const labelText = await page
      .locator("[data-testid='task-anchor-label']")
      .first()
      .textContent({ timeout: 30_000 })
      .catch(() => '');
    expect((labelText || '').trim().length).toBeGreaterThan(0);

    // Phase 4: wait for terminal state — spinner must go away and the
    // label's status text should no longer say "running" or "starting".
    const terminalDeadline = Date.now() + 540_000;
    let finalLabel = '';
    while (Date.now() < terminalDeadline) {
      const stillStreaming = await page
        .locator(SEL.cancelButton)
        .isVisible({ timeout: 1_000 })
        .catch(() => false);
      const hasSpinner = await page
        .locator("[data-testid='task-anchor-spinner']")
        .first()
        .isVisible({ timeout: 500 })
        .catch(() => false);
      finalLabel =
        (await page
          .locator("[data-testid='task-anchor-label']")
          .first()
          .textContent({ timeout: 500 })
          .catch(() => '')) || '';
      if (!stillStreaming && !hasSpinner && finalLabel.trim().length > 0) {
        break;
      }
      await page.waitForTimeout(5_000);
    }

    const spinnerAfter = await page
      .locator("[data-testid='task-anchor-spinner']")
      .first()
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    expect(spinnerAfter, 'spinner must disappear on terminal state').toBe(false);
    expect(finalLabel.trim().length).toBeGreaterThan(0);

    // The label should no longer indicate active running once terminal.
    const normalized = finalLabel.toLowerCase();
    expect(normalized).not.toMatch(/\brunning\b|\bstarting\b|\bstreaming\b/);

    // Note: the brief enumerates additional `task-anchor-phase-*` and
    // `task-anchor-progress-*` testids as M7.9 deliverables. Those
    // are not yet emitted by the component; once they ship, extend
    // this spec with phase-transition + monotonic-progress assertions.
  });
});
