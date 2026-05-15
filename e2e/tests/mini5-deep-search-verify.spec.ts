/**
 * One-off verification spec: prove that `deep_search` is no longer silently
 * broken on mini5 (dspfac.ocean.ominix.io) after the AMFI/codesign repair.
 *
 * Usage:
 *
 *   OCTOS_TEST_URL=https://dspfac.ocean.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/mini5-deep-search-verify.spec.ts \
 *     --headed=false --reporter=list
 *
 * What this asserts:
 *  - Login + new session succeed on the deployed SPA.
 *  - Sending the Chinese deep_search prompt produces SOME observable signal
 *    within 4 minutes — either a synthesis-bearing report or an explicit
 *    error surfaced in the chat. Silent failure (empty bubble, infinite
 *    spinner, or a "deep_searchx2/x3" chip with no body) FAILS the test.
 *  - A screenshot is dumped to `test-results-mini5-deep-search/` so the
 *    caller can eyeball what the user would see.
 */

import { expect, test, type Page } from '@playwright/test';

import {
  SEL,
  createNewSession,
  getInput,
  getSendButton,
  isSpawnAckOnly,
  login,
  normalizeBubbleText,
} from './live-browser-helpers';

const PROMPT = process.env.MINI5_PROMPT || '深度研究美国和伊朗和谈的前景';
const MAX_WAIT_MS = 5 * 60 * 1000;
const POLL_MS = 5_000;

test.describe('mini5 deep_search post-codesign verification', () => {
  test.setTimeout(MAX_WAIT_MS + 60_000);

  test('deep_search prompt produces a substantive response or surfaced error', async ({
    page,
  }, testInfo) => {
    await login(page);
    await createNewSession(page);

    const input = getInput(page);
    const sendBtn = getSendButton(page);
    await input.fill(PROMPT);
    await sendBtn.click();

    const verdict = await waitForVerdict(page, MAX_WAIT_MS);

    // Always dump a screenshot + the latest assistant text so the operator
    // can see what the user actually saw.
    const screenshotPath = testInfo.outputPath('final-chat.png');
    await page
      .screenshot({ path: screenshotPath, fullPage: true })
      .catch(() => null);
    console.log(`[mini5-deep-search-verify] screenshot: ${screenshotPath}`);

    const threadText = await dumpThreadText(page);
    console.log(
      `[mini5-deep-search-verify] thread head:\n${threadText.slice(0, 1200)}`,
    );

    const toolChips = await dumpToolChips(page);
    console.log(`[mini5-deep-search-verify] tool chips: ${JSON.stringify(toolChips)}`);

    console.log(`[mini5-deep-search-verify] verdict: ${verdict.kind}`);
    console.log(`[mini5-deep-search-verify] last assistant: ${verdict.lastAssistantText.slice(0, 600)}`);

    // Success criteria per task brief:
    //   - "substantive Chinese-language research report" OR
    //   - "explicit error message surfaces in the chat"
    // are BOTH valid outcomes.
    // Failure criteria:
    //   - "deep_search x N" with no body
    //   - infinite spinner
    //   - silent tool failure with no diagnostic
    expect(
      verdict.kind === 'report' || verdict.kind === 'error',
      `expected report or explicit error, got '${verdict.kind}'. ` +
        `Last assistant text: ${verdict.lastAssistantText.slice(0, 400)}`,
    ).toBe(true);
  });
});

interface Verdict {
  kind: 'report' | 'error' | 'silent-failure' | 'timeout';
  lastAssistantText: string;
}

async function waitForVerdict(page: Page, timeoutMs: number): Promise<Verdict> {
  const deadline = Date.now() + timeoutMs;
  let lastAssistant = '';
  let lastSeenStreaming = false;

  while (Date.now() < deadline) {
    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible()
      .catch(() => false);
    lastSeenStreaming = lastSeenStreaming || streaming;

    const bubbles = await page
      .locator(SEL.assistantMessage)
      .allTextContents()
      .catch(() => []);
    lastAssistant = bubbles.length ? bubbles[bubbles.length - 1] : '';

    // Look across ALL assistant bubbles for either a report or an error.
    for (const raw of bubbles) {
      const t = raw || '';
      if (looksLikeReport(t)) {
        return { kind: 'report', lastAssistantText: t };
      }
      if (looksLikeExplicitError(t)) {
        return { kind: 'error', lastAssistantText: t };
      }
    }

    // Also check for an attached `.md` link — the spawn_only result delivery
    // path. Fetch its body to confirm it's a synthesized report.
    const mdHref = await sameOriginMdHref(page);
    if (mdHref) {
      const body = await fetchMdBody(page, mdHref);
      if (body && body.length > 500) {
        return {
          kind: 'report',
          lastAssistantText: `[fetched ${mdHref}, ${body.length} chars]\n${body.slice(0, 800)}`,
        };
      }
    }

    if (!streaming && bubbles.length > 0) {
      // No more streaming. Either we got a final bubble or the assistant
      // stopped early.
      const normalized = normalizeBubbleText(lastAssistant);
      if (normalized.length > 0 && !isSpawnAckOnly(lastAssistant)) {
        // Settled state with a non-empty, non-ack body.
        if (looksLikeReport(lastAssistant) || looksLikeExplicitError(lastAssistant)) {
          // Already handled above — defensive fallthrough.
        }
        // Otherwise: settled but no clear report/error signal. Continue
        // polling — the spawn_only result bubble may still be in flight.
      }
    }

    await page.waitForTimeout(POLL_MS);
  }

  // Timed out. Classify what we ended up with.
  const normalized = normalizeBubbleText(lastAssistant);
  if (normalized.length === 0 || isSpawnAckOnly(lastAssistant)) {
    return { kind: 'silent-failure', lastAssistantText: lastAssistant };
  }
  return { kind: 'timeout', lastAssistantText: lastAssistant };
}

