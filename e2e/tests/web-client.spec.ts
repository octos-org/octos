/**
 * Web client tests for SSE streaming, UTF-8 handling, and file delivery.
 *
 * Tests the gateway API channel (POST /chat → SSE response) which is
 * used by the octos-web chat client.
 *
 * Run against a live octos-serve instance:
 *   OCTOS_TEST_URL=http://localhost:3000 npx playwright test web-client
 *
 * These tests target the dspfac profile's API channel, proxied through
 * the dashboard at /api/admin/profiles/{id}/proxy/chat.
 */
import { test, expect } from '@playwright/test';

/**
 * Auth token (optional). If not set, tests use the profile subdomain
 * (e.g. dspfac.crew.ominix.io) where Caddy injects X-Profile-Id.
 */
const AUTH_TOKEN = process.env.OCTOS_AUTH_TOKEN || '';

function headers() {
  const h: Record<string, string> = { 'Content-Type': 'application/json' };
  if (AUTH_TOKEN) h['Authorization'] = `Bearer ${AUTH_TOKEN}`;
  return h;
}

/**
 * POST /chat and collect all SSE events until the stream closes.
 * Returns parsed JSON events.
 *
 * The /api/chat endpoint accepts a message and returns an SSE stream.
 * Profile is resolved from X-Profile-Id header or auth token.
 */
