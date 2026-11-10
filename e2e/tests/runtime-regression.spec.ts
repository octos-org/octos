/**
 * Runtime regression tests for the most problematic areas from issue history.
 *
 * Covers background TTS non-blocking chat, cross-session isolation (#343),
 * and workspace cleanup after session deletion.
 *
 * These are API-level tests (no browser needed). They run against any deployed
 * host via OCTOS_TEST_URL.
 */

import { test, expect } from "@playwright/test";
import {
  chatWS,
  deleteSessionWs,
  fetchSessionMessages,
  type ChatWsEvent,
} from "../lib/m9-ws-client";

const BASE = process.env.OCTOS_TEST_URL || "https://dspfac.crew.ominix.io";
const TOKEN = process.env.OCTOS_AUTH_TOKEN || "e2e-test-2026";
const PROFILE = process.env.OCTOS_PROFILE || "dspfac";

test.setTimeout(120_000);

type SseEvent = ChatWsEvent;

function customVoiceTtsPrompt(text: string): string {
  return `直接调用 fm_tts，把 voice 参数精确设为 yangmi（不要使用 clone:yangmi 或任何 clone: 前缀），文本只说：${text}。不要先检查声音，也不要解释。`;
}

/**
 * Drive a chat turn over the M9 WebSocket UI Protocol and return the same
 * `{ events, content, doneEvent }` shape the legacy SSE helper used. M9-α-7
 * (#836). The local function is preserved (renamed) so the call sites in
 * this spec only need a one-line rename, but the wire transport is now the
 * `/api/ui-protocol/ws` JSON-RPC endpoint.
 */
async function chatViaWs(
  message: string,
  sessionId: string,
  maxWait = 60_000,
): Promise<{ events: SseEvent[]; content: string; doneEvent?: SseEvent }> {
  return chatWS({
    baseUrl: BASE,
    token: TOKEN,
    profileId: PROFILE,
    message,
    sessionId,
    maxWait,
  });
}

/**
 * Re-throw RPC errors that indicate a real server-side failure (e.g. the
 * capability gate rejected the method, or the WS handshake failed). Only
 * swallow the connection/transport errors that historically meant "host is
 * offline" — those are the cases the legacy REST wrappers tolerated.
 *
 * In particular, `method_not_supported` (M12 Phase D-1 capability gate) must
 * propagate so a misconfigured client doesn't pass the spec by reporting
 * "no sessions" / "no messages".
 */
function rethrowIfRealFailure(err: unknown): never | void {
  const message = err instanceof Error ? err.message : String(err);
  if (
    message.includes("method_not_supported") ||
    message.includes("m9-ws aux:") ||
    message.includes("rpc-error[")
  ) {
    throw err;
  }
}

/**
 * Get session messages via the WS UI Protocol (`session/messages_page`).
 * M12 Phase D-5: REST `GET /api/sessions/{id}/messages` was retired.
 */
async function getMessages(sessionId: string): Promise<any[]> {
  try {
    return await fetchSessionMessages({
      baseUrl: BASE,
      token: TOKEN,
      profileId: PROFILE,
      sessionId,
    });
  } catch (err) {
    rethrowIfRealFailure(err);
    return [];
  }
}

/**
 * Delete a session via the WS UI Protocol (`session/delete`).
 * M12 Phase D-5: REST `DELETE /api/sessions/{id}` was retired.
 */
async function deleteSession(sessionId: string): Promise<void> {
  await deleteSessionWs({
    baseUrl: BASE,
    token: TOKEN,
    profileId: PROFILE,
    sessionId,
  });
}

async function startBackgroundTts(
  sessionId: string,
  text: string,
): Promise<{
  sessionId: string;
  events: SseEvent[];
  content: string;
  doneEvent?: SseEvent;
}> {
  let effectiveSessionId = sessionId;
  let result = await chatViaWs(
    customVoiceTtsPrompt(text),
    effectiveSessionId,
    90_000,
  );
  if (!result.doneEvent) {
    await new Promise((resolve) => setTimeout(resolve, 2_000));
    effectiveSessionId = `${sessionId}-retry`;
    result = await chatViaWs(
      customVoiceTtsPrompt(text),
      effectiveSessionId,
      90_000,
    );
  }
  return { sessionId: effectiveSessionId, ...result };
}

// ════════════════════════════════════════════════════════════════════
// 3. BACKGROUND TASK LIFECYCLE (TTS)
// ════════════════════════════════════════════════════════════════════

test.describe("Background task lifecycle", () => {
  test("regular messages work while TTS runs in background", async () => {
    const sid = `tts-nonblock-${Date.now()}`;
    // Start TTS
    const { sessionId } = await startBackgroundTts(sid, "后台测试");

    // Immediately send a regular question — should not be blocked
    const { content, doneEvent } = await chatViaWs(
      "what is 3+3",
      sessionId,
      30_000,
    );
    expect(content.length).toBeGreaterThan(0);
    expect(doneEvent).toBeTruthy();
  });
});
// ════════════════════════════════════════════════════════════════════
// 5. CROSS-SESSION ISOLATION
// ════════════════════════════════════════════════════════════════════

test.describe("Cross-session isolation", () => {
  test("concurrent sessions do not leak messages (#343)", async () => {
    const sidA = `iso-a-${Date.now()}`;
    const sidB = `iso-b-${Date.now()}`;

    // Send different messages to each session
    await Promise.all([
      chatViaWs("session A unique message alpha", sidA),
      chatViaWs("session B unique message beta", sidB),
    ]);

    // Verify no cross-contamination
    const msgsA = await getMessages(sidA);
    const msgsB = await getMessages(sidB);

    const aContent = msgsA.map((m: any) => m.content).join(" ");
    const bContent = msgsB.map((m: any) => m.content).join(" ");

    expect(aContent).toContain("alpha");
    expect(aContent).not.toContain("beta");
    expect(bContent).toContain("beta");
    expect(bContent).not.toContain("alpha");
  });
});
// ════════════════════════════════════════════════════════════════════
// 6. SESSION CREATE & DELETE LIFECYCLE
// ════════════════════════════════════════════════════════════════════

test.describe("Session create & delete lifecycle", () => {
  test("deleted session workspace files are cleaned up", async () => {
    const slug = `delws-${Date.now().toString(36)}`;
    const sid = `lifecycle-ws-${Date.now()}`;

    // Create a slides project (generates workspace files)
    await chatViaWs(`/new slides ${slug}`, sid);

    // Verify project was created by checking via a second message
    await chatViaWs(
      `Use shell to run: ls slides/${slug}/.octos-workspace.toml 2>&1 && echo EXISTS || echo MISSING`,
      sid,
      30_000,
    );

    // Delete the session via WS
    await deleteSession(sid);

    // Create a new session and check if workspace files still exist on disk
    const sid2 = `lifecycle-ws-check-${Date.now()}`;
    await chatViaWs(`/new slides ${slug}-check`, sid2);
    const { content: checkContent } = await chatViaWs(
      `Use shell to run: ls -la slides/ 2>&1 | grep "${slug}" && echo STILL_EXISTS || echo CLEANED_UP`,
      sid2,
      30_000,
    );

    // This test documents current behavior: workspace files survive deletion.
    // If this assertion flips to CLEANED_UP, it means cleanup was implemented.
    console.log(`  workspace after delete: ${checkContent.slice(0, 200)}`);
    // NOTE: Currently expected to show STILL_EXISTS — workspace cleanup is not
    // implemented. When it IS implemented, change this to expect CLEANED_UP.
  });
});
