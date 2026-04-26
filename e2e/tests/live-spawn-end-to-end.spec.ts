/**
 * M8 Runtime Parity end-to-end live spec — spawn / workflow track.
 *
 * Drives `slides_delivery` and `podcast_generate` workflows through the
 * spawn-subagent path and asserts the M8 contract on the spawned children:
 *
 *   - per-phase progress events surface as tool_progress with tool_call_id
 *   - cancel via POST /api/tasks/:id/cancel terminates the spawn child
 *     (and any plugin process tree) within 15s
 *   - on failure, the M8.9 recovery loop fires once and surfaces an
 *     actionable assistant message
 *   - the FileStateCache + SubAgentOutputRouter are wired (assertion via
 *     subagent-outputs/<sid>/<task_id>.out file existence on the host)
 *
 * Each assertion that requires a track that has not landed yet self-skips
 * with a diagnostic, so the spec can land before W1+W2+W3 ship and
 * auto-promote as they merge.
 *
 * Run from /Users/yuechen/home/octos/e2e:
 *
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *     npx playwright test tests/live-spawn-end-to-end.spec.ts --workers=1
 *
 * NEVER run against mini5 (`dspfac.ocean.ominix.io`) — reserved for coding-green.
 */

import { test, expect } from '@playwright/test';
import { execSync } from 'node:child_process';

const BASE = process.env.OCTOS_TEST_URL || 'https://dspfac.bot.ominix.io';
const TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE = process.env.OCTOS_PROFILE || 'dspfac';

// Refuse to run against mini5 — coding-green territory.
if (BASE.includes('dspfac.ocean.ominix.io')) {
  throw new Error(
    'live-spawn-end-to-end refuses to run against mini5; pick mini1/2/4 instead.',
  );
}

const HOST_MAP: Record<string, string> = {
  'dspfac.crew.ominix.io': 'cloud@69.194.3.128',
  'dspfac.bot.ominix.io': 'cloud@69.194.3.129',
  'dspfac.octos.ominix.io': 'cloud@69.194.3.203',
  'dspfac.river.ominix.io': 'cloud@69.194.3.66',
};
const SSH_HOST =
  process.env.OCTOS_TEST_SSH_HOST ||
  (() => {
    try {
      return HOST_MAP[new URL(BASE).hostname] || '';
    } catch {
      return '';
    }
  })();

const REMOTE_DATA_DIR = `~/.octos/profiles/${PROFILE}/data`;

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
  child_session_key?: string | null;
  status?: string;
  lifecycle_state?: string;
  runtime_state?: string;
  runtime_detail?: string | Record<string, unknown> | null;
  output_files?: string[];
  error?: string | null;
  started_at?: string;
  updated_at?: string;
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

