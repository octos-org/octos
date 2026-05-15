/**
 * Live end-to-end probe against mini5 (dspfac.ocean.ominix.io):
 *
 * 1. Does the spawn_only `run_pipeline` flow deliver a follow-up
 *    `BackgroundResultPayload` to the assistant bubble after 5-10 min?
 * 2. Does that bubble carry a TEXT SUMMARY (the synthesize node's
 *    executive summary), not just a file attachment chip?
 *
 * This is the user's open question: the synthesize node prompt asks for
 * a 1000-word executive summary as the final text response, but the user
 * reports the chat bubble only contains a file link. We verify by:
 *   - submitting the Chinese deep-research prompt the user used,
 *   - watching the chat for both the kickoff ack and the result bubble,
 *   - capturing the result bubble's text content, files, and timing.
 *
 * Run:
 *   cd /Users/yuechen/home/octos/e2e
 *   OCTOS_TEST_URL=https://dspfac.ocean.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *     npx playwright test tests/mini5-run-pipeline-summary-bubble.spec.ts \
 *     --reporter=list --workers=1
 */

import { expect, test, type Page } from '@playwright/test';

import {
  SEL,
  createNewSession,
  getInput,
  getSendButton,
  login,
} from './live-browser-helpers';

const PROMPT =
  process.env.MINI5_PIPELINE_PROMPT ||
  '深度研究中国电动汽车 2026 年出口的前景';

// 12-minute hard wait per task brief.
const MAX_WAIT_MS = 12 * 60 * 1000;
const POLL_MS = 5_000;
// Test wrapper must outlive the longest wait + tear-down headroom.
const TEST_TIMEOUT_MS = MAX_WAIT_MS + 90_000;

interface BubbleSnapshot {
  index: number;
  textContent: string;
  textLen: number;
  hasMdLink: boolean;
  links: Array<{ text: string; href: string; download: string }>;
}

