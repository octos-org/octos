/**
 * Shared helpers for the coding-blue fragile-case e2e matrix (Suite 1-10).
 *
 * Each helper is kept narrow and composable so individual specs in the
 * matrix stay short and readable. Nothing here mutates production state;
 * these are read/send helpers that wrap the chat-input DOM interaction.
 */
import { expect, type APIRequestContext, type Page } from '@playwright/test';

import {
  SEL,
  countAssistantBubbles,
  countUserBubbles,
  getInput,
  getSendButton,
} from './live-browser-helpers';

export type QueueMode =
  | 'followup'
  | 'collect'
  | 'steer'
  | 'interrupt'
  | 'speculative';

export interface QueueModeResult {
  badgeText: string;
  feedbackText: string;
}

/**
 * Switch the active session's queue mode by sending a `/queue <mode>` slash
 * command, then wait for the cmd-feedback ack and optionally the
 * queue-mode-badge to reflect the new mode. Returns both text snapshots.
 */
export async function setQueueMode(
  page: Page,
  mode: QueueMode,
  opts: { timeoutMs?: number } = {},
): Promise<QueueModeResult> {
  const { timeoutMs = 30_000 } = opts;
  const input = getInput(page);
  const sendBtn = getSendButton(page);

  await input.fill(`/queue ${mode}`);
  await sendBtn.click();

  const feedback = page.locator("[data-testid='cmd-feedback']");
  await feedback
    .waitFor({ state: 'visible', timeout: timeoutMs })
    .catch(() => undefined);
  const feedbackText = ((await feedback.textContent().catch(() => '')) || '').trim();

  const badge = page.locator("[data-testid='queue-mode-badge']");
  const badgeText = ((await badge
    .first()
    .textContent({ timeout: 5_000 })
    .catch(() => '')) || '').trim();

  return { badgeText, feedbackText };
}

/**
 * Fire N prompts with a fixed interval between each send. Returns the number
 * of user bubbles observed after the last send — useful for early sanity
 * checks before waiting for assistants.
 */
export async function fireRapidPrompts(
  page: Page,
  prompts: readonly string[],
  intervalMs = 500,
): Promise<{ sent: number; userBubbles: number }> {
  const input = getInput(page);
  const sendBtn = getSendButton(page);

  let sent = 0;
  for (const prompt of prompts) {
    await input.fill(prompt);
    await sendBtn.click();
    sent += 1;
    if (sent < prompts.length) {
      await page.waitForTimeout(intervalMs);
    }
  }

  // Give the DOM one tick to reflect the final send.
  await page.waitForTimeout(250);
  const userBubbles = await countUserBubbles(page);
  return { sent, userBubbles };
}

/**
 * Poll until N assistant bubbles each hold non-empty text (length > 20) OR
 * the timeout elapses. Returns the count of "filled" bubbles seen at exit.
 *
 * This is intentionally stricter than countAssistantBubbles because some
 * backend modes spawn placeholder bubbles that stay empty while the real
 * content is being streamed onto a different bubble.
 */
export async function waitForAllAssistantsContent(
  page: Page,
  expectedCount: number,
  timeoutMs = 120_000,
): Promise<number> {
  const deadline = Date.now() + timeoutMs;
  let lastFilled = 0;

  while (Date.now() < deadline) {
    const filled = await page.evaluate((sel) => {
      const bubbles = document.querySelectorAll(sel);
      return Array.from(bubbles).filter((node) => {
        const text = (node.textContent || '').trim();
        return text.length > 20;
      }).length;
    }, SEL.assistantMessage);

    lastFilled = filled;
    if (filled >= expectedCount) {
      // Confirm streaming has stopped so the "filled" count is final.
      const streaming = await page
        .locator(SEL.cancelButton)
        .isVisible({ timeout: 500 })
        .catch(() => false);
      if (!streaming) {
        return filled;
      }
    }

    await page.waitForTimeout(1_500);
  }

  return lastFilled;
}

/**
 * Snapshot every localStorage key whose name matches either the brief's
 * documented `octos_web:task_store:v1:*` prefix OR the currently-shipped
 * session keys (so the helper is useful even before that prefix lands).
 */
export async function snapshotTaskStore(
  page: Page,
): Promise<Record<string, string>> {
  return page.evaluate(() => {
    const out: Record<string, string> = {};
    for (let i = 0; i < localStorage.length; i += 1) {
      const key = localStorage.key(i);
      if (!key) continue;
      if (
        key.startsWith('octos_web:task_store:v1:') ||
        key.startsWith('octos_session_') ||
        key.startsWith('octos_current_session') ||
        key.startsWith('octos_web:') ||
        key.startsWith('task_store:')
      ) {
        out[key] = localStorage.getItem(key) || '';
      }
    }
    return out;
  });
}

