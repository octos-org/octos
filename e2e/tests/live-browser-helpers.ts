import { expect, type Page } from '@playwright/test';

const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE_ID = process.env.OCTOS_PROFILE || 'dspfac';
const TEST_EMAIL = process.env.OCTOS_TEST_EMAIL || 'dspfac@gmail.com';
const BASE_URL = process.env.OCTOS_TEST_URL || 'http://localhost:3000';

// When the daemon comes up with no `admin_token.json` (bootstrap mode), the
// dashboard's BootstrapGate redirects every `/admin/*` route to
// `/admin/setup/welcome` until the bootstrap token has been rotated to a
// hashed persistent record. That breaks any spec that drives the dashboard
// SPA. The token below is what we rotate to on first use; it satisfies the
// daemon's strength check (>=32 chars, >=3 char classes from
// {lowercase, uppercase, digits, symbols}).
const STRONG_ADMIN_TOKEN =
  process.env.OCTOS_TEST_ADMIN_TOKEN || 'Octos-E2E-Strong-Token-2026-XYZ-123!';

let tokenRotationPromise: Promise<string> | null = null;

/**
 * Rotate the bootstrap admin token to a known strong value if the daemon is
 * still in bootstrap mode. Returns the token that callers should use for all
 * subsequent auth — either the rotated strong token, or the original token
 * if rotation isn't needed (or isn't possible because it was already rotated
 * to a different value).
 *
 * The returned token is memoised at the module level so multiple specs in
 * the same Playwright process share the work.
 */
export async function ensureAdminTokenRotated(
  baseUrl: string = BASE_URL,
  currentToken: string = AUTH_TOKEN,
): Promise<string> {
  if (!tokenRotationPromise) {
    tokenRotationPromise = (async () => {
      const probe = async (token: string): Promise<{ rotated?: boolean } | null> => {
        try {
          const resp = await fetch(`${baseUrl}/api/admin/token/status`, {
            headers: { Authorization: `Bearer ${token}` },
          });
          if (!resp.ok) return null;
          return (await resp.json().catch(() => null)) as
            | { rotated?: boolean }
            | null;
        } catch {
          return null;
        }
      };

      // 1) Probe with the caller-provided token first.
      const currentStatus = await probe(currentToken);

      if (currentStatus) {
        if (!currentStatus.rotated) {
          // Bootstrap mode — rotate to the strong token now.
          const rotateResp = await fetch(`${baseUrl}/api/admin/token/rotate`, {
            method: 'POST',
            headers: {
              Authorization: `Bearer ${currentToken}`,
              'Content-Type': 'application/json',
            },
            body: JSON.stringify({ new_token: STRONG_ADMIN_TOKEN }),
          });
          if (!rotateResp.ok) {
            // Cross-worker race: another Playwright worker process may have
            // rotated first, which leaves us with a 401 (current bootstrap
            // token now invalid) or a 409 (token store already populated).
            // In both cases, probe with STRONG_ADMIN_TOKEN; if it works,
            // the other worker rotated to the same target and we're done.
            const fallbackStatus = await probe(STRONG_ADMIN_TOKEN);
            if (fallbackStatus) return STRONG_ADMIN_TOKEN;
            if (rotateResp.status !== 409) {
              const text = await rotateResp.text().catch(() => '');
              throw new Error(
                `failed to rotate admin token: ${rotateResp.status} ${text}`,
              );
            }
          }
          return STRONG_ADMIN_TOKEN;
        }
        // Already rotated and the current token still works — caller passed
        // the rotated token directly. Use it as-is.
        return currentToken;
      }

      // 2) currentToken didn't authenticate. Common case after the first
      //    run on a host: the bootstrap token is gone but the caller is
      //    still passing it. Fall back to the strong token we previously
      //    rotated to (or that someone matching `OCTOS_TEST_ADMIN_TOKEN`
      //    rotated to). If THAT works, use it.
      const strongStatus = await probe(STRONG_ADMIN_TOKEN);
      if (strongStatus) {
        return STRONG_ADMIN_TOKEN;
      }

      // 3) Neither token works. Return currentToken so the spec fails
      //    with the original auth error rather than a silent fallback.
      return currentToken;
    })();
  }
  return tokenRotationPromise;
}

/**
 * The token consumers should use for `Authorization: Bearer ...` and for the
 * `octos_session_token` / `octos_auth_token` localStorage entries. Resolves
 * to the rotated strong token when the helper had to bootstrap the daemon,
 * otherwise to whatever `OCTOS_AUTH_TOKEN` was passed in.
 */
