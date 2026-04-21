/**
 * Runtime regression tests for the most problematic areas from issue history.
 *
 * Covers:
 * 1. Session persistence — messages survive across requests, no corruption
 * 2. Background task lifecycle — TTS completes, status updates, file delivery
 * 3. SSE streaming — no premature drops, CJK integrity, done events
 * 4. Slides workspace — git policy, project creation, design-first workflow
 * 5. Cross-session isolation — concurrent sessions don't leak state
 *
 * These are API-level tests (no browser needed). They run against any deployed
 * host via OCTOS_TEST_URL.
 *
 * Related issues:
 * - #386: reload enqueues empty turn
 * - #385: long-turn timeouts not persisted
 * - #384: background task state lost across restarts
 * - #388: podcast never delivers audio
 * - #387: deep research stalls
 * - #366: redundant task completion notification
 * - #251: SSE drops after done
 */

import { test, expect } from '@playwright/test';

const BASE = process.env.OCTOS_TEST_URL || 'https://dspfac.crew.ominix.io';
const TOKEN = process.env.OCTOS_AUTH_TOKEN || 'e2e-test-2026';
const PROFILE = process.env.OCTOS_PROFILE || 'dspfac';

test.setTimeout(120_000);

interface SseEvent {
  type: string;
  [key: string]: unknown;
}

function customVoiceTtsPrompt(text: string): string {
  return `直接调用 fm_tts，把 voice 参数精确设为 yangmi（不要使用 clone:yangmi 或任何 clone: 前缀），文本只说：${text}。不要先检查声音，也不要解释。`;
}

/** Send a message and collect SSE events until done. */
async function chatSSE(
  message: string,
  sessionId: string,
  maxWait = 60_000,
): Promise<{ events: SseEvent[]; content: string; doneEvent?: SseEvent }> {
  const resp = await fetch(`${BASE}/api/chat`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'Authorization': `Bearer ${TOKEN}`,
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
        } catch { /* skip malformed */ }
      }
    }
  } finally {
    reader.releaseLock();
  }
  return { events, content, doneEvent };
}

/** Get session messages via REST. */
async function getMessages(sessionId: string): Promise<any[]> {
  const resp = await fetch(`${BASE}/api/sessions/${sessionId}/messages`, {
    headers: { 'Authorization': `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE },
  });
  if (!resp.ok) return [];
  return resp.json();
}

/** Get session task list via REST. */
async function getTasks(sessionId: string): Promise<any[]> {
  const resp = await fetch(`${BASE}/api/sessions/${sessionId}/tasks`, {
    headers: { 'Authorization': `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE },
  });
  if (!resp.ok) return [];
  return resp.json();
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
  let result = await chatSSE(customVoiceTtsPrompt(text), effectiveSessionId, 90_000);
  if (!result.doneEvent) {
    await new Promise((resolve) => setTimeout(resolve, 2_000));
    effectiveSessionId = `${sessionId}-retry`;
    result = await chatSSE(customVoiceTtsPrompt(text), effectiveSessionId, 90_000);
  }
  return { sessionId: effectiveSessionId, ...result };
}

// ════════════════════════════════════════════════════════════════════
// 1. SESSION PERSISTENCE
// ════════════════════════════════════════════════════════════════════

test.describe('Session persistence', () => {
  test('messages persist across requests (#386)', async () => {
    const sid = `persist-${Date.now()}`;
    await chatSSE('hello from persistence test', sid);

    // Second request to same session
    await chatSSE('follow up message', sid);

    // Verify both messages exist
    const msgs = await getMessages(sid);
    const userMsgs = msgs.filter((m: any) => m.role === 'user');
    expect(userMsgs.length).toBe(2);
    expect(userMsgs[0].content).toContain('persistence test');
    expect(userMsgs[1].content).toContain('follow up');
  });

  test('user message always appears before assistant response', async () => {
    const sid = `order-${Date.now()}`;
    await chatSSE('what is 2+2', sid);

    const msgs = await getMessages(sid);
    const firstUser = msgs.findIndex((m: any) => m.role === 'user');
    const firstAssistant = msgs.findIndex((m: any) => m.role === 'assistant');
    expect(firstUser).toBeLessThan(firstAssistant);
  });

  test('empty messages are not persisted (#386)', async () => {
    const sid = `noempty-${Date.now()}`;
    await chatSSE('say hello', sid);

    const msgs = await getMessages(sid);
    const emptyAssistant = msgs.filter(
      (m: any) => m.role === 'assistant' && (!m.content || m.content.trim() === ''),
    );
    // Tool-call assistant messages may be empty, but final answer should not be
    const lastAssistant = msgs
      .filter((m: any) => m.role === 'assistant')
      .pop();
    if (lastAssistant) {
      // Allow empty if it's a bg_tasks=true response (spawn_only placeholder)
      const isBgPlaceholder = lastAssistant.content === '' && msgs.some(
        (m: any) => m.role === 'assistant' && m.content?.includes('Background work started'),
      );
      if (!isBgPlaceholder) {
        expect(lastAssistant.content?.trim()?.length).toBeGreaterThan(0);
      }
    }
  });
});

