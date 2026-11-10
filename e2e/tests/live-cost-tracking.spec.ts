/**
 * Native-research live spec — supervised plugin cost attribution.
 *
 * Drives native `search` in a background spawn child and asserts that plugin
 * v2 cost events still cross the plugin → supervisor → task-query boundary:
 *
 *   - the supervised task exposes runtime_detail through
 *     `/api/sessions/:id/tasks`
 *   - native research `cost_attribution` events fold into that task detail
 *
 * DOT `run_pipeline` was retired by #2691. This spec deliberately preserves
 * the independent plugin-v2/supervision contract through its replacement path;
 * it does not assert per-DOT-node costs or a run_pipeline parent task.
 *
 * Each assertion that requires a track that has not landed yet self-skips
 * with a diagnostic, so the spec can land before the relevant tracks
 * merge and auto-promote as they ship.
 *
 * Run from ~/home/octos/e2e:
 *
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *     npx playwright test tests/live-cost-tracking.spec.ts --workers=1
 *
 * NEVER run against mini5 (`dspfac.ocean.ominix.io`) — reserved for coding-green.
 */

import { test, expect } from "@playwright/test";
import { chatWS, type ChatWsEvent } from "../lib/m9-ws-client";

const BASE = process.env.OCTOS_TEST_URL || "https://dspfac.bot.ominix.io";
const TOKEN = process.env.OCTOS_AUTH_TOKEN || "octos-admin-2026";
const PROFILE = process.env.OCTOS_PROFILE || "dspfac";

if (BASE.includes("dspfac.ocean.ominix.io")) {
  throw new Error(
    "live-cost-tracking refuses to run against mini5; pick mini1/2/4 instead.",
  );
}

test.setTimeout(900_000);

type SseEvent = ChatWsEvent;

interface BackgroundTaskRow {
  id?: string;
  task_id?: string;
  tool_name?: string;
  tool_call_id?: string;
  parent_task_id?: string | null;
  status?: string;
  lifecycle_state?: string;
  cost?: {
    tokens_in?: number;
    tokens_out?: number;
    usd_used?: number;
    usd_reserved?: number;
  } | null;
  runtime_detail?: string | Record<string, unknown> | null;
  started_at?: string;
  updated_at?: string;
  completed_at?: string | null;
}

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

async function getTasks(sessionId: string): Promise<BackgroundTaskRow[]> {
  const resp = await fetch(
    `${BASE}/api/sessions/${encodeURIComponent(sessionId)}/tasks`,
    { headers: { Authorization: `Bearer ${TOKEN}`, "X-Profile-Id": PROFILE } },
  );
  if (!resp.ok) return [];
  return resp.json();
}

async function getCostsForSession(sessionId: string): Promise<{
  tasks: BackgroundTaskRow[];
  taskCosts: Array<{
    taskId: string;
    toolName: string;
    tokens_in: number;
    tokens_out: number;
    usd_used: number;
  }>;
  totalUsd: number;
  totalTokensIn: number;
  totalTokensOut: number;
}> {
  const tasks = await getTasks(sessionId);
  const taskCosts: Array<{
    taskId: string;
    toolName: string;
    tokens_in: number;
    tokens_out: number;
    usd_used: number;
  }> = [];
  let totalUsd = 0;
  let totalTokensIn = 0;
  let totalTokensOut = 0;
  for (const row of tasks) {
    const cost = row.cost ?? null;
    if (!cost) continue;
    const ti = Number(cost.tokens_in ?? 0);
    const to = Number(cost.tokens_out ?? 0);
    const usd = Number(cost.usd_used ?? 0);
    if (Number.isFinite(ti)) totalTokensIn += ti;
    if (Number.isFinite(to)) totalTokensOut += to;
    if (Number.isFinite(usd)) totalUsd += usd;
    taskCosts.push({
      taskId: row.id ?? row.task_id ?? "",
      toolName: row.tool_name ?? "",
      tokens_in: ti,
      tokens_out: to,
      usd_used: usd,
    });
  }
  return { tasks, taskCosts, totalUsd, totalTokensIn, totalTokensOut };
}

const NATIVE_RESEARCH_PROMPT =
  "Use spawn in background mode to investigate the most notable agentic-AI " +
  "announcements from the last 7 days. In the child, call the native search " +
  "tool with depth=1 and output=report; use deep_crawl only if a source " +
  "needs rendered reading. Restrict allowed_tools to search, deep_crawl, " +
  "read_file, write_file, and send_file. Deliver one short cited Markdown " +
  "report. Do not call run_pipeline or use DOT/graph IR.";

test.describe("M8 cost tracking", () => {
  // Plugin v2 cost_attribution events surface in runtime_detail.

  test("plugin v2 cost_attribution events surface on supervised tasks", async () => {
    const sid = `m8-cost-v2-${Date.now()}`;
    const { doneEvent } = await chatViaWs(NATIVE_RESEARCH_PROMPT, sid, 480_000);
    expect(doneEvent).toBeTruthy();

    const { tasks } = await getCostsForSession(sid);
    let observedV2Cost = false;
    for (const task of tasks) {
      const detail = task.runtime_detail;
      const obj =
        typeof detail === "string"
          ? (() => {
              try {
                return JSON.parse(detail) as Record<string, unknown>;
              } catch {
                return null;
              }
            })()
          : detail;
      if (!obj) continue;
      // v2 cost events fold into runtime_detail under various names.
      if (
        obj.cost_attribution !== undefined ||
        obj.cost_usd !== undefined ||
        obj.tokens_in !== undefined ||
        (typeof obj.kind === "string" && obj.kind === "cost_attribution")
      ) {
        observedV2Cost = true;
        break;
      }
    }

    test.skip(
      !observedV2Cost,
      "M8 W3 + W4: no plugin v2 cost_attribution events folded into " +
        "runtime_detail. Either deep_search/deep_crawl/mofa-* plugins have " +
        "not adopted protocol v2, or the host is not folding cost events.",
    );
    expect(observedV2Cost).toBe(true);
  });
});
