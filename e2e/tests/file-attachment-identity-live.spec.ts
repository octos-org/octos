/**
 * Suite 7 — File-attachment identity race (live).
 *
 * Adapted from the octos-web mocked spec to run against live octos-web.
 * We ask the agent to produce a file via the shell tool (so there's a
 * deterministic artifact), rapid-fire a second quick prompt before the
 * file bubble finalizes, and verify the file attaches to the bubble whose
 * tool_call_id matches — not the bubble for the latest prompt.
 *
 * Harder without a deterministic file-generating plugin; we use the
 * built-in shell to `send_file` a small artifact.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/file-attachment-identity-live.spec.ts
 */
import { expect, test } from '@playwright/test';

import {
  SEL,
  createNewSession,
  login,
} from './live-browser-helpers';
import {
  buildEchoShellPrompt,
  fireRapidPrompts,
  resetQueueMode,
  setQueueMode,
  waitForAllAssistantsContent,
} from './matrix-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL;

function buildFileProducePrompt(fileName: string, marker: string): string {
  return [
    'Use shell tool only.',
    'If shell is not already active, call activate_tools with exactly ["shell"] once and only once.',
    `Run \`mkdir -p ./attach-race && cd ./attach-race && printf '${marker}\\n' > ${fileName} && cat ${fileName}\` to create the file.`,
    `Then call send_file on ./attach-race/${fileName} so the file is attached to this turn's assistant bubble.`,
    `Finally return a short message that includes ${marker}.`,
    'Do not start background work.',
  ].join(' ');
}

test.describe('Suite 7 file-attachment-identity (live)', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(360_000);

  test.afterEach(async ({ page }) => {
    await resetQueueMode(page);
  });

  test('attached file lands on the originating turn, not the latest', async ({
    page,
  }) => {
    // Depends on send_file + tool_call_id routing landing on the current
    // mini. Gated fixme until FA-12 + M7.9 attach the artifact to the
    // correct bubble in speculative mode.
    test.fixme(
      true,
      'Live tool_call_id attachment routing pending FA-12 + M7.9 deploy',
    );

    await login(page);
    await createNewSession(page);
    await setQueueMode(page, 'speculative');

    const marker = `ATTACH-${Date.now()}`;
    const quickMarker = `QUICK-${Date.now() + 1}`;

    const producePrompt = buildFileProducePrompt('identity.txt', marker);
    const quickPrompt = buildEchoShellPrompt(quickMarker);

    await fireRapidPrompts(page, [producePrompt, quickPrompt], 300);

    await waitForAllAssistantsContent(page, 2, 240_000);
    await page.waitForTimeout(5_000);

    // Map each assistant bubble to its owned file attachments.
    const bubbleAttachments = await page.evaluate(() => {
      const bubbles = document.querySelectorAll("[data-testid='assistant-message']");
      return Array.from(bubbles).map((bubble) => {
        const text = (bubble.textContent || '').trim();
        const files = Array.from(
          bubble.querySelectorAll(
            "[data-testid='file-attachment'], [data-testid='audio-attachment']",
          ),
        ).map((attachment) => {
          const el = attachment as HTMLElement;
          return {
            filename: el.dataset.filename || '',
            path: el.dataset.filePath || '',
            testid: el.getAttribute('data-testid') || '',
          };
        });
        return { text, files };
      });
    });

    expect(bubbleAttachments.length).toBeGreaterThanOrEqual(2);

    // The file-producing prompt's bubble must own the identity.txt
    // attachment; the quick prompt's bubble must NOT.
    const producerBubble = bubbleAttachments.find((bubble) =>
      bubble.text.includes(marker),
    );
    const quickBubble = bubbleAttachments.find((bubble) =>
      bubble.text.includes(quickMarker),
    );

    expect(producerBubble, 'missing producer bubble').toBeTruthy();
    expect(quickBubble, 'missing quick bubble').toBeTruthy();

    const producerHasFile = (producerBubble?.files || []).some((file) =>
      (file.filename || file.path).includes('identity.txt'),
    );
    const quickHasFile = (quickBubble?.files || []).some((file) =>
      (file.filename || file.path).includes('identity.txt'),
    );

    expect(producerHasFile, 'file must attach to the producer bubble').toBe(true);
    expect(quickHasFile, 'file must NOT attach to the quick bubble').toBe(false);

    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    expect(streaming).toBe(false);
  });
});
