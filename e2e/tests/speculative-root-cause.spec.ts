/**
 * FA-11 root-cause probe for the queue-speculative "BRAVO never completes"
 * failure. FA-10 labelled this a "single-provider backend limitation"; that
 * theory is wrong — a single LLM provider serves N concurrent HTTP dispatches
 * just fine. This probe captures the network + DOM evidence needed to pin
 * the real root cause.
 *
 * Hypotheses (see FA-11 investigation brief):
 *   1. Client drops 2nd POST under queue-mode=speculative.
 *   2. Backend accepts 2nd POST but doesn't dispatch.
 *   3. Backend dispatches + responds, client drops it (out-of-order race).
 *   4. Backend SSE stream for B is started but never terminated.
 *
 * What this probe measures:
 *   - Every POST /api/chat request body + response status + first 2KB of body.
 *   - Whether B's POST is a JSON "queued" acknowledgment (indicating the
 *     client never opens a fresh SSE for B and relies on A's stream).
 *   - Whether B's assistant reply actually lands in session history via
 *     GET /api/sessions/:id/messages (backend persisted but not delivered).
 *   - Final DOM state for both ALPHA and BRAVO bubbles.
 *
 * Run against mini1:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/speculative-root-cause.spec.ts \
 *     --workers=1 --reporter=list
 */

import { test, expect } from '@playwright/test';
import { login, SEL } from './live-browser-helpers';

const TEST_URL = process.env.OCTOS_TEST_URL || '';
const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE_ID = process.env.OCTOS_PROFILE || 'dspfac';

const SKIP_REASON = 'OCTOS_TEST_URL not set; skipping FA-11 probe';

interface ChatPostRecord {
  ts: number;
  urlPath: string;
  requestBody: string;
  status: number;
  contentType: string;
  bodyPreview: string;
  bodyTotalBytes: number;
}

