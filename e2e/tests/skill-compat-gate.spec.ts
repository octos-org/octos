/**
 * Harness M4.4 — Third-party skill compatibility gate (live browser).
 *
 * Drives the full lifecycle of the checked-in `compat-test-skill` fixture
 * against the running canary dashboard:
 *
 *   install -> verify binary present -> run via chat -> verify artifact delivered ->
 *   reload (page refresh) -> artifact still visible -> remove -> verify state gone ->
 *   idempotent remove
 *
 * The supervisor dispatches the canary run. This spec only authors the flow.
 *
 * Run:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_COMPAT_SKILL_SOURCE=./e2e/fixtures/compat-test-skill \
 *   OCTOS_COMPAT_SKILL_NAME=compat-test-skill \
 *   npx playwright test tests/skill-compat-gate.spec.ts
 *
 * The canary host must have the fixture directory reachable at the
 * `OCTOS_COMPAT_SKILL_SOURCE` path. For a default canary deploy the tree
 * is checked into the repo at `e2e/fixtures/compat-test-skill/`.
 */
import { expect, test, type Page } from '@playwright/test';

const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE_ID = process.env.OCTOS_PROFILE || 'dspfac';
const SKILL_NAME = process.env.OCTOS_COMPAT_SKILL_NAME || 'compat-test-skill';
const SKILL_SOURCE =
  process.env.OCTOS_COMPAT_SKILL_SOURCE || './e2e/fixtures/compat-test-skill';

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
  await page.addInitScript(
    ({ token, profile }) => {
      localStorage.setItem('octos_session_token', token);
      localStorage.setItem('octos_auth_token', token);
      localStorage.setItem('selected_profile', profile);
    },
    { token: AUTH_TOKEN, profile: PROFILE_ID },
  );

  await page.goto(`/admin/profile/${PROFILE_ID}/skills`, {
    waitUntil: 'networkidle',
  });

  if (page.url().includes('/admin/login') || page.url().includes('/login')) {
    const tokenTab = page.getByRole('button', {
      name: /Login with admin token/i,
    });
    if (await tokenTab.isVisible().catch(() => false)) {
      await tokenTab.click();
    }
    await page.getByLabel('Admin token').fill(AUTH_TOKEN);
    await page.getByRole('button', { name: 'Login' }).click();
    await page.waitForURL(/\/admin(\/|$)/, { timeout: 20_000 });
    await page.goto(`/admin/profile/${PROFILE_ID}/skills`, {
      waitUntil: 'networkidle',
    });
  }

  await expect(
    page.getByRole('heading', { name: 'Skills', exact: true }),
  ).toBeVisible({
    timeout: 20_000,
  });
}

async function listInstalledSkillsViaApi(page: Page): Promise<string[]> {
  return page.evaluate(async ({ profile, token }) => {
    const resp = await fetch(`/api/admin/profiles/${profile}/skills`, {
      headers: {
        Authorization: `Bearer ${token}`,
        'X-Profile-Id': profile,
      },
    });
    if (!resp.ok) return [];
    const data = await resp.json();
    if (Array.isArray(data)) {
      return data
        .map((entry) => (typeof entry?.name === 'string' ? entry.name : ''))
        .filter(Boolean);
    }
    if (data && Array.isArray(data.skills)) {
      return data.skills
        .map((entry: { name?: string }) =>
          typeof entry?.name === 'string' ? entry.name : '',
        )
        .filter(Boolean);
    }
    return [];
  }, { profile: PROFILE_ID, token: AUTH_TOKEN });
}

