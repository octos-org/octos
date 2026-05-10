/**
 * Web client tests for chat streaming, UTF-8 handling, and file delivery.
 *
 * M9-α-7 (#836): rewritten to drive the chat turn through the M9 WebSocket
 * UI Protocol via `chatWS()` instead of the legacy `/api/chat` SSE endpoint.
 * The original SSE assertions on UTF-8 byte integrity are preserved by
 * inspecting the cumulative `content` produced from `message/delta`
 * notifications — the JSON-RPC frame layer carries text losslessly so any
 * encoding bug regressing in the LLM provider chain still surfaces.
 *
 * Run against a live octos-serve instance:
 *   OCTOS_TEST_URL=http://localhost:3000 npx playwright test web-client
 */
import { test, expect } from '@playwright/test';
import { chatWS, type ChatWsEvent } from '../lib/m9-ws-client';

test.setTimeout(240_000);

const AUTH_TOKEN =
  process.env.OCTOS_AUTH_TOKEN ||
  process.env.OCTOS_LIVE_TOKEN ||
  process.env.OCTOS_TEST_TOKEN ||
  '';

function headers() {
  const h: Record<string, string> = { 'Content-Type': 'application/json' };
  if (AUTH_TOKEN) h['Authorization'] = `Bearer ${AUTH_TOKEN}`;
  return h;
}

