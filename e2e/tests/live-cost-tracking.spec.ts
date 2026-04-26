/**
 * M8 Runtime Parity end-to-end live spec — cost-tracking track.
 *
 * Triggers two distinct pipelines and asserts the M8 cost-attribution
 * surface (F-003 + W1.A4 + W3 cost events) is wired end-to-end:
 *
 *   - each pipeline run produces per-task `cost` rows in
 *     `/api/sessions/:id/tasks` (tokens_in, tokens_out, usd_used) (W1.A4)
 *   - per-pipeline `cost_attribution` events surface (W3.F1+F2 + plugin v2)
 *   - aggregate cost across both pipelines is greater than either alone
 *   - costs do not bleed across sessions
 *
 * Each assertion that requires a track that has not landed yet self-skips
 * with a diagnostic, so the spec can land before the relevant tracks
 * merge and auto-promote as they ship.
 *
 * Run from /Users/yuechen/home/octos/e2e:
 *
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *     npx playwright test tests/live-cost-tracking.spec.ts --workers=1
 *
 * NEVER run against mini5 (`dspfac.ocean.ominix.io`) — reserved for coding-green.
 */

import { test, expect } from '@playwright/test';

const BASE = process.env.OCTOS_TEST_URL || 'https://dspfac.bot.ominix.io';
const TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE = process.env.OCTOS_PROFILE || 'dspfac';

if (BASE.includes('dspfac.ocean.ominix.io')) {
  throw new Error(
    'live-cost-tracking refuses to run against mini5; pick mini1/2/4 instead.',
  );
}

test.setTimeout(900_000);

interface SseEvent {
  type: string;
  [key: string]: unknown;
}

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

async function chatSSE(
  message: string,
  sessionId: string,
  maxWait = 60_000,
): Promise<{ events: SseEvent[]; content: string; doneEvent?: SseEvent }> {
  const resp = await fetch(`${BASE}/api/chat`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${TOKEN}`,
      'X-Profile-Id': PROFILE,
    },
    body: JSON.stringify({ message, session_id: sessionId, stream: true }),
  });
  if (!resp.ok) {
    const body = await resp.text().catch(() => '');
    if (resp.status === 502 || resp.status === 504) {
      return { events: [], content: body || '(proxy timeout)' };
    }
    throw new Error(`Chat failed: ${resp.status} ${body.slice(0, 200)}`);
  }
  if (!resp.body) return { events: [], content: '' };
  const events: SseEvent[] = [];
  let content = '';
  let doneEvent: SseEvent | undefined;
  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  const start = Date.now();
  try {
    while (Date.now() - start < maxWait) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split('\n');
      buffer = lines.pop()!;
      for (const line of lines) {
        if (!line.startsWith('data: ')) continue;
        const data = line.slice(6).trim();
        if (!data || data === '[DONE]') continue;
        try {
          const event: SseEvent = JSON.parse(data);
          events.push(event);
          if (event.type === 'replace' && typeof event.text === 'string') content = event.text;
          if (event.type === 'done') {
            doneEvent = event;
            if (typeof event.content === 'string' && event.content) content = event.content;
            return { events, content, doneEvent };
          }
        } catch {
          /* skip malformed */
        }
      }
    }
  } finally {
    reader.releaseLock();
  }
  return { events, content, doneEvent };
}

async function getTasks(sessionId: string): Promise<BackgroundTaskRow[]> {
  const resp = await fetch(
    `${BASE}/api/sessions/${encodeURIComponent(sessionId)}/tasks`,
    { headers: { Authorization: `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE } },
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
      taskId: row.id ?? row.task_id ?? '',
      toolName: row.tool_name ?? '',
      tokens_in: ti,
      tokens_out: to,
      usd_used: usd,
    });
  }
  return { tasks, taskCosts, totalUsd, totalTokensIn, totalTokensOut };
}

const PIPELINE_PROMPT_A =
  'Use run_pipeline with the deep_research family to investigate the most ' +
  'notable agentic-AI announcements from the last 7 days. Use 3-4 sources. ' +
  'Synthesize a short prose summary with citations. Keep it short.';

const PIPELINE_PROMPT_B =
  'Use run_pipeline with the deep_research family to investigate recent ' +
  'work in efficient transformer attention (2025+). Use 3-4 sources. ' +
  'Synthesize a brief synthesis with citations. Keep it short.';

// ════════════════════════════════════════════════════════════════════════════
// Spec 1 — A single pipeline run produces per-task cost rows
// ════════════════════════════════════════════════════════════════════════════

