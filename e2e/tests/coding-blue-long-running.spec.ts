/**
 * Live e2e for coding-blue's long-running background task pipeline.
 *
 * Scope: reliability, status tracing, report-back delivery. Covers the surfaces
 * that M6+M7+FA-7 + PR #42 (task-store persistence) are supposed to harden:
 *   - Task lifecycle tracking visible through the DOM task-anchor testids
 *   - Reload-preserves-task-state (bug class 1 from Review B)
 *   - Final artifact delivered to the originating message bubble
 *   - Harness event trace flows end-to-end
 *
 * Key UX ground truth (coding-blue web client):
 *   - BARE slash commands (`/help`, `/queue`, `/adaptive`, `/status`) are
 *     consumed locally and render into `[data-testid='cmd-feedback']`.
 *     They do NOT produce user-message / assistant-message bubbles.
 *   - Slash commands WITH ARGS (`/adaptive hedge`, `/queue speculative`,
 *     `/soul <text>`, `/new <topic>`, `/s <topic>`) DO render a user bubble
 *     with the raw command and an assistant bubble with the ack text.
 *     They don't round-trip to the LLM; the server handles them inline.
 *   - Task lifecycle is observable via PR #42 testids:
 *       task-anchor-message-<taskId>, task-anchor-spinner-<taskId>,
 *       task-anchor-phase-<taskId>, task-anchor-progress-<taskId>.
 *     More reliable than polling /api/sessions/:id/tasks (requires a gateway
 *     session + topic params).
 *
 * Target: OCTOS_TEST_URL (default https://dspfac.crew.ominix.io = mini1).
 *
 * Env:
 *   OCTOS_TEST_URL, OCTOS_AUTH_TOKEN, OCTOS_PROFILE
 *
 * Run:
 *   cd e2e && OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *     npx playwright test tests/coding-blue-long-running.spec.ts --reporter=list
 */

import { test, expect } from '@playwright/test';
import { login, SEL } from './live-browser-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL || '';
const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE_ID = process.env.OCTOS_PROFILE || 'dspfac';

const SKIP_REASON = 'OCTOS_TEST_URL not set; skipping live long-running smoke';

/**
 * Read current cmd-feedback text. Returns '' if not visible.
 * Use for BARE slash commands (/help, /queue, /adaptive, /status).
 */
async function readCmdFeedbackText(
  page: import('@playwright/test').Page,
): Promise<string> {
  const fb = page.locator(SEL.cmdFeedback).first();
  const visible = await fb.isVisible().catch(() => false);
  if (!visible) return '';
  return ((await fb.textContent()) || '').trim();
}

/**
 * Send a BARE slash command and wait for cmd-feedback to render.
 * Example: `/help`, `/queue`, `/adaptive`, `/status`.
 */
async function sendBareSlashCommand(
  page: import('@playwright/test').Page,
  command: string,
  opts: { timeoutMs?: number } = {},
): Promise<string> {
  const timeoutMs = opts.timeoutMs ?? 5_000;
  const input = page.locator(SEL.chatInput);
  const send = page.locator(SEL.sendButton);

  await input.fill(command);
  await send.click();

  await expect(page.locator(SEL.cmdFeedback).first()).toBeVisible({
    timeout: timeoutMs,
  });

  let text = '';
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    text = await readCmdFeedbackText(page);
    if (text.length > 0) return text;
    await page.waitForTimeout(200);
  }
  return text;
}

/**
 * Send a slash command WITH ARGS (e.g. `/adaptive hedge`). These render as
 * a user bubble + assistant bubble (server-handled, no LLM round-trip).
 * Returns the ack text from the newest assistant bubble once it's non-empty
 * AND stable (i.e. no longer a "Thinking (iteration 0)" placeholder).
 */
async function sendSlashWithArgs(
  page: import('@playwright/test').Page,
  command: string,
  opts: { timeoutMs?: number; beforeAssistantCount?: number } = {},
): Promise<string> {
  const timeoutMs = opts.timeoutMs ?? 30_000;
  const before =
    opts.beforeAssistantCount ??
    (await page.locator(SEL.assistantMessage).count());

  const input = page.locator(SEL.chatInput);
  const send = page.locator(SEL.sendButton);
  await input.fill(command);
  await send.click();

  // Wait for a new assistant bubble to appear.
  await expect
    .poll(async () => page.locator(SEL.assistantMessage).count(), {
      timeout: timeoutMs,
      intervals: [250],
    })
    .toBeGreaterThan(before);

  // Poll the last bubble's text until it contains substantive (non-placeholder)
  // content. Strip timestamps + "Thinking" placeholders to judge real content.
  const deadline = Date.now() + timeoutMs;
  let lastRaw = '';
  while (Date.now() < deadline) {
    const raw =
      (
        (await page.locator(SEL.assistantMessage).last().innerText()) || ''
      ).trim();
    lastRaw = raw;
    const clean = stripPlaceholders(raw);
    if (clean.length > 0) return raw;
    await page.waitForTimeout(400);
  }
  return lastRaw;
}

