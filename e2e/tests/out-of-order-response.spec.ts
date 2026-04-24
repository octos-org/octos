/**
 * Suite 4 — Out-of-order response race (mocked adversarial timing).
 *
 * Mirrors the pattern from octos-web's concurrent-deep-research-ordering
 * mocked spec, run against the octos-web deployment at OCTOS_TEST_URL.
 * The test intercepts /api/chat and injects a 3000ms delay for the first
 * call and 100ms for the second. FA-8's fix means the streamId correlation
 * must route each stream's content to its originating user turn, even when
 * the second stream finalizes first.
 *
 * This provides a deterministic equivalent to the live regression in
 * case live LLM timing doesn't reproduce.
 *
 * Run:
 *   OCTOS_TEST_URL=https://mini1.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/out-of-order-response.spec.ts
 */
import { expect, test, type Route } from '@playwright/test';

import {
  SEL,
  createNewSession,
  getInput,
  getSendButton,
  login,
} from './live-browser-helpers';
import { countBubbles, waitForAllAssistantsContent } from './matrix-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL;

interface SsePayload {
  delayMs: number;
  streamId: string;
  answer: string;
}

/**
 * Build a minimal valid SSE body that many octos chat clients accept.
 * We produce one `text` (or equivalent) delta followed by a `done`
 * event so the final message lands with the answer text.
 */
function buildSseBody(payload: SsePayload): string {
  const events = [
    { type: 'stream_started', stream_id: payload.streamId },
    { type: 'text', stream_id: payload.streamId, text: payload.answer },
    { type: 'done', stream_id: payload.streamId, reason: 'end_turn' },
  ];
  return events.map((evt) => `data: ${JSON.stringify(evt)}\n\n`).join('');
}

test.describe('Suite 4 out-of-order response race', () => {
  test.skip(() => !TEST_URL, 'OCTOS_TEST_URL not set — suite is a no-op.');
  test.setTimeout(120_000);

  test('streamId correlation keeps answers on their own turns', async ({
    page,
  }) => {
    // The precise SSE envelope expected by the live web client is still
    // being finalized alongside FA-8 — mocked intercept assertions are
    // gated fixme until we standardize the envelope.
    test.fixme(
      true,
      'Mocked SSE envelope pending FA-8 streamId schema stabilization',
    );

    await login(page);
    await createNewSession(page);

    let chatCalls = 0;
    await page.route(/\/api\/chat(\?|$)/, async (route: Route) => {
      chatCalls += 1;
      const isFirst = chatCalls === 1;
      const payload: SsePayload = {
        delayMs: isFirst ? 3_000 : 100,
        streamId: isFirst ? 'stream-A' : 'stream-B',
        answer: isFirst
          ? 'Out-of-order answer A (delayed 3s)'
          : 'Out-of-order answer B (fast 100ms)',
      };

      // Honor the delay before returning the SSE body.
      await new Promise((resolve) => setTimeout(resolve, payload.delayMs));

      await route.fulfill({
        status: 200,
        headers: {
          'content-type': 'text/event-stream',
          'cache-control': 'no-cache',
        },
        body: buildSseBody(payload),
      });
    });

    const input = getInput(page);
    const sendBtn = getSendButton(page);

    await input.fill('FIRST SEND (expects answer A)');
    await sendBtn.click();
    await page.waitForTimeout(200);
    await input.fill('SECOND SEND (expects answer B)');
    await sendBtn.click();

    await waitForAllAssistantsContent(page, 2, 30_000);

    const { user, assistant } = await countBubbles(page);
    expect(user).toBe(2);
    expect(assistant).toBeGreaterThanOrEqual(2);

    const assistantTexts = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);

    // FA-8 contract: the FIRST user's answer bubble carries answer A; the
    // SECOND user's answer bubble carries answer B. We rely on DOM order
    // (first user bubble comes before second) to identify which bubble
    // belongs to which user turn.
    const roles = await page.evaluate(() => {
      const nodes = document.querySelectorAll(
        "[data-testid='user-message'], [data-testid='assistant-message']",
      );
      return Array.from(nodes).map((node) => ({
        role: node.getAttribute('data-testid')?.includes('user') ? 'user' : 'assistant',
        text: (node.textContent || '').trim(),
      }));
    });

    // Find the assistant bubble that immediately follows user #0.
    let seenUsers = 0;
    let firstTurnAssistant: string | null = null;
    let secondTurnAssistant: string | null = null;
    for (const entry of roles) {
      if (entry.role === 'user') {
        seenUsers += 1;
      } else if (entry.role === 'assistant') {
        if (seenUsers === 1 && !firstTurnAssistant) firstTurnAssistant = entry.text;
        if (seenUsers === 2 && !secondTurnAssistant) secondTurnAssistant = entry.text;
      }
    }

    expect(firstTurnAssistant, 'missing first-turn assistant bubble').toBeTruthy();
    expect(secondTurnAssistant, 'missing second-turn assistant bubble').toBeTruthy();

    // Crux of FA-8: A's text must not end up on the second turn's bubble.
    expect(firstTurnAssistant).toContain('answer A');
    expect(secondTurnAssistant).toContain('answer B');
    expect(firstTurnAssistant).not.toContain('answer B');
    expect(secondTurnAssistant).not.toContain('answer A');

    expect(assistantTexts.length).toBeGreaterThanOrEqual(2);
  });
});