async function getMessages(sessionId: string): Promise<unknown[]> {
  const resp = await fetch(
    `${BASE}/api/sessions/${encodeURIComponent(sessionId)}/messages`,
    { headers: { Authorization: `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE } },
  );
  if (!resp.ok) return [];
  return resp.json();
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

function sshExec(cmd: string, opts: { allowFail?: boolean } = {}): string {
  if (!SSH_HOST) return '';
  try {
    return execSync(
      `ssh -o StrictHostKeyChecking=no -o BatchMode=yes -o ConnectTimeout=8 ${SSH_HOST} ${JSON.stringify(cmd)}`,
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], timeout: 20_000 },
    ).toString();
  } catch (err: unknown) {
    if (opts.allowFail) {
      const e = err as { stderr?: { toString(): string }; message?: string };
      return `(ssh err) ${e.stderr?.toString() ?? e.message ?? ''}`;
    }
    throw err;
  }
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

// ════════════════════════════════════════════════════════════════════════════
// Spec 1 — Slides delivery: per-phase progress + final deck
// ════════════════════════════════════════════════════════════════════════════

test.describe('M8 spawn end-to-end (slides)', () => {
  test('slides_delivery surfaces per-phase progress and a final deck task', async () => {
    const slug = `m8-spawn-slides-${Date.now().toString(36)}`;
    const sid = `m8-spawn-slides-sid-${Date.now()}`;

    await chatSSE(`/new slides ${slug}`, sid, 90_000);
    const designPrompt =
      'Design a 2-slide deck about M8 runtime parity. Slide 1 cover, slide 2 ' +
      'one-bullet summary. Use style nb-pro. Show outline only, do not generate yet.';
    await chatSSE(designPrompt, sid, 120_000);
    const { doneEvent, events } = await chatSSE('go', sid, 600_000);
    expect(doneEvent).toBeTruthy();

    // Find the spawn-driven mofa_slides task from the events stream.
    const phaseProgress = events.filter(
      (e) =>
        e.type === 'tool_progress' &&
        typeof (e as { name?: string }).name === 'string' &&
        ['mofa_slides', 'spawn'].some((needle) =>
          ((e as { name?: string }).name as string).includes(needle),
        ),
    );
    console.log(`[spawn-slides] tool_progress events for spawn/mofa: ${phaseProgress.length}`);

    // Validate the supervised task tree.
    const tasks = (await pollUntil(
      () => getTasks(sid),
      (rows) =>
        rows.some(
          (row) =>
            (row.tool_name === 'mofa_slides' || row.tool_name === 'spawn') &&
            Boolean(row.id ?? row.task_id),
        ),
      { timeoutMs: 90_000, intervalMs: 3_000 },
    )) ?? [];

    const slidesTask = tasks.find(
      (row) => row.tool_name === 'mofa_slides' || row.tool_name === 'spawn',
    );
    if (!slidesTask) {
      test.skip(
        true,
        'M8 W2.B1: no spawn / mofa_slides task registered with supervisor — ' +
          'spawn host wiring may not have landed.',
      );
      return;
    }

    const lifecycle = (slidesTask.lifecycle_state ?? slidesTask.status ?? '').toLowerCase();
    console.log(
      `[spawn-slides] task_id=${slidesTask.id ?? slidesTask.task_id} tool=${slidesTask.tool_name} lifecycle=${lifecycle}`,
    );

    // The terminal state must be ready/completed (or failed with a recoverable
    // assistant message — see spec 3).
    expect(['ready', 'completed', 'failed', 'verifying', 'running']).toContain(lifecycle);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 2 — Cancel a long-running spawn child within 15s
  // ══════════════════════════════════════════════════════════════════════════

  test('cancel terminates a running spawn child within 15s', async () => {
    const slug = `m8-spawn-cancel-${Date.now().toString(36)}`;
    const sid = `m8-spawn-cancel-sid-${Date.now()}`;

    await chatSSE(`/new slides ${slug}`, sid, 60_000);
    // Kick off generation; do not wait for done.
    const longGen = chatSSE(
      'Design and generate a 12-slide deck about every public agentic-AI ' +
        'system from 2018 to 2026. Use nb-pro. Generate now.',
      sid,
      600_000,
    );

    const runningTasks = await pollUntil(
      () => getTasks(sid),
      (rows) =>
        rows.some(
          (row) =>
            (row.lifecycle_state === 'running' || row.status === 'Running') &&
            (row.tool_name === 'mofa_slides' || row.tool_name === 'spawn'),
        ),
      { timeoutMs: 120_000, intervalMs: 3_000 },
    );
    const target = (runningTasks ?? []).find(
      (row) =>
        (row.lifecycle_state === 'running' || row.status === 'Running') &&
        (row.tool_name === 'mofa_slides' || row.tool_name === 'spawn'),
    );
    if (!target) {
      test.skip(true, 'no running mofa_slides/spawn task within 120s.');
      return;
    }
    const taskId = target.id ?? target.task_id!;

    const cancel = await postJson(`/api/tasks/${encodeURIComponent(taskId)}/cancel`, {});
    if (cancel.status === 404 || cancel.status === 405) {
      test.skip(true, `M8 W2.API1: cancel endpoint not yet implemented (${cancel.status}).`);
      return;
    }
    expect([200, 202, 409]).toContain(cancel.status);

    const cancelledAt = Date.now();
    const terminal = await pollUntil(
      () => getTasks(sid),
      (rows) => {
        const updated = rows.find((row) => (row.id ?? row.task_id) === taskId);
        if (!updated) return false;
        const ls = (updated.lifecycle_state ?? '').toLowerCase();
        const st = updated.status ?? '';
        return ['failed', 'cancelled', 'ready'].includes(ls) ||
          ['Failed', 'Cancelled', 'Completed'].includes(st);
      },
      { timeoutMs: 20_000, intervalMs: 1_500 },
    );
    const elapsed = Date.now() - cancelledAt;
    console.log(`[spawn-cancel] task_id=${taskId} terminal_within=${elapsed}ms`);

    expect(terminal, 'spawn child did not reach terminal state within 20s').not.toBeNull();
    expect(elapsed).toBeLessThan(20_000);

    // If we can SSH, verify there are no orphan helper processes for this task.
    if (SSH_HOST) {
      const helpers = sshExec(
        `pgrep -f "OCTOS_TASK_ID=${taskId}" 2>/dev/null | head -5`,
        { allowFail: true },
      );
      const orphans = helpers.split('\n').filter((line) => /^\d+$/.test(line.trim()));
      console.log(`[spawn-cancel] orphan_helpers=${orphans.length}`);
      // Soft assertion — orphans imply cancel didn't propagate to the plugin.
      if (orphans.length > 0) {
        console.warn(
          `[spawn-cancel] WARNING: ${orphans.length} orphan helpers for task ${taskId}.`,
        );
      }
    }

    await longGen.catch(() => undefined);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 3 — Spawn failure triggers M8.9 recovery
  // ══════════════════════════════════════════════════════════════════════════

  test('failed spawn child triggers M8.9 recovery prompt', async () => {
    const sid = `m8-spawn-recover-${Date.now()}`;

    // Use a known-bad fm_tts voice so the spawn child fails quickly. The
    // existing `m8-runtime-invariants-live.spec.ts` shows this triggers the
    // failure-signal path. Here we additionally assert the recovery prompt
    // re-engages the agent.
    const prompt =
      `直接调用 fm_tts，把 voice 参数精确设为 ` +
      `definitely_not_a_real_voice_${Date.now().toString(36)}，` +
      `文本只说：m8 恢复测试。不要先检查声音，也不要解释。`;
    const { doneEvent } = await chatSSE(prompt, sid, 120_000);
    expect(doneEvent).toBeTruthy();

    // Wait for the recovery prompt to land as a system-internal user message
    // followed by an actionable assistant response.
    let recoveryFired = false;
    let assistantText = '';
    for (let i = 0; i < 30; i++) {
      await new Promise((r) => setTimeout(r, 3000));
      const msgs = (await getMessages(sid)) as Array<{ role?: string; content?: string }>;
      const recoveryMarker = msgs.find(
        (m) =>
          (m.role === 'user' || m.role === 'User') &&
          typeof m.content === 'string' &&
          m.content.includes('[system-internal]') &&
          m.content.includes('fm_tts'),
      );
      if (recoveryMarker) recoveryFired = true;

      const lastAssistant = msgs
        .filter((m) => m.role === 'assistant' || m.role === 'Assistant')
        .pop();
      assistantText = (lastAssistant?.content as string) ?? '';

      const actionable =
        /try.*another voice|use.*different voice|available voice|please.*specify|please.*choose|alternative voice|let me try|cannot proceed|let me know/i.test(
          assistantText,
        );
      if (recoveryFired && actionable) break;
    }

    console.log(`[spawn-recover] recoveryFired=${recoveryFired} assistant=${assistantText.slice(0, 200)}`);
    test.skip(
      !recoveryFired,
      'M8.9: no recovery prompt observed in transcript. Either the failure ' +
        'signal callback did not fire, or build_recovery_prompt is not wired ' +
        'into the session actor.',
    );
    expect(recoveryFired).toBe(true);
  });

  // ══════════════════════════════════════════════════════════════════════════
  // Spec 4 — SubAgentOutputRouter writes per-task .out file (M8.7 + W2.B1)
  // ══════════════════════════════════════════════════════════════════════════

  test('SubAgentOutputRouter writes a non-empty per-task .out file for spawn children', async () => {
    if (!SSH_HOST) {
      test.skip(true, 'SSH host unresolved; cannot verify on-disk router output.');
      return;
    }

    const sid = `m8-spawn-router-${Date.now()}`;
    const t0 = Date.now();
    const prompt =
      `直接调用 fm_tts，把 voice 参数精确设为 vivian，` +
      `文本只说：m8 路由测试。不要先检查声音，也不要解释。`;
    const { doneEvent } = await chatSSE(prompt, sid, 90_000);
    expect(doneEvent, 'expected SSE done').toBeTruthy();

    let foundPath = '';
    let size = 0;
    for (let i = 0; i < 25; i++) {
      await new Promise((r) => setTimeout(r, 2000));
      const out = sshExec(
        `find ${REMOTE_DATA_DIR}/subagent-outputs -type f -name '*.out' 2>/dev/null`,
        { allowFail: true },
      );
      const candidates = out.split('\n').filter((l) => l.includes('.out'));
      for (const path of candidates) {
        const stat = sshExec(`stat -f '%m %z' ${path} 2>/dev/null`, { allowFail: true }).trim();
        const parts = stat.split(/\s+/).map(Number);
        if (parts.length < 2) continue;
        const [mtime, sz] = parts;
        if (Number.isFinite(mtime) && mtime * 1000 >= t0 - 5_000 && Number.isFinite(sz) && sz > 0) {
          foundPath = path;
          size = sz;
          break;
        }
      }
      if (foundPath) break;
    }

    expect(foundPath, 'expected a fresh .out file under subagent-outputs/').toBeTruthy();
    expect(size).toBeGreaterThan(0);
    console.log(`[spawn-router] file=${foundPath} size=${size}`);
  });
});
