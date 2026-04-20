/**
 * Live browser smoke coverage for the deployed chat UI.
 *
 * These tests should stay focused on the highest-signal regressions:
 * - exactly one final audio/file card for background media tasks
 * - no ghost/empty turns after reload
 * - user prompt remains ordered before the final assistant artifact
 *
 * Run against a live browser host:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/live-browser.spec.ts
 */
import { expect, test, type Page } from '@playwright/test';

const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE_ID = process.env.OCTOS_PROFILE || 'dspfac';
const TEST_EMAIL = process.env.OCTOS_TEST_EMAIL || 'dspfac@gmail.com';

const SEL = {
  chatInput: "[data-testid='chat-input']",
  sendButton: "[data-testid='send-button']",
  cancelButton: "[data-testid='cancel-button']",
  userMessage: "[data-testid='user-message']",
  assistantMessage: "[data-testid='assistant-message']",
  newChatButton: "[data-testid='new-chat-button']",
  loginTokenInput: "[data-testid='token-input']",
  loginButton: "[data-testid='login-button']",
} as const;

interface RenderedAudioAttachment {
  filename: string;
  path: string;
  text: string;
}

interface RenderedThreadBubble {
  role: 'user' | 'assistant';
  text: string;
  audioAttachments: RenderedAudioAttachment[];
}

async function login(page: Page) {
  await page.addInitScript(
    ({ token, profile }) => {
      localStorage.setItem('octos_session_token', token);
      localStorage.setItem('octos_auth_token', token);
      localStorage.setItem('selected_profile', profile);
    },
    { token: AUTH_TOKEN, profile: PROFILE_ID },
  );

  await page.goto('/chat', { waitUntil: 'networkidle' });

  const onChat = await page
    .locator(SEL.chatInput)
    .isVisible({ timeout: 5_000 })
    .catch(() => false);
  if (onChat) return;

  await page.goto('/chat', { waitUntil: 'networkidle' });
  const chatVisible = await page
    .locator(SEL.chatInput)
    .isVisible({ timeout: 5_000 })
    .catch(() => false);
  if (chatVisible) return;

  const authTokenTab = page.locator('button', { hasText: 'Auth Token' });
  if (await authTokenTab.isVisible().catch(() => false)) {
    await authTokenTab.click();
    await page.locator(SEL.loginTokenInput).fill(AUTH_TOKEN);
    await page.locator(SEL.loginButton).click();
    const tokenChatVisible = await page
      .locator(SEL.chatInput)
      .isVisible({ timeout: 10_000 })
      .catch(() => false);
    if (tokenChatVisible) return;
  }

  try {
    const apiLoginResult = await page.evaluate(
      async ({ email, code }) => {
        const resp = await fetch('/api/auth/verify', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ email, code }),
        });
        if (!resp.ok) return null;
        const data = await resp.json();
        if (!data.ok || !data.token) return null;
        localStorage.setItem('octos_session_token', data.token);
        return data.token as string;
      },
      { email: TEST_EMAIL, code: AUTH_TOKEN },
    );

    if (apiLoginResult) {
      await page.reload({ waitUntil: 'networkidle' });
      const apiLoginVisible = await page
        .locator(SEL.chatInput)
        .isVisible({ timeout: 10_000 })
        .catch(() => false);
      if (apiLoginVisible) return;

      await page.goto('/chat', { waitUntil: 'networkidle' });
      const chatAfterLogin = await page
        .locator(SEL.chatInput)
        .isVisible({ timeout: 10_000 })
        .catch(() => false);
      if (chatAfterLogin) return;
    }
  } catch {
    // Fall through to the last-chance UI wait below.
  }

  await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
}

async function createNewSession(page: Page) {
  await page.locator(SEL.newChatButton).click();
  await page.waitForTimeout(1_000);
}

function getInput(page: Page) {
  return page.locator(SEL.chatInput).first();
}

function getSendButton(page: Page) {
  return page.locator(SEL.sendButton).first();
}

