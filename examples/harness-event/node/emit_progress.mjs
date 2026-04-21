#!/usr/bin/env node
/**
 * Dependency-light Octos harness event emitter for Node tools.
 */

import fs from 'node:fs/promises';
import net from 'node:net';
import { fileURLToPath } from 'node:url';
import process from 'node:process';

const SCHEMA = 'octos.harness.event.v1';

export function buildProgressEvent(sessionId, taskId, workflow, phase, message, progress) {
  const event = {
    schema: SCHEMA,
    kind: 'progress',
    session_id: sessionId,
    task_id: taskId,
    workflow,
    phase,
    message,
  };
  if (progress !== undefined) {
    event.progress = progress;
  }
  return event;
}

function parseSink(sink) {
  if (!sink.includes('://')) {
    return { transport: 'file', path: sink };
  }

  const url = new URL(sink);
  if (url.protocol === 'file:') {
    return { transport: 'file', path: fileURLToPath(url) };
  }
  if (url.protocol === 'unix:') {
    return { transport: 'unix', path: decodeURIComponent(url.pathname || url.host || '') };
  }
  return { transport: url.protocol.replace(/:$/, ''), path: sink };
}

async function appendFile(path, line) {
  await fs.appendFile(path, line, 'utf8');
}

async function sendUnixSocket(path, line) {
  await new Promise((resolve, reject) => {
    const socket = net.createConnection({ path }, () => {
      socket.end(line, 'utf8', resolve);
    });
    socket.on('error', reject);
  });
}

export async function emitEvent(event, sink = process.env.OCTOS_EVENT_SINK || '') {
  if (!sink) {
    return false;
  }

  const line = `${JSON.stringify(event)}\n`;
  const target = parseSink(sink);

  try {
    if (target.transport === 'unix') {
      if (!target.path) {
        throw new Error('unix sink path is empty');
      }
      await sendUnixSocket(target.path, line);
    } else {
      await appendFile(target.path, line);
    }
    return true;
  } catch (error) {
    console.error(`octos event sink write failed: ${error instanceof Error ? error.message : String(error)}`);
    return false;
  }
}

export async function emitProgress(sessionId, taskId, workflow, phase, message, progress, sink) {
  return emitEvent(
    buildProgressEvent(sessionId, taskId, workflow, phase, message, progress),
    sink,
  );
}

function parseArgs(argv) {
  const args = {};
  for (let i = 2; i < argv.length; i += 1) {
    const item = argv[i];
    if (!item.startsWith('--')) {
      continue;
    }
    const key = item.slice(2);
    const value = argv[i + 1];
    if (value === undefined || value.startsWith('--')) {
      args[key] = true;
      continue;
    }
    args[key] = value;
    i += 1;
  }
  return args;
}

async function main() {
  const args = parseArgs(process.argv);
  if (!args['session-id'] || !args['task-id'] || !args.workflow || !args.phase || !args.message) {
    console.error('usage: emit_progress.mjs --session-id ... --task-id ... --workflow ... --phase ... --message ... [--progress N] [--sink URI]');
    return 2;
  }

  const progress = args.progress === undefined ? undefined : Number(args.progress);
  const sink = args.sink ?? process.env.OCTOS_EVENT_SINK ?? '';
  const wrote = await emitProgress(
    args['session-id'],
    args['task-id'],
    args.workflow,
    args.phase,
    args.message,
    Number.isNaN(progress) ? undefined : progress,
    sink,
  );
  return sink ? (wrote ? 0 : 1) : 0;
}

if (fileURLToPath(import.meta.url) === process.argv[1]) {
  const code = await main();
  process.exitCode = code;
}
