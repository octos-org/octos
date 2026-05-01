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

// Cache of `host -> effective token`. The token rotation flow is per-host
// because every mini in the fleet has its own `admin_token.json`. Memoising
// across hosts (the previous module-scope `tokenRotationPromise`) caused
// every spec in a single Playwright process to inherit the FIRST host's
// answer — which broke as soon as different specs targeted different hosts
// or the page's actual `baseURL` differed from the env-derived `BASE_URL`.
const tokenCacheByHost: Map<string, Promise<string>> = new Map();

/**
 * Resolve a working admin Bearer token for the SPA auth surface (i.e. one
 * that `/api/auth/me` accepts as `AuthIdentity::Admin`).
 *
 * The dashboard SPA's `AuthGuard` runs `syncMe()` (calls `/api/auth/me`) on
 * every page load that has a token in localStorage. If the token doesn't
 * authenticate, AuthGuard CLEARS localStorage and redirects to `/login`.
 * That redirect is what makes `[data-testid='chat-input']` never appear and
 * the helper time out.
 *
 * Failure modes the previous helper missed:
 *
 *  1. **Already-rotated mini**: production minis have a hashed
 *     `admin_token.json` whose hash does NOT match the bootstrap value
 *     (`octos-admin-2026`). The bootstrap token returns 401 against
 *     `/api/auth/me` — even though `/api/sessions` still serves traffic
 *     because the Caddy proxy injects `X-Profile-Id` from the subdomain
 *     and that bypasses the admin gate.
 *
 *  2. **Wrong base URL**: the previous probe used the env-derived
 *     `BASE_URL` (default `http://localhost:3000`). When tests run against
 *     a remote mini without `OCTOS_TEST_URL` set, the probe hit a
 *     non-existent local daemon and silently returned the bootstrap token
 *     unchanged — guaranteeing 401 against the actual target.
 *
 * The fix probes BOTH candidate tokens against `/api/auth/me` (the same
 * endpoint the SPA uses) on the requested host. The strong token is tried
 * first because it's the steady-state on every production mini. If both
 * fail we fall back to the bootstrap-rotation flow used on freshly
 * provisioned daemons.
 */
