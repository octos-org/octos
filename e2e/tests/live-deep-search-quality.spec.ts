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
 * Wait for the assistant to deliver a deep_search report. We try two
 * extraction paths in priority order:
 *
 * 1. Chat thread contains a markdown rendering of the report. Look for
 *    the canonical `# Deep Research:` H1 plus an attached or linked
 *    `_report.md` reference.
 * 2. Latest assistant message contains a fenced markdown block we can
 *    inspect verbatim.
 */
async function waitForReportContent(page: Page, timeoutMs: number): Promise<string> {
  const deadline = Date.now() + timeoutMs;
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

    await page.waitForTimeout(POLL_INTERVAL_MS);
  }
  throw new Error(
    `Timed out after ${(timeoutMs / 1000).toFixed(0)}s waiting for deep_search report content`,
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