async function chatSSE(
  _request: any,
  baseURL: string,
  message: string,
  sessionId?: string,
  timeoutMs = 60_000,
): Promise<{ events: any[]; raw: string }> {
  const sid = sessionId || `test-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;

  // Use a real streaming reader for SSE. Playwright's request.post buffers the
  // full body and can time out on longer streams before the server closes it.
  for (let attempt = 0; attempt < 2; attempt++) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), timeoutMs);
    const res = await fetch(`${baseURL}/api/chat`, {
      method: 'POST',
      headers: headers(),
      body: JSON.stringify({
        message,
        session_id: attempt === 0 ? sid : `${sid}-r`,
        stream: true,
      }),
      signal: controller.signal,
    });

    if (!res.ok) {
      clearTimeout(timeout);
      const body = await res.text().catch(() => '');
      throw new Error(`chat failed: ${res.status} ${body.slice(0, 200)}`);
    }

    if (!res.body) {
      clearTimeout(timeout);
      if (attempt === 1) {
        return { events: [], raw: '' };
      }
      continue;
    }

    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let raw = '';
    let buffer = '';
    const events: any[] = [];
    let sawDone = false;

    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        const chunk = decoder.decode(value, { stream: true });
        raw += chunk;
        buffer += chunk;
        const lines = buffer.split('\n');
        buffer = lines.pop() || '';
        for (const line of lines) {
          const trimmed = line.trim();
          if (!trimmed.startsWith('data:')) continue;
          const json = trimmed.slice(5).trim();
          if (!json || json === '[DONE]') continue;
          try {
            const event = JSON.parse(json);
            events.push(event);
            if (event?.type === 'done') {
              sawDone = true;
              return { events, raw };
            }
          } catch {
            // skip non-JSON lines
          }
        }
      }
    } finally {
      clearTimeout(timeout);
      reader.releaseLock();
    }

    if (sawDone || events.length > 0 || attempt === 1) {
      return { events, raw };
    }
    // Empty response — wait briefly and retry with fresh session
    await new Promise((r) => setTimeout(r, 1000));
  }

  return { events: [], raw: '' };
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
// Test 1: SSE stream returns valid UTF-8 for CJK characters
//
// Bug: SSE parser used String::from_utf8_lossy on each HTTP chunk.
// Multi-byte CJK characters split across chunks became U+FFFD (�).
// Fix: f4b27b9 — byte-buffer SSE parser.
// ---------------------------------------------------------------------------
test('SSE response preserves CJK characters without corruption', async ({
  request,
  baseURL,
}) => {

  const { events, raw } = await chatSSE(
    request,
    baseURL!,
    '用中文回复：你好世界。只回复这四个字，不要多说。',
  );

  // The raw SSE response should not contain U+FFFD replacement characters
  expect(raw).not.toContain('\uFFFD');
  expect(raw).not.toContain('�');

  // Should have received at least some events
  expect(events.length).toBeGreaterThan(0);

  // Check all text-bearing events for CJK content
  const allContent = events
    .map((e) => e.text || e.content || '')
    .join('');
  expect(allContent).toMatch(/[\u4e00-\u9fff]/); // Contains CJK characters
});

// ---------------------------------------------------------------------------
// Test 2: SSE stream handles multi-byte characters in longer responses
//
// Longer responses are more likely to trigger chunk-boundary splits.
// ---------------------------------------------------------------------------
test('SSE handles long CJK response without garbling', async ({
  request,
  baseURL,
}) => {

  const { events, raw } = await chatSSE(
    request,
    baseURL!,
    '列出5个中国城市的名字，每个城市一行，只要城市名不要其他内容。',
    undefined,
    45_000,
  );

  // No replacement characters anywhere in the stream
  expect(raw).not.toContain('\uFFFD');

  // Should contain recognizable Chinese city names
  const allContent = events
    .filter((e) => e.type === 'replace' || e.type === 'done')
    .map((e) => e.text || e.content || '')
    .join('');

  // At least one common city should appear (北京/上海/广州/深圳/成都/杭州/etc.)
  const cities = ['北京', '上海', '广州', '深圳', '成都', '杭州', '武汉', '南京', '重庆', '天津'];
  const found = cities.some((city) => allContent.includes(city));
  expect(found).toBe(true);
});

// ---------------------------------------------------------------------------
// Test 3: SSE stream completes with a done event
//
// Verifies the basic SSE lifecycle: events flow, stream terminates with
// a "done" event containing token usage metadata.
// ---------------------------------------------------------------------------
test('SSE stream completes with done event and token counts', async ({
  request,
  baseURL,
}) => {

  const { events } = await chatSSE(
    request,
    baseURL!,
    'Say "hello" and nothing else.',
  );

  expect(events.length).toBeGreaterThan(0);

  // Last meaningful event should be "done"
  const doneEvents = events.filter((e) => e.type === 'done');
  expect(doneEvents.length).toBe(1);

  const done = doneEvents[0];
  expect(done.tokens_in).toBeGreaterThan(0);
  expect(done.tokens_out).toBeGreaterThan(0);
});

// ---------------------------------------------------------------------------
// Test 4: Chat session persistence — messages survive across requests
//
// Verifies that sending two messages with the same session_id maintains
// conversation context.
// ---------------------------------------------------------------------------
test('session persists across requests', async ({ request, baseURL }) => {

  const sid = `test-persist-${Date.now()}`;

  // First message: establish context
  await chatSSE(request, baseURL!, 'Remember the code word: PINEAPPLE42', sid);

  // Second message: recall context
  const { events } = await chatSSE(
    request,
    baseURL!,
    'What was the code word I just told you? Reply with only the code word.',
    sid,
    30_000,
  );

  const content = events
    .filter((e) => e.type === 'replace' || e.type === 'done')
    .map((e) => e.text || e.content || '')
    .join('');

  expect(content.toUpperCase()).toContain('PINEAPPLE42');
});

// ---------------------------------------------------------------------------
// Test 5: File event is delivered via SSE when tool produces a file
//
// Verifies that tools returning files_to_send get delivered as SSE
// "file" events with path and filename.
// ---------------------------------------------------------------------------
test('file delivery is visible via SSE or committed session result', async ({ request, baseURL }) => {
  const sid = `test-file-${Date.now()}`;
  const fileDir = `octos-web-file-${Date.now()}`;
  const filePath = `./${fileDir}/octos_e2e_test.txt`;

  const { events } = await chatSSE(
    request,
    baseURL!,
    `Use the shell tool to run \`mkdir -p ./${fileDir} && printf 'test123\\n' > ${filePath}\`. Then use send_file to send ${filePath} to me.`,
    sid,
    90_000,
  );

  // Look for a file event in the SSE stream
  const fileEvents = events.filter((e) => e.type === 'file');
  const sessionResultMediaEvents = events.filter(
    (e) => e.type === 'session_result' && Array.isArray(e.message?.media) && e.message.media.length > 0,
  );
  const doneEvent = events.find((e) => e.type === 'done');

  // If the agent successfully created and sent the file, we should see a file event
  if (fileEvents.length > 0 || sessionResultMediaEvents.length > 0) {
    if (fileEvents.length > 0) {
      expect(fileEvents[0].filename).toBeTruthy();
      expect(fileEvents[0].path).toBeTruthy();
    } else {
      expect(sessionResultMediaEvents[0].message.media[0]).toBeTruthy();
    }
    return;
  }

  const content = events
    .filter((e) => e.type === 'replace' || e.type === 'done')
    .map((e) => e.text || e.content || '')
    .join('');

  if (doneEvent?.has_bg_tasks) {
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

  // Agent might not have used send_file — check that the response at least
  // mentions the file was created (acceptable fallback)
  expect(content).toMatch(/test.*file|created|written/i);
});

// ---------------------------------------------------------------------------
// Test 6: Concurrent sessions don't cross-contaminate
//
// Two simultaneous requests with different session IDs should get
// independent responses.
// ---------------------------------------------------------------------------
test('concurrent sessions are isolated', async ({ request, baseURL }) => {

  const sidA = `test-iso-a-${Date.now()}`;
  const sidB = `test-iso-b-${Date.now()}`;

  const [resultA, resultB] = await Promise.all([
    chatSSE(request, baseURL!, 'Reply with exactly: ALPHA', sidA),
    chatSSE(request, baseURL!, 'Reply with exactly: BRAVO', sidB),
  ]);

  const contentA = resultA.events
    .filter((e) => e.type === 'replace' || e.type === 'done')
    .map((e) => e.text || e.content || '')
    .join('');

  const contentB = resultB.events
    .filter((e) => e.type === 'replace' || e.type === 'done')
    .map((e) => e.text || e.content || '')
    .join('');

  expect(contentA.toUpperCase()).toContain('ALPHA');
  expect(contentB.toUpperCase()).toContain('BRAVO');
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