async function chatViaWs(
  baseURL: string,
  message: string,
  sessionId?: string,
  timeoutMs = 120_000,
): Promise<{ events: ChatWsEvent[]; content: string; doneEvent?: ChatWsEvent }> {
  const sid =
    sessionId ||
    `test-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  return chatWS({
    baseUrl: baseURL,
    token: AUTH_TOKEN,
    message,
    sessionId: sid,
    maxWait: timeoutMs,
  });
}

async function getSessionMessages(
  request: any,
  baseURL: string,
  sessionId: string,
  params: { source?: 'full' | 'memory'; sinceSeq?: number } = {},
): Promise<any[]> {
  const search = new URLSearchParams();
  if (params.source) search.set('source', params.source);
  if (typeof params.sinceSeq === 'number') {
    search.set('since_seq', String(params.sinceSeq));
  }
  const suffix = search.size > 0 ? `?${search.toString()}` : '';
  const res = await request.get(`${baseURL}/api/sessions/${sessionId}/messages${suffix}`, {
    headers: headers(),
  });
  if (!res.ok()) return [];
  return res.json();
}

// ---------------------------------------------------------------------------
// Test 1: WS chat preserves UTF-8 for CJK characters
//
// Historical bug (SSE-era): the SSE parser used String::from_utf8_lossy on
// each HTTP chunk and multi-byte CJK split across chunks became U+FFFD.
// Fix: f4b27b9 (byte-buffer SSE parser). The chat path now runs over
// JSON-RPC frames on WebSocket, so this regression is structurally
// impossible at the transport layer; we keep an equivalent assertion
// against the cumulative `content` string so any *upstream* (LLM provider)
// regression would still surface.
// ---------------------------------------------------------------------------
test('WS chat preserves CJK characters without corruption', async ({
  baseURL,
}) => {

  const { events, content } = await chatViaWs(
    baseURL!,
    '用中文回复：你好世界。只回复这四个字，不要多说。',
  );

  // The synthesized chat content should be free of replacement characters.
  expect(content).not.toContain('�');

  // Should have received at least some events
  expect(events.length).toBeGreaterThan(0);

  // Check all text-bearing events for CJK content
  const allContent = events
    .map((e) =>
      (typeof e.text === 'string' ? e.text : '') ||
      (typeof e.content === 'string' ? e.content : ''),
    )
    .join('');
  expect(allContent).toMatch(/[一-鿿]/); // Contains CJK characters
});

// ---------------------------------------------------------------------------
// Test 2: WS chat handles multi-byte characters in longer responses
//
// Longer responses surface chunk-boundary handling bugs. Same protective
// assertion as test 1, scaled up.
// ---------------------------------------------------------------------------
test('WS chat handles long CJK response without garbling', async ({
  baseURL,
}) => {

  const { content } = await chatViaWs(
    baseURL!,
    '列出5个中国城市的名字，每个城市一行，只要城市名不要其他内容。',
    undefined,
    120_000,
  );

  // No replacement characters anywhere in the assistant content stream.
  expect(content).not.toContain('�');

  // The final streamed content should still contain substantial CJK text.
  const cjkChars = content.match(/[一-鿿]/g) || [];
  expect(cjkChars.length).toBeGreaterThan(8);
});

// ---------------------------------------------------------------------------
// Test 3: WS chat completes with a turn/completed -> synthesized done
//
// Verifies the basic chat lifecycle terminates with a `done` event
// synthesized from `turn/completed`.
//
// NOTE on token counts: The legacy SSE `done` event carried `tokens_in` /
// `tokens_out`. The M9 `turn/completed` notification does NOT yet carry
// them (deferred follow-up — α-3 punted token usage to γ-3). For now this
// test asserts on the lifecycle terminal only; the cost-tracking spec
// covers token math via the REST /api/sessions/:id/tasks snapshot.
// ---------------------------------------------------------------------------
test('WS chat completes with terminal done event', async ({
  baseURL,
}) => {

  const { events, doneEvent } = await chatViaWs(
    baseURL!,
    'Say "hello" and nothing else.',
  );

  expect(events.length).toBeGreaterThan(0);

  // Exactly one synthesized `done` event from `turn/completed`.
  const doneEvents = events.filter((e) => e.type === 'done');
  expect(doneEvents.length).toBe(1);
  expect(doneEvent).toBeTruthy();
});

// ---------------------------------------------------------------------------
// Test 4: Chat session persistence — messages survive across requests
//
// Verifies that sending two messages with the same session_id maintains
// conversation context (the persistence guarantee is independent of the
// streaming transport — REST `/api/sessions/:id/messages` is the source
// of truth, the WS path just drives the live turn).
// ---------------------------------------------------------------------------
test('session persists across requests', async ({ request, baseURL }) => {

  const sid = `test-persist-${Date.now()}`;

  const { content } = await chatViaWs(
    baseURL!,
    'Say "OK" and nothing else.',
    sid,
    180_000,
  );

  expect(content.toUpperCase()).toContain('OK');

  const messages = await getSessionMessages(request, baseURL!, sid, { source: 'full' });
  expect(messages.some((message: any) => message.role === 'user')).toBeTruthy();
  expect(
    messages.some(
      (message: any) =>
        message.role === 'assistant' &&
        typeof message.content === 'string' &&
        message.content.toUpperCase().includes('OK'),
    ),
  ).toBeTruthy();
});

// ---------------------------------------------------------------------------
// Test 5: File event is delivered when tool produces a file
//
// SSE-era this test asserted on `type === 'file'` events and on
// `session_result.message.media`. The M9 WS protocol has NO equivalent
// notification yet — `session_result` is on the α-3 deferred set
// (UPCR-2026-012 / γ-3 follow-up) and per-turn file events have not been
// promoted to a typed UI notification at all. We mark the test as fixme;
// once the WS variant lands, the body can use the same REST fallback path
// it already uses for has_bg_tasks turns.
// ---------------------------------------------------------------------------
test.fixme('file delivery is visible via WS or committed session result', async ({ request, baseURL }) => {
  test.slow();
  const sid = `test-file-${Date.now()}`;
  const fileDir = `octos-web-file-${Date.now()}`;
  const filePath = `./${fileDir}/octos_e2e_test.txt`;

  const { events, doneEvent } = await chatViaWs(
    baseURL!,
    `Use write_file to create ${filePath} with exactly this content: test123. Then use send_file to send ${filePath} to me. Do not use shell.`,
    sid,
    180_000,
  );

  // Look for a file event (deferred per α-3) or session_result media.
  const fileEvents = events.filter((e) => e.type === 'file');
  const sessionResultMediaEvents = events.filter(
    (e) =>
      e.type === 'session_result' &&
      Array.isArray((e as any).message?.media) &&
      (e as any).message.media.length > 0,
  );

  if (fileEvents.length > 0 || sessionResultMediaEvents.length > 0) {
    if (fileEvents.length > 0) {
      expect((fileEvents[0] as any).filename).toBeTruthy();
      expect((fileEvents[0] as any).path).toBeTruthy();
    } else {
      expect((sessionResultMediaEvents[0] as any).message.media[0]).toBeTruthy();
    }
    return;
  }

  if ((doneEvent as any)?.has_bg_tasks) {
    const deadline = Date.now() + 90_000;
    let latestMessages: any[] = [];
    while (Date.now() < deadline) {
      latestMessages = await getSessionMessages(request, baseURL!, sid, { source: 'full' });
      const delivered = latestMessages.find(
        (message: any) => message.role === 'assistant' && Array.isArray(message.media) && message.media.length > 0,
      );
      if (delivered) {
        expect(delivered.media[0]).toBeTruthy();
        return;
      }
      const failure = latestMessages.find(
        (message: any) =>
          message.role === 'assistant' &&
          typeof message.content === 'string' &&
          message.content.startsWith('✗'),
      );
      if (failure) {
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 2_000));
    }

    const historyText = latestMessages
      .map((message: any) => message.content || '')
      .join('\n');
    expect(historyText).toMatch(/test.*file|created|written|sent/i);
    return;
  }

  // Acceptable fallback: the agent at least mentions creation.
  const synthContent = events
    .filter((e) => e.type === 'token' || e.type === 'done')
    .map((e) => (typeof e.text === 'string' ? e.text : '') || (typeof e.content === 'string' ? e.content : ''))
    .join('');
  expect(synthContent).toMatch(/test.*file|created|written/i);
});

// ---------------------------------------------------------------------------
// Test 6: Concurrent sessions don't cross-contaminate
//
// Two simultaneous chat turns with different session IDs should get
// independent responses on independent WS connections.
// ---------------------------------------------------------------------------
test('concurrent sessions are isolated', async ({ baseURL }) => {

  const sidA = `test-iso-a-${Date.now()}`;
  const sidB = `test-iso-b-${Date.now()}`;

  const [resultA, resultB] = await Promise.all([
    chatViaWs(baseURL!, 'Reply with exactly: ALPHA', sidA),
    chatViaWs(baseURL!, 'Reply with exactly: BRAVO', sidB),
  ]);

  expect(resultA.content.toUpperCase()).toContain('ALPHA');
  expect(resultB.content.toUpperCase()).toContain('BRAVO');
});

// ---------------------------------------------------------------------------
// Test 7: Session list API returns valid data
// ---------------------------------------------------------------------------
test('session list API works', async ({ request, baseURL }) => {

  const res = await request.get(`${baseURL}/api/sessions`, {
    headers: headers(),
  });

  expect(res.ok()).toBe(true);
  const body = await res.json();
  expect(Array.isArray(body)).toBe(true);
});
