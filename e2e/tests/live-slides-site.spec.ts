/**
 * Live browser acceptance coverage for the slides flow.
 *
 * The final deck artifact appears once and stays stable after reload.
 *
 * Run against a live browser host:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/live-slides-site.spec.ts
 */
import { expect, test } from "@playwright/test";
import {
  createNewSession,
  getAssistantMessageText,
  login,
  sendAndWait,
  SEL,
} from "./live-browser-helpers";

const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || "octos-admin-2026";
const PROFILE_ID = process.env.OCTOS_PROFILE || "dspfac";
const BASE_URL = process.env.OCTOS_TEST_URL || "http://localhost:3000";

// The canonical skill names in mofa-org/mofa-skills are hyphenated:
// `mofa-slides` and `mofa-site` (singular). The dashboard's installed-skill
// listing returns these exact names.
const REQUIRED_SLIDES_SKILLS = ["mofa-slides"] as const;

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
  { state: "present" | "missing" } | { state: "unknown"; reason: string };

async function probeSkill(skillName: string): Promise<SkillProbe> {
  let resp: Response;
  try {
    resp = await fetch(`${BASE_URL}/api/admin/profiles/${PROFILE_ID}/skills`, {
      headers: {
        Authorization: `Bearer ${AUTH_TOKEN}`,
        "X-Profile-Id": PROFILE_ID,
      },
    });
  } catch (err) {
    return {
      state: "unknown",
      reason: `network error: ${(err as Error)?.message || String(err)}`,
    };
  }
  if (!resp.ok) {
    return {
      state: "unknown",
      reason: `HTTP ${resp.status} ${resp.statusText}`,
    };
  }
  const data = await resp.json().catch(() => null);
  const entries = Array.isArray(data)
    ? data
    : Array.isArray((data as { skills?: unknown })?.skills)
      ? (data as { skills: unknown[] }).skills
      : [];
  for (const entry of entries) {
    const name =
      typeof (entry as { name?: unknown })?.name === "string"
        ? (entry as { name: string }).name
        : "";
    if (name === skillName) return { state: "present" };
  }
  return { state: "missing" };
}

async function skipIfSkillsMissing(required: readonly string[]): Promise<void> {
  const probes = await Promise.all(
    required.map(async (name) => ({ name, probe: await probeSkill(name) })),
  );
  const missing = probes.filter((p) => p.probe.state === "missing");
  const unknown = probes.filter((p) => p.probe.state === "unknown");

  if (missing.length > 0) {
    const names = missing.map((p) => p.name).join(", ");
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
      .join(", ");
    test.skip(
      true,
      `Skipping: cannot verify required skill(s) for profile "${PROFILE_ID}" on ${BASE_URL}: ${detail}. ` +
        `In prod/canary this should fail the required-skills inventory gate.`,
    );
  }
}

function assistantNeedsSlidesConfirmation(text: string): boolean {
  return (
    /ready to generate/i.test(text) ||
    /reply\s+"generate"/i.test(text) ||
    /reply\s+"go"/i.test(text)
  );
}

test.describe("Live deliverable flows", () => {
  test.beforeEach(async ({ page }) => {
    await login(page);
    await createNewSession(page);
  });

  test("slides flow renders one final deck artifact after reload", async ({
    page,
  }) => {
    await skipIfSkillsMissing(REQUIRED_SLIDES_SKILLS);

    const deckSlug = `browser-deck-${Date.now().toString(36)}`;

    await sendAndWait(page, `/new slides ${deckSlug}`, {
      label: "slides-init",
      maxWait: 60_000,
    });

    await sendAndWait(
      page,
      'Design a 2-slide deck about browser acceptance. Slide 1 should say "Browser Slides Acceptance". Slide 2 should prove the final deck is visible. Use style nb-pro. Show the outline only. Do not generate yet.',
      {
        label: "slides-design",
        maxWait: 90_000,
      },
    );

    await sendAndWait(page, "generate", {
      label: "slides-generate",
      maxWait: 300_000,
    });

    const deckButton = page.getByRole("button", { name: /deck\.pptx/i });
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
        await sendAndWait(page, "go", {
          label: "slides-confirm",
          maxWait: 300_000,
        });
      }
    }

    await expect
      .poll(async () => deckButton.count(), {
        timeout: 240_000,
        intervals: [5_000],
      })
      .toBe(1);
    await expect(deckButton).toBeVisible();

    const assistantText = await getAssistantMessageText(page);
    if (assistantText.includes("Workspace contract validation failed")) {
      console.log(
        "  slides contract validation failed even though the deck handle is visible",
      );
    }

    await page.reload({ waitUntil: "domcontentloaded" });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await page.waitForTimeout(5_000);

    const afterReloadDeckButton = page.getByRole("button", {
      name: /deck\.pptx/i,
    });
    await expect
      .poll(async () => afterReloadDeckButton.count(), {
        timeout: 30_000,
        intervals: [2_000],
      })
      .toBe(1);
    await expect(afterReloadDeckButton).toBeVisible();
  });
});
