/**
 * Round-2 fleet-wide Playwright sweep:
 *
 * All 4 minis now on the SAME daemon binary AND the SAME web bundle
 * (`index-B_vbnr5m.js`). The question: does the spawn_only ack-bubble UI
 * regression reproduce uniformly on this bundle, or was the round-1 mini5
 * timeout coincidental load?
 *
 * For each trial we capture:
 *   1. Does the kickoff ack bubble render as a real `assistant-message`
 *      (NOT just an inline `run_pipeline / run_pipeline: running` progress
 *      chip)?
 *   2. Does the final summary bubble eventually arrive carrying ~5K chars
 *      of synthesized text, OR does only the file attachment land?
 *   3. Screenshots at t=15s (kickoff), t=4min (mid-wait), t=10min/completion.
 *   4. Browser console errors / warnings from the new bundle.
 *   5. Per-trial duration to summary bubble OR 12-min timeout.
 *
 * Trial table (mini -> domain -> prompt):
 *   1. mini1 (dspfac.crew.ominix.io)  -> 深度研究全球量子计算商业化进展 2026
 *   2. mini2 (dspfac.bot.ominix.io)   -> 深度研究印度太空产业崛起 2026
 *   3. mini3 (dspfac.octos.ominix.io) -> Deep research on autonomous trucking commercial rollout 2026
 *   4. mini5 (dspfac.ocean.ominix.io) -> 深度研究全球海上风电产业 2026 前景
 *   5. mini1 (dspfac.crew.ominix.io)  -> Deep research on Apple Vision Pro adoption and ecosystem 2026
 *
 * Run sequentially per spec (workers=1) so the 12-min trial budget per
 * mini is observed cleanly:
 *
 *   cd /Users/yuechen/home/octos/e2e
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 OCTOS_PROFILE=dspfac \
 *     npx playwright test tests/fleet-round2-spawn-only-ui.spec.ts \
 *     --reporter=list --workers=1 \
 *     --output=test-results-fleet-round2
 */

import { expect, test, type ConsoleMessage, type Page } from '@playwright/test';

import {
  SEL,
  createNewSession,
  getInput,
  getSendButton,
  login,
} from './live-browser-helpers';

interface BubbleSnapshot {
  index: number;
  textContent: string;
  textLen: number;
  hasMdLink: boolean;
  hasProgressChip: boolean;
  links: Array<{ text: string; href: string; download: string }>;
}

interface TrialConfig {
  mini: string;
  baseUrl: string;
  prompt: string;
  // Substring(s) that should appear in the final synthesized summary text
  // to recognise it (defensive — primarily counted by length).
  topicMarkers: RegExp;
}

const TRIALS: TrialConfig[] = [
  {
    mini: 'mini1-trial1',
    baseUrl: 'https://dspfac.crew.ominix.io',
    prompt: '深度研究全球量子计算商业化进展 2026',
    topicMarkers: /量子|quantum|2026/i,
  },
  {
    mini: 'mini2-trial2',
    baseUrl: 'https://dspfac.bot.ominix.io',
    prompt: '深度研究印度太空产业崛起 2026',
    topicMarkers: /印度|太空|space|india|2026/i,
  },
  {
    mini: 'mini3-trial3',
    baseUrl: 'https://dspfac.octos.ominix.io',
    prompt: 'Deep research on autonomous trucking commercial rollout 2026',
    topicMarkers: /trucking|autonomous|truck|2026/i,
  },
  {
    mini: 'mini5-trial4',
    baseUrl: 'https://dspfac.ocean.ominix.io',
    prompt: '深度研究全球海上风电产业 2026 前景',
    topicMarkers: /海上|风电|offshore|wind|2026/i,
  },
  {
    mini: 'mini1-trial5',
    baseUrl: 'https://dspfac.crew.ominix.io',
    prompt: 'Deep research on Apple Vision Pro adoption and ecosystem 2026',
    topicMarkers: /vision pro|apple|adoption|2026/i,
  },
];

const MAX_WAIT_MS = 12 * 60 * 1000;
const POLL_MS = 5_000;
const TEST_TIMEOUT_MS = MAX_WAIT_MS + 120_000;

