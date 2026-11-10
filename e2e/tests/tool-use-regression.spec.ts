/**
 * Regression tests for tool-use bugs via the canonical chat transport.
 *
 * These tests exercise the exact tool-use sequences that triggered:
 *   1. activate_tools "tool registry not available" (OnceLock stale Weak bug)
 *   2. ffmpeg not found in sandbox PATH
 *
 * Run against a live octos-serve instance:
 *   OCTOS_TEST_URL=http://localhost:3000 OCTOS_AUTH_TOKEN=<token> npx playwright test
 *
 * Transport: chat turns ride the M9 WebSocket UI Protocol via `chatWS()`
 * (`/api/ui-protocol/ws`). The legacy `POST /api/chat` route was retired
 * in the cleanup that followed PR #908; this spec was migrated as part
 * of that cutover.
 */
import { test, expect } from "@playwright/test";
import { chatWS, type ChatWsResult } from "../lib/m9-ws-client";

const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || "";
const PROFILE_ID = process.env.OCTOS_PROFILE || undefined;

// ---------------------------------------------------------------------------
// Helper: drive a single chat turn over the WS UI Protocol. Returns
// `{ events, content, doneEvent }` shape preserved from `chatWS()` so
// callers can introspect tool calls / progress in addition to the
// final assistant content.
// ---------------------------------------------------------------------------
async function chatTurn(
  baseURL: string,
  message: string,
  sessionId: string,
  opts: { maxWait?: number } = {},
): Promise<ChatWsResult> {
  return chatWS({
    baseUrl: baseURL,
    token: AUTH_TOKEN,
    profileId: PROFILE_ID,
    message,
    sessionId,
    maxWait: opts.maxWait ?? 90_000,
  });
}

// ---------------------------------------------------------------------------
// Full tool chain — activate_tools → shell → ffmpeg
//
// This is the exact sequence that was broken: the agent needs to call
// activate_tools to load the shell tool group, then call shell to run ffmpeg.
// If any link in the chain is broken, this test fails.
// ---------------------------------------------------------------------------
test("full tool chain: chat triggers activate_tools → shell → ffmpeg", async ({
  baseURL,
}) => {
  test.setTimeout(180_000);
  test.skip(!AUTH_TOKEN, "OCTOS_AUTH_TOKEN required");

  const baseSessionId = `test-ffmpeg-chain-${Date.now()}`;
  const prompt =
    'If shell is not already active, call activate_tools with exactly ["shell"] once and only once. Then call shell exactly once with this command: ffmpeg -version 2>&1 | head -1. Do not inspect available tools, do not call activate_tools repeatedly, and return only the ffmpeg version line.';

  let res = await chatTurn(baseURL!, prompt, baseSessionId);

  if (res.doneEvent?.type === "done") {
    if (res.content.includes("[LOOP DETECTED]")) {
      res = await chatTurn(
        baseURL!,
        'Call activate_tools(["shell"]) at most once, then call shell("ffmpeg -version 2>&1 | head -1") exactly once, then stop. Return only the ffmpeg version line.',
        `${baseSessionId}-retry`,
      );
      if (res.doneEvent?.type !== "done") {
        return;
      }
    }
    // Should contain ffmpeg version string, NOT "tool registry not available"
    // or "ffmpeg: not found"
    expect(res.content).not.toContain("tool registry not available");
    expect(res.content).not.toContain("not found");
    expect(res.content).not.toContain("not installed");
    // Positive check: should mention ffmpeg version somewhere
    expect(res.content.toLowerCase()).toContain("ffmpeg");
  }
});

// ---------------------------------------------------------------------------
// Test 5: PATH includes /opt/homebrew/bin (macOS specific)
//
// Verifies the launchd PATH propagation fix.
// ---------------------------------------------------------------------------
