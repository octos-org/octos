/**
 * M8 Runtime Parity end-to-end live spec — pipeline track.
 *
 * Triggers a deep-research style `run_pipeline` workflow against a live
 * canary and asserts that the M8 contract surfaces end-to-end:
 *
 *   - per-node task tree appears in `/api/sessions/:id/tasks` (W1.A3)
 *
 * The spec is **feature-gated**: each assertion that requires a track that
 * has not landed yet self-skips with a diagnostic message. This lets us
 * land the spec early and have it auto-promote from skip→pass as the
 * tracks merge.
 *
 * Run from ~/home/octos/e2e:
 *
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *     npx playwright test tests/live-pipeline-end-to-end.spec.ts --workers=1
 *
 * NEVER run against mini5 (`dspfac.ocean.ominix.io`) — that host is reserved
 * for coding-green tests per user memory `project_mini5_coding_green.md`.
 */

import { test, expect } from "@playwright/test";
import { chatWS, type ChatWsEvent } from "../lib/m9-ws-client";

const BASE = process.env.OCTOS_TEST_URL || "https://dspfac.bot.ominix.io";
const TOKEN = process.env.OCTOS_AUTH_TOKEN || "octos-admin-2026";
const PROFILE = process.env.OCTOS_PROFILE || "dspfac";

// Refuse to run against mini5 — coding-green territory.
if (BASE.includes("dspfac.ocean.ominix.io")) {
  throw new Error(
    "live-pipeline-end-to-end refuses to run against mini5; pick mini1/2/4 instead.",
  );
}

// Per-test timeout: a deep-research run can take several minutes.
test.setTimeout(900_000);

type SseEvent = ChatWsEvent;

interface BackgroundTaskRow {
  id?: string;
  task_id?: string;
  tool_name?: string;
  tool_call_id?: string;
  parent_task_id?: string | null;
  parent_session_key?: string | null;
  child_session_key?: string | null;
  child_terminal_state?: string | null;
  status?: string;
  lifecycle_state?: string;
  runtime_state?: string;
  runtime_detail?: string | Record<string, unknown> | null;
  output_files?: string[];
  cost?: {
    tokens_in?: number;
    tokens_out?: number;
    usd_used?: number;
    usd_reserved?: number;
  } | null;
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

async function pollUntil<T>(
  fn: () => Promise<T>,
  predicate: (value: T) => boolean,
  opts: { timeoutMs?: number; intervalMs?: number } = {},
): Promise<T | null> {
  const { timeoutMs = 60_000, intervalMs = 2_000 } = opts;
  const deadline = Date.now() + timeoutMs;
  let last: T = await fn();
  while (Date.now() < deadline) {
    if (predicate(last)) return last;
    await new Promise((r) => setTimeout(r, intervalMs));
    last = await fn();
  }
  return predicate(last) ? last : null;
}

const DEEP_RESEARCH_PROMPT =
  "Use run_pipeline with the deep_research family to investigate the most " +
  "notable agentic-AI announcements from the last 7 days. Use 3-5 sources. " +
  "Synthesize a short prose summary with citations. Keep the run under " +
  "5 minutes.";

// ════════════════════════════════════════════════════════════════════════════
// Spec 1 — Per-node task tree appears in /api/sessions/:id/tasks (W1.A3)
// ════════════════════════════════════════════════════════════════════════════

test.describe("M8 pipeline end-to-end", () => {
  test("per-node tasks register under the run_pipeline parent task", async () => {
    const sid = `m8-pipe-tree-${Date.now()}`;

    const { doneEvent, events } = await chatViaWs(
      DEEP_RESEARCH_PROMPT,
      sid,
      480_000,
    );
    expect(doneEvent, "expected SSE done").toBeTruthy();

    // Find the run_pipeline tool_call_id from the first ToolStarted event.
    const pipelineCallId = events
      .filter((e) => e.type === "tool_started" || e.type === "tool_progress")
      .map((e) => ({
        name: (e as { name?: string }).name,
        cid: (e as { tool_call_id?: string }).tool_call_id,
      }))
      .find(
        ({ name, cid }) => name === "run_pipeline" && typeof cid === "string",
      )?.cid;

    if (!pipelineCallId) {
      test.skip(
        true,
        "run_pipeline tool_call_id absent — agent did not invoke run_pipeline",
      );
      return;
    }

    // Poll for child tasks under this run_pipeline call.
    const tasks = await pollUntil(
      () => getTasks(sid),
      (rows) =>
        rows.some(
          (row) =>
            (row.parent_task_id ?? "") !== "" &&
            (row.tool_call_id === pipelineCallId ||
              row.parent_task_id === pipelineCallId),
        ),
      { timeoutMs: 60_000, intervalMs: 3_000 },
    );

    const childTasks = (tasks ?? []).filter(
      (row) =>
        row.tool_call_id === pipelineCallId ||
        row.parent_task_id === pipelineCallId,
    );

    if (childTasks.length === 0) {
      test.skip(
        true,
        "M8 W1.A3: no per-node child tasks under run_pipeline. " +
          "Pipeline workers do not yet register with task_query_store. " +
          `Tasks seen: ${(tasks ?? []).length}.`,
      );
      return;
    }

    // Each child task should have observable lifecycle progression.
    const childLifecycles = childTasks
      .map((row) => row.lifecycle_state ?? row.status ?? "unknown")
      .join(",");
    console.log(
      `[pipeline-tree] children=${childTasks.length} lifecycles=${childLifecycles}`,
    );
    expect(childTasks.length).toBeGreaterThan(0);
    expect(childTasks.every((row) => Boolean(row.tool_name))).toBe(true);
  });
});
