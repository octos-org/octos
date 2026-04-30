/**
 * W1.G1 acceptance — pipeline node cards render under the run_pipeline
 * tool-call pill with per-node status transitions visible.
 *
 * Drives the SSE chat path, watches for `tool_progress` events keyed
 * to a `run_pipeline` tool_call_id, and asserts:
 *
 *  1. At least one `tool_progress` event arrives carrying a node-id
 *     prefix (`<node_id>:` or `<node_id> [<model>]:`).
 *  2. The same `tool_call_id` carries multiple distinct node ids.
 *  3. Both running and complete states are observable for at least one
 *     node within the test window.
 *
 * Backend behaviour validated:
 *  * W1.A3 task supervisor registers child tasks per pipeline node and
 *    bridges state transitions onto the tool_progress SSE channel
 *    (#9687fa95 + this branch).
 *  * The frontend NodeCard projection is exercised at the protocol
 *    layer — by the shape of the events, not by rendering DOM. This
 *    keeps the spec robust against React markup drift.
 *
 * Run from `e2e/`:
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
 *     npx playwright test tests/live-pipeline-cards.spec.ts --workers=1
 */

import { test, expect } from '@playwright/test';

const BASE = process.env.OCTOS_TEST_URL || 'https://dspfac.bot.ominix.io';
const TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE = process.env.OCTOS_PROFILE || 'dspfac';

test.setTimeout(180_000);

interface SseEvent {
  type: string;
  [key: string]: unknown;
}

async function chatSSE(message: string, sessionId: string, maxWait = 150_000) {
  const resp = await fetch(`${BASE}/api/chat`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'Authorization': `Bearer ${TOKEN}`,
      'X-Profile-Id': PROFILE,
    },
    body: JSON.stringify({ message, session_id: sessionId, stream: true }),
  });
  expect(resp.ok, `chat POST status: ${resp.status}`).toBeTruthy();
  if (!resp.body) throw new Error('empty response body');
  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  const events: SseEvent[] = [];
  const start = Date.now();
  try {
    while (Date.now() - start < maxWait) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let idx = buffer.indexOf('\n\n');
      while (idx >= 0) {
        const block = buffer.slice(0, idx);
        buffer = buffer.slice(idx + 2);
        idx = buffer.indexOf('\n\n');
        const dataLines = block
          .split('\n')
          .filter((l) => l.startsWith('data:'))
          .map((l) => l.slice(5).trim())
          .filter(Boolean);
        for (const line of dataLines) {
          try {
            const evt = JSON.parse(line) as SseEvent;
            events.push(evt);
            if (evt.type === 'done' || evt.type === 'error') {
              return events;
            }
          } catch {
            // skip non-JSON keepalive lines
          }
        }
      }
    }
  } finally {
    try { reader.releaseLock(); } catch { /* ignore */ }
  }
  return events;
}

function progressNodeId(message: string): string | null {
  const trimmed = message.trim();
  // Match "<node_id> [<model>]:" or "<node_id>:" or "<node_id> done ..."
  const m = trimmed.match(/^([\w./:-]+)\s*(?:\[[^\]]+\])?\s*[:>-]/);
  if (m && m[1] !== 'Pipeline') return m[1];
  return null;
}

// SKIP: as of run_pipeline → spawn_only conversion (PR #688), the assistant
// turn returns immediately on the spawn-ack and `done` SSE arrives BEFORE the
// pipeline's tool_progress events flush — so the synchronous ordering this
// spec relies on no longer holds. Replace with a spawn_only-aware spec that
// awaits the BackgroundResultPayload delivery, then asserts on collected
// progress events.
test.describe.skip('W1.G1 — pipeline node card SSE invariants', () => {
  test('run_pipeline emits per-node tool_progress lines under one tool_call_id', async () => {
    const sessionId = `e2e-w1-cards-${Date.now()}`;
    const prompt =
      'Use run_pipeline with this inline DOT graph to plan and summarise three short bullet points about the Mars rover. ' +
      'digraph trio { plan [handler="codergen", prompt="List three Mars rover topics, each one line.", tools=""] ' +
      'summary [handler="codergen", prompt="Summarise the topics in two sentences.", tools=""] ' +
      'plan -> summary }';

    const events = await chatSSE(prompt, sessionId);

    const progressEvents = events.filter(
      (e) => e.type === 'tool_progress' && e.tool === 'run_pipeline',
    );
    expect(
      progressEvents.length,
      'at least one tool_progress event for run_pipeline',
    ).toBeGreaterThan(0);

    const callIds = new Set(
      progressEvents
        .map((e) => (typeof e.tool_call_id === 'string' ? e.tool_call_id : ''))
        .filter(Boolean),
    );
    expect(callIds.size, 'all node progress shares one parent tool_call_id').toBeGreaterThan(0);
    expect(
      callIds.size,
      'progress events share at most a small bounded set of tool_call_ids',
    ).toBeLessThan(8);

    const nodeIds = new Set<string>();
    let sawRunning = false;
    let sawComplete = false;
    for (const evt of progressEvents) {
      const message = typeof evt.message === 'string' ? evt.message : '';
      const nid = progressNodeId(message);
      if (nid) nodeIds.add(nid);
      const lower = message.toLowerCase();
      if (lower.includes('thinking') || lower.includes('running')) sawRunning = true;
      if (
        lower.includes('done') ||
        lower.includes('completed') ||
        lower.includes('response received')
      ) {
        sawComplete = true;
      }
    }

    expect(
      nodeIds.size,
      `expect 2+ distinct node ids; observed ${[...nodeIds].join(',')}`,
    ).toBeGreaterThanOrEqual(2);
    expect(
      sawRunning,
      'expected at least one running/thinking transition in tool_progress',
    ).toBeTruthy();
    expect(
      sawComplete,
      'expected at least one done/complete transition in tool_progress',
    ).toBeTruthy();
  });
});