async function getRenderedAudioAttachments(
  page: Page,
): Promise<RenderedAudioAttachment[]> {
  return page.locator("[data-testid='audio-attachment']").evaluateAll((nodes) =>
    nodes.map((node) => {
      const el = node as HTMLElement;
      return {
        filename: el.dataset.filename || '',
        path: el.dataset.filePath || '',
        text: (el.textContent || '').trim(),
      };
    }),
  );
}

async function getRenderedThreadBubbles(
  page: Page,
): Promise<RenderedThreadBubble[]> {
  return page.evaluate(() => {
    const nodes = document.querySelectorAll(
      "[data-testid='user-message'], [data-testid='assistant-message']",
    );
    return Array.from(nodes).map((node) => {
      const el = node as HTMLElement;
      const role = el.dataset.testid?.includes('user') ? 'user' : 'assistant';
      const audioAttachments = Array.from(
        el.querySelectorAll("[data-testid='audio-attachment']"),
      ).map((attachment) => {
        const audioEl = attachment as HTMLElement;
        return {
          filename: audioEl.dataset.filename || '',
          path: audioEl.dataset.filePath || '',
          text: (audioEl.textContent || '').trim(),
        };
      });
      return {
        role,
        text: (el.textContent || '').trim(),
        audioAttachments,
      };
    });
  });
}

function findDuplicateAudioAttachments(
  attachments: RenderedAudioAttachment[],
): { key: string; count: number }[] {
  const counts = new Map<string, number>();
  for (const attachment of attachments) {
    const key = attachment.path || attachment.filename || attachment.text;
    if (!key) continue;
    counts.set(key, (counts.get(key) || 0) + 1);
  }
  return Array.from(counts.entries())
    .filter(([, count]) => count > 1)
    .map(([key, count]) => ({ key, count }));
}

async function sendAndWait(
  page: Page,
  message: string,
  opts: { maxWait?: number; label?: string; throwOnTimeout?: boolean } = {},
) {
  const { maxWait = 120_000, label = '', throwOnTimeout = true } = opts;
  const input = getInput(page);
  const sendBtn = getSendButton(page);

  await input.fill(message);
  await sendBtn.click();

  const start = Date.now();
  let lastAssistantCount = 0;
  let lastText = '';
  let stableCount = 0;
  let textStableCount = 0;

  while (Date.now() - start < maxWait) {
    await page.waitForTimeout(3_000);

    const isStreaming = await page
      .locator(SEL.cancelButton)
      .isVisible()
      .catch(() => false);

    const assistantCount = await page.locator(SEL.assistantMessage).count();

    let currentText = '';
    if (assistantCount > 0) {
      currentText =
        (await page
          .locator(SEL.assistantMessage)
          .last()
          .textContent()
          .catch(() => '')) || '';
    }

    if (assistantCount === lastAssistantCount && !isStreaming) {
      stableCount++;
      if (stableCount >= 2) break;
    } else {
      stableCount = 0;
    }

    if (
      assistantCount > 0 &&
      currentText.length > 0 &&
      currentText === lastText
    ) {
      textStableCount++;
      if (textStableCount >= 3) break;
    } else {
      textStableCount = 0;
    }

    lastAssistantCount = assistantCount;
    lastText = currentText;

    if (label) {
      const elapsed = ((Date.now() - start) / 1000).toFixed(0);
      console.log(
        `  [${label}] ${elapsed}s: ${assistantCount} bubbles, streaming=${isStreaming}, textLen=${currentText.length}`,
      );
    }
  }

  if (Date.now() - start >= maxWait && throwOnTimeout) {
    throw new Error(
      `sendAndWait timed out after ${maxWait / 1000}s for message: "${message.slice(0, 60)}"`,
    );
  }

  const assistantBubbles = await page.locator(SEL.assistantMessage).count();
  const finalText =
    assistantBubbles > 0
      ? await page.locator(SEL.assistantMessage).last().textContent()
      : '';

  return {
    assistantBubbles,
    responseText: finalText?.trim() || '',
    responseLen: finalText?.trim().length || 0,
  };
}