export interface SessionTaskRow {
  id?: string | null;
  status?: string | null;
  tool_name?: string | null;
  lifecycle_state?: string | null;
  child_terminal_state?: string | null;
  child_join_state?: string | null;
  completed_at?: string | null;
  [extra: string]: unknown;
}

/**
 * Poll GET /api/sessions/{id}/tasks until at least one row reports a
 * terminal lifecycle (completed/failed/cancelled) or until the timeout
 * elapses. Returns the first terminal row, or null on timeout.
 *
 * The request context can be the Playwright APIRequestContext or an
 * object exposing .post/.get — we only need GET.
 */
export async function waitForTerminalTask(
  request: APIRequestContext,
  baseURL: string,
  token: string,
  profile: string,
  sessionId: string,
  timeoutMs = 240_000,
): Promise<SessionTaskRow | null> {
  const deadline = Date.now() + timeoutMs;
  const headers: Record<string, string> = {};
  if (token) headers.Authorization = `Bearer ${token}`;
  if (profile) headers['X-Profile-Id'] = profile;

  while (Date.now() < deadline) {
    const resp = await request
      .get(`${baseURL}/api/sessions/${encodeURIComponent(sessionId)}/tasks`, {
        headers,
      })
      .catch(() => null);

    if (resp && resp.ok()) {
      const tasks = (await resp.json().catch(() => [])) as SessionTaskRow[];
      if (Array.isArray(tasks)) {
        for (const task of tasks) {
          const terminal =
            task.status === 'completed' ||
            task.status === 'failed' ||
            task.status === 'cancelled' ||
            task.lifecycle_state === 'ready' ||
            Boolean(task.completed_at);
          if (terminal) return task;
        }
      }
    }

    await new Promise((resolve) => setTimeout(resolve, 3_000));
  }

  return null;
}

/**
 * Get the active session id from the sidebar or from the sessions API if the
 * sidebar isn't rendering a data-active entry yet.
 */
export async function getActiveSessionIdOrNull(page: Page): Promise<string | null> {
  const active = await page
    .evaluate(() => {
      const el = document.querySelector<HTMLElement>(
        "[data-session-id][data-active='true']",
      );
      return el?.dataset.sessionId || null;
    })
    .catch(() => null);
  if (active) return active;

  return page
    .evaluate(async () => {
      const token =
        localStorage.getItem('octos_session_token') ||
        localStorage.getItem('octos_auth_token') ||
        '';
      const profile = localStorage.getItem('selected_profile') || '';
      const headers: Record<string, string> = {};
      if (token) headers.Authorization = `Bearer ${token}`;
      if (profile) headers['X-Profile-Id'] = profile;
      const resp = await fetch('/api/sessions', { headers });
      if (!resp.ok) return null;
      const data = await resp.json().catch(() => []);
      if (!Array.isArray(data) || data.length === 0) return null;
      return (typeof data[0]?.id === 'string' ? (data[0].id as string) : null);
    })
    .catch(() => null);
}

/**
 * Count user and assistant bubbles in a single round-trip.
 */
export async function countBubbles(page: Page): Promise<{
  user: number;
  assistant: number;
}> {
  const [user, assistant] = await Promise.all([
    countUserBubbles(page),
    countAssistantBubbles(page),
  ]);
  return { user, assistant };
}

/**
 * Build a quick-echo shell prompt that runs in a couple of seconds via the
 * shell tool. Useful for most Suite 1 tests where the only thing that
 * matters is the marker landing in the assistant text.
 */
export function buildEchoShellPrompt(marker: string): string {
  return [
    'Use shell tool only.',
    'If shell is not already active, call activate_tools with exactly ["shell"] once and only once.',
    `Run \`echo ${marker}\` and return only its stdout.`,
    'Do not explain tool availability.',
    'Do not start background work.',
  ].join(' ');
}

/**
 * Reset back to followup queue mode. Best-effort — silently succeeds if the
 * command errors (e.g. offline during teardown).
 */
export async function resetQueueMode(page: Page) {
  try {
    await setQueueMode(page, 'followup');
  } catch {
    // Best effort.
  }
}

/**
 * Assert that at least `expectedCount` bubbles each contain any of the
 * provided markers. Useful for verifying two independently-routed answers
 * both landed.
 */
export async function expectAssistantsContainAll(
  page: Page,
  markers: readonly string[],
) {
  const texts = await page
    .locator(SEL.assistantMessage)
    .allTextContents()
    .catch(() => []);
  const combined = texts.join('\n');
  for (const marker of markers) {
    expect(combined).toContain(marker);
  }
}
