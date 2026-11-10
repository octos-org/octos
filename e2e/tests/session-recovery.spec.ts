/**
 * Repo-level live browser recovery acceptance coverage.
 *
 * This case extends the smoke suite by proving that double reload during an
 * active stream still collapses into one final turn.
 *
 * Run against a live deployment:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/session-recovery.spec.ts
 */
import { expect, test, type Page } from "@playwright/test";
import {
  SEL,
  countAssistantBubbles,
  countUserBubbles,
  createNewSession,
  getInput,
  getSendButton,
  login,
} from "./live-browser-helpers";

async function resetChat(page: Page) {
  await login(page);
  await createNewSession(page);
}

async function waitForRecoveredTurn(page: Page, timeoutMs = 240_000) {
  const deadline = Date.now() + timeoutMs;
  let lastAssistantCount = -1;
  let lastText = "";
  let stableCount = 0;

  while (Date.now() < deadline) {
    const assistantCount = await countAssistantBubbles(page);
    const userCount = await countUserBubbles(page);
    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    const text =
      assistantCount > 0
        ? (
            (await page
              .locator(SEL.assistantMessage)
              .last()
              .textContent()
              .catch(() => "")) || ""
          ).trim()
        : "";

    if (userCount === 1 && assistantCount > 0 && !streaming && text) {
      if (assistantCount === lastAssistantCount && text === lastText) {
        stableCount++;
        if (stableCount >= 2) {
          return text;
        }
      } else {
        stableCount = 0;
      }
    } else {
      stableCount = 0;
    }

    lastAssistantCount = assistantCount;
    lastText = text;
    await page.waitForTimeout(2_000);
  }

  throw new Error("Timed out waiting for the recovered turn to settle");
}

function normalizeRecoveryMarkerText(text: string) {
  return text.normalize("NFKC").replace(/[\u2010-\u2015\u2212]/g, "-");
}

test.describe("Live session recovery", () => {
  test.setTimeout(360_000);

  test("reloading twice during an active stream resumes the same turn", async ({
    page,
  }) => {
    await resetChat(page);

    const marker = `RECONNECT-${Date.now()}`;
    await getInput(page).fill(
      `Write a detailed memo about reconnect storms and session recovery directly in chat. Do not use tools or write files. Include ${marker} exactly once near the end and keep the answer long enough to survive a couple of reloads.`,
    );
    await getSendButton(page).click();

    await page.waitForFunction(
      () =>
        document.querySelectorAll("[data-testid='assistant-message']").length >
          0 && document.querySelector("[data-testid='cancel-button']") !== null,
      undefined,
      { timeout: 30_000 },
    );

    await page.waitForTimeout(2_500);
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await page.waitForTimeout(1_500);
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });

    const finalText = await waitForRecoveredTurn(page);
    expect(finalText.length).toBeGreaterThan(0);
    expect(await countUserBubbles(page)).toBe(1);
    expect(await countAssistantBubbles(page)).toBeGreaterThanOrEqual(1);
    expect(normalizeRecoveryMarkerText(finalText)).toContain(marker);
  });
});
