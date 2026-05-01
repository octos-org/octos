/**
 * Live deep_search quality gate (W3.C1).
 *
 * Validates on a real canary that the user-facing report delivered after
 * a deep research run is a synthesized, multi-paragraph document with
 * source citations — NOT a raw Bing/Brave dump.
 *
 * Usage:
 *
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/live-deep-search-quality.spec.ts
 *
 * What this test asserts:
 *   1. Submit a deep_research-triggering prompt and wait for the assistant
 *      to deliver a `_report.md` link or attach the file inline.
 *   2. Fetch the delivered report content via the chat thread (rendered
 *      markdown) or directly via the file API.
 *   3. The report MUST contain a `## Synthesis` section with at least 2
 *      paragraphs of prose (verified via paragraph break count).
 *   4. The report MUST contain at least 3 `[N]` citation markers, and
 *      every citation must resolve to a `### Source [N]:` entry in the
 *      Sources section.
 *   5. The report MUST NOT be a verbatim raw search dump (no large
 *      contiguous numbered list of "Title\n   url\n   snippet" triples
 *      as the only content).
 */

import { expect, test, type Page } from '@playwright/test';

import {
  SEL,
  createNewSession,
  getInput,
  getSendButton,
  login,
} from './live-browser-helpers';

const QUALITY_PROMPT =
  process.env.OCTOS_DEEP_SEARCH_QUERY ||
  'Do a deep research on the latest developments in Rust async runtimes in 2026. Run the deep_search pipeline directly.';

const PER_RUN_TIMEOUT_MS = 12 * 60 * 1000;
const POLL_INTERVAL_MS = 4_000;

test.describe('W3.C1 deep_search quality gate', () => {
  test.setTimeout(PER_RUN_TIMEOUT_MS + 60_000);

  test.beforeEach(async ({ page }) => {
    await login(page);
    await createNewSession(page);
  });

  test('delivered report is synthesized prose with [N] citations', async ({
    page,
  }) => {
    await getInput(page).fill(QUALITY_PROMPT);
    await getSendButton(page).click();

    const reportText = await waitForReportContent(page, PER_RUN_TIMEOUT_MS);
    expect(reportText.length).toBeGreaterThan(500);

    // Structural assertions.
    expect(
      hasSynthesisSection(reportText),
      `expected '## Synthesis' or italic headline section. report head: ${reportText.slice(0, 400)}`,
    ).toBe(true);

    const paragraphs = paragraphCount(reportText);
    expect(
      paragraphs,
      `expected at least 2 paragraphs of synthesis prose, got ${paragraphs}. ` +
        `report head: ${reportText.slice(0, 400)}`,
    ).toBeGreaterThanOrEqual(2);

    const citations = extractCitationIndexes(reportText);
    expect(
      citations.size,
      `expected at least 3 unique [N] citations, got ${citations.size}.`,
    ).toBeGreaterThanOrEqual(3);

    // Each citation must have a corresponding `### Source [N]:` header.
    const sourceIndexes = extractSourceIndexes(reportText);
    for (const idx of citations) {
      expect(
        sourceIndexes.has(idx),
        `citation [${idx}] does not match any '### Source [${idx}]:' entry. ` +
          `sources: ${[...sourceIndexes].join(',')}`,
      ).toBe(true);
    }

    // Negative check: the report MUST NOT be only the raw dump. Heuristic:
    // if 80%+ of non-sources content is "n. Title\n   URL\n   snippet" the
    // synthesis didn't run.
    expect(
      isLikelyRawDump(reportText),
      `report looks like a raw search dump rather than synthesized prose:\n` +
        reportText.slice(0, 600),
    ).toBe(false);
  });
});

/**
 * Wait for the assistant to deliver a deep_search report. We try three
 * extraction paths in priority order:
 *
 * 1. Chat thread contains a markdown rendering of the report. Look for
 *    the canonical `# Deep Research:` H1 plus an attached or linked
 *    `_report.md` reference.
 * 2. Latest assistant message contains a fenced markdown block we can
 *    inspect verbatim.
 * 3. An assistant bubble exposes a `.md` link/attachment (the
 *    spawn_only delivery path post-PR #688: `run_pipeline` runs in the
 *    background and the SSE `done` event fires when the LLM EndTurns
 *    after the spawn-ack, NOT when the artifact is produced. The
 *    actual `_report.md` is delivered as a media attachment 1-3 min
 *    later via a separate assistant bubble — see issue #731). When we
 *    find that anchor, we fetch its content using the browser's auth
 *    token so the assertions still run against the report body.
 */
