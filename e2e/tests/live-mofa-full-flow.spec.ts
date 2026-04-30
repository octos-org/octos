/**
 * MoFA full-flow live e2e: install → exercise → remove.
 *
 * Walks through the complete lifecycle of MoFA skills against a live
 * dashboard:
 *
 *   1. Login + navigate to the profile's skill page.
 *   2. Install the `mofa-cli` skill (and any siblings) for the profile.
 *      If the skill is already installed, remove it first to force a
 *      fresh install path.
 *   3. Open a fresh chat session with the v2 thread-store flag on.
 *   4. Exercise every MoFA-related feature in succession, collecting
 *      proof-of-delivery from the DOM:
 *        a. Builtin-voice TTS (vivian)            — fast, ~10s
 *        b. Cloned-voice TTS (yangmi)             — fast, ~15s, requires
 *           a registered voice clone in OminiX
 *        c. MoFA podcast (research_podcast)       — slow, 3-5 min
 *        d. MoFA slide deck (slides_delivery)     — slow, 3-5 min
 *        e. MoFA site preview                     — slow, 3-5 min
 *      Each step asserts the assistant bubble paired with the user
 *      message contains an expected artifact marker.
 *   5. Navigate back to the skill page and remove the skill. Verify
 *      it's gone from the DOM.
 *
 * Required env:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io
 *   OCTOS_AUTH_TOKEN=octos-admin-2026
 *   OCTOS_PROFILE=dspfac
 *
 * Optional env:
 *   OCTOS_MOFA_INSTALL_SOURCE  default: mofa-org/mofa-skills/mofa-cli
 *   OCTOS_MOFA_SKILL_NAME      default: mofa-cli
 *   OCTOS_MOFA_BUILTIN_VOICE   default: vivian
 *   OCTOS_MOFA_CLONED_VOICE    default: yangmi
 *   OCTOS_MOFA_SKIP_INSTALL=1  reuse an already-installed skill
 *   OCTOS_MOFA_SKIP_REMOVE=1   leave skill installed for later runs
 *
 * NEVER point at mini5 — that host is reserved for coding-green.
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

const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE_ID = process.env.OCTOS_PROFILE || 'dspfac';
const INSTALL_SOURCE =
  process.env.OCTOS_MOFA_INSTALL_SOURCE || 'mofa-org/mofa-skills/mofa-cli';
const SKILL_NAME = process.env.OCTOS_MOFA_SKILL_NAME || 'mofa-cli';
const BUILTIN_VOICE = process.env.OCTOS_MOFA_BUILTIN_VOICE || 'vivian';
const CLONED_VOICE = process.env.OCTOS_MOFA_CLONED_VOICE || 'yangmi';
const SKIP_INSTALL = process.env.OCTOS_MOFA_SKIP_INSTALL === '1';
const SKIP_REMOVE = process.env.OCTOS_MOFA_SKIP_REMOVE === '1';

const FLAG_KEY = 'octos_thread_store_v2';

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
    ({ token, profile, flagKey }) => {
      localStorage.setItem('octos_session_token', token);
      localStorage.setItem('octos_auth_token', token);
      localStorage.setItem('selected_profile', profile);
      localStorage.setItem(flagKey, '1');
    },
    { token: AUTH_TOKEN, profile: PROFILE_ID, flagKey: FLAG_KEY },
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
  ).toBeVisible({ timeout: 20_000 });
}

async function removeSkillIfPresent(
  page: Page,
  skillName: string,
): Promise<boolean> {
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

async function installSkillFresh(page: Page) {
  // Force a fresh install: if the skill is already installed, remove
  // first so the install path is the same as a brand-new profile.
  const wasPreinstalled = await removeSkillIfPresent(page, SKILL_NAME);
  console.log(`mofa-flow: preinstalled_removed=${wasPreinstalled}`);

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
  console.log(`mofa-flow: install_response=${JSON.stringify(installJson)}`);
  expect(Array.isArray(installJson.installed)).toBeTruthy();
  expect(installJson.installed).toContain(SKILL_NAME);

  await expect(
    page.locator('span').filter({ hasText: /^Error:/ }),
  ).toHaveCount(0, { timeout: 5_000 });

  await expect(skillNameLocator(page, SKILL_NAME)).toBeVisible({
    timeout: 180_000,
  });
}

interface FeatureStep {
  name: string;
  prompt: string;
  expected_markers: string[];
  // Per-step wait window for the assistant to finalise. Slow MoFA
  // operations (podcast / slides / sites) can take several minutes;
  // fast TTS settles in under a minute.
  timeout_ms: number;
}

const FEATURES: FeatureStep[] = [
  {
    name: 'tts-builtin',
    prompt: `用 ${BUILTIN_VOICE} 说一段：今天是个好日子，让我们开始吧`,
    expected_markers: [
      BUILTIN_VOICE,
      'audio',
      '.wav',
      '.mp3',
      '语音',
      '好日子',
    ],
    timeout_ms: 90_000,
  },
  {
    name: 'tts-cloned',
    prompt: `用 ${CLONED_VOICE} 念这段：我是你的数字助理`,
    expected_markers: [
      CLONED_VOICE,
      'audio',
      '.wav',
      '.mp3',
      '数字',
      '助理',
    ],
    timeout_ms: 120_000,
  },
  {
    name: 'mofa-podcast',
    prompt: '生成一个关于AI智能体平台的FM播客 (research_podcast)',
    expected_markers: [
      'podcast',
      '播客',
      'episode',
      'audio',
      '.mp3',
      '智能体',
    ],
    timeout_ms: 600_000,
  },
  {
    name: 'mofa-slides',
    prompt: '生成一个关于 AI 智能体技术发展的幻灯片 (mofa slides, full deck)',
    expected_markers: [
      'slides',
      '幻灯片',
      '.pptx',
      'deck',
      'pptx',
      'slide',
    ],
    timeout_ms: 600_000,
  },
  {
    name: 'mofa-sites',
    prompt: '生成一个产品介绍网站 (mofa sites, full site preview)',
    expected_markers: [
      'site',
      '网站',
      'preview',
      'preview/',
      '.html',
      'index',
    ],
    timeout_ms: 600_000,
  },
];

async function waitForAssistantText(
  page: Page,
  expectedAssistantCount: number,
  maxWaitMs: number,
  label: string,
): Promise<string> {
  const start = Date.now();
  let lastFilled = 0;
  let stable = 0;
  while (Date.now() - start < maxWaitMs) {
    const isStreaming = await page
      .locator(SEL.cancelButton)
      .isVisible()
      .catch(() => false);
    const filled = await page.evaluate((sel) => {
      const bubbles = document.querySelectorAll(sel);
      return Array.from(bubbles).filter((el) => {
        const text = ((el as HTMLElement).innerText || '').trim();
        return text.length > 1;
      }).length;
    }, SEL.assistantMessage);

    if (filled >= expectedAssistantCount && !isStreaming) {
      stable += 1;
      if (stable >= 2) {
        const lastText = await page.evaluate((sel) => {
          const bubbles = document.querySelectorAll(sel);
          if (!bubbles.length) return '';
          const last = bubbles[bubbles.length - 1] as HTMLElement;
          return (last.innerText || '').trim();
        }, SEL.assistantMessage);
        return lastText;
      }
    } else {
      stable = 0;
    }

    if (filled !== lastFilled) {
      const elapsed = ((Date.now() - start) / 1000).toFixed(0);
      console.log(
        `  [${label}] ${elapsed}s: filled=${filled}/${expectedAssistantCount} streaming=${isStreaming}`,
      );
      lastFilled = filled;
    }
    await page.waitForTimeout(3_000);
  }
  return '';
}

test.describe('MoFA full-flow: install → exercise → remove', () => {
  test.setTimeout(2_400_000); // 40 min — install + 5 features (~5 min slow ops × 3) + remove

  test('install mofa skill, exercise all features, then remove', async ({
    page,
  }) => {
    // -- Phase 1: dashboard login + install ----------------------------
    await loginToDashboard(page);

    if (!SKIP_INSTALL) {
      await installSkillFresh(page);
    } else {
      console.log('mofa-flow: SKIP_INSTALL=1, expecting skill already present');
      await expect(skillNameLocator(page, SKILL_NAME)).toBeVisible({
        timeout: 30_000,
      });
    }

    // -- Phase 2: switch to chat, run every MoFA feature ---------------
    await login(page); // ensures /chat is open with the right session/profile
    await createNewSession(page);

    const failures: string[] = [];

    for (let i = 0; i < FEATURES.length; i++) {
      const step = FEATURES[i];
      const userBefore = await countUserBubbles(page);
      const assistantBefore = await countAssistantBubbles(page);

      await getInput(page).fill(step.prompt);
      await getSendButton(page).click();
      await expect
        .poll(() => countUserBubbles(page), { timeout: 30_000 })
        .toBe(userBefore + 1);
      console.log(`mofa-flow: sent "${step.name}" prompt`);

      const lastText = await waitForAssistantText(
        page,
        assistantBefore + 1,
        step.timeout_ms,
        step.name,
      );

      const lower = lastText.toLowerCase();
      const matched = step.expected_markers.some((m) =>
        lower.includes(m.toLowerCase()),
      );
      if (!matched) {
        failures.push(
          `${step.name}: assistant text did not contain any expected marker [${step.expected_markers.join(', ')}]. Got "${lastText.slice(0, 200)}"`,
        );
        console.log(`mofa-flow: FAIL ${step.name} — text="${lastText.slice(0, 120)}"`);
      } else {
        console.log(`mofa-flow: OK ${step.name}`);
      }
    }

    // -- Phase 3: remove the skill -------------------------------------
    if (!SKIP_REMOVE) {
      await page.goto(`/admin/profile/${PROFILE_ID}/skills`, {
        waitUntil: 'networkidle',
      });
      await expect(skillNameLocator(page, SKILL_NAME)).toBeVisible({
        timeout: 30_000,
      });
      const removed = await removeSkillIfPresent(page, SKILL_NAME);
      expect(
        removed,
        `Expected skill "${SKILL_NAME}" to be present and removable`,
      ).toBeTruthy();
      console.log(`mofa-flow: skill removed`);
    } else {
      console.log('mofa-flow: SKIP_REMOVE=1, leaving skill installed');
    }

    // Surface any feature failures last so the install/remove gates the
    // skill state cleanly even when one feature regresses.
    expect(
      failures,
      `MoFA feature failures:\n  - ${failures.join('\n  - ')}`,
    ).toEqual([]);
  });
});
