/**
 * Capability proofs for the refactored runtime.
 *
 * These are not generic regressions. Each case targets behavior that the old
 * runtime handled poorly or could not keep correct:
 * - same session id split into multiple topic-scoped histories
 * - structured child-session contracts for spawn-backed workflow tasks
 * - typed workflow metadata exposed through the normalized task API
 *
 * Run against a live deployment:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   npx playwright test tests/refactor-capabilities.spec.ts
 */
import { expect, test } from '@playwright/test';
import { chatWS, type ChatWsEvent } from '../lib/m9-ws-client';

const BASE = process.env.OCTOS_TEST_URL || 'https://dspfac.crew.ominix.io';
const TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE = process.env.OCTOS_PROFILE || 'dspfac';

test.setTimeout(240_000);

type SseEvent = ChatWsEvent;

/**
 * M9-α-7 (#836): chat helper drives the M9 WebSocket UI Protocol —
 * `/api/ui-protocol/ws`. The legacy `POST /api/chat` route was retired
 * as a follow-up to PR #908.
 *
 * NOTE: The legacy SSE helper accepted an optional `topic` field on the
 * request body — used by `topic-scoped histories` to partition a single
 * session_id into isolated conversation threads. The M9 `turn/start`
 * request does NOT yet have a `topic` parameter (deferred follow-up).
 * Tests that pass `topic` are marked `test.fixme` until the WS protocol
 * grows the field.
 */
async function chatViaWs(
  message: string,
  sessionId: string,
  opts: { topic?: string; maxWait?: number } = {},
): Promise<{ events: SseEvent[]; content: string; doneEvent?: SseEvent }> {
  if (opts.topic !== undefined) {
    throw new Error(
      'chatViaWs: `topic` is not yet supported on the M9 WebSocket; mark the test as test.fixme',
    );
  }
  return chatWS({
    baseUrl: BASE,
    token: TOKEN,
    profileId: PROFILE,
    message,
    sessionId,
    maxWait: opts.maxWait ?? 90_000,
  });
}

async function getMessages(sessionId: string, topic?: string): Promise<any[]> {
  const suffix = topic
    ? `?topic=${encodeURIComponent(topic)}`
    : '';
  const resp = await fetch(`${BASE}/api/sessions/${sessionId}/messages${suffix}`, {
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'X-Profile-Id': PROFILE,
    },
  });
  if (!resp.ok) return [];
  return resp.json();
}

async function getTasks(sessionId: string, topic?: string): Promise<any[]> {
  const suffix = topic
    ? `?topic=${encodeURIComponent(topic)}`
    : '';
  const resp = await fetch(`${BASE}/api/sessions/${sessionId}/tasks${suffix}`, {
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'X-Profile-Id': PROFILE,
    },
  });
  if (!resp.ok) return [];
  return resp.json();
}

function taskWorkflowKind(task: any): string | undefined {
  return task?.workflow_kind ?? task?.runtime_detail?.workflow_kind;
}

function taskCurrentPhase(task: any): string | undefined {
  return task?.current_phase ?? task?.runtime_detail?.current_phase;
}

