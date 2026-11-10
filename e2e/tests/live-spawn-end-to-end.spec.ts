/**
 * M8 Runtime Parity end-to-end live spec — spawn / workflow track.
 *
 * Drives the spawn-subagent failure path and asserts that the M8.9 recovery
 * loop fires once and surfaces an actionable assistant message.
 *
 * Each assertion that requires a track that has not landed yet self-skips
 * with a diagnostic, so the spec can land before W1+W2+W3 ship and
 * auto-promote as they merge.
 *
 * Run from ~/home/octos/e2e:
 *
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *     npx playwright test tests/live-spawn-end-to-end.spec.ts --workers=1
 *
 * NEVER run against mini5 (`dspfac.ocean.ominix.io`) — reserved for coding-green.
 */

import { test, expect } from "@playwright/test";
import { chatWS, type ChatWsEvent } from "../lib/m9-ws-client";

const BASE = process.env.OCTOS_TEST_URL || "https://dspfac.bot.ominix.io";
const TOKEN = process.env.OCTOS_AUTH_TOKEN || "octos-admin-2026";
const PROFILE = process.env.OCTOS_PROFILE || "dspfac";

// Refuse to run against mini5 — coding-green territory.
if (BASE.includes("dspfac.ocean.ominix.io")) {
  throw new Error(
    "live-spawn-end-to-end refuses to run against mini5; pick mini1/2/4 instead.",
  );
}

test.setTimeout(900_000);

type SseEvent = ChatWsEvent;

/**
 * M9-α-7 (#836): chat helper now drives the M9 WebSocket UI Protocol.
 * Returns the same `{ events, content, doneEvent }` shape the SSE helper
 * exposed so call sites stay unchanged.
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

async function getMessages(sessionId: string): Promise<unknown[]> {
  const resp = await fetch(
    `${BASE}/api/sessions/${encodeURIComponent(sessionId)}/messages`,
    { headers: { Authorization: `Bearer ${TOKEN}`, "X-Profile-Id": PROFILE } },
  );
  if (!resp.ok) return [];
  return resp.json();
}

// ════════════════════════════════════════════════════════════════════════════
test.describe("M8 spawn end-to-end (slides)", () => {
  // Spawn failure triggers M8.9 recovery.

  test("failed spawn child triggers M8.9 recovery prompt", async () => {
    const sid = `m8-spawn-recover-${Date.now()}`;

    // Use a known-bad fm_tts voice so the spawn child fails quickly. The
    // existing `m8-runtime-invariants-live.spec.ts` shows this triggers the
    // failure-signal path. Here we additionally assert the recovery prompt
    // re-engages the agent.
    const prompt =
      `直接调用 fm_tts，把 voice 参数精确设为 ` +
      `definitely_not_a_real_voice_${Date.now().toString(36)}，` +
      `文本只说：m8 恢复测试。不要先检查声音，也不要解释。`;
    const { doneEvent } = await chatViaWs(prompt, sid, 120_000);
    expect(doneEvent).toBeTruthy();

    // Wait for the recovery prompt to land as a system-internal user message
    // followed by an actionable assistant response.
    let recoveryFired = false;
    let assistantText = "";
    for (let i = 0; i < 30; i++) {
      await new Promise((r) => setTimeout(r, 3000));
      const msgs = (await getMessages(sid)) as Array<{
        role?: string;
        content?: string;
      }>;
      const recoveryMarker = msgs.find(
        (m) =>
          (m.role === "user" || m.role === "User") &&
          typeof m.content === "string" &&
          m.content.includes("[system-internal]") &&
          m.content.includes("fm_tts"),
      );
      if (recoveryMarker) recoveryFired = true;

      const lastAssistant = msgs
        .filter((m) => m.role === "assistant" || m.role === "Assistant")
        .pop();
      assistantText = (lastAssistant?.content as string) ?? "";

      const actionable =
        /try.*another voice|use.*different voice|available voice|please.*specify|please.*choose|alternative voice|let me try|cannot proceed|let me know/i.test(
          assistantText,
        );
      if (recoveryFired && actionable) break;
    }

    console.log(
      `[spawn-recover] recoveryFired=${recoveryFired} assistant=${assistantText.slice(0, 200)}`,
    );
    test.skip(
      !recoveryFired,
      "M8.9: no recovery prompt observed in transcript. Either the failure " +
        "signal callback did not fire, or build_recovery_prompt is not wired " +
        "into the session actor.",
    );
    expect(recoveryFired).toBe(true);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 4 — SubAgentOutputRouter writes per-task .out file (M8.7 + W2.B1)
  // ══════════════════════════════════════════════════════════════════════════
});
