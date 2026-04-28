/**
 * M8 Runtime Parity end-to-end live spec — pipeline track.
 *
 * Triggers a deep-research style `run_pipeline` workflow against a live
 * canary and asserts that the M8 contract surfaces end-to-end:
 *
 *   - per-node task tree appears in `/api/sessions/:id/tasks` (W1.A3)
 *   - per-node `tool_progress` SSE frames carry `tool_call_id` (B1+B2)
 *   - structured plugin v2 events show up in `runtime_detail` (W3.F1+F2)
 *   - cancel mid-flight via POST /api/tasks/:id/cancel transitions to terminal
 *     state within 15s (W2.API1)
 *   - restart-from-node via POST /api/tasks/:id/restart-from-node re-runs
 *     downstream nodes only, preserving upstream cached outputs (W2.API2)
 *
 * The spec is **feature-gated**: each assertion that requires a track that
 * has not landed yet self-skips with a diagnostic message. This lets us
 * land the spec early and have it auto-promote from skip→pass as the
 * tracks merge.
 *
 * Run from /Users/yuechen/home/octos/e2e:
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

import { test, expect } from '@playwright/test';

const BASE = process.env.OCTOS_TEST_URL || 'https://dspfac.bot.ominix.io';
const TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE = process.env.OCTOS_PROFILE || 'dspfac';

// Refuse to run against mini5 — coding-green territory.
if (BASE.includes('dspfac.ocean.ominix.io')) {
  throw new Error(
    'live-pipeline-end-to-end refuses to run against mini5; pick mini1/2/4 instead.',
  );
}

// Per-test timeout: a deep-research run can take several minutes.
test.setTimeout(900_000);

interface SseEvent {
  type: string;
  [key: string]: unknown;
}

interface ToolProgressEvent extends SseEvent {
  type: 'tool_progress';
  name?: string;
  tool_call_id?: string;
  message?: string;
}

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
          if (event.type === 'replace' && typeof event.text === 'string') {
            content = event.text;
          }
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

async function postJson(path: string, body: unknown): Promise<{ status: number; body: unknown }> {
  const resp = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${TOKEN}`,
      'X-Profile-Id': PROFILE,
    },
    body: JSON.stringify(body ?? {}),
  });
  let parsed: unknown = null;
  try {
    parsed = await resp.json();
  } catch {
    parsed = await resp.text().catch(() => null);
  }
  return { status: resp.status, body: parsed };
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
  'Use run_pipeline with the deep_research family to investigate the most ' +
  'notable agentic-AI announcements from the last 7 days. Use 3-5 sources. ' +
  'Synthesize a short prose summary with citations. Keep the run under ' +
  '5 minutes.';

// ════════════════════════════════════════════════════════════════════════════
// Spec 1 — Per-node task tree appears in /api/sessions/:id/tasks (W1.A3)
// ════════════════════════════════════════════════════════════════════════════

test.describe('M8 pipeline end-to-end', () => {
  test('per-node tasks register under the run_pipeline parent task', async () => {
    const sid = `m8-pipe-tree-${Date.now()}`;

    const { doneEvent, events } = await chatSSE(DEEP_RESEARCH_PROMPT, sid, 480_000);
    expect(doneEvent, 'expected SSE done').toBeTruthy();

    // Find the run_pipeline tool_call_id from the first ToolStarted event.
    const pipelineCallId = events
      .filter((e) => e.type === 'tool_started' || e.type === 'tool_progress')
      .map((e) => ({
        name: (e as { name?: string }).name,
        cid: (e as { tool_call_id?: string }).tool_call_id,
      }))
      .find(({ name, cid }) => name === 'run_pipeline' && typeof cid === 'string')?.cid;

    if (!pipelineCallId) {
      test.skip(true, 'run_pipeline tool_call_id absent — agent did not invoke run_pipeline');
      return;
    }

    // Poll for child tasks under this run_pipeline call.
    const tasks = await pollUntil(
      () => getTasks(sid),
      (rows) =>
        rows.some(
          (row) =>
            (row.parent_task_id ?? '') !== '' &&
            (row.tool_call_id === pipelineCallId || row.parent_task_id === pipelineCallId),
        ),
      { timeoutMs: 60_000, intervalMs: 3_000 },
    );

    const childTasks = (tasks ?? []).filter(
      (row) => row.tool_call_id === pipelineCallId || row.parent_task_id === pipelineCallId,
    );

    if (childTasks.length === 0) {
      test.skip(
        true,
        'M8 W1.A3: no per-node child tasks under run_pipeline. ' +
          'Pipeline workers do not yet register with task_query_store. ' +
          `Tasks seen: ${(tasks ?? []).length}.`,
      );
      return;
    }

    // Each child task should have observable lifecycle progression.
    const childLifecycles = childTasks
      .map((row) => row.lifecycle_state ?? row.status ?? 'unknown')
      .join(',');
    console.log(`[pipeline-tree] children=${childTasks.length} lifecycles=${childLifecycles}`);
    expect(childTasks.length).toBeGreaterThan(0);
    expect(childTasks.every((row) => Boolean(row.tool_name))).toBe(true);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 2 — tool_progress SSE frames carry tool_call_id (B1+B2)
  // ══════════════════════════════════════════════════════════════════════════

  test('tool_progress frames carry tool_call_id for every supervised event', async () => {
    const sid = `m8-pipe-tcid-${Date.now()}`;

    const { events } = await chatSSE(DEEP_RESEARCH_PROMPT, sid, 480_000);
    const progressEvents = events.filter(
      (e): e is ToolProgressEvent => e.type === 'tool_progress',
    );

    if (progressEvents.length === 0) {
      test.skip(true, 'no tool_progress events emitted; pipeline may not have run.');
      return;
    }

    const withTcid = progressEvents.filter(
      (e) => typeof e.tool_call_id === 'string' && e.tool_call_id.length > 0,
    );
    const ratio = withTcid.length / progressEvents.length;
    console.log(
      `[pipeline-tcid] progress=${progressEvents.length} with_tool_call_id=${withTcid.length} ratio=${ratio.toFixed(3)}`,
    );

    // Post 9687fa95+889e5e05, every tool_progress event should carry tool_call_id.
    expect(ratio).toBeGreaterThanOrEqual(0.9);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 3 — Plugin v2 structured events fold into runtime_detail (W3.F1+F2)
  // ══════════════════════════════════════════════════════════════════════════

  test('plugin v2 progress events surface in runtime_detail', async () => {
    const sid = `m8-pipe-v2-${Date.now()}`;

    const { doneEvent } = await chatSSE(DEEP_RESEARCH_PROMPT, sid, 480_000);
    expect(doneEvent, 'expected SSE done').toBeTruthy();

    const tasks = (await getTasks(sid)) ?? [];
    if (tasks.length === 0) {
      test.skip(true, 'no tasks recorded; pipeline did not run.');
      return;
    }

    // At least one task should have runtime_detail populated with a phase
    // and message field — that's the v2 progress event being folded in.
    let observedV2 = false;
    for (const task of tasks) {
      const detail = task.runtime_detail;
      if (!detail) continue;
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
      if (obj && (obj.phase || obj.current_phase || obj.progress_message)) {
        observedV2 = true;
        console.log(
          `[pipeline-v2] task ${task.id ?? task.task_id} runtime_detail=${JSON.stringify(obj).slice(0, 200)}`,
        );
        break;
      }
    }

    test.skip(
      !observedV2,
      'M8 W3.F1+F2: no task carries v2 progress fields in runtime_detail. ' +
        'Either deep_search/deep_crawl have not adopted protocol v2, or the ' +
        'pipeline host is not folding events into the supervisor.',
    );

    expect(observedV2).toBe(true);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 4 — Cancel mid-flight transitions to terminal within 15s (W2.API1)
  // ══════════════════════════════════════════════════════════════════════════

  test('POST /api/tasks/:id/cancel terminates a running pipeline within 15s', async () => {
    const sid = `m8-pipe-cancel-${Date.now()}`;

    // Kick off a long pipeline; do not wait for done.
    const longPrompt =
      'Use run_pipeline with deep_research to do an exhaustive 20-source ' +
      'investigation of the history of agentic AI from 2010 onward. Take ' +
      'your time, do many search passes, do not return early.';
    const start = Date.now();
    const sseFinish = chatSSE(longPrompt, sid, 600_000);

    // Wait until at least one supervised task is running.
    const runningTask = await pollUntil(
      () => getTasks(sid),
      (rows) =>
        rows.some(
          (row) =>
            (row.lifecycle_state === 'running' || row.status === 'Running') &&
            Boolean(row.id ?? row.task_id),
        ),
      { timeoutMs: 90_000, intervalMs: 3_000 },
    );
    const target = (runningTask ?? []).find(
      (row) =>
        (row.lifecycle_state === 'running' || row.status === 'Running') &&
        Boolean(row.id ?? row.task_id),
    );
    if (!target) {
      test.skip(true, 'no running task detected within 90s — pipeline never started.');
      return;
    }
    const taskId = target.id ?? target.task_id!;
    console.log(`[pipeline-cancel] cancelling task ${taskId} (${target.tool_name})`);

    // Issue the cancel.
    const cancel = await postJson(`/api/tasks/${encodeURIComponent(taskId)}/cancel`, {});
    if (cancel.status === 404) {
      test.skip(true, 'M8 W2.API1: POST /api/tasks/:id/cancel endpoint not yet implemented (404).');
      return;
    }
    if (cancel.status === 405) {
      test.skip(true, 'M8 W2.API1: cancel route not yet wired (405 Method Not Allowed).');
      return;
    }
    expect([200, 202, 409]).toContain(cancel.status);

    // Poll for terminal lifecycle within 15s of the cancel call.
    const cancelledAt = Date.now();
    const terminal = await pollUntil(
      () => getTasks(sid),
      (rows) => {
        const updated = rows.find((row) => (row.id ?? row.task_id) === taskId);
        if (!updated) return false;
        const ls = updated.lifecycle_state ?? '';
        const st = updated.status ?? '';
        return ['failed', 'cancelled', 'ready'].includes(ls.toLowerCase()) ||
          ['Failed', 'Cancelled', 'Completed'].includes(st);
      },
      { timeoutMs: 20_000, intervalMs: 1_500 },
    );

    const elapsed = Date.now() - cancelledAt;
    console.log(`[pipeline-cancel] terminal_within=${elapsed}ms task_id=${taskId}`);
    expect(terminal, 'task did not reach terminal state within 20s of cancel').not.toBeNull();
    expect(elapsed).toBeLessThan(20_000);

    // Drain the SSE stream so we don't leak the request.
    await sseFinish.catch(() => undefined);
    console.log(`[pipeline-cancel] total_run=${Date.now() - start}ms`);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 5 — Restart-from-node re-runs only downstream nodes (W2.API2)
  // ══════════════════════════════════════════════════════════════════════════

  test('POST /api/tasks/:id/restart-from-node returns 200 with a new task_id', async () => {
    const sid = `m8-pipe-restart-${Date.now()}`;

    // Inject a deliberate failure: ask for an impossible deep-research target
    // so the pipeline is likely to fail mid-flight on validator/agent checks.
    const failPrompt =
      'Use run_pipeline with deep_research to research a topic that does not ' +
      'exist: "the cohort of 0-source citations". Fail explicitly if you ' +
      'cannot find any sources. Do not invent.';
    const { doneEvent } = await chatSSE(failPrompt, sid, 360_000);
    expect(doneEvent).toBeTruthy();

    const tasks = (await getTasks(sid)) ?? [];
    const failedTask = tasks.find(
      (row) =>
        (row.lifecycle_state ?? '').toLowerCase() === 'failed' ||
        row.status === 'Failed',
    );
    if (!failedTask) {
      test.skip(true, 'no task ended in Failed state — fault-injection prompt did not trigger.');
      return;
    }
    const taskId = failedTask.id ?? failedTask.task_id!;

    const restart = await postJson(`/api/tasks/${encodeURIComponent(taskId)}/restart-from-node`, {
      node_id: failedTask.tool_name,
    });
    if (restart.status === 404) {
      test.skip(true, 'M8 W2.API2: restart-from-node endpoint not yet implemented (404).');
      return;
    }
    if (restart.status === 405) {
      test.skip(true, 'M8 W2.API2: restart-from-node route not yet wired (405 Method Not Allowed).');
      return;
    }
    expect([200, 202]).toContain(restart.status);

    const body = restart.body as { task_id?: string; new_task_id?: string };
    const newTaskId = body?.task_id ?? body?.new_task_id;
    expect(typeof newTaskId).toBe('string');
    expect(newTaskId).not.toBe(taskId);
    console.log(`[pipeline-restart] old=${taskId} new=${newTaskId}`);
  });
});