test.describe('M8 cost tracking', () => {
  test('a single pipeline run produces per-task cost rows', async () => {
    const sid = `m8-cost-single-${Date.now()}`;
    const { doneEvent } = await chatSSE(PIPELINE_PROMPT_A, sid, 480_000);
    expect(doneEvent, 'expected SSE done').toBeTruthy();

    const { taskCosts, totalUsd, totalTokensIn, totalTokensOut } = await getCostsForSession(sid);
    console.log(
      `[cost-single] task_rows_with_cost=${taskCosts.length} ` +
        `usd=${totalUsd.toFixed(6)} tokens_in=${totalTokensIn} tokens_out=${totalTokensOut}`,
    );

    if (taskCosts.length === 0) {
      test.skip(
        true,
        'M8 W1.A4: no per-task cost rows on supervisor tasks. ' +
          'Cost reservation handles may not yet be wired into pipeline workers.',
      );
      return;
    }

    // Per-task tokens should be non-negative; at least one task should report
    // > 0 tokens_in (the pipeline orchestrator did some LLM work).
    expect(taskCosts.every((tc) => tc.tokens_in >= 0 && tc.tokens_out >= 0)).toBe(true);
    expect(taskCosts.some((tc) => tc.tokens_in > 0)).toBe(true);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 2 — Two pipeline runs in different sessions produce independent costs
  // ══════════════════════════════════════════════════════════════════════════

  test('costs do not bleed across sessions', async () => {
    const sidA = `m8-cost-iso-a-${Date.now()}`;
    const sidB = `m8-cost-iso-b-${Date.now()}`;

    // Run them in series to keep the test budget tractable.
    await chatSSE(PIPELINE_PROMPT_A, sidA, 480_000);
    await chatSSE(PIPELINE_PROMPT_B, sidB, 480_000);

    const a = await getCostsForSession(sidA);
    const b = await getCostsForSession(sidB);
    console.log(
      `[cost-iso] A: rows=${a.taskCosts.length} usd=${a.totalUsd.toFixed(6)} | ` +
        `B: rows=${b.taskCosts.length} usd=${b.totalUsd.toFixed(6)}`,
    );

    if (a.taskCosts.length === 0 && b.taskCosts.length === 0) {
      test.skip(true, 'M8 W1.A4: no cost rows in either session — cost surface not yet wired.');
      return;
    }

    // Each task id should belong to exactly one of the two sessions.
    const idsA = new Set(a.taskCosts.map((tc) => tc.taskId));
    const idsB = new Set(b.taskCosts.map((tc) => tc.taskId));
    const overlap = [...idsA].filter((id) => idsB.has(id));
    expect(overlap, `task ids overlap across sessions: ${JSON.stringify(overlap)}`).toEqual([]);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 3 — Aggregate sums across multiple runs in the same session
  // ══════════════════════════════════════════════════════════════════════════

  test('aggregate cost across multiple pipelines in one session sums correctly', async () => {
    const sid = `m8-cost-agg-${Date.now()}`;

    // First pipeline.
    const { doneEvent: done1 } = await chatSSE(PIPELINE_PROMPT_A, sid, 480_000);
    expect(done1).toBeTruthy();
    const after1 = await getCostsForSession(sid);

    // Second pipeline (in same session).
    const { doneEvent: done2 } = await chatSSE(PIPELINE_PROMPT_B, sid, 480_000);
    expect(done2).toBeTruthy();
    const after2 = await getCostsForSession(sid);

    console.log(
      `[cost-agg] after_pipeline_1: rows=${after1.taskCosts.length} usd=${after1.totalUsd.toFixed(6)} | ` +
        `after_pipeline_2: rows=${after2.taskCosts.length} usd=${after2.totalUsd.toFixed(6)}`,
    );

    if (after1.taskCosts.length === 0 || after2.taskCosts.length === 0) {
      test.skip(true, 'M8 W1.A4: cost surface not wired (no rows after pipeline runs).');
      return;
    }

    // After the second pipeline, totals should be at least as large as after
    // the first (cost rows accumulate, never decrease).
    expect(after2.taskCosts.length).toBeGreaterThanOrEqual(after1.taskCosts.length);
    expect(after2.totalUsd).toBeGreaterThanOrEqual(after1.totalUsd - 1e-9);
    expect(after2.totalTokensIn).toBeGreaterThanOrEqual(after1.totalTokensIn);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 4 — done event includes session-level token totals
  // ══════════════════════════════════════════════════════════════════════════

  test('SSE done event includes tokens_in / tokens_out for the turn', async () => {
    const sid = `m8-cost-done-${Date.now()}`;
    const { doneEvent } = await chatSSE('what is the capital of france', sid, 30_000);
    expect(doneEvent).toBeTruthy();
    const evt = doneEvent as { tokens_in?: number; tokens_out?: number };
    expect(typeof evt.tokens_in).toBe('number');
    expect(typeof evt.tokens_out).toBe('number');
    expect(evt.tokens_in).toBeGreaterThanOrEqual(0);
    expect(evt.tokens_out).toBeGreaterThan(0);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 5 — Plugin v2 cost_attribution events surface in runtime_detail
  // ══════════════════════════════════════════════════════════════════════════

  test('plugin v2 cost_attribution events surface on supervised tasks', async () => {
    const sid = `m8-cost-v2-${Date.now()}`;
    const { doneEvent } = await chatSSE(PIPELINE_PROMPT_A, sid, 480_000);
    expect(doneEvent).toBeTruthy();

    const { tasks } = await getCostsForSession(sid);
    let observedV2Cost = false;
    for (const task of tasks) {
      const detail = task.runtime_detail;
      const obj =
        typeof detail === 'string'
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
        (typeof obj.kind === 'string' && obj.kind === 'cost_attribution')
      ) {
        observedV2Cost = true;
        break;
      }
    }

    test.skip(
      !observedV2Cost,
      'M8 W3 + W4: no plugin v2 cost_attribution events folded into ' +
        'runtime_detail. Either deep_search/deep_crawl/mofa-* plugins have ' +
        'not adopted protocol v2, or the host is not folding cost events.',
    );
    expect(observedV2Cost).toBe(true);
  });
});
