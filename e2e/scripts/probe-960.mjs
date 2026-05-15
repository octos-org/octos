#!/usr/bin/env node
// Probe whether `turn/spawn_complete` now carries
// `response_to_client_message_id` (issue #960 fix).

import WebSocket from 'ws';
import { randomBytes, randomUUID } from 'node:crypto';

const url = process.env.PROBE_URL || 'http://127.0.0.1:50080';
const token = process.env.PROBE_TOKEN || 'octos-admin-2026';
const sessionId = process.env.PROBE_SESSION || `dspfac:api:probe-960-${Date.now()}`;
const profileId = process.env.PROBE_PROFILE || 'dspfac';
const message = process.env.PROBE_MESSAGE || 'Use run_pipeline to research the weather in San Francisco today (1 paragraph).';
const maxWaitMs = Number(process.env.PROBE_WAIT_MS || 600_000);
const drainAfterCompletedMs = Number(process.env.PROBE_DRAIN_MS || 540_000);

const wsUrl = url
  .replace(/^http:/, 'ws:')
  .replace(/^https:/, 'wss:')
  .replace(/\/$/, '')
  .concat('/api/ui-protocol/ws?ui_feature=event.spawn_complete.v1&ui_feature=event.message_persisted.v1&ui_feature=state.session_hydrate.v1');

console.error(`[probe] connecting to ${wsUrl}`);
const ws = new WebSocket(wsUrl, { headers: { Authorization: `Bearer ${token}` } });

const pending = new Map();
const events = [];
let turnId;
let done = false;
let timer;
let drainTimer;
let spawnCompleteEvents = [];

function send(method, params) {
  const id = `req-${Date.now()}-${randomBytes(2).toString('hex')}`;
  ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }));
  return new Promise((resolve, reject) => {
    const to = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`rpc timeout: ${method}`));
    }, 30_000);
    pending.set(id, {
      resolve: (v) => { clearTimeout(to); resolve(v); },
      reject: (e) => { clearTimeout(to); reject(e); },
    });
  });
}

function finish(exitCode) {
  if (done) return;
  done = true;
  clearTimeout(timer);
  clearTimeout(drainTimer);
  const summary = {
    turnId,
    sessionId,
    spawn_complete_count: spawnCompleteEvents.length,
    spawn_complete_events: spawnCompleteEvents,
    all_methods: events.map(e => e.method),
  };
  console.log(JSON.stringify(summary, null, 2));
  try { ws.close(); } catch { /* ignore */ }
  setTimeout(() => process.exit(exitCode), 50);
}

ws.on('open', async () => {
  try {
    const openResp = await send('session/open', { session_id: sessionId, profile_id: profileId });
    console.error(`[probe] session/open ok; capabilities=${JSON.stringify(openResp?.capabilities || openResp || {}).slice(0, 200)}`);
    turnId = randomUUID();
    await send('turn/start', {
      session_id: sessionId,
      turn_id: turnId,
      input: [{ kind: 'text', text: message }],
    });
    console.error(`[probe] turn/start accepted; turn_id=${turnId}`);
    timer = setTimeout(() => {
      console.error(`[probe] timeout after ${maxWaitMs}ms`);
      finish(4);
    }, maxWaitMs);
  } catch (err) {
    console.error(`[probe] open/turn error: ${err?.message || err}`);
    finish(3);
  }
});

ws.on('message', (data) => {
  let frame;
  try { frame = JSON.parse(data.toString()); } catch { return; }
  if (frame && 'id' in frame && frame.id != null) {
    const p = pending.get(String(frame.id));
    if (p) {
      pending.delete(String(frame.id));
      if (frame.error) p.reject(new Error(`rpc-error[${frame.error.code}] ${frame.error.message}`));
      else p.resolve(frame.result);
    }
    return;
  }
  if (frame && frame.method) {
    const params = frame.params || {};
    if (params.turn_id !== undefined && params.turn_id !== turnId) return;
    events.push({ method: frame.method, params });
    if (frame.method === 'turn/spawn_complete') {
      spawnCompleteEvents.push({
        task_id: params.task_id,
        thread_id: params.thread_id,
        response_to_client_message_id: params.response_to_client_message_id,
        content_preview: typeof params.content === 'string' ? params.content.slice(0, 120) : null,
        media_count: Array.isArray(params.media) ? params.media.length : 0,
        message_id: params.message_id,
        seq: params.seq,
        param_keys: Object.keys(params).sort(),
        raw_envelope_truncated: JSON.stringify(frame).slice(0, 600),
      });
      console.error(`[probe] turn/spawn_complete: task_id=${params.task_id} response_to_cmid=${params.response_to_client_message_id} thread_id=${params.thread_id}`);
      console.error(`[probe]   content_preview: ${(params.content || '').slice(0, 120)}`);
      // Found one — finish quickly.
      setTimeout(() => finish(0), 2000);
    } else if (frame.method === 'turn/completed' || frame.method === 'turn/error') {
      console.error(`[probe] ${frame.method} — draining for spawn_complete (max ${drainAfterCompletedMs}ms)`);
      drainTimer = setTimeout(() => {
        console.error(`[probe] drain window elapsed without turn/spawn_complete`);
        finish(spawnCompleteEvents.length > 0 ? 0 : 5);
      }, drainAfterCompletedMs);
    }
  }
});

ws.on('error', (err) => {
  console.error(`[probe] ws-error: ${err.message}`);
  finish(3);
});

ws.on('close', () => {
  if (!done) {
    console.error(`[probe] ws-closed-before-completion`);
    finish(3);
  }
});