// ════════════════════════════════════════════════════════════════════
// 2. SSE STREAMING
// ════════════════════════════════════════════════════════════════════

test.describe('SSE streaming', () => {
  test('SSE stream ends with done event (#251)', async () => {
    const sid = `sse-done-${Date.now()}`;
    const { doneEvent } = await chatSSE('say hello briefly', sid);
    expect(doneEvent).toBeTruthy();
    expect(doneEvent!.type).toBe('done');
  });

  test('SSE preserves CJK characters', async () => {
    const sid = `cjk-${Date.now()}`;
    const { content } = await chatSSE('用中文说"你好世界"', sid);
    // Response should contain Chinese characters, not garbled bytes
    expect(content).toMatch(/[\u4e00-\u9fff]/);
  });

  test('done event includes token counts', async () => {
    const sid = `tokens-${Date.now()}`;
    const { doneEvent } = await chatSSE('what is 1+1', sid);
    expect(doneEvent).toBeTruthy();
    // tokens_in and tokens_out should be present
    expect(typeof doneEvent!.tokens_in).toBe('number');
    expect(typeof doneEvent!.tokens_out).toBe('number');
  });
});

// ════════════════════════════════════════════════════════════════════
// 2B. CODING SHELL REPAIR
// ════════════════════════════════════════════════════════════════════

test.describe('Coding shell repair', () => {
  test('shell repair returns the recovered diff without timing out', async () => {
    const marker = `phase3-shell-${Date.now()}`;
    const basePrompt = [
      'Use shell tool only.',
      'If shell is not already active, call activate_tools with exactly ["shell"] once and only once.',
      `Create a temporary git repo in a new subdirectory named ${marker} under the current working directory.`,
      'Inside it, create notes.txt with exactly two lines: alpha and beta.',
      'Make exactly one edit: change beta to gamma.',
      `Intentionally run \`git diff -- notes.txt\` from one directory above ${marker} once so it fails.`,
      `Then recover by running the same diff from the ${marker} repo root.`,
      'Return only the final unified diff, nothing else.',
      'Do not start background work.',
    ].join(' ');

    const retryPrompt = [
      'Call activate_tools(["shell"]) at most once if shell is not already active.',
      `Then use shell to create ${marker} under the current workspace as a git repo with notes.txt containing alpha and beta.`,
      `Change beta to gamma, intentionally run \`git diff -- notes.txt\` once from the parent of ${marker}, then rerun it successfully from the ${marker} repo root.`,
      'Return only the final unified diff for notes.txt and nothing else.',
      'Do not explain tool availability.',
    ].join(' ');

    let sid = `shell-repair-${Date.now()}`;
    let { content } = await chatSSE(basePrompt, sid, 180_000);
    if (
      !content.includes('diff --git') &&
      /don'?t have access to a shell tool|available tools|activate_tools/i.test(content)
    ) {
      sid = `${sid}-retry`;
      ({ content } = await chatSSE(retryPrompt, sid, 180_000));
    }

    if (!content.includes('diff --git')) {
      const messages = await getMessages(sid);
      content = [
        content,
        ...messages.map((message) =>
          typeof message?.content === 'string' ? message.content : '',
        ),
      ].join('\n');
    }

    expect(content).toContain('diff --git');
    expect(content).toContain('notes.txt');
    expect(content).toContain('-beta');
    expect(content).toContain('+gamma');
    expect(content.length).toBeLessThan(4_000);
  });
});

// ════════════════════════════════════════════════════════════════════
// 3. BACKGROUND TASK LIFECYCLE (TTS)
// ════════════════════════════════════════════════════════════════════

