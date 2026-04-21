/**
 * Live regressions for user-visible session lists.
 *
 * These tests lock down two related failure modes:
 * - internal runtime sessions (#child-*, #*.tasks) must never appear as chats
 * - deleting a chat from the real UI must remove it after reload and in another browser
 */
import { expect, test, type Browser, type Page } from '@playwright/test';
import {
  SEL,
  createNewSession,
  getInput,
  getSendButton,
  login,
  sendAndWait,
} from './live-browser-helpers';
import {
  findSessionIdByMessageText,
  getActiveSessionId,
  getSessionTasks,
} from './coding-hardcases-helpers';

interface ApiSession {
  id?: unknown;
  message_count?: unknown;
}

function isInternalRuntimeSessionId(id: string): boolean {
  const topic = id.split('#')[1] || '';
  return topic.startsWith('child-') || topic === 'default.tasks' || topic.endsWith('.tasks');
}

async function fetchSessionIds(page: Page): Promise<string[]> {
  return page.evaluate(async () => {
    const token =
      localStorage.getItem('octos_session_token') ||
      localStorage.getItem('octos_auth_token') ||
      sessionStorage.getItem('octos_token') ||
      '';
    const profile = localStorage.getItem('selected_profile') || '';
    const headers: Record<string, string> = {};

    if (token) {
      headers.Authorization = `Bearer ${token}`;
    }
    if (profile) {
      headers['X-Profile-Id'] = profile;
    }

    const resp = await fetch('/api/sessions', { headers });
    if (!resp.ok) {
      throw new Error(`/api/sessions returned ${resp.status}`);
    }
    const sessions = (await resp.json()) as ApiSession[];
    return sessions
      .map((session) => (typeof session?.id === 'string' ? session.id : null))
      .filter((id): id is string => Boolean(id));
  });
}

async function visibleSessionIds(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll<HTMLElement>('[data-session-id]'))
      .map((node) => node.dataset.sessionId || '')
      .filter(Boolean),
  );
}

async function clickSessionDelete(page: Page, sessionId: string) {
  await page.evaluate((targetSessionId) => {
    const row = Array.from(document.querySelectorAll<HTMLElement>('[data-session-id]')).find(
      (node) => node.dataset.sessionId === targetSessionId,
    );
    if (!row) {
      throw new Error(`session row not found: ${targetSessionId}`);
    }
    const button = row.querySelector<HTMLElement>(
      "[data-testid='session-delete-button'], .session-delete, button[title='Delete session']",
    );
    if (!button) {
      throw new Error(`delete button not found for session: ${targetSessionId}`);
    }
    button.click();
  }, sessionId);
}

async function openAuthedChat(browser: Browser) {
  const context = await browser.newContext();
  const page = await context.newPage();
  await login(page);
  return { context, page };
}

test.describe('session list regressions', () => {
  test.setTimeout(240_000);

  test('deep research creates one child task and no internal sessions in the visible list', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    let chatPosts = 0;
    page.on('request', (request) => {
      if (request.method() === 'POST' && new URL(request.url()).pathname === '/api/chat') {
        chatPosts += 1;
      }
    });

    await getInput(page).fill('深度搜索一下美国伊朗第二轮谈判可能的后果');
    await getSendButton(page).click();
    const originSessionId = await getActiveSessionId(page);

    await expect
      .poll(
        async () => {
          const tasks = await getSessionTasks(page, originSessionId);
          return tasks.filter((task) => Boolean(task.child_session_key)).length;
        },
        { timeout: 90_000, intervals: [2_000, 3_000, 5_000] },
      )
      .toBe(1);

    expect(chatPosts).toBe(1);

    await expect
      .poll(
        async () => {
          const ids = await fetchSessionIds(page);
          return ids.filter(isInternalRuntimeSessionId);
        },
        { timeout: 60_000, intervals: [1_000, 2_000, 5_000] },
      )
      .toEqual([]);

    const visibleIds = await visibleSessionIds(page);
    expect(visibleIds.filter(isInternalRuntimeSessionId)).toEqual([]);
    expect(visibleIds.filter((id) => id.startsWith(`${originSessionId}#`))).toEqual([]);
  });

  test('delete from chat UI removes the session after reload and in another browser', async ({
    browser,
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    const marker = `DELETE-SESSION-${Date.now()}`;
    await sendAndWait(page, `Reply with exactly: ${marker}. Do not use tools.`, {
      label: 'session-delete-ui',
      maxWait: 90_000,
    });

    const sessionId = await findSessionIdByMessageText(page, marker);
    expect(sessionId).not.toContain('#');
    await expect.poll(async () => (await fetchSessionIds(page)).includes(sessionId)).toBe(true);

    page.on('dialog', (dialog) => dialog.accept().catch(() => {}));
    const deleteResponse = page.waitForResponse(
      (response) =>
        response.request().method() === 'DELETE' &&
        new URL(response.url()).pathname === `/api/sessions/${encodeURIComponent(sessionId)}`,
      { timeout: 30_000 },
    );
    await clickSessionDelete(page, sessionId);
    const confirmDelete = page.getByRole('button', { name: /confirm delete/i });
    if (await confirmDelete.isVisible({ timeout: 1_000 }).catch(() => false)) {
      await confirmDelete.click();
    }
    expect((await deleteResponse).status()).toBe(204);

    await expect
      .poll(async () => (await fetchSessionIds(page)).includes(sessionId), {
        timeout: 30_000,
        intervals: [1_000, 2_000],
      })
      .toBe(false);

    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await expect.poll(async () => (await fetchSessionIds(page)).includes(sessionId)).toBe(false);

    const other = await openAuthedChat(browser);
    try {
      await expect
        .poll(async () => (await fetchSessionIds(other.page)).includes(sessionId), {
          timeout: 30_000,
          intervals: [1_000, 2_000],
        })
        .toBe(false);
    } finally {
      await other.context.close();
    }
  });
});
