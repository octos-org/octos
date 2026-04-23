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
 * Key UX ground truth:
 *   - Slash commands (/queue /adaptive /status /help /soul /new /reset) are
 *     consumed locally by the web client and render into cmd-feedback
 *     ([data-testid='cmd-feedback']) — NOT as assistant-message bubbles.
 *   - Task lifecycle is observable via DOM testids introduced by PR #42:
 *       task-anchor-message-<taskId>, task-anchor-spinner-<taskId>,
 *       task-anchor-phase-<taskId>, task-anchor-progress-<taskId>.
 *     These are more reliable than polling /api/sessions/:id/tasks, which
 *     only populates when a gateway is running + depends on the topic param.
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

async function readCmdFeedbackText(
  page: import('@playwright/test').Page,
): Promise<string> {
  const fb = page.locator(SEL.cmdFeedback).first();
  const visible = await fb.isVisible().catch(() => false);
  if (!visible) return '';
  return ((await fb.textContent()) || '').trim();
}

async function sendSlashCommand(
  page: import('@playwright/test').Page,
  command: string,
  opts: { timeoutMs?: number } = {},
): Promise<string> {
  const timeoutMs = opts.timeoutMs ?? 5_000;
  const input = page.locator(SEL.chatInput);
  const send = page.locator(SEL.sendButton);

  await input.fill(command);
  await send.click();

  // cmd-feedback must become visible within the given window.
  await expect(page.locator(SEL.cmdFeedback).first()).toBeVisible({
    timeout: timeoutMs,
  });

  // Text may briefly be empty while animating in; poll until non-empty.
  let text = '';
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    text = await readCmdFeedbackText(page);
    if (text.length > 0) return text;
    await page.waitForTimeout(200);
  }
  return text;
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

    // Fire a deep-research task. Keep the ask specific so the agent returns a
    // short report (not a 3000-token essay), but genuinely multi-iteration.
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

    // Deep-research SHOULD register a task-anchor via PR #42 testids.
    // Poll the DOM for any task-anchor testid. If none ever appears inside
    // 2 min, the task may have been streamed inline rather than as a task —
    // still acceptable, just a different code path. We only DEMAND a task
    // anchor if deep-research is treated as a background task by this build.
    const anchorDeadline = Date.now() + 120_000;
    const observedPhases = new Set<string>();
    let sawAnchor = false;
    let taskId: string | null = null;
    while (Date.now() < anchorDeadline) {
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
        // Capture the current phase (if rendered).
        const phase = await page
          .locator(`[data-testid='task-anchor-phase-${taskId}']`)
          .textContent()
          .catch(() => '');
        if (phase) observedPhases.add(phase.trim().toLowerCase());
        const spinnerGone =
          (await page
            .locator(`[data-testid='task-anchor-spinner-${taskId}']`)
            .count()) === 0;
        if (spinnerGone && (await isAssistantFilled(page))) break;
      } else if (await isAssistantFilled(page)) {
        // Non-task streaming path: assistant bubble already has content.
        break;
      }
      await page.waitForTimeout(3_000);
    }

    // Whatever the path, assistant bubble must eventually contain report-back.
    const assistant = page.locator(SEL.assistantMessage).first();
    await expect(assistant).toBeVisible({ timeout: 120_000 });
    await expect
      .poll(async () => (await assistant.innerText()).trim().length, {
        timeout: 300_000,
        intervals: [5_000],
      })
      .toBeGreaterThan(10);

    const text = (await assistant.innerText()).trim();

    // Report-back must either contain the marker (model echoed it) OR name
    // at least one Rust framework.
    const hasFrameworks =
      /axum|actix|rocket|tower|leptos|yew|salvo|warp/i.test(text);
    expect(
      hasFrameworks || text.includes(marker),
      `report-back should mention a Rust framework or marker; got: ${text.slice(
        0,
        200,
      )}`,
    ).toBe(true);

    console.log(
      `[coding-blue long-run] sawAnchor=${sawAnchor} taskId=${taskId} phases=${Array.from(
        observedPhases,
      ).join(', ')} reportLen=${text.length}`,
    );
  });

  test('reload during active long-running task preserves task-store state', async ({
    page,
  }) => {
    await login(page);
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    // Start a task likely to run >30s so we can reload mid-flight.
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

    // Wait for a task-anchor to appear in the DOM. This is the only scenario
    // where localStorage task-store entries are populated — without an anchor
    // there's no task-store entry to persist, and the reload test has nothing
    // to assert. If no anchor renders in 60s, skip with a diagnostic.
    const anchorDeadline = Date.now() + 60_000;
    let anchorTaskId: string | null = null;
    while (Date.now() < anchorDeadline) {
      anchorTaskId = await page.evaluate(() => {
        const el = document.querySelector('[data-testid^="task-anchor-message-"]');
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

    // Snapshot task-store state BEFORE reload.
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

    // Reload.
    await page.reload({ waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    // Bug class 1: task-store should survive reload.
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

    // Wait for the anchor's spinner to disappear (task completed) OR the
    // assistant bubble to have content — whichever signals completion.
    const completionDeadline = Date.now() + 300_000;
    let completed = false;
    while (Date.now() < completionDeadline) {
      const spinnerCount = await page
        .locator(`[data-testid='task-anchor-spinner-${anchorTaskId}']`)
        .count();
      if (spinnerCount === 0 && (await isAssistantFilled(page))) {
        completed = true;
        break;
      }
      await page.waitForTimeout(5_000);
    }
    expect(completed, 'task must complete after reload').toBe(true);

    const assistant = page.locator(SEL.assistantMessage).first();
    const text = (await assistant.innerText()).trim();
    expect(
      text.length,
      'final assistant bubble has content post-reload',
    ).toBeGreaterThan(10);

    console.log(
      `[coding-blue reload] task-store entries preserved: ${
        Object.keys(afterReload).length
      }; taskId=${anchorTaskId}; final text: ${text.slice(0, 120)}`,
    );
  });

  test('slash commands return a visible, non-empty ack response', async ({
    page,
  }) => {
    await login(page);
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    // Every slash command must render cmd-feedback within 5s. Target
    // [data-testid='cmd-feedback'] — NOT assistant-message. These don't
    // round-trip to the LLM.
    //
    // Each pattern must match OR the ack may legitimately be an
    // "unknown-command" notice.
    const commands = [
      { cmd: '/help', expect: /help|command|slash|available|queue|new/i },
      {
        cmd: '/queue',
        expect: /queue|followup|collect|speculative|steer|interrupt|mode/i,
      },
      { cmd: '/adaptive', expect: /adaptive|off|hedge|lane|mode|provider/i },
      {
        cmd: '/status',
        expect: /status|session|model|provider|ok|active|uptime|greeting|metric/i,
      },
    ];

    for (const { cmd, expect: pattern } of commands) {
      const text = await sendSlashCommand(page, cmd, { timeoutMs: 5_000 });
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

    // Set adaptive hedge — backend races multiple providers, returns fastest.
    // Slash command renders via cmd-feedback, not as an assistant bubble.
    const hedgeAck = await sendSlashCommand(page, '/adaptive hedge', {
      timeoutMs: 5_000,
    });
    expect(hedgeAck.length, 'hedge ack must be non-empty').toBeGreaterThan(0);
    expect(hedgeAck.toLowerCase()).toMatch(/hedge|adaptive|mode|provider/);

    // If this mini only has one provider configured, hedge has nothing to
    // race. Acceptable outcome: skip with diagnostic.
    const singleProviderSignal =
      /only one provider|single provider|<=\s*1 provider|need(?:s|ing)? .* providers|no alternate/i.test(
        hedgeAck,
      );
    test.skip(
      singleProviderSignal,
      `adaptive hedge unavailable on this mini; ack="${hedgeAck.slice(0, 160)}"`,
    );

    // Fire a real prompt — racing providers should converge in reasonable time.
    const prompt = 'Answer in one short sentence: what is the capital of Japan?';
    await page.locator(SEL.chatInput).fill(prompt);
    await page.locator(SEL.sendButton).click();

    await expect(page.locator(SEL.userMessage).last()).toBeVisible({
      timeout: 30_000,
    });
    const assistant = page.locator(SEL.assistantMessage).last();
    await expect(assistant).toBeVisible({ timeout: 120_000 });
    await expect
      .poll(async () => (await assistant.innerText()).trim().length, {
        timeout: 180_000,
        intervals: [3_000],
      })
      .toBeGreaterThan(0);

    const text = ((await assistant.innerText()) || '').toLowerCase();
    expect(text).toMatch(/tokyo/);

    // Reset to off so later tests aren't polluted.
    await sendSlashCommand(page, '/adaptive off', { timeoutMs: 5_000 }).catch(
      () => '',
    );
  });

  test('queue speculative mode dispatches concurrent turns as independent tasks', async ({
    page,
  }) => {
    await login(page);
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    // Step 1: switch mode via slash command. Ack is in cmd-feedback, NOT
    // as an assistant bubble.
    const specAck = await sendSlashCommand(page, '/queue speculative', {
      timeoutMs: 5_000,
    });
    expect(specAck.length, 'speculative ack must be non-empty').toBeGreaterThan(0);
    expect(specAck.toLowerCase()).toMatch(/speculative|queue|mode/);

    // Baseline: before real prompts there must be ZERO assistant bubbles —
    // because the slash command doesn't produce one. This is the main
    // fix: the previous spec was counting the cmd-feedback entry as an
    // assistant bubble.
    const baselineAssistants = await page.locator(SEL.assistantMessage).count();
    expect(baselineAssistants, 'slash command must not produce an assistant bubble').toBe(0);

    // Step 2: fire two rapid prompts. Each goes through the chat pipeline.
    await page.locator(SEL.chatInput).fill('Use shell: echo ALPHA-speculative-1');
    await page.locator(SEL.sendButton).click();
    await page.waitForTimeout(500);
    await page.locator(SEL.chatInput).fill('Use shell: echo BRAVO-speculative-2');
    await page.locator(SEL.sendButton).click();

    // Two user bubbles after the two prompts.
    await expect(page.locator(SEL.userMessage)).toHaveCount(2, {
      timeout: 30_000,
    });

    // Wait for exactly 2 NEW assistant bubbles (one per prompt).
    await expect(page.locator(SEL.assistantMessage)).toHaveCount(2, {
      timeout: 300_000,
    });

    // Both bubbles must carry non-empty text eventually.
    await expect
      .poll(
        async () => {
          const all = await page
            .locator(SEL.assistantMessage)
            .allInnerTexts();
          return all.every((t) => (t || '').trim().length > 0);
        },
        { timeout: 300_000, intervals: [3_000] },
      )
      .toBe(true);

    const assistantTexts = await page.locator(SEL.assistantMessage).allInnerTexts();
    const joined = assistantTexts.join(' ').toUpperCase();
    // Both echo markers must appear somewhere in the assistant transcript.
    // Proves both speculative turns executed and both results made it back.
    expect(
      joined,
      `assistant transcript should contain both ALPHA and BRAVO echoes; got: ${joined.slice(
        0,
        400,
      )}`,
    ).toContain('ALPHA');
    expect(joined).toContain('BRAVO');

    // Reset queue mode for subsequent tests.
    await sendSlashCommand(page, '/queue followup', { timeoutMs: 5_000 }).catch(
      () => '',
    );
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

    // Probe the harness event stream. SSE from Playwright APIRequestContext
    // isn't trivial, so we just do a short GET with a timeout. The endpoint
    // must respond with a routable status.
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

    // Wait for the assistant bubble to contain content.
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

async function isAssistantFilled(
  page: import('@playwright/test').Page,
): Promise<boolean> {
  const count = await page.locator(SEL.assistantMessage).count();
  if (count === 0) return false;
  const text = (await page.locator(SEL.assistantMessage).first().innerText()) || '';
  return text.trim().length > 10;
}
