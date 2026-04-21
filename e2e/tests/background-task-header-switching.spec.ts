/**
 * Live UI contract for long-running background task state.
 *
 * Run against mini2:
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io npx playwright test tests/background-task-header-switching.spec.ts --workers=1
 */
import { expect, test, type Page } from '@playwright/test';
import {
  SEL,
  createNewSession,
  getChatThreadText,
  getInput,
  getSendButton,
  login,
  sendAndWait,
} from './live-browser-helpers';
import { getActiveSessionId, getSessionTasks } from './coding-hardcases-helpers';

const TASK_INDICATOR = 'main .session-task-indicator';
const TASK_WORKFLOW = "[data-testid='task-workflow-kind']";
const TASK_PHASE = "[data-testid='task-current-phase']";
const TASK_MESSAGE = "[data-testid='task-progress-message']";
const TASK_PROGRESS_VALUE = "[data-testid='task-progress-value']";
const AUDIO_ATTACHMENT = "[data-testid='audio-attachment']";

function humanize(value: string) {
  return value
    .replace(/[_-]+/g, ' ')
    .trim()
    .replace(/\b\w/g, (ch) => ch.toUpperCase());
}

function taskKey(task: any): string {
  return (
    task?.id ||
    task?.child_session_key ||
    task?.tool_call_id ||
    task?.session_key ||
    task?.tool_name ||
    `${task?.started_at || ''}:${task?.updated_at || ''}:${task?.status || ''}`
  );
}

function isActiveTask(task: any): boolean {
  const status = String(task?.status || '').toLowerCase();
  const lifecycle = String(task?.lifecycle_state || '').toLowerCase();
  return (
    status === 'spawned' ||
    status === 'running' ||
    lifecycle === 'queued' ||
    lifecycle === 'running' ||
    lifecycle === 'verifying'
  );
}