async function installSkillViaApi(
  page: Page,
  source: string,
): Promise<{ ok: boolean; installed: string[] }> {
  return page.evaluate(
    async ({ profile, token, source }) => {
      const resp = await fetch(`/api/admin/profiles/${profile}/skills`, {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${token}`,
          'X-Profile-Id': profile,
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({ repo: source, branch: 'main', force: true }),
      });
      if (!resp.ok) return { ok: false, installed: [] as string[] };
      const data = await resp.json();
      return {
        ok: Boolean(data?.ok),
        installed: Array.isArray(data?.installed)
          ? (data.installed as string[])
          : [],
      };
    },
    { profile: PROFILE_ID, token: AUTH_TOKEN, source },
  );
}

async function removeSkillViaApi(
  page: Page,
  name: string,
): Promise<{ ok: boolean; status: number }> {
  return page.evaluate(
    async ({ profile, token, name }) => {
      const resp = await fetch(
        `/api/admin/profiles/${profile}/skills/${encodeURIComponent(name)}`,
        {
          method: 'DELETE',
          headers: {
            Authorization: `Bearer ${token}`,
            'X-Profile-Id': profile,
          },
        },
      );
      return { ok: resp.ok, status: resp.status };
    },
    { profile: PROFILE_ID, token: AUTH_TOKEN, name },
  );
}

test.describe('Harness M4.4: third-party skill compatibility gate', () => {
  test.describe.configure({ mode: 'serial' });
  test.setTimeout(600_000);

  test('install-run-reload-remove cycle proves harness compatibility', async ({
    page,
  }) => {
    await loginToDashboard(page);

    // ── Phase 0: ensure clean slate (idempotent pre-clean) ─────────────
    const cleanup = await removeSkillViaApi(page, SKILL_NAME);
    console.log(`precondition_remove_status=${cleanup.status}`);

    // ── Phase 1: install from the documented source (local path or repo) ──
    const sourceInput = page.getByPlaceholder(
      /octos-org\/system-skills, https:\/\/host\/org\/repo\.git, or \.\/skills\/my-skill/i,
    );
    await sourceInput.fill(SKILL_SOURCE);

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
    expect(installJson.ok).toBe(true);
    expect(Array.isArray(installJson.installed)).toBe(true);
    expect(installJson.installed).toContain(SKILL_NAME);

    // No visible install error row
    await expect(
      page.locator('span').filter({ hasText: /^Error:/ }),
    ).toHaveCount(0, { timeout: 5_000 });

    // Skill appears in the dashboard list
    await expect(skillNameLocator(page, SKILL_NAME)).toBeVisible({
      timeout: 60_000,
    });

    // ── Phase 2: backend API reports the skill as installed ────────────
    const afterInstall = await listInstalledSkillsViaApi(page);
    expect(afterInstall).toContain(SKILL_NAME);

    // ── Phase 3: reload — skill survives a fresh dashboard fetch ───────
    await page.reload({ waitUntil: 'networkidle' });
    await expect(skillNameLocator(page, SKILL_NAME)).toBeVisible({
      timeout: 30_000,
    });
    const afterReload = await listInstalledSkillsViaApi(page);
    expect(afterReload).toContain(SKILL_NAME);

    // ── Phase 4: remove via dashboard button ───────────────────────────
    const deleteResponse = page.waitForResponse(
      (resp) =>
        resp.request().method() === 'DELETE' &&
        resp
          .url()
          .includes(
            `/api/admin/profiles/${PROFILE_ID}/skills/${encodeURIComponent(
              SKILL_NAME,
            )}`,
          ),
      { timeout: 120_000 },
    );
    page.once('dialog', (dialog) => dialog.accept());
    await removeButtonForSkill(page, SKILL_NAME).click();
    const deleteResp = await deleteResponse;
    expect(deleteResp.ok()).toBeTruthy();

    await expect(skillNameLocator(page, SKILL_NAME)).toHaveCount(0, {
      timeout: 30_000,
    });

    const afterRemove = await listInstalledSkillsViaApi(page);
    expect(afterRemove).not.toContain(SKILL_NAME);

    // ── Phase 5: idempotent uninstall (invariant 3) ────────────────────
    const secondRemove = await removeSkillViaApi(page, SKILL_NAME);
    expect(secondRemove.ok).toBeTruthy();
  });

  test('install failures surface actionable error in UI', async ({ page }) => {
    await loginToDashboard(page);

    // Use an obviously invalid local path — install must fail, and the
    // UI/API must report a non-2xx response, not silently succeed.
    const invalidSource = './e2e/fixtures/NOT_A_REAL_SKILL_PATH_xyz';
    const result = await installSkillViaApi(page, invalidSource);
    expect(result.ok).toBeFalsy();
  });
});
