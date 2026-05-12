#!/usr/bin/env node
/**
 * Tiny standalone driver for a single chat turn over the M9 UI Protocol
 * WebSocket. Used by validate-m4-1a-live.sh and other bash-based release
 * gates that previously POSTed `POST /api/chat` (retired post-#908).
 *
 * Wire contract:
 *   session/open + turn/start with one text input item, then collect
 *   notifications until `turn/completed`, `turn/error`, or the deadline
 *   elapses. Mirrors the inner loop in e2e/lib/m9-ws-client.ts::chatWS
 *   but with zero TypeScript / Playwright deps so it can run from a
 *   plain shell harness.
 *
 * Usage:
 *   node ws-chat.mjs \
 *     --url http://127.0.0.1:56831 \
 *     --token octos-admin-2026 \
 *     --session m4-1a-live-… \
 *     --message "deep research prompt" \
 *     [--profile dspfac] \
 *     [--max-wait-ms 90000]
 *
 * Exit code:
 *   0  turn/completed received
 *   2  bad usage
 *   3  WS connection or RPC error
 *   4  deadline elapsed without turn/completed
 *
 * Output (stdout, single line):
 *   {"status":"completed","content":"…","events":[…]}  (on success)
 *   {"status":"error","message":"…"}                   (on failure)
 */

import WebSocket from 'ws';
import { randomBytes, randomUUID } from 'node:crypto';

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    if (!k.startsWith('--')) continue;
    const key = k.slice(2);
    const v = argv[i + 1];
    if (v && !v.startsWith('--')) {
      out[key] = v;
      i++;
    } else {
      out[key] = true;
    }
  }
  return out;
}

const args = parseArgs(process.argv.slice(2));
const url = args.url;
const token = args.token;
const sessionId = args.session;
const message = args.message;
const profileId = args.profile;
const maxWaitMs = Number(args['max-wait-ms'] || 60_000);

if (!url || !token || !sessionId || !message) {
  console.error('usage: ws-chat.mjs --url <url> --token <t> --session <id> --message <msg> [--profile <p>] [--max-wait-ms <n>]');
  process.exit(2);
}

const wsUrl = url
  .replace(/^http:/, 'ws:')
  .replace(/^https:/, 'wss:')
  .replace(/\/$/, '')
  .concat(url.includes('/api/ui-protocol/ws') ? '' : '/api/ui-protocol/ws');

const ws = new WebSocket(wsUrl, { headers: { Authorization: `Bearer ${token}` } });

const pending = new Map();
let content = '';
const events = [];
let turnId;
let done = false;
let timer;

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

function finish(payload, exitCode) {
  if (done) return;
  done = true;
  clearTimeout(timer);
  console.log(JSON.stringify(payload));
  try { ws.close(); } catch { /* ignore */ }
  setTimeout(() => process.exit(exitCode), 50);
}

ws.on('open', async () => {
  try {
    await send('session/open', { session_id: sessionId, profile_id: profileId });
    turnId = randomUUID();
    await send('turn/start', {
      session_id: sessionId,
      turn_id: turnId,
      input: [{ kind: 'text', text: message }],
    });
    timer = setTimeout(() => {
      finish({ status: 'timeout', content, events }, 4);
    }, maxWaitMs);
  } catch (err) {
    finish({ status: 'error', message: String(err?.message ?? err) }, 3);
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
    switch (frame.method) {
      case 'message/delta':
        if (typeof params.text === 'string') content += params.text;
        break;
      case 'turn/completed':
        finish({ status: 'completed', content, events }, 0);
        break;
      case 'turn/error':
        finish({ status: 'error', message: params.message || params.code || 'turn/error', events }, 3);
        break;
    }
  }
});

ws.on('error', (err) => {
  finish({ status: 'error', message: `ws-error: ${err.message}` }, 3);
});

ws.on('close', () => {
  if (!done) finish({ status: 'error', message: 'ws-closed-before-turn-completed' }, 3);
});
