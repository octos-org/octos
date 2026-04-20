import { type Browser, type Page } from '@playwright/test';

import {
  SEL,
  countAssistantBubbles,
  countUserBubbles,
  createNewSession,
  login,
} from './live-browser-helpers';

export interface SessionTask {
  label?: string | null;
  status?: string | null;
  tool_name?: string | null;
  child_join_state?: string | null;
  child_session_key?: string | null;
  child_terminal_state?: string | null;
}

function selectChildSessionTasks(tasks: SessionTask[]): SessionTask[] {
  return tasks.filter((task) => Boolean(task.child_session_key));
}

async function getLatestAssistantText(page: Page): Promise<string> {
  const assistantCount = await countAssistantBubbles(page);
  if (assistantCount === 0) {
    return '';
  }

  return ((await page.locator(SEL.assistantMessage).last().textContent().catch(() => '')) || '')
    .trim();
}

export function uniqueRepoName(prefix: string): string {
  return `${prefix}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

export async function openAuthedChat(browser: Browser) {
  const context = await browser.newContext();
  const page = await context.newPage();
  await login(page);
  await createNewSession(page);
  return { context, page };
}

export async function waitForStreamingAssistantTurn(page: Page, timeoutMs = 90_000) {
  await page.waitForFunction(
    () =>
      document.querySelectorAll("[data-testid='assistant-message']").length > 0 &&
      document.querySelector("[data-testid='cancel-button']") !== null,
    undefined,
    { timeout: timeoutMs },
  );
}

export async function waitForAssistantTextProgress(
  page: Page,
  opts: { timeoutMs?: number; minLength?: number; minGrowthEvents?: number } = {},
) {
  const { timeoutMs = 20_000, minLength = 120, minGrowthEvents = 2 } = opts;
  const deadline = Date.now() + timeoutMs;
  let lastLength = 0;
  let growthEvents = 0;

  while (Date.now() < deadline) {
    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 250 })
      .catch(() => false);
    const currentText = await getLatestAssistantText(page);
    const currentLength = currentText.length;

    if (currentLength > lastLength) {
      growthEvents += 1;
      lastLength = currentLength;
    }

    if (streaming && currentLength >= minLength && growthEvents >= minGrowthEvents) {
      return currentText;
    }

    await page.waitForTimeout(500);
  }

  throw new Error('Timed out waiting for streaming assistant text progress');
}

export async function waitForSingleSettledTurn(page: Page, timeoutMs = 240_000) {
  const deadline = Date.now() + timeoutMs;

  while (Date.now() < deadline) {
    const assistantCount = await countAssistantBubbles(page);
    const userCount = await countUserBubbles(page);
    const streaming = await page
      .locator(SEL.cancelButton)
      .isVisible({ timeout: 1_000 })
      .catch(() => false);
    const text = await getLatestAssistantText(page);

    if (userCount === 1 && assistantCount === 1 && !streaming && text) {
      return text;
    }

    await page.waitForTimeout(2_000);
  }

  throw new Error('Timed out waiting for a single settled coding turn');
}

export async function getActiveSessionId(page: Page): Promise<string> {
  const sessionId = await page
    .locator("[data-active='true']")
    .first()
    .getAttribute('data-session-id');

  if (!sessionId) {
    throw new Error('No active session id found in the sidebar');
  }

  return sessionId;
}

export async function getSessionTasks(page: Page, sessionId: string): Promise<SessionTask[]> {
  return page.evaluate(async ({ sessionId: sid }) => {
    const token =
      localStorage.getItem('octos_session_token') ||
      localStorage.getItem('octos_auth_token') ||
      '';
    const profile = localStorage.getItem('selected_profile') || '';
    const headers: Record<string, string> = {};

    if (token) {
      headers.Authorization = `Bearer ${token}`;
    }
    if (profile) {
      headers['X-Profile-Id'] = profile;
    }

    const resp = await fetch(`/api/sessions/${sid}/tasks`, { headers });
    if (!resp.ok) {
      return [];
    }

    return resp.json();
  }, { sessionId });
}

export async function waitForChildSessionTasksToSettle(
  page: Page,
  sessionId: string,
  timeoutMs = 120_000,
) {
  const deadline = Date.now() + timeoutMs;

  while (Date.now() < deadline) {
    const childTasks = selectChildSessionTasks(await getSessionTasks(page, sessionId));

    if (
      childTasks.length > 0 &&
      childTasks.every((task) => task.child_terminal_state && task.child_join_state)
    ) {
      return childTasks;
    }

    await page.waitForTimeout(2_000);
  }

  throw new Error('Timed out waiting for child-session tasks to settle');
}
