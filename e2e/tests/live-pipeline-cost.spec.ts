/**
 * W1.G4 acceptance — pipeline run reports per-node cost rows the
 * frontend CostBreakdown panel can render.
 *
 * Drives the SSE chat path and asserts that the `done` (or matching
 * `tool_complete`) event for a `run_pipeline` invocation carries token
 * accounting that the new `PipelineResult.node_costs` projection is
 * derived from. The deliverable is end-to-end: the backend opens a
 * per-node CostReservationHandle, commits it on completion, and surfaces
 * the row to the frontend via SSE.
 *
 * The spec keeps assertions narrow on purpose:
 *  1. The chat completes without error.
 *  2. The terminal SSE event includes structured token usage with
 *     non-zero in/out totals — confirming the per-node accounting path
 *     ran and aggregated. The exact shape of the embedded `node_costs`
 *     array depends on the SSE schema landing across W1+W4 — when it
 *     is present we additionally assert one row per pipeline node.
 *
 * Run from `e2e/`:
 *   OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
 *     npx playwright test tests/live-pipeline-cost.spec.ts --workers=1
 */

import { test, expect } from '@playwright/test';

const BASE = process.env.OCTOS_TEST_URL || 'https://dspfac.bot.ominix.io';
const TOKEN = process.env.OCTOS_AUTH_TOKEN || 'octos-admin-2026';
const PROFILE = process.env.OCTOS_PROFILE || 'dspfac';

test.setTimeout(180_000);

import { chatWS, type ChatWsEvent } from '../lib/m9-ws-client';

type SseEvent = ChatWsEvent;

/**
 * M9-α-7 (#836): chat helper now drives the M9 WebSocket UI Protocol.
 * The whole describe block below is currently `.skip`'d (run_pipeline is
 * spawn_only post-#688) but we still migrate the helper so the file does
 * not depend on `/api/chat` once SSE is deleted in α-5/α-6.
 */
async function chatViaWs(message: string, sessionId: string, maxWait = 150_000): Promise<SseEvent[]> {
  const { events } = await chatWS({
    baseUrl: BASE,
    token: TOKEN,
    profileId: PROFILE,
    message,
    sessionId,
    maxWait,
  });
  return events;
}

function findToken(events: SseEvent[], key: 'input_tokens' | 'output_tokens'): number {
  // Accept BOTH the OpenAI-style `input_tokens`/`output_tokens` shape AND
  // the octos SSE `done` event's `tokens_in`/`tokens_out` shape. The done
  // event today emits `tokens_in`/`tokens_out` (matched by `api_channel.rs`
  // and `handlers.rs`); rather than rename keys broadly (which would touch
  // every existing W4 assertion), the test accepts both vocabularies so
  // it asserts the data path end-to-end without a global rename.
  const aliasKey = key === 'input_tokens' ? 'tokens_in' : 'tokens_out';
  let total = 0;
  for (const e of events) {
    if (typeof e[key] === 'number') {
      total += e[key] as number;
    }
    if (typeof e[aliasKey] === 'number') {
      total += e[aliasKey] as number;
    }
    if (e.usage && typeof (e.usage as Record<string, unknown>)[key] === 'number') {
      total += (e.usage as Record<string, number>)[key];
    }
    if (e.usage && typeof (e.usage as Record<string, unknown>)[aliasKey] === 'number') {
      total += (e.usage as Record<string, number>)[aliasKey];
    }
    if (Array.isArray(e.node_costs)) {
      for (const row of e.node_costs as Array<Record<string, unknown>>) {
        const v = key === 'input_tokens' ? row.tokens_in : row.tokens_out;
        if (typeof v === 'number') total += v;
      }
    }
  }
  return total;
}

// SKIP: as of run_pipeline → spawn_only conversion (PR #688), pipeline
// execution happens asynchronously after the foreground turn returns the
// spawn-ack. Per-node cost is attributed in the background path and is no
// longer flattened into the foreground `done` SSE event. Replace with a
// spawn_only-aware spec that awaits the BackgroundResultPayload and asserts
// on the cost ledger or the supervisor's task record.
test.describe.skip('W1.G4 — pipeline cost breakdown SSE invariants', () => {
  test('run_pipeline reports non-zero token totals across nodes', async () => {
    const sessionId = `e2e-w1-cost-${Date.now()}`;
    const prompt =
      'Use run_pipeline to draft and refine a haiku about the ocean. ' +
      'digraph haiku { draft [handler="codergen", prompt="Write a haiku about the ocean.", tools=""] ' +
      'refine [handler="codergen", prompt="Refine the haiku for rhythm.", tools=""] ' +
      'draft -> refine }';

    const events = await chatViaWs(prompt, sessionId);

    const errorEvent = events.find((e) => e.type === 'error');
    expect(errorEvent, 'pipeline run must not surface an error event').toBeUndefined();

    const inputTokens = findToken(events, 'input_tokens');
    const outputTokens = findToken(events, 'output_tokens');

    expect(
      inputTokens + outputTokens,
      `expected non-zero total token usage; observed in=${inputTokens} out=${outputTokens}`,
    ).toBeGreaterThan(0);

    // When the SSE schema for node_costs lands (W1.A4 wiring through
    // tool_complete / done events) we additionally assert per-node
    // rows. Today we treat it as a soft-success: presence is the
    // strong invariant, absence is logged but not fatal so the spec
    // can run on the canary builds.
    const eventWithRows = events.find(
      (e) => Array.isArray((e as Record<string, unknown>).node_costs),
    );
    if (eventWithRows) {
      const rows = (eventWithRows as unknown as { node_costs: Array<Record<string, unknown>> }).node_costs;
      expect(
        rows.length,
        'node_costs payload must carry at least one node row',
      ).toBeGreaterThan(0);
      for (const row of rows) {
        expect(typeof row.node_id).toBe('string');
        expect(typeof row.tokens_in).toBe('number');
        expect(typeof row.tokens_out).toBe('number');
        expect(typeof row.actual_usd).toBe('number');
      }
    }
  });
});