test.describe('Refactor capability proofs', () => {
  // Deferred follow-up: the legacy SSE chat body carried an optional
  // `topic` parameter that has not yet been mirrored on the M9
  // `turn/start` request — un-fixme once the WS protocol gains a
  // `topic` field on `TurnStartParams`.
  test.fixme('same session id can host separate topic-scoped histories', async () => {
    const sid = `cap-topic-${Date.now()}`;
    const baseMarker = `BASE-${Date.now()}`;
    const topicMarker = `TOPIC-${Date.now()}`;
    const topic = `slides capability-${Date.now().toString(36)}`;

    await chatViaWs(`Reply with exactly: ${baseMarker}`, sid);
    await chatViaWs(`Reply with exactly: ${topicMarker}`, sid, { topic });

    const baseMsgs = await getMessages(sid);
    const topicMsgs = await getMessages(sid, topic);

    const baseText = baseMsgs
      .map((m: any) => m.content || '')
      .join('\n');
    const topicText = topicMsgs
      .map((m: any) => m.content || '')
      .join('\n');

    const baseUsers = baseMsgs.filter((m: any) => m.role === 'user');
    const topicUsers = topicMsgs.filter((m: any) => m.role === 'user');

    expect(baseUsers).toHaveLength(1);
    expect(topicUsers).toHaveLength(1);

    expect(baseText).toContain(baseMarker);
    expect(baseText).not.toContain(topicMarker);

    expect(topicText).toContain(topicMarker);
    expect(topicText).not.toContain(baseMarker);
  });

  test('spawn-backed workflow task exposes structured child-session contract fields', async () => {
    const sid = `cap-child-${Date.now()}`;
    const prompt =
      '不要搜索，直接生成一个简短测试播客并把音频发回会话。脚本： [杨幂 - clone:yangmi, professional] 这里验证子会话契约。 [窦文涛 - clone:douwentao, professional] 任务应该暴露结构化终态。 [杨幂 - clone:yangmi, professional] 只需简短音频。';

    const initial = await chatViaWs(prompt, sid, {
      maxWait: 90_000,
    });
    expect(initial.doneEvent).toBeTruthy();
    // Deferred (γ-3): WS `turn/completed` does not yet carry `has_bg_tasks`.
    // The downstream task-API poll is the authoritative invariant.
    // expect(initial.doneEvent?.has_bg_tasks).toBe(true);

    let task: any | undefined;
    const start = Date.now();
    while (Date.now() - start < 120_000) {
      const tasks = await getTasks(sid);
      task =
        tasks.find((entry: any) => taskWorkflowKind(entry) === 'research_podcast') ||
        tasks[0];
      if (
        task &&
        task.child_session_key &&
        task.child_terminal_state &&
        task.child_join_state
      ) {
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 3_000));
    }

    expect(task).toBeTruthy();
    expect(typeof task.child_session_key).toBe('string');
    expect(task.child_session_key.length).toBeGreaterThan(0);
    expect(['completed', 'retryable_failure', 'terminal_failure']).toContain(
      task.child_terminal_state,
    );
    expect(['joined', 'orphaned']).toContain(task.child_join_state);
  });

  test('podcast workflow task exposes typed runtime workflow metadata', async () => {
    const sid = `cap-workflow-${Date.now()}`;
    const prompt =
      '不要搜索，直接生成一个简短测试播客并把音频发回会话。脚本： [杨幂 - clone:yangmi, professional] 大家好。 [窦文涛 - clone:douwentao, professional] 这里是能力验证。 [杨幂 - clone:yangmi, professional] 我们只检查工作流元数据。 [窦文涛 - clone:douwentao, professional] 感谢收听。';

    const initial = await chatViaWs(prompt, sid, { maxWait: 90_000 });
    expect(initial.doneEvent).toBeTruthy();
    // Deferred (γ-3): WS `turn/completed` does not yet carry `has_bg_tasks`.
    // The downstream task-API poll is the authoritative invariant.
    // expect(initial.doneEvent?.has_bg_tasks).toBe(true);

    const seenPhases = new Set<string>();
    let workflowTask: any | undefined;
    const start = Date.now();
    while (Date.now() - start < 120_000) {
      const tasks = await getTasks(sid);
      workflowTask = tasks.find(
        (entry: any) => taskWorkflowKind(entry) === 'research_podcast',
      );
      if (taskCurrentPhase(workflowTask)) {
        seenPhases.add(taskCurrentPhase(workflowTask)!);
      }
      if (
        workflowTask &&
        taskWorkflowKind(workflowTask) === 'research_podcast' &&
        workflowTask.child_terminal_state
      ) {
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 3_000));
    }

    expect(workflowTask).toBeTruthy();
    expect(taskWorkflowKind(workflowTask)).toBe('research_podcast');
    expect(typeof taskCurrentPhase(workflowTask)).toBe('string');
    expect(taskCurrentPhase(workflowTask)!.length).toBeGreaterThan(0);
    expect(seenPhases.size).toBeGreaterThan(0);
  });
});