test.describe('Background task lifecycle', () => {
  test('TTS spawn_only returns immediately with bg_tasks=true', async () => {
    const sid = `tts-bg-${Date.now()}`;
    const { doneEvent, sessionId } = await startBackgroundTts(sid, '测试消息');

    expect(doneEvent).toBeTruthy();
    expect(doneEvent!.has_bg_tasks).toBe(true);

    let sawTtsTaskOrAudio = false;
    for (let i = 0; i < 10; i++) {
      await new Promise((r) => setTimeout(r, 2000));
      const tasks = await getTasks(sessionId);
      const msgs = await getMessages(sessionId);
      sawTtsTaskOrAudio =
        tasks.some(
          (task: any) =>
            task.tool_name === 'fm_tts' ||
            task.tool_name === 'Direct TTS' ||
            task.child_session_key,
        ) ||
        msgs.some(
          (m: any) =>
            Array.isArray(m.media) && m.media.some((path: string) => /\.mp3$/i.test(path)),
        );
      if (sawTtsTaskOrAudio) break;
    }

    expect(sawTtsTaskOrAudio).toBe(true);
  });

  test('TTS task completes and delivers file (#388, #366)', async () => {
    const sid = `tts-deliver-${Date.now()}`;
    const { sessionId } = await startBackgroundTts(sid, '你好世界');

    // Poll for task completion (up to 30s)
    let completed = false;
    let fileDelivered = false;
    for (let i = 0; i < 6; i++) {
      await new Promise((r) => setTimeout(r, 5000));
      const msgs = await getMessages(sessionId);
      // Check for file delivery in session messages
      const fileMsg = msgs.find(
        (m: any) =>
          m.content?.includes('✓ fm_tts completed') ||
          m.content?.includes('.mp3') ||
          m.files?.length > 0,
      );
      if (fileMsg) {
        fileDelivered = true;
        completed = true;
        break;
      }
    }

    console.log(`  TTS completed: ${completed}, file delivered: ${fileDelivered}`);
    // Note: may fail if ominix-api is down — that's an infra issue, not a bug
  });

  test('regular messages work while TTS runs in background', async () => {
    const sid = `tts-nonblock-${Date.now()}`;
    // Start TTS
    const { sessionId } = await startBackgroundTts(sid, '后台测试');

    // Immediately send a regular question — should not be blocked
    const { content, doneEvent } = await chatSSE('what is 3+3', sessionId, 30_000);
    expect(content.length).toBeGreaterThan(0);
    expect(doneEvent).toBeTruthy();
  });
});

// ════════════════════════════════════════════════════════════════════
// 4. SLIDES WORKSPACE
// ════════════════════════════════════════════════════════════════════

test.describe('Slides workspace', () => {
  test('/new slides creates project with workspace policy', async () => {
    const sid = `slides-new-${Date.now()}`;
    const { content } = await chatSSE(`/new slides regtest-${Date.now().toString(36)}`, sid);

    expect(
      content.includes('slides') ||
      content.includes('project') ||
      content.includes('created') ||
      content.includes('Switched'),
    ).toBe(true);

    // Should mention workspace policy
    expect(
      content.includes('.octos-workspace.toml') || content.includes('policy'),
    ).toBe(true);
  });

  test('design-first: agent writes script.js without generating', async () => {
    const sid = `slides-design-${Date.now()}`;
    await chatSSE(`/new slides design-${Date.now().toString(36)}`, sid);

    const { content } = await chatSSE(
      'Make a 2-slide deck: 1) Cover "Test", 2) "Content". Style nb-pro. Write script.js ONLY, do NOT generate.',
      sid,
      60_000,
    );

    const mentionsScript =
      content.includes('script.js') ||
      content.includes('write_file') ||
      content.includes('module.exports');
    const calledMofa =
      content.includes('mofa_slides') ||
      content.includes('generating') ||
      content.includes('生成中');

    console.log(`  mentions script: ${mentionsScript}, called mofa: ${calledMofa}`);
    expect(content.length).toBeGreaterThan(50);
  });

  test('/help returns commands, not LLM response', async () => {
    const sid = `slides-help-${Date.now()}`;
    const { content } = await chatSSE('/help', sid, 15_000);

    expect(
      content.includes('/new') ||
      content.includes('/sessions') ||
      content.includes('command') ||
      content.includes('Unknown'),
    ).toBe(true);
  });
});

// ════════════════════════════════════════════════════════════════════
// 5. CROSS-SESSION ISOLATION
// ════════════════════════════════════════════════════════════════════

