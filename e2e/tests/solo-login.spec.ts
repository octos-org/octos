import { test, expect } from '@playwright/test';

/**
 * Real-browser UX test for the no-password "solo" login on the admin
 * dashboard. Drives the full first-run path:
 *
 *   /admin/  → (no token) login page → "Continue without a password"
 *   → soloLogin 404 (no profile yet) → solo profile form
 *   → fill name/username/email → soloCreate → token minted
 *   → BootstrapGate sees a solo host → lands in the dashboard (NOT the
 *     rotate-token wizard).
 *
 * Prereqs (started out-of-band by the runner):
 *   - `octos serve --solo --port 8080` with a FRESH data dir. The `--solo`
 *     opt-in is REQUIRED — solo login is OFF by default and must never be
 *     enabled on a proxy-fronted host (see `api::solo_auth`).
 *   - `vite dev` on :5173 serving the dashboard at /admin/, proxying /api
 *     to :8080. Set OCTOS_TEST_URL=http://localhost:5173.
 */
// Those prerequisites are NOT provided by any CI job: `e2e-live-nightly`
// starts `octos serve --port 3000` with no `--solo` and no vite dev server,
// and points the suite at :3000. Solo login is off by default, so
// `solo-continue` never renders and this spec has failed on every nightly
// run rather than testing anything. Skipping on an explicit opt-in keeps the
// reason visible instead of burning it as permanent red — see #2073 for
// giving CI the prerequisites so it can run for real.
const SOLO_E2E = process.env.OCTOS_SOLO_E2E === '1';

test.skip(
  !SOLO_E2E,
  'needs `octos serve --solo` on a fresh data dir plus a vite dev server ' +
    'proxying /api to it, with OCTOS_TEST_URL pointed at vite. Set ' +
    'OCTOS_SOLO_E2E=1 once both are running.',
);

test('solo no-password login creates a local profile and enters the dashboard', async ({
  page,
}) => {
  await page.goto('/admin/');

  // Unauthenticated → login page. The solo affordance appears once the
  // public /api/auth/status probe resolves with local_solo_enabled.
  const solo = page.getByTestId('solo-continue');
  await expect(solo).toBeVisible();
  await solo.click();

  // First run: no profile yet, so soloLogin 404s and we get the create form.
  await expect(page.getByTestId('solo-profile-form')).toBeVisible();

  await page.getByTestId('solo-name').fill('Ada Lovelace');
  await page.getByTestId('solo-username').fill('ada');
  await page.getByTestId('solo-email').fill('ada@example.com');
  await page.getByTestId('solo-submit').click();

  // Lands in the authenticated dashboard (home), not the setup wizard.
  await expect(page).toHaveURL(/\/admin\/?(\?.*)?$/);
  await expect(page.getByText('All Profiles')).toBeVisible();
  await expect(page.getByText('ada@example.com')).toBeVisible();

  // Must NOT have been hijacked into the rotate-token / welcome wizard.
  await expect(page.getByText('Two quick steps')).toHaveCount(0);
  await expect(page).not.toHaveURL(/\/setup\//);
});
