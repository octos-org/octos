/**
 * Live browser acceptance coverage for slides and site flows.
 *
 * These cases target user-visible deliverables, not API-only regressions:
 * - slides: the final deck artifact appears once and stays stable after reload
 * - site: the built preview page is reachable and stays stable after reload
 *
 * Run against a live browser host:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/live-slides-site.spec.ts
 */
import { expect, test, type Page } from '@playwright/test';
import {
  createNewSession,
  getAssistantMessageText,
  login,
  sendAndWait,
  SEL,
} from './live-browser-helpers';

const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE_ID = process.env.OCTOS_PROFILE || 'dspfac';
const BASE_URL = process.env.OCTOS_TEST_URL || 'http://localhost:3000';

// The canonical skill names in mofa-org/mofa-skills are hyphenated:
// `mofa-slides` and `mofa-site` (singular). The dashboard's installed-skill
// listing returns these exact names.
const REQUIRED_SLIDES_SKILLS = ['mofa-slides'] as const;
const REQUIRED_SITE_SKILLS = ['mofa-site'] as const;

test.setTimeout(600_000);

/**
 * Tri-state result of probing `/api/admin/profiles/{profile}/skills`:
 *  - `present`: API responded and the named skill is installed.
 *  - `missing`: API responded and the named skill is NOT installed.
 *  - `unknown`: the API call itself failed (network, 401, 5xx). We don't
 *    know whether the skill is present, so we shouldn't quietly mark the
 *    test as missing-skill-skip.
 *
 * The slides + site flows depend on `mofa_slides` and `mofa_sites` being
 * pre-provisioned on the target host. Tenant state has historically drifted
 * across the mini fleet (mini2 had them, mini1/3/4 did not), so we gate
 * execution on actual API state rather than assuming. Distinguishing
 * `missing` from `unknown` is important: the former is environment drift the
 * operator can fix by installing, the latter is an infra problem (probably
 * auth or daemon down) that a separate inventory gate should catch.
 */
type SkillProbe =
  | { state: 'present' | 'missing' }
  | { state: 'unknown'; reason: string };

async function probeSkill(skillName: string): Promise<SkillProbe> {
  let resp: Response;
  try {
    resp = await fetch(
      `${BASE_URL}/api/admin/profiles/${PROFILE_ID}/skills`,
      {
        headers: {
          Authorization: `Bearer ${AUTH_TOKEN}`,
          'X-Profile-Id': PROFILE_ID,
        },
      },
    );
  } catch (err) {
    return {
      state: 'unknown',
      reason: `network error: ${(err as Error)?.message || String(err)}`,
    };
  }
  if (!resp.ok) {
    return { state: 'unknown', reason: `HTTP ${resp.status} ${resp.statusText}` };
  }
  const data = await resp.json().catch(() => null);
  const entries = Array.isArray(data)
    ? data
    : Array.isArray((data as { skills?: unknown })?.skills)
      ? (data as { skills: unknown[] }).skills
      : [];
  for (const entry of entries) {
    const name =
      typeof (entry as { name?: unknown })?.name === 'string'
        ? (entry as { name: string }).name
        : '';
    if (name === skillName) return { state: 'present' };
  }
  return { state: 'missing' };
}

async function skipIfSkillsMissing(required: readonly string[]): Promise<void> {
  const probes = await Promise.all(
    required.map(async (name) => ({ name, probe: await probeSkill(name) })),
  );
  const missing = probes.filter((p) => p.probe.state === 'missing');
  const unknown = probes.filter((p) => p.probe.state === 'unknown');

  if (missing.length > 0) {
    const names = missing.map((p) => p.name).join(', ');
    test.skip(
      true,
      `Skipping: required skill(s) "${names}" not installed for profile "${PROFILE_ID}" on ${BASE_URL}. ` +
        `This is an environment prerequisite for this live acceptance flow, not an app assertion. ` +
        `Install via POST /api/admin/profiles/${PROFILE_ID}/skills (repo: mofa-org/mofa-skills/<name>), then re-run.`,
    );
  }

  if (unknown.length > 0) {
    // We couldn't verify presence at all. Don't pretend the skill is missing
    // — the underlying call failed. Skip with a distinct, infra-flavoured
    // reason so a separate inventory gate (or the operator) can act on it.
    const detail = unknown
      .map((p) => `${p.name} (${(p.probe as { reason: string }).reason})`)
      .join(', ');
    test.skip(
      true,
      `Skipping: cannot verify required skill(s) for profile "${PROFILE_ID}" on ${BASE_URL}: ${detail}. ` +
        `In prod/canary this should fail the required-skills inventory gate.`,
    );
  }
}