function uniqueActiveTasks(tasks: any[]) {
  const seen = new Set<string>();
  return tasks.filter((task) => {
    if (!isActiveTask(task)) return false;
    const key = taskKey(task);
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

async function waitForActiveTask(page: Page, sessionId: string) {
  const deadline = Date.now() + 60_000;
  let lastTasks: any[] = [];

  while (Date.now() < deadline) {
    lastTasks = await getSessionTasks(page, sessionId);
    const activeTasks = uniqueActiveTasks(lastTasks);
    if (activeTasks.length > 0) {
      return activeTasks[0];
    }
    await page.waitForTimeout(2_000);
  }

  throw new Error(
    `Timed out waiting for an active task in ${sessionId}. Last tasks: ${JSON.stringify(lastTasks)}`,
  );
}

async function startPodcast(page: Page, marker: string) {
  const prompt =
    `不要搜索，直接生成一个简短测试播客并把音频发回会话。脚本： ` +
    `[杨幂 - clone:yangmi, professional] ${marker} 大家好。 ` +
    `[窦文涛 - clone:douwentao, professional] 这里是后台任务切换测试。 ` +
    `[杨幂 - clone:yangmi, professional] 请只生成一次最终音频。 ` +
    `[窦文涛 - clone:douwentao, professional] 感谢收听。`;

  await getInput(page).fill(prompt);
  await getSendButton(page).click();

  await expect(page.locator(TASK_INDICATOR)).toHaveCount(1, {
    timeout: 60_000,
  });

  return prompt;
}

async function switchToSession(page: Page, sessionId: string) {
  const sessionButton = page.locator(
    `[data-session-id="${sessionId}"] [data-testid="session-switch-button"]`,
  );
  await sessionButton.waitFor({ state: 'visible', timeout: 30_000 });
  await sessionButton.click();
  await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
}

async function getSessionMedia(page: Page, sessionId: string): Promise<string[]> {
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

    const resp = await fetch(`/api/sessions/${encodeURIComponent(sid)}/messages?limit=100`, {
      headers,
    });
    if (!resp.ok) {
      return [];
    }

    const messages = await resp.json().catch(() => []);
    if (!Array.isArray(messages)) {
      return [];
    }

    return messages.flatMap((message) =>
      Array.isArray(message?.media)
        ? message.media.filter((path: unknown): path is string => typeof path === 'string')
        : [],
    );
  }, { sessionId });
}

async function waitForSessionMedia(page: Page, sessionId: string, timeoutMs: number) {
  const deadline = Date.now() + timeoutMs;
  let lastMedia: string[] = [];

  while (Date.now() < deadline) {
    lastMedia = await getSessionMedia(page, sessionId);
    if (lastMedia.length > 0) {
      return lastMedia;
    }
    await page.waitForTimeout(3_000);
  }

  throw new Error(
    `Timed out waiting for media in session ${sessionId}. Last media: ${JSON.stringify(
      lastMedia,
    )}`,
  );
}

test.describe('background task header session switching', () => {
  test.setTimeout(360_000);

  test('podcast task indicator survives switching away and back', async ({ page }) => {
    await login(page);
    await createNewSession(page);

    const marker = `BG-PODCAST-${Date.now()}`;
    const prompt = await startPodcast(page, marker);
    const originSessionId = await getActiveSessionId(page);
    const activeTask = await waitForActiveTask(page, originSessionId);
    const runtimeDetail = activeTask.runtime_detail || {};

    await expect(page.locator(TASK_INDICATOR)).toHaveCount(1);
    if (runtimeDetail.workflow_kind) {
      await expect(page.locator(TASK_WORKFLOW)).toContainText(
        humanize(String(runtimeDetail.workflow_kind)),
      );
    }
    if (runtimeDetail.current_phase) {
      await expect(page.locator(TASK_PHASE)).toContainText(
        humanize(String(runtimeDetail.current_phase)),
      );
    }
    if (runtimeDetail.progress_message) {
      await expect(page.locator(TASK_MESSAGE)).toContainText(
        String(runtimeDetail.progress_message),
      );
    }
    if (typeof runtimeDetail.progress === 'number') {
      await expect(page.locator(TASK_PROGRESS_VALUE)).toContainText(/%$/);
    }

    await expect(page.locator(TASK_INDICATOR)).toBeVisible();
    await expect(page.locator(SEL.userMessage).last()).toContainText(prompt.slice(0, 80));

    await createNewSession(page);
    await expect(page.locator(TASK_INDICATOR)).toHaveCount(0);

    const otherMarker = `OTHER-${Date.now()}`;
    await sendAndWait(
      page,
      `Reply with exactly: ${otherMarker}. Do not use tools or background work.`,
      { label: 'background-switch-other-session', maxWait: 90_000 },
    );
    const otherSessionId = await getActiveSessionId(page, {
      ignoreSessionIds: [originSessionId],
    });
    const otherTextBefore = await getChatThreadText(page);
    expect(otherTextBefore).toContain(otherMarker);
    expect(otherTextBefore).not.toContain(marker);

    await switchToSession(page, originSessionId);
    await expect(page.locator(TASK_INDICATOR)).toBeVisible({ timeout: 15_000 });
    await expect(page.locator(TASK_INDICATOR)).toHaveCount(1);
    await expect(await getChatThreadText(page)).toContain(marker);

    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await expect.poll(async () => getChatThreadText(page), {
      timeout: 15_000,
    }).toContain(marker);
    const reloadedTask = await waitForActiveTask(page, originSessionId);
    const reloadedDetail = reloadedTask.runtime_detail || {};
    await expect(page.locator(TASK_INDICATOR)).toHaveCount(1, { timeout: 15_000 });
    if (reloadedDetail.workflow_kind) {
      await expect(page.locator(TASK_WORKFLOW)).toContainText(
        humanize(String(reloadedDetail.workflow_kind)),
      );
    }
    if (reloadedDetail.current_phase) {
      await expect(page.locator(TASK_PHASE)).toContainText(
        humanize(String(reloadedDetail.current_phase)),
      );
    }
    if (reloadedDetail.progress_message) {
      await expect(page.locator(TASK_MESSAGE)).toContainText(
        String(reloadedDetail.progress_message),
      );
    }

    const originMedia = await waitForSessionMedia(page, originSessionId, 240_000);
    await switchToSession(page, originSessionId);
    await expect.poll(() => page.locator(AUDIO_ATTACHMENT).count(), {
      timeout: 60_000,
    }).toBeGreaterThan(0);

    const otherMedia = await getSessionMedia(page, otherSessionId);
    expect(originMedia.length).toBeGreaterThan(0);
    expect(otherMedia).toHaveLength(0);
  });
});