async function waitForReportContent(page: Page, timeoutMs: number): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  let lastErr: string | undefined;
  while (Date.now() < deadline) {
    // Look at the rendered chat thread text for the report content.
    const threadText = await page.evaluate(() => {
      const root = document.querySelector('[data-testid="chat-thread"]');
      return root?.textContent ?? '';
    });
    if (threadText.includes('# Deep Research:') || threadText.includes('## Synthesis')) {
      return threadText;
    }

    // Also probe assistant message containers individually so we don't
    // miss the report when it's wrapped in a code block.
    const lastAssistant = await page.evaluate((sel) => {
      const nodes = Array.from(
        document.querySelectorAll(sel),
      ) as HTMLElement[];
      const last = nodes.length ? nodes[nodes.length - 1] : null;
      return last?.textContent ?? '';
    }, SEL.assistantBubble || '[data-role="assistant"]');
    if (
      lastAssistant.includes('## Synthesis') ||
      lastAssistant.includes('# Deep Research:')
    ) {
      return lastAssistant;
    }

    // Spawn_only delivery path (issue #731): the report lands as a
    // media attachment in a *later* assistant bubble. Look for any
    // SAME-ORIGIN anchor with an `.md` href and fetch it via the same
    // auth token the SPA holds in localStorage. Restricting to
    // same-origin avoids matching external citation links (e.g.
    // `https://github.com/.../README.md`) that the LLM may emit in
    // its synthesis prose, and prevents leaking the bearer token to
    // a third-party host. We pick the most-recent matching anchor so
    // that re-runs in the same session pick the latest report.
    const mdHref = await page.evaluate(() => {
      const re = /\.md(?:$|[?#])/i;
      const here = window.location.origin;
      const anchors = Array.from(
        document.querySelectorAll(
          "[data-testid='assistant-message'] a[href]",
        ),
      ) as HTMLAnchorElement[];
      const matches = anchors.filter((a) => {
        const href = a.href || '';
        if (!re.test(href)) return false;
        try {
          return new URL(href, here).origin === here;
        } catch {
          // Relative URLs that can't be parsed against the doc base
          // would have already been resolved by `a.href` getter;
          // anything that throws here is malformed and we skip.
          return false;
        }
      });
      return matches.length ? matches[matches.length - 1].href : '';
    });
    if (mdHref) {
      const fetched = await page.evaluate(async (href) => {
        try {
          const token =
            localStorage.getItem('octos_session_token') ||
            localStorage.getItem('octos_auth_token') ||
            '';
          const profile = localStorage.getItem('selected_profile') || '';
          const headers: Record<string, string> = {};
          if (token) headers.Authorization = `Bearer ${token}`;
          if (profile) headers['X-Profile-Id'] = profile;
          const resp = await fetch(href, {
            headers,
            credentials: 'include',
          });
          if (!resp.ok) return { ok: false, status: resp.status, text: '' };
          const text = await resp.text();
          return { ok: true, status: resp.status, text };
        } catch (err) {
          return { ok: false, status: -1, text: String(err) };
        }
      }, mdHref);
      if (fetched.ok && fetched.text && fetched.text.length > 200) {
        return fetched.text;
      }
      lastErr = `fetched ${mdHref}: ok=${fetched.ok} status=${fetched.status} bodyLen=${fetched.text.length}`;
    }

    await page.waitForTimeout(POLL_INTERVAL_MS);
  }
  throw new Error(
    `Timed out after ${(timeoutMs / 1000).toFixed(0)}s waiting for deep_search report content` +
      (lastErr ? ` (last attachment fetch: ${lastErr})` : ''),
  );
}

function hasSynthesisSection(text: string): boolean {
  // Either an explicit "## Synthesis" heading OR a "# Deep Research" with
  // an italicized headline + multi-paragraph body. Tolerant: the LLM may
  // misformat the section name.
  if (text.includes('## Synthesis')) return true;
  // The fallback `## Overview\n\n_LLM synthesis unavailable_` would FAIL
  // this check intentionally — that path means C1 didn't run.
  return false;
}

function paragraphCount(text: string): number {
  // Count blank-line-separated paragraph blocks within the synthesis
  // section. We anchor on '## Synthesis' to avoid counting source-listing
  // paragraphs.
  const start = text.indexOf('## Synthesis');
  if (start < 0) return 0;
  const end = text.indexOf('## Sources', start);
  const slice = end > 0 ? text.slice(start, end) : text.slice(start);
  // Drop the heading line itself.
  const body = slice.replace(/^##\s+Synthesis\s*\n/, '');
  return body
    .split(/\n\s*\n/)
    .map((p) => p.trim())
    .filter((p) => p.length > 30) // ignore headlines, italics, single-word lines
    .length;
}

function extractCitationIndexes(text: string): Set<number> {
  const out = new Set<number>();
  // Only count citations within the synthesis section, not the source
  // listing (where `[N]` appears as part of `### Source [N]:`).
  const start = text.indexOf('## Synthesis');
  const end = text.indexOf('## Sources');
  if (start < 0) return out;
  const slice = end > 0 ? text.slice(start, end) : text.slice(start);
  // Match `[N]` where N is 1-3 digits, NOT inside `### Source [...]`.
  const matches = slice.matchAll(/\[(\d{1,3})\]/g);
  for (const m of matches) {
    const n = Number(m[1]);
    if (Number.isFinite(n) && n >= 1) {
      out.add(n);
    }
  }
  return out;
}

function extractSourceIndexes(text: string): Set<number> {
  const out = new Set<number>();
  const matches = text.matchAll(/###\s+Source\s+\[(\d{1,3})\]/g);
  for (const m of matches) {
    const n = Number(m[1]);
    if (Number.isFinite(n) && n >= 1) {
      out.add(n);
    }
  }
  return out;
}

/**
 * Heuristic: if 4+ consecutive lines match the v1 raw search dump pattern
 * (`n. Title\n   URL\n   snippet`) and there is NO `## Synthesis` section,
 * we conclude the LLM synthesis path didn't run.
 */
function isLikelyRawDump(text: string): boolean {
  if (hasSynthesisSection(text)) return false;
  const lines = text.split('\n');
  let consecutive = 0;
  let maxConsecutive = 0;
  const numberedLine = /^\s*\d+\.\s+\S/;
  for (const line of lines) {
    if (numberedLine.test(line)) {
      consecutive += 1;
      maxConsecutive = Math.max(maxConsecutive, consecutive);
    } else if (line.trim().length === 0) {
      // blank line is fine, doesn't reset
    } else {
      consecutive = 0;
    }
  }
  return maxConsecutive >= 4;
}
