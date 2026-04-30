/**
 * Live browser validation for profile skill install/remove via dashboard UI.
 *
 * Run:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   npx playwright test tests/live-mofa-skills.spec.ts
 */
import { expect, test, type Page } from '@playwright/test';

import { ensureAdminTokenRotated } from './live-browser-helpers';

const PROFILE_ID = process.env.OCTOS_PROFILE || 'dspfac';
const INSTALL_SOURCE = process.env.OCTOS_MOFA_INSTALL_SOURCE || 'mofa-org/mofa-skills/mofa-cli';
const SKILL_NAME = process.env.OCTOS_MOFA_SKILL_NAME || 'mofa-cli';

function escapeRegExp(input: string): string {
  return input.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function skillNameLocator(page: Page, skillName: string) {
  return page.getByText(new RegExp(`^${escapeRegExp(skillName)}$`)).first();
}

function removeButtonForSkill(page: Page, skillName: string) {
  return page
    .locator('div.flex.items-center.justify-between')
    .filter({ has: skillNameLocator(page, skillName) })
    .getByRole('button', { name: /Remove|Removing/i })
    .first();
}

async function loginToDashboard(page: Page) {
  // BootstrapGate redirects all `/admin/*` routes to `/admin/setup/welcome`
  // until the bootstrap token has been rotated. Rotate once and use the
  // strong token thereafter so the SPA renders the real dashboard.
  const effectiveToken = await ensureAdminTokenRotated();

  await page.addInitScript(
    ({ token, profile }) => {
      localStorage.setItem('octos_session_token', token);
      localStorage.setItem('octos_auth_token', token);
      localStorage.setItem('selected_profile', profile);
    },
    { token: effectiveToken, profile: PROFILE_ID },
  );

  await page.goto(`/admin/profile/${PROFILE_ID}/skills`, { waitUntil: 'networkidle' });

  if (page.url().includes('/admin/login') || page.url().includes('/login')) {
    const tokenTab = page.getByRole('button', { name: /Login with admin token/i });
    if (await tokenTab.isVisible().catch(() => false)) {
      await tokenTab.click();
    }
    await page.getByLabel('Admin token').fill(effectiveToken);
    // `getByRole('button', { name: 'Login' })` matches *both* the submit
    // button and the "Login with email instead" toggle button (Playwright's
    // accessible-name match is substring-by-default), tripping strict mode.
    // PR #625 added `data-testid="login-button"` on the submit button — use
    // it directly and fall back to the type=submit + exact "Login" text for
    // older deployed bundles.
    await page
      .locator(
        "[data-testid='login-button'], button[type='submit']:text-is('Login')",
      )
      .first()
      .click();
    await page.waitForURL(/\/admin(\/|$)/, { timeout: 20_000 });
    await page.goto(`/admin/profile/${PROFILE_ID}/skills`, { waitUntil: 'networkidle' });
  }

  await expect(page.getByRole('heading', { name: 'Skills', exact: true })).toBeVisible({
    timeout: 20_000,
  });
}

async function removeSkillIfPresent(page: Page, skillName: string) {
  const skill = skillNameLocator(page, skillName);
  const present = await skill.isVisible().catch(() => false);
  if (!present) return false;

  const deleteResponse = page.waitForResponse(
    (resp) =>
      resp.request().method() === 'DELETE' &&
      resp.url().includes(`/api/admin/profiles/${PROFILE_ID}/skills/${skillName}`),
    { timeout: 180_000 },
  );

  page.once('dialog', (dialog) => dialog.accept());
  await removeButtonForSkill(page, skillName).click();

  const resp = await deleteResponse;
  expect(resp.ok()).toBeTruthy();
  await expect(skill).toHaveCount(0, { timeout: 30_000 });
  return true;
}

test.describe('Live mofa skills install/remove via dashboard', () => {
  test.describe.configure({ mode: 'serial' });
  test.setTimeout(600_000);

  test('install mofa skill through skills page', async ({ page }) => {
    await loginToDashboard(page);

    const wasPreinstalled = await removeSkillIfPresent(page, SKILL_NAME);
    console.log(`preinstalled_removed=${wasPreinstalled}`);

    const sourceInput = page.getByPlaceholder(
      /octos-org\/system-skills, https:\/\/host\/org\/repo\.git, or \.\/skills\/my-skill/i,
    );
    await sourceInput.fill(INSTALL_SOURCE);

    const installResponse = page.waitForResponse(
      (resp) =>
        resp.request().method() === 'POST' &&
        resp.url().includes(`/api/admin/profiles/${PROFILE_ID}/skills`),
      { timeout: 300_000 },
    );

    await page.getByRole('button', { name: 'Install' }).click();
    const installResp = await installResponse;
    expect(installResp.ok()).toBeTruthy();

    const installJson = await installResp.json();
    console.log(`install_response=${JSON.stringify(installJson)}`);
    expect(Array.isArray(installJson.installed)).toBeTruthy();
    expect(installJson.installed).toContain(SKILL_NAME);

    await expect(
      page.locator('span').filter({ hasText: /^Error:/ }),
    ).toHaveCount(0, { timeout: 5_000 });

    await expect(skillNameLocator(page, SKILL_NAME)).toBeVisible({
      timeout: 180_000,
    });
  });

  test('remove mofa skill through skills page', async ({ page }) => {
    await loginToDashboard(page);
    await expect(skillNameLocator(page, SKILL_NAME)).toBeVisible({
      timeout: 30_000,
    });

    const deleteResponse = page.waitForResponse(
      (resp) =>
        resp.request().method() === 'DELETE' &&
        resp.url().includes(`/api/admin/profiles/${PROFILE_ID}/skills/${SKILL_NAME}`),
      { timeout: 180_000 },
    );

    page.once('dialog', (dialog) => dialog.accept());
    await removeButtonForSkill(page, SKILL_NAME).click();

    const deleteResp = await deleteResponse;
    expect(deleteResp.ok()).toBeTruthy();
    const deleteJson = await deleteResp.json();
    console.log(`remove_response=${JSON.stringify(deleteJson)}`);

    await expect(skillNameLocator(page, SKILL_NAME)).toHaveCount(0, {
      timeout: 30_000,
    });
  });
});