function looksLikeReport(text: string): boolean {
  if (!text) return false;
  // Canonical deep_search synthesis markers (English or Chinese).
  if (text.includes('## Synthesis')) return true;
  if (text.includes('# Deep Research:')) return true;
  if (text.includes('# 深度研究') || text.includes('## 综合')) return true;
  // Heuristic: a long bubble with citation markers is a strong signal.
  if (text.length > 600 && /\[\d+\]/.test(text)) return true;
  // Or a long Chinese body that mentions the topic substantively (>1.5kB).
  if (text.length > 1500 && /(美国|伊朗|和谈|核协议|JCPOA|内塔尼亚胡|哈梅内伊)/.test(text)) {
    return true;
  }
  return false;
}

function looksLikeExplicitError(text: string): boolean {
  if (!text) return false;
  // The new error surfacing path from the M9 envelope work + skill
  // execution failure messages. Match anything that looks like a clear
  // diagnostic the user can act on.
  const patterns = [
    /\bError\b[:\s]/,
    /\berror\b[:\s].{4,}/i,
    /failed to (execute|run|spawn|start)/i,
    /skill .{0,40} failed/i,
    /tool .{0,40} failed/i,
    /exit (?:code|status) [1-9]/i,
    /code signing|amfi|killed by signal/i,
    /\bSIGKILL\b/i,
    /unable to (find|load|launch)/i,
    /没有.{0,20}(权限|可执行|可用)/,
    /失败|出错|错误.{0,40}/,
  ];
  return patterns.some((p) => p.test(text));
}

async function sameOriginMdHref(page: Page): Promise<string> {
  return page.evaluate(() => {
    const re = /\.md(?:$|[?#])/i;
    const here = window.location.origin;
    const anchors = Array.from(
      document.querySelectorAll("[data-testid='assistant-message'] a[href]"),
    ) as HTMLAnchorElement[];
    const matches = anchors.filter((a) => {
      const href = a.href || '';
      if (!re.test(href)) return false;
      try {
        return new URL(href, here).origin === here;
      } catch {
        return false;
      }
    });
    return matches.length ? matches[matches.length - 1].href : '';
  });
}

async function fetchMdBody(page: Page, href: string): Promise<string | null> {
  const fetched = await page.evaluate(async (url) => {
    try {
      const token =
        localStorage.getItem('octos_session_token') ||
        localStorage.getItem('octos_auth_token') ||
        '';
      const profile = localStorage.getItem('selected_profile') || '';
      const headers: Record<string, string> = {};
      if (token) headers.Authorization = `Bearer ${token}`;
      if (profile) headers['X-Profile-Id'] = profile;
      const resp = await fetch(url, { headers, credentials: 'include' });
      if (!resp.ok) return null;
      return await resp.text();
    } catch {
      return null;
    }
  }, href);
  return fetched;
}

async function dumpThreadText(page: Page): Promise<string> {
  const texts = await page
    .locator(
      "[data-testid='user-message'], [data-testid='assistant-message']",
    )
    .allTextContents()
    .catch(() => []);
  return texts.join('\n----\n');
}

async function dumpToolChips(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    // Try a few selectors. The SPA renders tool events as chips with
    // various class names depending on the build. Capture anything
    // matching `deep_search` or known tool-chip wrappers.
    const out: string[] = [];
    const candidates = [
      "[data-testid='tool-chip']",
      "[data-testid*='tool']",
      ".tool-chip",
      ".tool-event",
    ];
    for (const sel of candidates) {
      document.querySelectorAll(sel).forEach((node) => {
        const t = (node as HTMLElement).innerText || '';
        if (t.trim()) out.push(`${sel}: ${t.trim().slice(0, 200)}`);
      });
    }
    // Also: any element whose visible text mentions deep_search.
    document.querySelectorAll('*').forEach((node) => {
      const el = node as HTMLElement;
      if (
        el.childElementCount === 0 &&
        el.innerText &&
        /deep_search/i.test(el.innerText) &&
        el.innerText.length < 200
      ) {
        out.push(`text: ${el.innerText.trim()}`);
      }
    });
    return Array.from(new Set(out)).slice(0, 50);
  });
}