test.describe('mini5 run_pipeline summary bubble', () => {
  test.setTimeout(TEST_TIMEOUT_MS);

  test('BackgroundResultPayload lands and bubble carries executive summary', async ({
    page,
  }, testInfo) => {
    await login(page);
    await createNewSession(page);

    // Kick off the prompt.
    const input = getInput(page);
    const sendBtn = getSendButton(page);
    await input.fill(PROMPT);
    await sendBtn.click();

    // Snapshot 1 — immediate kickoff (~10s in, after spawn ack)
    await waitForBubble(page, 1, 30_000);
    await page.waitForTimeout(2_000);
    const kickoffShot = testInfo.outputPath('01-kickoff.png');
    await page.screenshot({ path: kickoffShot, fullPage: true });
    const kickoffSnap = await captureBubbles(page);
    console.log(
      `[mini5-pipeline] kickoff bubbles=${kickoffSnap.length} content="${kickoffSnap[0]?.textContent.slice(0, 200) ?? ''}"`,
    );

    // Confirm there is an `assistant-message` whose text matches the
    // spawn_only ack the user reported.
    const ackPresent = kickoffSnap.some((s) =>
      /Background work started for|已在后台启动|started in (the )?background/i.test(
        s.textContent,
      ),
    );
    console.log(`[mini5-pipeline] kickoff ack visible: ${ackPresent}`);

    // Wait — poll for either: (a) a NEW assistant bubble appearing AFTER the
    // ack, or (b) a `.md` attachment appearing in any assistant bubble. Note
    // the spawn_only completion handler emits multiple bubbles per the
    // execution.rs path: an envelope bubble with content like
    // "✓ run_pipeline completed (file.md)" + envelope media,
    // plus an additional "produced files" notification bubble.
    const start = Date.now();
    const ackBubbleCount = kickoffSnap.length;
    let midShotTaken = false;
    let landed: { duration: number; snap: BubbleSnapshot[] } | null = null;
    let last: BubbleSnapshot[] = kickoffSnap;

    while (Date.now() - start < MAX_WAIT_MS) {
      await page.waitForTimeout(POLL_MS);
      last = await captureBubbles(page);
      const elapsed = Date.now() - start;
      const newBubble = last.length > ackBubbleCount;
      const anyMd = last.some((s) => s.hasMdLink);

      // Mid-wait screenshot at 5 min in (per task brief).
      if (!midShotTaken && elapsed >= 5 * 60 * 1000) {
        midShotTaken = true;
        const midShot = testInfo.outputPath('02-mid-wait.png');
        await page.screenshot({ path: midShot, fullPage: true });
        console.log(`[mini5-pipeline] mid-wait screenshot: ${midShot}`);
        console.log(
          `[mini5-pipeline] mid-wait bubbles=${last.length} mdLinks=${anyMd}`,
        );
      }

      if (newBubble || anyMd) {
        // Give the SPA a moment to render the full completion payload
        // (envelope event + per-file rows can arrive a beat apart).
        await page.waitForTimeout(3_000);
        last = await captureBubbles(page);
        landed = { duration: Date.now() - start, snap: last };
        break;
      }

      if (elapsed % 60_000 < POLL_MS) {
        console.log(
          `[mini5-pipeline] waiting... t+${Math.round(elapsed / 1000)}s bubbles=${last.length} md=${anyMd}`,
        );
      }
    }

    // Always dump final state.
    const finalShot = testInfo.outputPath('03-final.png');
    await page.screenshot({ path: finalShot, fullPage: true });

    if (!landed) {
      console.log(`[mini5-pipeline] VERDICT: TIMEOUT after ${MAX_WAIT_MS / 1000}s`);
      console.log(`[mini5-pipeline] final bubbles=${last.length}`);
      for (const s of last) {
        console.log(
          `[mini5-pipeline]   bubble[${s.index}] textLen=${s.textLen} hasMd=${s.hasMdLink} content="${s.textContent.slice(0, 200)}"`,
        );
      }
      console.log(`[mini5-pipeline] kickoff shot: ${kickoffShot}`);
      console.log(`[mini5-pipeline] final shot: ${finalShot}`);
      expect(
        false,
        `BackgroundResultPayload did not arrive within ${MAX_WAIT_MS / 1000}s`,
      ).toBe(true);
      return;
    }

    const { duration, snap } = landed;
    console.log(
      `[mini5-pipeline] VERDICT: result landed after ${Math.round(duration / 1000)}s with ${snap.length} bubbles (kickoff had ${ackBubbleCount})`,
    );
    console.log(`[mini5-pipeline] kickoff shot: ${kickoffShot}`);
    console.log(`[mini5-pipeline] final shot: ${finalShot}`);

    // Catalog every assistant bubble — the user wants to know if any of
    // them carry the synthesized executive summary as TEXT.
    let longestBubble: BubbleSnapshot | null = null;
    let synthesizedTextSeen = false;
    for (const s of snap) {
      console.log(
        `[mini5-pipeline] bubble[${s.index}] textLen=${s.textLen} hasMd=${s.hasMdLink} content[0:500]="${s.textContent.slice(0, 500)}"`,
      );
      if (s.textLen > 400 && /电动|汽车|出口|EV|2026/i.test(s.textContent)) {
        synthesizedTextSeen = true;
      }
      if (!longestBubble || s.textLen > longestBubble.textLen) {
        longestBubble = s;
      }
    }

    console.log(
      `[mini5-pipeline] longest bubble textLen=${longestBubble?.textLen ?? 0}`,
    );
    console.log(
      `[mini5-pipeline] synthesized text in any bubble? ${synthesizedTextSeen}`,
    );

    // The PRIMARY hypothesis we're testing — the bubble should contain
    // the synthesize node's executive summary. If this fails, the user's
    // complaint is confirmed at the UI layer.
    //
    // We don't fail the spec on the summary check — instead we surface
    // every signal needed to verify the trace. Failing on completion
    // (the spawn_only result must arrive) is the hard contract.
    expect(snap.length, 'no assistant bubbles at all').toBeGreaterThan(0);
  });
});

async function waitForBubble(page: Page, n: number, timeoutMs: number) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const count = await page.locator(SEL.assistantMessage).count();
    if (count >= n) return;
    await page.waitForTimeout(500);
  }
}

async function captureBubbles(page: Page): Promise<BubbleSnapshot[]> {
  return page.evaluate(() => {
    const out: Array<{
      index: number;
      textContent: string;
      textLen: number;
      hasMdLink: boolean;
      links: Array<{ text: string; href: string; download: string }>;
    }> = [];
    const nodes = Array.from(
      document.querySelectorAll("[data-testid='assistant-message']"),
    );
    nodes.forEach((node, idx) => {
      const el = node as HTMLElement;
      const text = (el.innerText || '').trim();
      const anchors = Array.from(
        el.querySelectorAll('a[href]'),
      ) as HTMLAnchorElement[];
      const links = anchors.map((a) => ({
        text: (a.textContent || '').trim(),
        href: a.href || '',
        download: a.download || '',
      }));
      const hasMd = links.some((l) => /\.md(?:$|[?#])/i.test(l.href));
      out.push({
        index: idx,
        textContent: text,
        textLen: text.length,
        hasMdLink: hasMd,
        links,
      });
    });
    return out;
  });
}