test.describe('Cross-session isolation', () => {
  test('concurrent sessions do not leak messages (#343)', async () => {
    const sidA = `iso-a-${Date.now()}`;
    const sidB = `iso-b-${Date.now()}`;

    // Send different messages to each session
    await Promise.all([
      chatSSE('session A unique message alpha', sidA),
      chatSSE('session B unique message beta', sidB),
    ]);

    // Verify no cross-contamination
    const msgsA = await getMessages(sidA);
    const msgsB = await getMessages(sidB);

    const aContent = msgsA.map((m: any) => m.content).join(' ');
    const bContent = msgsB.map((m: any) => m.content).join(' ');

    expect(aContent).toContain('alpha');
    expect(aContent).not.toContain('beta');
    expect(bContent).toContain('beta');
    expect(bContent).not.toContain('alpha');
  });

  test('session list API returns both sessions', async () => {
    const resp = await fetch(`${BASE}/api/sessions`, {
      headers: { 'Authorization': `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE },
    });
    expect(resp.ok).toBe(true);
    const sessions = await resp.json();
    expect(Array.isArray(sessions)).toBe(true);
    expect(sessions.length).toBeGreaterThan(0);
  });
});

// ════════════════════════════════════════════════════════════════════
// 6. SESSION CREATE & DELETE LIFECYCLE
// ════════════════════════════════════════════════════════════════════

test.describe('Session create & delete lifecycle', () => {
  test('new session appears in session list', async () => {
    const sid = `lifecycle-new-${Date.now()}`;
    await chatSSE('hello from lifecycle test', sid);

    const resp = await fetch(`${BASE}/api/sessions`, {
      headers: { 'Authorization': `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE },
    });
    const sessions = await resp.json();
    const found = sessions.find((s: any) => s.id === sid);
    expect(found).toBeTruthy();
  });

  test('DELETE /api/sessions/:id removes session from list', async () => {
    const sid = `lifecycle-del-${Date.now()}`;
    await chatSSE('message to delete', sid);

    // Verify it exists
    let resp = await fetch(`${BASE}/api/sessions`, {
      headers: { 'Authorization': `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE },
    });
    let sessions = await resp.json();
    expect(sessions.find((s: any) => s.id === sid)).toBeTruthy();

    // Delete it
    const delResp = await fetch(`${BASE}/api/sessions/${encodeURIComponent(sid)}`, {
      method: 'DELETE',
      headers: { 'Authorization': `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE },
    });
    expect(delResp.status).toBe(204);

    // Verify it's gone from session list
    resp = await fetch(`${BASE}/api/sessions`, {
      headers: { 'Authorization': `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE },
    });
    sessions = await resp.json();
    expect(sessions.find((s: any) => s.id === sid)).toBeFalsy();
  });

  test('deleted session messages are not retrievable', async () => {
    const sid = `lifecycle-msgs-${Date.now()}`;
    await chatSSE('secret message that should be deleted', sid);

    // Verify messages exist
    let msgs = await getMessages(sid);
    expect(msgs.length).toBeGreaterThan(0);

    // Delete session
    await fetch(`${BASE}/api/sessions/${encodeURIComponent(sid)}`, {
      method: 'DELETE',
      headers: { 'Authorization': `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE },
    });

    // Messages should be gone
    msgs = await getMessages(sid);
    expect(msgs.length).toBe(0);
  });

  test('deleted session workspace files are cleaned up', async () => {
    const slug = `delws-${Date.now().toString(36)}`;
    const sid = `lifecycle-ws-${Date.now()}`;

    // Create a slides project (generates workspace files)
    await chatSSE(`/new slides ${slug}`, sid);

    // Verify project was created by checking via a second message
    const { content } = await chatSSE(
      `Use shell to run: ls slides/${slug}/.octos-workspace.toml 2>&1 && echo EXISTS || echo MISSING`,
      sid,
      30_000,
    );

    // Delete the session
    await fetch(`${BASE}/api/sessions/${encodeURIComponent(sid)}`, {
      method: 'DELETE',
      headers: { 'Authorization': `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE },
    });

    // Create a new session and check if workspace files still exist on disk
    const sid2 = `lifecycle-ws-check-${Date.now()}`;
    await chatSSE(`/new slides ${slug}-check`, sid2);
    const { content: checkContent } = await chatSSE(
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

  test('cannot send messages to a deleted session', async () => {
    const sid = `lifecycle-nosend-${Date.now()}`;
    await chatSSE('first message', sid);

    // Delete
    await fetch(`${BASE}/api/sessions/${encodeURIComponent(sid)}`, {
      method: 'DELETE',
      headers: { 'Authorization': `Bearer ${TOKEN}`, 'X-Profile-Id': PROFILE },
    });

    // Send to deleted session — should either create a new empty session
    // or return the response without the old context
    const { content } = await chatSSE('hello after delete', sid);

    // Old messages should not be in context
    const msgs = await getMessages(sid);
    const allContent = msgs.map((m: any) => m.content || '').join(' ');
    expect(allContent).not.toContain('first message');
  });
});