for (const trial of TRIALS) {
  test.describe(`fleet-round2 ${trial.mini}`, () => {
    test.setTimeout(TEST_TIMEOUT_MS);
    test.use({ baseURL: trial.baseUrl });

    test(`spawn_only UI render path on ${trial.mini}`, async ({ page }, testInfo) => {
      const consoleLog: Array<{ type: string; text: string; at: number }> = [];
      const t0 = Date.now();
      const elapsed = () => Math.round((Date.now() - t0) / 1000);

      page.on('console', (msg: ConsoleMessage) => {
        if (msg.type() === 'error' || msg.type() === 'warning') {
          consoleLog.push({
            type: msg.type(),
            text: msg.text().slice(0, 1000),
            at: elapsed(),
          });
        }
      });
      page.on('pageerror', (err) => {
        consoleLog.push({
          type: 'pageerror',
          text: `${err.name}: ${err.message}`.slice(0, 1000),
          at: elapsed(),
        });
      });

      console.log(`[${trial.mini}] start @ ${trial.baseUrl}`);
      await login(page);
      await createNewSession(page);

      const input = getInput(page);
      const sendBtn = getSendButton(page);
      await input.fill(trial.prompt);
      await sendBtn.click();

      // ---- t=15s kickoff screenshot ----
      await page.waitForTimeout(15_000);
      const kickoffShot = testInfo.outputPath(`${trial.mini}-01-kickoff-15s.png`);
      await page.screenshot({ path: kickoffShot, fullPage: true });
      const kickoffSnap = await captureBubbles(page);
      const kickoffAckSeen = kickoffSnap.some((s) => isSpawnAckBubble(s.textContent));
      const kickoffProgressChip = kickoffSnap.some((s) => s.hasProgressChip);
      console.log(
        `[${trial.mini}] kickoff(15s) bubbles=${kickoffSnap.length} ackBubbleRendered=${kickoffAckSeen} progressChip=${kickoffProgressChip}`,
      );
      for (const s of kickoffSnap) {
        console.log(
          `[${trial.mini}]   k-bubble[${s.index}] len=${s.textLen} md=${s.hasMdLink} chip=${s.hasProgressChip} text="${s.textContent.slice(0, 220).replace(/\s+/g, ' ')}"`,
        );
      }

      const ackBubbleCount = kickoffSnap.length;
      let landed: { duration: number; snap: BubbleSnapshot[] } | null = null;
      let last: BubbleSnapshot[] = kickoffSnap;
      let midShotTaken = false;
      let tenMinShotTaken = false;
      const start = Date.now();

      while (Date.now() - start < MAX_WAIT_MS) {
        await page.waitForTimeout(POLL_MS);
        last = await captureBubbles(page);
        const ms = Date.now() - start;
        const newBubble = last.length > ackBubbleCount;
        const anyMd = last.some((s) => s.hasMdLink);
        const longText = last.some((s) => s.textLen >= 1500);

        // ---- t=4min mid-wait screenshot ----
        if (!midShotTaken && ms >= 4 * 60 * 1000) {
          midShotTaken = true;
          const midShot = testInfo.outputPath(`${trial.mini}-02-mid-4min.png`);
          await page.screenshot({ path: midShot, fullPage: true });
          console.log(
            `[${trial.mini}] mid(4m) bubbles=${last.length} md=${anyMd} longText=${longText}`,
          );
        }

        // ---- t=10min screenshot ----
        if (!tenMinShotTaken && ms >= 10 * 60 * 1000) {
          tenMinShotTaken = true;
          const tenShot = testInfo.outputPath(`${trial.mini}-03-t10min.png`);
          await page.screenshot({ path: tenShot, fullPage: true });
          console.log(
            `[${trial.mini}] t10m bubbles=${last.length} md=${anyMd} longText=${longText}`,
          );
        }

        if (newBubble || anyMd) {
          // Allow envelope + per-file rows to settle.
          await page.waitForTimeout(4_000);
          last = await captureBubbles(page);
          landed = { duration: Date.now() - start, snap: last };
          break;
        }

        if (ms % 60_000 < POLL_MS) {
          console.log(
            `[${trial.mini}] waiting... t+${Math.round(ms / 1000)}s bubbles=${last.length} md=${anyMd}`,
          );
        }
      }

      const finalShot = testInfo.outputPath(`${trial.mini}-04-final.png`);
      await page.screenshot({ path: finalShot, fullPage: true });

      // ---- Verdict ----
      let summaryRendered = false;
      let summaryLen = 0;
      let mdAttachment = false;
      if (landed) {
        for (const s of landed.snap) {
          if (s.hasMdLink) mdAttachment = true;
          if (s.textLen > 1500 && trial.topicMarkers.test(s.textContent)) {
            summaryRendered = true;
            summaryLen = Math.max(summaryLen, s.textLen);
          }
        }
      } else {
        for (const s of last) {
          if (s.hasMdLink) mdAttachment = true;
          if (s.textLen > 1500 && trial.topicMarkers.test(s.textContent)) {
            summaryRendered = true;
            summaryLen = Math.max(summaryLen, s.textLen);
          }
        }
      }

      const durationSec = landed ? Math.round(landed.duration / 1000) : MAX_WAIT_MS / 1000;
      const verdictLine = JSON.stringify({
        mini: trial.mini,
        baseUrl: trial.baseUrl,
        bundle: 'index-B_vbnr5m.js',
        prompt: trial.prompt,
        durationSec,
        landed: !!landed,
        kickoffAckBubbleRendered: kickoffAckSeen,
        kickoffProgressChipOnly: !kickoffAckSeen && kickoffProgressChip,
        summaryBubbleRendered: summaryRendered,
        summaryLen,
        mdAttachment,
        bubbleCountFinal: (landed?.snap || last).length,
        kickoffShot,
        finalShot,
        consoleErrors: consoleLog.filter((e) => e.type === 'error' || e.type === 'pageerror').length,
        consoleWarnings: consoleLog.filter((e) => e.type === 'warning').length,
      });
      console.log(`[${trial.mini}] VERDICT ${verdictLine}`);

      // Dump final bubbles for debugging.
      for (const s of landed?.snap || last) {
        console.log(
          `[${trial.mini}]   f-bubble[${s.index}] len=${s.textLen} md=${s.hasMdLink} chip=${s.hasProgressChip} text="${s.textContent.slice(0, 400).replace(/\s+/g, ' ')}"`,
        );
      }

      // Dump top console errors.
      const errs = consoleLog.filter(
        (e) => e.type === 'error' || e.type === 'pageerror',
      );
      console.log(`[${trial.mini}] CONSOLE_ERRS count=${errs.length}`);
      for (const e of errs.slice(0, 10)) {
        console.log(
          `[${trial.mini}]   [${e.type}@${e.at}s] ${e.text.replace(/\s+/g, ' ').slice(0, 300)}`,
        );
      }

      // Soft assertion — we want every signal but don't want the spec to
      // fail-fast on the first hung mini (which would block the remaining
      // trials). Just verify the harness itself worked.
      expect((landed?.snap || last).length, 'no bubbles at all').toBeGreaterThan(0);
    });
  });
}

