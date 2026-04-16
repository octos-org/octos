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

const BASE = process.env.OCTOS_TEST_URL || 'https://dspfac.crew.ominix.io';
const TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE = process.env.OCTOS_PROFILE || 'dspfac';

test.setTimeout(240_000);

interface SseEvent {
  type: string;
  [key: string]: unknown;
}

async function chatSSE(
  message: string,
  sessionId: string,
  opts: { topic?: string; maxWait?: number } = {},
): Promise<{ events: SseEvent[]; content: string; doneEvent?: SseEvent }> {
  const { topic, maxWait = 90_000 } = opts;
  const resp = await fetch(`${BASE}/api/chat`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${TOKEN}`,
      'X-Profile-Id': PROFILE,
    },
    body: JSON.stringify({
      message,
      session_id: sessionId,
      stream: true,
      ...(topic ? { topic } : {}),
    }),
  });

  if (!resp.ok) {
    const body = await resp.text().catch(() => '');
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
      buffer = lines.pop() || '';
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
            if (typeof event.content === 'string' && event.content) {
              content = event.content;
            }
            return { events, content, doneEvent };
          }
        } catch {
          // Ignore non-JSON SSE lines.
        }
      }
    }
  } finally {
    reader.releaseLock();
  }

  return { events, content, doneEvent };
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
  test('same session id can host separate topic-scoped histories', async () => {
    const sid = `cap-topic-${Date.now()}`;
    const baseMarker = `BASE-${Date.now()}`;
    const topicMarker = `TOPIC-${Date.now()}`;
    const topic = `slides capability-${Date.now().toString(36)}`;

    await chatSSE(`Reply with exactly: ${baseMarker}`, sid);
    await chatSSE(`Reply with exactly: ${topicMarker}`, sid, { topic });

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

    const initial = await chatSSE(prompt, sid, {
      maxWait: 90_000,
    });
    expect(initial.doneEvent).toBeTruthy();
    expect(initial.doneEvent?.has_bg_tasks).toBe(true);

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

    const initial = await chatSSE(prompt, sid, { maxWait: 90_000 });
    expect(initial.doneEvent).toBeTruthy();
    expect(initial.doneEvent?.has_bg_tasks).toBe(true);

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