export async function ensureAdminTokenRotated(
  baseUrl: string = BASE_URL,
  currentToken: string = AUTH_TOKEN,
): Promise<string> {
  const host = normaliseHost(baseUrl);
  let cached = tokenCacheByHost.get(host);
  if (cached) return cached;

  cached = (async () => {
    // `/api/auth/me` is the authoritative gate the SPA uses. A token that
    // returns 200 here will satisfy AuthGuard and let `/chat` render. The
    // server accepts both admin tokens and email-OTP user sessions on
    // this endpoint, but we want admin specifically — non-admin sessions
    // fail downstream when specs hit `/admin/*` SPA routes (e.g.
    // live-mofa-skills, live-realtime-status). Require `role === 'admin'`
    // so we don't silently degrade.
    const meProbe = async (token: string): Promise<boolean> => {
      try {
        const resp = await fetch(`${host}/api/auth/me`, {
          headers: { Authorization: `Bearer ${token}` },
        });
        if (!resp.ok) return false;
        const body = (await resp.json().catch(() => null)) as
          | { user?: { role?: string } }
          | null;
        return body?.user?.role === 'admin';
      } catch {
        return false;
      }
    };

    // 0) Steady state on production minis: STRONG_ADMIN_TOKEN matches the
    //    rotated `admin_token.json`. Try this first — both `currentToken`
    //    and `STRONG_ADMIN_TOKEN` are equally likely to be the right one
    //    depending on how the host was bootstrapped, but in practice any
    //    long-lived deploy has already been rotated.
    if (await meProbe(STRONG_ADMIN_TOKEN)) return STRONG_ADMIN_TOKEN;

    // 1) Caller passed the right token directly (e.g. fresh local daemon
    //    where `currentToken` IS the bootstrap token AND no rotation has
    //    happened yet, OR the caller threaded a known-rotated token via
    //    `OCTOS_AUTH_TOKEN`).
    if (await meProbe(currentToken)) return currentToken;

    // 2) Bootstrap mode — `/api/admin/token/status` says `rotated: false`,
    //    so we own the rotation. This branch only runs against fresh local
    //    daemons; on production minis step 0 already returned.
    const statusProbe = async (token: string): Promise<{ rotated?: boolean } | null> => {
      try {
        const resp = await fetch(`${host}/api/admin/token/status`, {
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

    const currentStatus = await statusProbe(currentToken);
    if (currentStatus && !currentStatus.rotated) {
      const rotateResp = await fetch(`${host}/api/admin/token/rotate`, {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${currentToken}`,
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({ new_token: STRONG_ADMIN_TOKEN }),
      });
      if (rotateResp.ok || rotateResp.status === 409) {
        // 409 = another worker rotated first; STRONG_ADMIN_TOKEN should
        // now work either way.
        if (await meProbe(STRONG_ADMIN_TOKEN)) return STRONG_ADMIN_TOKEN;
      }
    }

    // 3) Neither known token authenticates and we couldn't rotate. Return
    //    the strong token rather than the bootstrap value: the spec will
    //    still fail at the chat-input wait, but the surfaced auth error
    //    will hint at "rotated to a different secret" rather than the
    //    misleading "bootstrap token expired".
    // eslint-disable-next-line no-console
    console.warn(
      `[live-browser-helpers] no admin Bearer authenticates against ${host}/api/auth/me ` +
        `(tried STRONG_ADMIN_TOKEN and OCTOS_AUTH_TOKEN). Set OCTOS_TEST_ADMIN_TOKEN ` +
        `to the rotated admin secret for this host.`,
    );
    return STRONG_ADMIN_TOKEN;
  })();

  tokenCacheByHost.set(host, cached);
  return cached;
}

function normaliseHost(input: string): string {
  try {
    const u = new URL(input);
    return `${u.protocol}//${u.host}`;
  } catch {
    return input.replace(/\/+$/, '');
  }
}

/**
 * Best-effort recovery of the page's effective base URL. Playwright stores
 * it on `BrowserContext._options.baseURL` which is not on the public type,
 * so we fall back to `page.url()` when the page has navigated, then to the
 * env-derived `BASE_URL` constant.
 */
function pageBaseUrl(page: Page): string {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const ctxBase = (page.context() as any)?._options?.baseURL as
    | string
    | undefined;
  if (ctxBase) return ctxBase;
  const current = page.url();
  if (current && current !== 'about:blank') {
    try {
      return new URL(current).origin;
    } catch {
      // fall through
    }
  }
  return BASE_URL;
}

/**
 * The token consumers should use for `Authorization: Bearer ...` and for the
 * `octos_session_token` / `octos_auth_token` localStorage entries. Resolves
 * to the rotated strong token when the helper had to bootstrap the daemon,
 * otherwise to whatever `OCTOS_AUTH_TOKEN` was passed in.
 *
 * Pass `baseUrl` when the caller targets a host that doesn't match
 * `OCTOS_TEST_URL` (e.g. when a spec uses its own `BASE` constant or
 * threads the page baseURL through). Defaults preserve the previous
 * env-only behaviour so this call is backward compatible.
 */
export async function getEffectiveAdminToken(baseUrl?: string): Promise<string> {
  return ensureAdminTokenRotated(baseUrl);
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
  // The dashboard SPA `AuthGuard` calls `/api/auth/me` on mount. If that
  // returns 401 it CLEARS localStorage and redirects to `/login` — which is
  // why merely stuffing a Bearer into localStorage isn't enough. The token
  // we stuff has to authenticate as `AuthIdentity::Admin` against
  // `/api/auth/me` on THIS host. Resolve the right token (per host)
  // before we navigate.
  const effectiveToken = await ensureAdminTokenRotated(pageBaseUrl(page));

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
