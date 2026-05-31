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