function isSpawnAckBubble(text: string): boolean {
  if (!text) return false;
  const t = text.replace(/\s+/g, ' ').trim();
  if (t.length > 400) return false; // ack-only bodies are short
  return (
    /Background\s+work\s+started\s+for/i.test(t) ||
    /已在后台启动/.test(t) ||
    /已在後台啟動/.test(t) ||
    /started\s+in\s+(?:the\s+)?background/i.test(t) ||
    /running\s+in\s+(?:the\s+)?background/i.test(t)
  );
}

async function captureBubbles(page: Page): Promise<BubbleSnapshot[]> {
  return page.evaluate(() => {
    const out: Array<{
      index: number;
      textContent: string;
      textLen: number;
      hasMdLink: boolean;
      hasProgressChip: boolean;
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
      // Detect the inline progress chip — "<tool>: running" / "<tool>:
      // completed" / similar. The chip is rendered as small chrome in the
      // legacy bundle; the bug we're hunting is when only THIS appears
      // without the explicit kickoff text bubble.
      const chipMatch = /\brun_pipeline\b[\s:]+(running|pending|started|completed)\b/i.test(
        text,
      );
      out.push({
        index: idx,
        textContent: text,
        textLen: text.length,
        hasMdLink: hasMd,
        hasProgressChip: chipMatch,
        links,
      });
    });
    return out;
  });
}