export async function getEffectiveAdminToken(): Promise<string> {
  return ensureAdminTokenRotated();
}

export const SEL = {
  chatInput: "[data-testid='chat-input']",
  sendButton: "[data-testid='send-button']",
  cancelButton: "[data-testid='cancel-button']",
  userMessage: "[data-testid='user-message']",
  assistantMessage: "[data-testid='assistant-message']",
  newChatButton: "[data-testid='new-chat-button']",
  // Prefer testids; fall back to type-based selectors so this helper
  // works against both new builds (with testids from PR #625) and the
  // already-deployed fleet (which still uses pre-testid bundles).
  loginTokenInput:
    "[data-testid='token-input'], #admin-token, input[type='password']",
  loginButton:
    "[data-testid='login-button'], button[type='submit']:has-text('Login'), button[type='submit']:has-text('Verifying')",
} as const;

export async function login(page: Page) {
  // Bootstrap-mode daemons reject every `/admin/*` page until the admin
  // token has been rotated. Do this once per Playwright process and use the
  // rotated value everywhere below.
  const effectiveToken = await ensureAdminTokenRotated();

  await page.addInitScript(
    ({ token, profile }) => {
      localStorage.setItem('octos_session_token', token);
      localStorage.setItem('octos_auth_token', token);
      localStorage.setItem('selected_profile', profile);
    },
    { token: effectiveToken, profile: PROFILE_ID },
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

  // Dashboard renders the admin-token escape hatch as a small text button
  // (LoginPage.tsx) tagged `data-testid="admin-token-tab"`. Fall back to
  // the visible label for older builds that pre-date the testid.
  const authTokenTab = page
    .locator(
      "[data-testid='admin-token-tab'], button:has-text('Login with admin token'), button:has-text('Auth Token')",
    )
    .first();
  if (await authTokenTab.isVisible().catch(() => false)) {
    await authTokenTab.click();
    await page.locator(SEL.loginTokenInput).fill(effectiveToken);
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
      { email: TEST_EMAIL, code: effectiveToken },
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

export async function createNewSession(page: Page) {
  await page.locator(SEL.newChatButton).click();
  await page.waitForTimeout(1_000);
}

export function getInput(page: Page) {
  return page.locator(SEL.chatInput).first();
}

export function getSendButton(page: Page) {
  return page.locator(SEL.sendButton).first();
}

export async function getChatThreadText(page: Page): Promise<string> {
  const texts = await page
    .locator("[data-testid='user-message'], [data-testid='assistant-message']")
    .allTextContents()
    .catch(() => []);
  return texts.join('\n');
}

export interface AssistantLink {
  text: string;
  href: string;
  download: string;
}

export async function getAssistantLinks(page: Page): Promise<AssistantLink[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll("[data-testid='assistant-message'] a")).map(
      (node) => {
        const el = node as HTMLAnchorElement;
        return {
          text: (el.textContent || '').trim(),
          href: el.href || '',
          download: el.download || '',
        };
      },
    ),
  );
}

export async function getAssistantMessageText(page: Page): Promise<string> {
  const texts = await page
    .locator("[data-testid='assistant-message']")
    .allTextContents()
    .catch(() => []);
  return texts.join('\n');
}

export async function countUserBubbles(page: Page) {
  return page.locator(SEL.userMessage).count();
}

export async function countAssistantBubbles(page: Page) {
  return page.locator(SEL.assistantMessage).count();
}

export async function sendAndWait(
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
  let timedOut = false;

  while (Date.now() - start < maxWait) {
    await page.waitForTimeout(3_000);

    const isStreaming = await page
      .locator(SEL.cancelButton)
      .isVisible()
      .catch(() => false);

    const assistantCount = await countAssistantBubbles(page);

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

    if (!isStreaming && assistantCount > 0 && currentText.length > 0 && currentText === lastText) {
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

  if (Date.now() - start >= maxWait) {
    timedOut = true;
    if (throwOnTimeout) {
      throw new Error(
        `sendAndWait timed out after ${maxWait / 1000}s for message: "${message.slice(0, 60)}"`,
      );
    }
  }

  const assistantBubbles = await countAssistantBubbles(page);
  const finalText =
    assistantBubbles > 0
      ? await page.locator(SEL.assistantMessage).last().textContent()
      : '';

  return {
    assistantBubbles,
    responseText: finalText?.trim() || '',
    responseLen: finalText?.trim().length || 0,
    timedOut,
  };
}

export async function expectSingleTurn(page: Page) {
  await expect.poll(() => countUserBubbles(page)).toBe(1);
  await expect.poll(() => countAssistantBubbles(page)).toBe(1);
}