test.describe('FA-11 speculative queue-mode root-cause probe', () => {
  test.skip(() => !TEST_URL, SKIP_REASON);
  // 7 minutes — 60s observation window per prompt + login + slash ack overhead.
  test.setTimeout(420_000);

  test('capture POST/response + final history for ALPHA + BRAVO', async ({
    page,
  }) => {
    // ── Network capture: every /api/chat POST + response ─────────────────
    const posts: ChatPostRecord[] = [];
    const sseChunks: { ts: number; chunk: string }[] = [];

    // Capture POST request bodies BEFORE they hit the network.
    page.on('request', (req) => {
      try {
        const url = req.url();
        if (!url.includes('/api/chat')) return;
        if (req.method() !== 'POST') return;
        const pd = req.postData() || '';
        posts.push({
          ts: Date.now(),
          urlPath: new URL(url).pathname + (new URL(url).search || ''),
          requestBody: pd.slice(0, 1_000),
          status: -1,
          contentType: '',
          bodyPreview: '',
          bodyTotalBytes: -1,
        });
      } catch {
        /* ignore */
      }
    });

    // Capture responses for those POSTs.
    page.on('response', async (res) => {
      try {
        const req = res.request();
        if (!req.url().includes('/api/chat')) return;
        if (req.method() !== 'POST') return;
        const ct = res.headers()['content-type'] || '';
        const status = res.status();
        // Match to the most recent unfilled POST record.
        const record = posts
          .slice()
          .reverse()
          .find((r) => r.status === -1 && r.requestBody && req.postData() === r.requestBody);
        if (!record) return;
        record.status = status;
        record.contentType = ct;

        // SSE streams won't let us read the body without blocking. For JSON
        // we can. Detect the difference and behave accordingly.
        if (ct.includes('application/json')) {
          try {
            const buf = await res.body();
            record.bodyTotalBytes = buf.length;
            record.bodyPreview = buf.toString('utf8').slice(0, 2_000);
          } catch (e) {
            record.bodyPreview = `<<body-read-failed: ${(e as Error).message}>>`;
          }
        } else {
          // SSE path — mark it so we know it's a live stream.
          record.contentType = ct;
          record.bodyPreview = '<<SSE stream — body read-through intentionally skipped>>';
          record.bodyTotalBytes = -1;
        }
      } catch {
        /* ignore */
      }
    });

    // Separately hook raw SSE chunk arrivals so we can correlate what the
    // browser actually received on whichever stream is live.
    await page.exposeFunction('__fa11_pushSseChunk', (chunk: string) => {
      sseChunks.push({ ts: Date.now(), chunk });
    });

    await page.addInitScript(() => {
      // Wrap fetch so we can also peek at SSE streams. We tee the body into
      // a side channel readable from Node land.
      const origFetch = window.fetch;
      // @ts-ignore
      window.fetch = async (...args: Parameters<typeof fetch>) => {
        const res = await origFetch.apply(window, args as any);
        try {
          const url = typeof args[0] === 'string' ? args[0] : (args[0] as Request).url;
          if (url && url.includes('/api/chat') && res.body && !res.bodyUsed) {
            const ct = res.headers.get('content-type') || '';
            if (ct.includes('text/event-stream') || ct.includes('application/x-ndjson')) {
              const [a, b] = res.body.tee();
              const clone = new Response(a, {
                status: res.status,
                statusText: res.statusText,
                headers: res.headers,
              });
              (async () => {
                const reader = b.getReader();
                const dec = new TextDecoder();
                let buf = '';
                for (;;) {
                  const { value, done } = await reader.read();
                  if (done) break;
                  buf += dec.decode(value, { stream: true });
                  const parts = buf.split('\n\n');
                  buf = parts.pop() || '';
                  for (const p of parts) {
                    // @ts-ignore
                    if (window.__fa11_pushSseChunk) window.__fa11_pushSseChunk(p.slice(0, 400));
                  }
                }
                if (buf) {
                  // @ts-ignore
                  if (window.__fa11_pushSseChunk) window.__fa11_pushSseChunk(buf.slice(0, 400));
                }
              })();
              return clone;
            }
          }
        } catch {
          /* ignore */
        }
        return res;
      };
    });

    await login(page);
    await page.goto('/chat', { waitUntil: 'networkidle' });
    await page.waitForSelector(SEL.chatInput);

    // Fresh session — avoid dragging prior context.
    const newChat = page.locator(SEL.newChatButton);
    if (await newChat.isVisible().catch(() => false)) {
      await newChat.click();
      await page.waitForSelector(SEL.chatInput);
      await page.waitForTimeout(1_000);
    }

    const input = page.locator(SEL.chatInput);
    const sendBtn = page.locator(SEL.sendButton);

    // 1) Enable speculative mode.
    await input.fill('/queue speculative');
    await sendBtn.click();
    await expect(page.locator(SEL.assistantMessage)).toHaveCount(1, {
      timeout: 30_000,
    });

    // Poll for the badge to confirm mode took effect server-side.
    const badgeOrText = await page
      .locator('[data-testid="queue-mode-badge"], text=/speculative/i')
      .first()
      .isVisible({ timeout: 15_000 })
      .catch(() => false);
    console.log(`[FA-11] queue-mode-badge visible: ${badgeOrText}`);

    const sessionId = await page.evaluate(
      () => localStorage.getItem('octos_current_session') || '',
    );
    console.log(`[FA-11] current session_id (localStorage): "${sessionId}"`);

    // 2) Send ALPHA, wait for user bubble to confirm it committed.
    const tAlpha = Date.now();
    await input.fill('Use shell: echo FA11-ALPHA-marker');
    await sendBtn.click();
    await expect(page.locator(SEL.userMessage)).toHaveCount(2, {
      timeout: 30_000,
    });

    // Wait briefly — same as coding-blue-long-running.spec does.
    await page.waitForTimeout(1_500);

    // 3) Send BRAVO while ALPHA is presumably still running.
    const tBravo = Date.now();
    await input.fill('Use shell: echo FA11-BRAVO-marker');
    await sendBtn.click();
    await expect(page.locator(SEL.userMessage)).toHaveCount(3, {
      timeout: 30_000,
    });

    // 4) Observe for up to 75s to see ALPHA finish + BRAVO's fate.
    const OBS_MS = 75_000;
    const deadline = Date.now() + OBS_MS;
    while (Date.now() < deadline) {
      await page.waitForTimeout(3_000);
      const cancelVisible = await page
        .locator(SEL.cancelButton)
        .isVisible()
        .catch(() => false);
      const assistantCount = await page.locator(SEL.assistantMessage).count();
      const bubbleTexts = await page
        .locator(SEL.assistantMessage)
        .allInnerTexts()
        .catch(() => []);
      const joined = bubbleTexts.join(' | ').toUpperCase();
      const hasAlpha = joined.includes('ALPHA');
      const hasBravo = joined.includes('BRAVO');
      console.log(
        `[FA-11 +${((Date.now() - tAlpha) / 1000).toFixed(0)}s] bubbles=${assistantCount} streaming=${cancelVisible} ALPHA=${hasAlpha} BRAVO=${hasBravo}`,
      );
      if (hasAlpha && hasBravo && !cancelVisible) {
        console.log('[FA-11] both markers delivered, exiting observe early');
        break;
      }
    }

    // 5) Final DOM state.
    const cancelVisibleFinal = await page
      .locator(SEL.cancelButton)
      .isVisible()
      .catch(() => false);
    const spinners = await page
      .locator('[data-testid^="task-anchor-spinner-"]')
      .count();
    const finalBubbles = await page
      .locator(SEL.assistantMessage)
      .allInnerTexts()
      .catch(() => []);
    const finalUpper = finalBubbles.join(' | ').toUpperCase();

    console.log('\n========== FA-11 PROBE REPORT ==========\n');
    console.log(`POST /api/chat requests (count=${posts.length}):`);
    for (let i = 0; i < posts.length; i += 1) {
      const p = posts[i];
      console.log(
        `  [${i}] +${((p.ts - tAlpha) / 1000).toFixed(1)}s ` +
          `path=${p.urlPath} ` +
          `reqBody=${p.requestBody.slice(0, 180)} -> ` +
          `status=${p.status} ct=${p.contentType} ` +
          `bytes=${p.bodyTotalBytes} ` +
          `body=${p.bodyPreview.replace(/\s+/g, ' ').slice(0, 300)}`,
      );
    }

    console.log(`\nSSE chunks observed (count=${sseChunks.length}):`);
    for (let i = 0; i < sseChunks.length; i += 1) {
      const c = sseChunks[i];
      console.log(
        `  [${i}] +${((c.ts - tAlpha) / 1000).toFixed(1)}s ${c.chunk.replace(/\s+/g, ' ').slice(0, 240)}`,
      );
    }

    console.log('\nFinal DOM:');
    console.log(`  cancel visible: ${cancelVisibleFinal}`);
    console.log(`  spinners: ${spinners}`);
    console.log(`  assistant bubbles: ${finalBubbles.length}`);
    for (let i = 0; i < finalBubbles.length; i += 1) {
      console.log(`    [${i}] ${finalBubbles[i].replace(/\s+/g, ' ').slice(0, 300)}`);
    }
    console.log(`  transcript contains ALPHA: ${finalUpper.includes('ALPHA')}`);
    console.log(`  transcript contains BRAVO: ${finalUpper.includes('BRAVO')}`);

    // 6) Check if backend actually PERSISTED the BRAVO reply to session
    // history — if yes, the bug is on the delivery path (client never saw
    // it); if no, the bug is upstream in dispatch.
    //
    // We hit /api/sessions/:id/messages?source=full for definitive truth.
    if (sessionId) {
      try {
        const hist = await page.evaluate(
          async ({ sid, token, profile }) => {
            const res = await fetch(
              `/api/sessions/${encodeURIComponent(sid)}/messages?limit=200&source=full`,
              {
                headers: {
                  Authorization: `Bearer ${token}`,
                  'X-Profile-Id': profile,
                },
              },
            );
            if (!res.ok) return { ok: false, status: res.status, items: [] };
            const data = await res.json();
            return { ok: true, status: res.status, items: Array.isArray(data) ? data : [] };
          },
          { sid: sessionId, token: AUTH_TOKEN, profile: PROFILE_ID },
        );
        console.log('\nBackend session history (source=full):');
        console.log(`  status=${hist.status} items=${hist.items.length}`);
        if (hist.items && Array.isArray(hist.items)) {
          for (let i = 0; i < hist.items.length; i += 1) {
            const m = hist.items[i];
            const line =
              `${m.role || '?'} [${(m.timestamp || '').replace('T', ' ').slice(0, 19)}] ` +
              `${(m.content || '').replace(/\s+/g, ' ').slice(0, 180)}`;
            console.log(`    [${i}] ${line}`);
          }
          const persistedUpper = hist.items
            .map((m: { content?: string }) => (m.content || '').toUpperCase())
            .join(' | ');
          console.log(
            `  persisted transcript contains ALPHA: ${persistedUpper.includes('ALPHA')}`,
          );
          console.log(
            `  persisted transcript contains BRAVO: ${persistedUpper.includes('BRAVO')}`,
          );
        }
      } catch (e) {
        console.log(`  history fetch failed: ${(e as Error).message}`);
      }
    }

    console.log('\n========================================\n');

    // Soft assertion — this probe is diagnostic, not a blocker.
    // The only invariant we enforce is that the slash ack + ALPHA user bubble
    // + BRAVO user bubble all exist. If they don't, the probe itself is
    // broken — not a signal about the backend.
    expect(posts.length, 'at least 3 POST /api/chat (slash + ALPHA + BRAVO)').toBeGreaterThanOrEqual(
      3,
    );

    // Hypothesis flag: was BRAVO's POST response JSON (queued) or SSE?
    // If the 3rd POST came back as application/json with "queued", that's
    // strong evidence for hypothesis #3/#4 — the client never opened an SSE
    // for BRAVO and depended on ALPHA's stream for delivery (which closed
    // at ALPHA's _completion, stranding BRAVO's reply at the api_channel
    // pending tx).
    const bravoPost = posts[posts.length - 1];
    console.log(
      `\n[FA-11 HYPOTHESIS SIGNAL] BRAVO POST content-type: "${bravoPost?.contentType}" body: ${bravoPost?.bodyPreview?.slice(0, 160)}`,
    );
  });
});
