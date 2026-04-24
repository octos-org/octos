/**
 * Suite 10 — Review-gate PR (scaffold only).
 *
 * Targets the swarm-app UI at `/swarm/`. Only functional post-M7.9 —
 * scaffolded now so the spec is ready to go when the review-submission
 * UI lands. Verifies the page loads, tabs render, and the dispatch
 * form is present. Actual review-submission validation is pending the
 * M7.9 PR + deploy.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/swarm-review-gate.spec.ts
 */
import { expect, test } from '@playwright/test';

import { login } from './live-browser-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL;

test.describe('Suite 10 swarm-review-gate (scaffold)', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(120_000);

  test('swarm app loads and exposes dispatch + review tabs', async ({
    page,
  }) => {
    // M7.9 deliverable — the swarm UI is not yet live on the target
    // mini. Remove this fixme once the /swarm/ route + dispatch form
    // ship.
    test.fixme(
      true,
      'Swarm review UI is an M7.9 deliverable — scaffold only',
    );

    await login(page);

    // Navigate to the swarm app surface. The exact route may shift —
    // once M7.9 confirms, lock in here.
    await page.goto('/swarm/', { waitUntil: 'networkidle' });

    // The page must render *something* — a heading or a primary
    // landmark — not a 404.
    const bodyText = (await page.locator('body').innerText().catch(() => '')) || '';
    expect(bodyText.length).toBeGreaterThan(0);
    expect(bodyText.toLowerCase()).not.toContain('not found');

    // Tabs / dispatch form expectations (placeholders until M7.9):
    //  - A tab labelled "Dispatch" and one labelled "Review".
    //  - A form with at least one submit-style button.
    // These assertions stay in place so the scaffold fails loud the
    // moment M7.9 lands but the selectors don't match.
    await expect(page.getByRole('tab', { name: /dispatch/i })).toBeVisible();
    await expect(page.getByRole('tab', { name: /review/i })).toBeVisible();
    await expect(page.getByRole('button', { name: /dispatch|submit/i })).toBeVisible();
  });
});