test.describe('Live browser smoke', () => {
  test.setTimeout(600_000);

  test.beforeEach(async ({ page }) => {
    await login(page);
    await createNewSession(page);
  });

  test('short TTS success renders exactly one audio attachment', async ({
    page,
  }) => {
    await sendAndWait(page, '用自定义声音 yangmi 说：你好世界', {
      label: 'live-short-tts-smoke',
      maxWait: 60_000,
    });

    let audioAttachments: RenderedAudioAttachment[] = [];
    for (let i = 0; i < 15; i++) {
      await page.waitForTimeout(3_000);
      audioAttachments = await getRenderedAudioAttachments(page);
      if (audioAttachments.length > 0) break;
    }

    const threadBubbles = await getRenderedThreadBubbles(page);
    const userBubbles = threadBubbles.filter((bubble) => bubble.role === 'user');
    const duplicateAudio = findDuplicateAudioAttachments(audioAttachments);
    const firstAssistantIndex = threadBubbles.findIndex(
      (bubble) => bubble.role === 'assistant',
    );

    expect(userBubbles).toHaveLength(1);
    expect(threadBubbles[0]?.role).toBe('user');
    expect(firstAssistantIndex).toBeGreaterThan(0);
    expect(duplicateAudio).toHaveLength(0);
    expect(audioAttachments).toHaveLength(1);
  });

  test('deep research survives reload without ghost turns', async ({ page }) => {
    const prompt =
      "Do a deep research on the latest Rust programming language developments in 2026. Run the pipeline directly, don't ask me to choose.";

    const result = await sendAndWait(page, prompt, {
      label: 'live-deep-research',
      maxWait: 540_000,
      throwOnTimeout: false,
    });

    expect(result.responseLen).toBeGreaterThan(0);

    // Deep research keeps background runtime channels active, so waiting for
    // networkidle can hang even when the page has restored correctly.
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await page.waitForTimeout(5_000);

    const threadBubbles = await getRenderedThreadBubbles(page);
    const userBubbles = threadBubbles.filter((bubble) => bubble.role === 'user');
    const emptyAssistantBubbles = threadBubbles.filter(
      (bubble) =>
        bubble.role === 'assistant' &&
        bubble.audioAttachments.length === 0 &&
        bubble.text.trim() === '',
    );

    expect(userBubbles).toHaveLength(1);
    expect(userBubbles[0]?.text).toContain(
      'latest Rust programming language',
    );
    expect(emptyAssistantBubbles).toHaveLength(0);
    expect(threadBubbles[0]?.role).toBe('user');
  });

  test('research podcast delivers exactly one audio card after reload', async ({
    page,
  }) => {
    const prompt =
      '不要搜索，直接生成一个简短测试播客并把音频发回会话。脚本： [杨幂 - clone:yangmi, professional] 大家好。 [窦文涛 - clone:douwentao, professional] 这里是测试播客。 [杨幂 - clone:yangmi, professional] 今天只做一次快速验证。 [窦文涛 - clone:douwentao, professional] 感谢收听。';

    await sendAndWait(page, prompt, {
      label: 'live-podcast-smoke',
      maxWait: 90_000,
    });

    let audioAttachments: RenderedAudioAttachment[] = [];
    for (let i = 0; i < 30; i++) {
      await page.waitForTimeout(3_000);
      audioAttachments = await getRenderedAudioAttachments(page);
      if (audioAttachments.length > 0) break;
    }

    expect(audioAttachments.length).toBeGreaterThan(0);

    await page.reload({ waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await page.waitForTimeout(8_000);

    const threadBubbles = await getRenderedThreadBubbles(page);
    audioAttachments = await getRenderedAudioAttachments(page);
    const duplicateAudio = findDuplicateAudioAttachments(audioAttachments);
    const promptIndex = threadBubbles.findIndex(
      (bubble) =>
        bubble.role === 'user' &&
        bubble.text.includes('不要搜索，直接生成一个简短测试播客'),
    );
    const firstAssistantIndex = threadBubbles.findIndex(
      (bubble) => bubble.role === 'assistant',
    );

    expect(promptIndex).toBe(0);
    expect(firstAssistantIndex).toBeGreaterThan(promptIndex);
    expect(duplicateAudio).toHaveLength(0);
    expect(audioAttachments).toHaveLength(1);
  });
});