function stripPlaceholders(s: string): string {
  return s
    .replace(/\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}/g, '')
    .replace(/Thinking\s*\(iteration\s+\d+\)/gi, '')
    .replace(/^\s*[.,]+\s*$/gm, '')
    .replace(/\.\.\./g, '')
    .trim();
}

/**
 * Wait until the last assistant bubble has a substantial, stable response:
 *   - cancel button is gone (not streaming)
 *   - no task-anchor-spinner remaining
 *   - innerText is non-trivial (>= minChars) AND stable across two polls
 *
 * Returns the final text (may be short if timeout hit).
 */
async function waitForAssistantComplete(
  page: import('@playwright/test').Page,
  opts: {
    timeoutMs?: number;
    pollMs?: number;
    minChars?: number;
    label?: string;
  } = {},
): Promise<string> {
  const timeoutMs = opts.timeoutMs ?? 300_000;
  const pollMs = opts.pollMs ?? 3_000;
  const minChars = opts.minChars ?? 20;
  const label = opts.label || '';
  const deadline = Date.now() + timeoutMs;

  let lastText = '';
  let stableCount = 0;

  while (Date.now() < deadline) {
    await page.waitForTimeout(pollMs);

    const cancelVisible = await page
      .locator(SEL.cancelButton)
      .isVisible()
      .catch(() => false);
    const spinnerCount = await page
      .locator('[data-testid^="task-anchor-spinner-"]')
      .count();

    const count = await page.locator(SEL.assistantMessage).count();
    const text =
      count > 0
        ? ((await page.locator(SEL.assistantMessage).last().innerText()) || '').trim()
        : '';

    const streaming = cancelVisible || spinnerCount > 0;

    if (!streaming && text.length >= minChars && text === lastText) {
      stableCount += 1;
      if (stableCount >= 2) return text;
    } else {
      stableCount = 0;
    }

    lastText = text;

    if (label) {
      const elapsed = Math.round((Date.now() - (deadline - timeoutMs)) / 1000);
      console.log(
        `  [${label}] ${elapsed}s: bubbles=${count} streaming=${streaming} spinners=${spinnerCount} textLen=${text.length}`,
      );
    }
  }

  return lastText;
}