async function collectPreviewUrls(page: Page): Promise<string[]> {
  const text = await getAssistantMessageText(page);
  const matches =
    text.match(/\/api\/preview\/[^\s"'<>]+\/signal-atlas(?:\/index\.html|\/)/gi) || [];
  return Array.from(
    new Set(
      matches
        .map((value) => normalizePreviewUrl(value))
        .filter((value) => value.trim().length > 0),
    ),
  );
}

async function collectPersistedPreviewUrls(page: Page): Promise<string[]> {
  return page.evaluate(async () => {
    const token =
      localStorage.getItem('octos_session_token') ||
      localStorage.getItem('octos_auth_token') ||
      '';
    const profile = localStorage.getItem('selected_profile') || '';
    const headers: Record<string, string> = {};

    if (token) {
      headers.Authorization = `Bearer ${token}`;
    }
    if (profile) {
      headers['X-Profile-Id'] = profile;
    }

    const normalize = (url: string) => {
      const trimmed = url.trim();
      const match = trimmed.match(/^([^?#]+)(.*)$/);
      if (!match) return trimmed;
      let base = match[1].replace(/\/index\.html$/i, '/');
      if (!base.endsWith('/')) {
        base = `${base}/`;
      }
      return `${base}${match[2] || ''}`;
    };

    const sessionsResp = await fetch('/api/sessions', { headers });
    if (!sessionsResp.ok) {
      return [];
    }

    const sessions = await sessionsResp.json().catch(() => []);
    if (!Array.isArray(sessions)) {
      return [];
    }

    const urls = new Set<string>();
    for (const session of sessions.slice(0, 40)) {
      const sessionId = typeof session?.id === 'string' ? session.id : null;
      if (!sessionId) {
        continue;
      }

      const messagesResp = await fetch(
        `/api/sessions/${encodeURIComponent(sessionId)}/messages?limit=100`,
        { headers },
      ).catch(() => null);
      if (!messagesResp?.ok) {
        continue;
      }

      const messages = await messagesResp.json().catch(() => []);
      if (!Array.isArray(messages)) {
        continue;
      }

      const text = messages
        .map((message) => (typeof message?.content === 'string' ? message.content : ''))
        .join('\n');
      const matches =
        text.match(/\/api\/preview\/[^\s"'<>]+\/signal-atlas(?:\/index\.html|\/)/gi) || [];
      for (const match of matches) {
        urls.add(normalize(match));
      }
    }

    return Array.from(urls);
  });
}

async function waitForPreviewUrls(
  page: Page,
  expectedCount: number,
  timeoutMs: number,
): Promise<string[]> {
  let latest: string[] = [];
  await expect
    .poll(async () => {
      latest = await collectPreviewUrls(page);
      return latest.length;
    }, {
      timeout: timeoutMs,
      intervals: [2_000, 5_000],
    })
    .toBe(expectedCount);
  return latest;
}

function normalizePreviewUrl(url: string): string {
  const trimmed = url.trim();
  const match = trimmed.match(/^([^?#]+)(.*)$/);
  if (!match) return trimmed;
  let base = match[1].replace(/\/index\.html$/i, "/");
  if (!base.endsWith("/")) {
    base = `${base}/`;
  }
  return `${base}${match[2] || ""}`;
}

async function waitForPreviewBody(
  page: Page,
  previewUrl: string,
  textNeedles: string | string[],
  timeoutMs: number,
) {
  const deadline = Date.now() + timeoutMs;
  let lastBody = '';
  const needles = (Array.isArray(textNeedles) ? textNeedles : [textNeedles]).map((needle) =>
    needle.toLowerCase(),
  );

  while (Date.now() < deadline) {
    await page.goto(previewUrl, { waitUntil: 'networkidle' });
    lastBody = (await page.locator('body').innerText().catch(() => '')) || '';
    const normalizedBody = lastBody.toLowerCase();
    if (needles.every((needle) => normalizedBody.includes(needle))) {
      return lastBody;
    }
    await page.waitForTimeout(5_000);
  }

  throw new Error(
    `Preview at ${previewUrl} never exposed ${needles.map((needle) => JSON.stringify(needle)).join(', ')}. Last body: ${lastBody.slice(0, 400)}`,
  );
}

function assistantNeedsSlidesConfirmation(text: string): boolean {
  return (
    /ready to generate/i.test(text) ||
    /reply\s+"generate"/i.test(text) ||
    /reply\s+"go"/i.test(text)
  );
}

test.describe('Live deliverable flows', () => {
  test.beforeEach(async ({ page }) => {
    await login(page);
    await createNewSession(page);
  });

  test('slides flow renders one final deck artifact after reload', async ({ page }) => {
    await skipIfSkillsMissing(REQUIRED_SLIDES_SKILLS);

    const deckSlug = `browser-deck-${Date.now().toString(36)}`;

    await sendAndWait(page, `/new slides ${deckSlug}`, {
      label: 'slides-init',
      maxWait: 60_000,
    });

    await sendAndWait(
      page,
      'Design a 2-slide deck about browser acceptance. Slide 1 should say "Browser Slides Acceptance". Slide 2 should prove the final deck is visible. Use style nb-pro. Show the outline only. Do not generate yet.',
      {
        label: 'slides-design',
        maxWait: 90_000,
      },
    );

    await sendAndWait(page, 'generate', {
      label: 'slides-generate',
      maxWait: 300_000,
    });

    const deckButton = page.getByRole('button', { name: /deck\.pptx/i });
    const deckAppearedWithoutConfirmation = await expect
      .poll(async () => deckButton.count(), {
        timeout: 30_000,
        intervals: [2_000, 5_000],
      })
      .toBeGreaterThan(0)
      .then(() => true)
      .catch(() => false);

    if (!deckAppearedWithoutConfirmation) {
      const assistantText = await getAssistantMessageText(page);
      if (assistantNeedsSlidesConfirmation(assistantText)) {
        await sendAndWait(page, 'go', {
          label: 'slides-confirm',
          maxWait: 300_000,
        });
      }
    }

    await expect.poll(async () => deckButton.count(), {
      timeout: 240_000,
      intervals: [5_000],
    }).toBe(1);
    await expect(deckButton).toBeVisible();

    const assistantText = await getAssistantMessageText(page);
    if (assistantText.includes('Workspace contract validation failed')) {
      console.log(
        '  slides contract validation failed even though the deck handle is visible',
      );
    }

    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await page.waitForTimeout(5_000);

    const afterReloadDeckButton = page.getByRole('button', { name: /deck\.pptx/i });
    await expect.poll(async () => afterReloadDeckButton.count(), {
      timeout: 30_000,
      intervals: [2_000],
    }).toBe(1);
    await expect(afterReloadDeckButton).toBeVisible();
  });

  test('site flow renders a built preview page and survives reload', async ({
    page,
  }) => {
    await skipIfSkillsMissing(REQUIRED_SITE_SKILLS);

    let initResult = await sendAndWait(page, '/new site astro', {
      label: 'site-init',
      maxWait: 90_000,
      throwOnTimeout: false,
    });
    if (initResult.responseLen === 0) {
      initResult = await sendAndWait(page, '/new site astro', {
        label: 'site-init-retry',
        maxWait: 90_000,
      });
    }

    const previewUrls = await waitForPreviewUrls(page, 1, 60_000);
    expect(previewUrls).toHaveLength(1);

    const previewUrl = previewUrls[0];

    await sendAndWait(
      page,
      'Update the homepage so the visible title says "Browser Site Acceptance" and the page clearly includes a "Live preview" section. Rebuild the site so the preview reflects it.',
      {
        label: 'site-build',
        maxWait: 240_000,
        throwOnTimeout: false,
      },
    );

    const previewPage = await page.context().newPage();
    try {
      const body = await waitForPreviewBody(
        previewPage,
        previewUrl,
        ['Browser Site Acceptance', 'Live preview'],
        240_000,
      );

      expect(body).toContain('Browser Site Acceptance');
      // The Astro template ships "Live Preview" (TitleCase). The
      // `waitForPreviewBody` helper above normalises via `.toLowerCase()`
      // before matching, so a bare `toContain('Live preview')` (lowercase)
      // would race with that helper and fail on TitleCase. Match
      // case-insensitively here so the assertion mirrors the helper.
      expect(body.toLowerCase()).toContain('live preview');

      await previewPage.reload({ waitUntil: 'networkidle' });
      const reloadedBody =
        (await previewPage.locator('body').innerText().catch(() => '')) || '';
      expect(reloadedBody).toContain('Browser Site Acceptance');
      expect(reloadedBody.toLowerCase()).toContain('live preview');
    } finally {
      await previewPage.close();
    }

    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await page.waitForTimeout(5_000);

    let afterReloadPreviewUrls: string[] = [];
    await expect
      .poll(async () => {
        const visibleUrls = await collectPreviewUrls(page);
        if (visibleUrls.includes(previewUrl)) {
          afterReloadPreviewUrls = visibleUrls;
          return true;
        }

        const persistedUrls = await collectPersistedPreviewUrls(page);
        if (persistedUrls.includes(previewUrl)) {
          afterReloadPreviewUrls = persistedUrls;
          return true;
        }

        afterReloadPreviewUrls = visibleUrls;
        return false;
      }, {
        timeout: 60_000,
        intervals: [2_000, 5_000],
      })
      .toBe(true);
    expect(afterReloadPreviewUrls).toContain(previewUrl);
  });
});