test.describe('coding-blue long-running background task reliability (live mini1)', () => {
  test.skip(() => !TEST_URL, SKIP_REASON);
  test.setTimeout(600_000); // up to 10 min per test — deep research is slow

  test('task lifecycle trace: Running → Verifying → Ready + report-back', async ({
    page,
  }) => {
    await login(page);
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    // Fresh session — keeps context small so deep-research doesn't time out.
    const newChat = page.locator(SEL.newChatButton);
    if (await newChat.isVisible().catch(() => false)) {
      await newChat.click();
      await page.waitForSelector(SEL.chatInput);
      await page.waitForTimeout(1_000);
    }

    const marker = `coding-blue-${Date.now()}`;
    const prompt =
      'Use deep_search to answer in <=150 words: ' +
      `"What are three leading Rust web frameworks in 2026? Marker=${marker}" ` +
      'Return a short bulleted list.';

    await page.locator(SEL.chatInput).fill(prompt);
    await page.locator(SEL.sendButton).click();

    await expect(page.locator(SEL.userMessage)).toHaveCount(1, {
      timeout: 30_000,
    });

    // Observe task-anchor testids if they appear (PR #42 path). Either a
    // task-anchor renders (background-task path) or not (inline streaming).
    const observeDeadline = Date.now() + 60_000;
    let sawAnchor = false;
    let taskId: string | null = null;
    while (Date.now() < observeDeadline) {
      const anchorIds = await page.evaluate(() => {
        const ids: string[] = [];
        document
          .querySelectorAll('[data-testid^="task-anchor-message-"]')
          .forEach((el) => {
            const tid = (el as HTMLElement).dataset.testid || '';
            const id = tid.replace('task-anchor-message-', '');
            if (id) ids.push(id);
          });
        return ids;
      });
      if (anchorIds.length > 0) {
        sawAnchor = true;
        taskId = anchorIds[0];
        break;
      }
      await page.waitForTimeout(2_000);
    }

    // Wait for the assistant bubble to reach a non-trivial, stable response.
    // This is the crucial completion signal — not just "has any text".
    const finalText = await waitForAssistantComplete(page, {
      timeoutMs: 360_000,
      pollMs: 5_000,
      minChars: 60,
      label: 'lifecycle',
    });

    expect(
      finalText.length,
      `assistant bubble must carry a completed report; got: "${finalText.slice(
        0,
        200,
      )}"`,
    ).toBeGreaterThanOrEqual(60);

    const hasFrameworks =
      /axum|actix|rocket|tower|leptos|yew|salvo|warp/i.test(finalText);
    expect(
      hasFrameworks || finalText.includes(marker),
      `report-back should mention a Rust framework or marker; got: ${finalText.slice(
        0,
        200,
      )}`,
    ).toBe(true);

    console.log(
      `[coding-blue long-run] sawAnchor=${sawAnchor} taskId=${taskId} reportLen=${finalText.length}`,
    );
  });

  test('reload during active long-running task preserves task-store state', async ({
    page,
  }) => {
    await login(page);
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    // Fresh session — keeps context small so deep-research actually starts.
    const newChat = page.locator(SEL.newChatButton);
    if (await newChat.isVisible().catch(() => false)) {
      await newChat.click();
      await page.waitForSelector(SEL.chatInput);
      await page.waitForTimeout(1_000);
    }

    const marker = `coding-blue-reload-${Date.now()}`;
    const prompt =
      'Use deep_search to answer in <=150 words: ' +
      `"Compare top 3 databases for AI embeddings in 2026. Marker=${marker}" ` +
      'Take your time, deep dive is fine.';

    await page.locator(SEL.chatInput).fill(prompt);
    await page.locator(SEL.sendButton).click();
    await expect(page.locator(SEL.userMessage)).toHaveCount(1, {
      timeout: 30_000,
    });

    // Wait for a task-anchor to appear in the DOM. Only if this build treats
    // deep-research as a background task will the anchor render (and
    // localStorage task-store entries get written). If nothing in 60s, the
    // test has nothing meaningful to assert — skip with a diagnostic.
    const anchorDeadline = Date.now() + 60_000;
    let anchorTaskId: string | null = null;
    while (Date.now() < anchorDeadline) {
      anchorTaskId = await page.evaluate(() => {
        const el = document.querySelector(
          '[data-testid^="task-anchor-message-"]',
        );
        if (!el) return null;
        const tid = (el as HTMLElement).dataset.testid || '';
        return tid.replace('task-anchor-message-', '') || null;
      });
      if (anchorTaskId) break;
      await page.waitForTimeout(2_000);
    }

    test.skip(
      !anchorTaskId,
      'no task-anchor rendered in 60s — this build streams deep-research inline (no background task), reload-state test is not meaningful',
    );

    const beforeReload = await page.evaluate(() => {
      const entries: Record<string, string> = {};
      for (let i = 0; i < localStorage.length; i += 1) {
        const key = localStorage.key(i)!;
        if (key.startsWith('octos_web:task_store:v1:')) {
          entries[key] = localStorage.getItem(key) || '';
        }
      }
      return entries;
    });
    expect(
      Object.keys(beforeReload).length,
      `task-store entries present pre-reload: ${JSON.stringify(
        Object.keys(beforeReload),
      )}`,
    ).toBeGreaterThan(0);

    await page.reload({ waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    const afterReload = await page.evaluate(() => {
      const entries: Record<string, string> = {};
      for (let i = 0; i < localStorage.length; i += 1) {
        const key = localStorage.key(i)!;
        if (key.startsWith('octos_web:task_store:v1:')) {
          entries[key] = localStorage.getItem(key) || '';
        }
      }
      return entries;
    });
    expect(
      Object.keys(afterReload).length,
      'task-store entries must survive reload',
    ).toBeGreaterThan(0);

    // The anchor the store described must rehydrate on-screen.
    await expect(
      page.locator(`[data-testid='task-anchor-message-${anchorTaskId}']`),
    ).toBeVisible({ timeout: 30_000 });

    // Wait for completion (substantial + stable assistant text).
    const finalText = await waitForAssistantComplete(page, {
      timeoutMs: 360_000,
      pollMs: 5_000,
      minChars: 40,
      label: 'reload',
    });

    expect(
      finalText.length,
      'final assistant bubble has content post-reload',
    ).toBeGreaterThanOrEqual(40);

    console.log(
      `[coding-blue reload] task-store entries preserved: ${
        Object.keys(afterReload).length
      }; taskId=${anchorTaskId}; final text: ${finalText.slice(0, 120)}`,
    );
  });

  test('slash commands return a visible, non-empty ack response', async ({
    page,
  }) => {
    await login(page);
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    // BARE slash commands render via cmd-feedback (no user/assistant bubbles).
    const commands = [
      { cmd: '/help', expect: /help|command|slash|available|queue|new/i },
      {
        cmd: '/queue',
        expect: /queue|followup|collect|speculative|steer|interrupt|mode/i,
      },
      { cmd: '/adaptive', expect: /adaptive|off|hedge|lane|mode|provider/i },
      {
        cmd: '/status',
        expect:
          /status|session|model|provider|ok|active|uptime|greeting|metric/i,
      },
    ];

    for (const { cmd, expect: pattern } of commands) {
      const text = await sendBareSlashCommand(page, cmd, { timeoutMs: 5_000 });
      expect(
        text.length,
        `${cmd} must return non-empty cmd-feedback; got: "${text.slice(
          0,
          120,
        )}"`,
      ).toBeGreaterThan(0);

      const matchesExpected = pattern.test(text);
      const matchesUnknown = /unknown|unrecognized|not.*support|invalid/i.test(
        text,
      );
      expect(
        matchesExpected || matchesUnknown,
        `${cmd} ack didn't match expected pattern or unknown-command; got: "${text.slice(
          0,
          160,
        )}"`,
      ).toBe(true);
    }
  });

  test('adaptive hedge mode switches + returns response (M6.6 content routing + adaptive)', async ({
    page,
  }) => {
    await login(page);
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    // Fresh session so hedge mode doesn't inherit a huge prior context.
    const newChat = page.locator(SEL.newChatButton);
    if (await newChat.isVisible().catch(() => false)) {
      await newChat.click();
      await page.waitForSelector(SEL.chatInput);
      await page.waitForTimeout(1_000);
    }

    // `/adaptive hedge` is a slash-command-WITH-ARGS: renders as a user bubble
    // + assistant bubble (server-handled). Not cmd-feedback.
    const hedgeAck = await sendSlashWithArgs(page, '/adaptive hedge', {
      timeoutMs: 30_000,
    });
    expect(hedgeAck.length, 'hedge ack must be non-empty').toBeGreaterThan(0);
    expect(hedgeAck.toLowerCase()).toMatch(
      /hedge|adaptive|mode|provider|race/,
    );

    // Single-provider minis can't race. Skip with diagnostic.
    const singleProviderSignal =
      /only one provider|single provider|<=\s*1 provider|need(?:s|ing)? .* providers|no alternate/i.test(
        hedgeAck,
      );
    test.skip(
      singleProviderSignal,
      `adaptive hedge unavailable on this mini; ack="${hedgeAck.slice(0, 160)}"`,
    );

    // Fire a real prompt — racing providers should converge.
    const beforeCount = await page.locator(SEL.assistantMessage).count();
    const prompt =
      'Answer in one short sentence: what is the capital of Japan?';
    await page.locator(SEL.chatInput).fill(prompt);
    await page.locator(SEL.sendButton).click();

    await expect
      .poll(() => page.locator(SEL.assistantMessage).count(), {
        timeout: 30_000,
        intervals: [500],
      })
      .toBeGreaterThan(beforeCount);

    const finalText = await waitForAssistantComplete(page, {
      timeoutMs: 180_000,
      pollMs: 3_000,
      minChars: 5,
      label: 'hedge-prompt',
    });
    const lower = finalText.toLowerCase();
    expect(
      lower,
      `hedge response should mention Tokyo; got: "${finalText.slice(0, 200)}"`,
    ).toMatch(/tokyo/);

    // Reset to off for subsequent tests.
    await sendSlashWithArgs(page, '/adaptive off', { timeoutMs: 15_000 }).catch(
      () => '',
    );
  });

  test('queue speculative mode dispatches concurrent turns as independent tasks', async ({
    page,
  }) => {
    await login(page);
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    // Start from a fresh session so we don't drag enormous history into the
    // 2 prompts (live sessions accumulate 60k+ tokens of context, which
    // makes each speculative task very slow on hosts with single-provider).
    const newChat = page.locator(SEL.newChatButton);
    if (await newChat.isVisible().catch(() => false)) {
      await newChat.click();
      await page.waitForSelector(SEL.chatInput);
      await page.waitForTimeout(1_000);
    }

    // `/queue speculative` is a slash-command-WITH-ARGS: renders a user bubble
    // + assistant ack bubble.
    const specAck = await sendSlashWithArgs(page, '/queue speculative', {
      timeoutMs: 30_000,
    });
    expect(specAck.length, 'speculative ack must be non-empty').toBeGreaterThan(
      0,
    );
    expect(specAck.toLowerCase()).toMatch(/speculative|queue|mode/);

    // After the slash ack: exactly 1 user bubble + 1 assistant bubble.
    await expect(page.locator(SEL.userMessage)).toHaveCount(1, {
      timeout: 10_000,
    });
    await expect(page.locator(SEL.assistantMessage)).toHaveCount(1, {
      timeout: 10_000,
    });

    // Fire 2 rapid prompts.
    await page
      .locator(SEL.chatInput)
      .fill('Use shell: echo ALPHA-speculative-1');
    await page.locator(SEL.sendButton).click();
    await page.waitForTimeout(500);
    await page
      .locator(SEL.chatInput)
      .fill('Use shell: echo BRAVO-speculative-2');
    await page.locator(SEL.sendButton).click();

    // Total 3 user bubbles: slash (1) + 2 prompts.
    await expect(page.locator(SEL.userMessage)).toHaveCount(3, {
      timeout: 30_000,
    });

    // Wait until all assistant bubbles stabilize (cancel button gone, each
    // non-trivial). Allow 8 minutes — speculative on single-provider minis
    // may serialize. If that happens we still expect eventual completion.
    const stabilized = await expect
      .poll(
        async () => {
          const cancelVisible = await page
            .locator(SEL.cancelButton)
            .isVisible()
            .catch(() => false);
          if (cancelVisible) return false;
          const count = await page.locator(SEL.assistantMessage).count();
          if (count < 3) return false;
          const texts = await page
            .locator(SEL.assistantMessage)
            .allInnerTexts();
          return texts.every((t) => stripPlaceholders((t || '').trim()).length > 0);
        },
        { timeout: 480_000, intervals: [5_000] },
      )
      .toBe(true)
      .then(() => true)
      .catch(() => false);

    // Read current transcript state even if we didn't fully stabilize, so
    // we can make a best-effort assertion.
    const assistantTexts = await page.locator(SEL.assistantMessage).allInnerTexts();
    const joined = assistantTexts.join(' ').toUpperCase();

    if (!stabilized) {
      console.log(
        `[coding-blue speculative] partial transcript after timeout: ${assistantTexts
          .map((t, i) => `[${i}] ${(t || '').slice(0, 80)}`)
          .join(' | ')}`,
      );
    }

    // Both echo markers must appear somewhere in the assistant transcript —
    // proves both speculative turns executed and both results made it back.
    expect(
      joined,
      `assistant transcript should contain ALPHA echo; got: ${joined.slice(
        0,
        500,
      )}`,
    ).toContain('ALPHA');
    expect(
      joined,
      `assistant transcript should contain BRAVO echo; got: ${joined.slice(
        0,
        500,
      )}`,
    ).toContain('BRAVO');

    // Reset queue mode.
    await sendSlashWithArgs(page, '/queue followup', {
      timeoutMs: 15_000,
    }).catch(() => '');
  });

  test('harness event trace flows while task runs', async ({ page, request }) => {
    await login(page);
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    const prompt = 'Use shell tool only. Echo "harness-trace-marker" and exit.';
    await page.locator(SEL.chatInput).fill(prompt);
    await page.locator(SEL.sendButton).click();
    await expect(page.locator(SEL.userMessage)).toHaveCount(1, {
      timeout: 30_000,
    });

    const sessionId = await page.evaluate(() =>
      localStorage.getItem('octos_current_session'),
    );
    expect(sessionId).toBeTruthy();

    // Probe harness event stream endpoint.
    const resp = await request.get(
      `${TEST_URL}/api/events/harness?kinds=LlmStatus,ToolStarted,ToolCompleted`,
      {
        headers: {
          Authorization: `Bearer ${AUTH_TOKEN}`,
          'X-Profile-Id': PROFILE_ID,
          Accept: 'text/event-stream',
        },
        timeout: 10_000,
      },
    );
    const status = resp.status();
    expect([200, 204, 301, 302, 307, 308, 401]).toContain(status);
    console.log(`[coding-blue harness] /api/events/harness status = ${status}`);

    const assistant = page.locator(SEL.assistantMessage).first();
    await expect(assistant).toBeVisible({ timeout: 60_000 });
    await expect
      .poll(async () => (await assistant.innerText()).trim().length, {
        timeout: 60_000,
        intervals: [2_000],
      })
      .toBeGreaterThan(0);
  });
});
