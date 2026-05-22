#!/usr/bin/env node
// e2e/matrix/run.mjs
//
// M22-H operational matrix runner — first PR skeleton (#1056).
//
// Reads a scenario manifest (TOML), filters by `--pack <name> --tier <t>`,
// and executes each scenario's `steps` against `octos serve --stdio`.
//
// Scope of this PR:
//   - Pack: onboarding (tier=fast only)
//   - Mock-or-deterministic only; no provider key, no live tmux.
//   - Validators are intentionally limited to shape checks (`result_has`,
//     `ok`, `error_kind`). Real validators land in PR #2.
//
// Hard rules:
//   - Node stdlib only — no npm dependencies.
//   - We bundle a tiny TOML parser sufficient for `onboarding.toml`. It is
//     not a general-purpose TOML implementation; it supports what the
//     scenario manifest needs (tables, arrays-of-tables, inline tables,
//     inline arrays, strings, bools, integers, floats, comments).

import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';

// ---------------------------------------------------------------------------
// CLI parsing.
// ---------------------------------------------------------------------------

function parseCli(argv) {
  const args = { pack: null, tier: null, manifest: null };
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    const value = argv[i + 1];
    switch (flag) {
      case '--pack':
        args.pack = value;
        i += 1;
        break;
      case '--tier':
        args.tier = value;
        i += 1;
        break;
      case '--manifest':
        args.manifest = value;
        i += 1;
        break;
      case '--help':
      case '-h':
        printUsageAndExit(0);
        break;
      default:
        if (flag && flag.startsWith('-')) {
          console.error(`Unknown flag: ${flag}`);
          printUsageAndExit(2);
        }
        break;
    }
  }
  return args;
}

function printUsageAndExit(code) {
  const usage = [
    'Usage: node e2e/matrix/run.mjs --pack <name> --tier <fast|local|release>',
    '',
    'Flags:',
    '  --pack <name>     Required. Scenario pack id (e.g. "onboarding").',
    '  --tier <t>        Required. One of "fast", "local", "release".',
    '  --manifest <p>    Optional. Path to TOML manifest. Defaults to',
    '                    e2e/matrix/<pack>.toml relative to repo root.',
    '',
    'Environment:',
    '  OCTOS_BIN                  Override path to the octos binary.',
    '                             Defaults to <repo>/target/debug/octos.',
    '  OCTOS_MATRIX_DIR           Override run output root.',
    '                             Defaults to e2e/test-results-matrix/<UTC>/.',
    '  OCTOS_MATRIX_RPC_TIMEOUT_MS  Per-RPC timeout. Default 10000.',
  ].join('\n');
  console.log(usage);
  process.exit(code);
}

// ---------------------------------------------------------------------------
// Minimal TOML parser.
//
// Designed for the onboarding manifest. Supports:
//   - line comments (`#` to end of line, outside strings)
//   - bare keys, dotted keys (only used for `[table.subtable]` headers)
//   - `[table]` headers, `[[array.of.tables]]` headers
//   - inline tables   `{ a = 1, b = "x" }`
//   - inline arrays   `[1, 2, "x"]`
//   - strings: "..." with `\n`, `\t`, `\"`, `\\` escapes
//   - integers, floats (basic), `true`/`false`
//
// NOT supported (intentionally — keep it small):
//   - multi-line strings, literal strings, hex/oct integers, dates,
//     unicode escape sequences. The manifest does not need them.
// ---------------------------------------------------------------------------

function parseToml(source) {
  const root = {};
  let cursor = root;
  const rawLines = source.split(/\r?\n/);
  // Pre-pass: join multi-line inline values. If an opening `{` or `[` is
  // unbalanced at the end of a (comment-stripped) line, fold the next line
  // into it. This keeps the rest of the parser line-oriented.
  const lines = foldContinuations(rawLines);

  for (let lineNo = 0; lineNo < lines.length; lineNo += 1) {
    const raw = lines[lineNo];
    const line = stripComment(raw).trim();
    if (line === '') continue;

    if (line.startsWith('[[') && line.endsWith(']]')) {
      const key = line.slice(2, -2).trim();
      cursor = enterArrayOfTables(root, key);
      continue;
    }
    if (line.startsWith('[') && line.endsWith(']')) {
      const key = line.slice(1, -1).trim();
      cursor = enterTable(root, key);
      continue;
    }
    const eq = findTopLevelEquals(line);
    if (eq < 0) {
      throw new Error(`TOML parse error: malformed line ${lineNo + 1}: ${raw}`);
    }
    const keyPart = line.slice(0, eq).trim();
    const valuePart = line.slice(eq + 1).trim();
    const value = parseValue(valuePart, lineNo + 1);
    assignKey(cursor, keyPart, value, lineNo + 1);
  }
  return root;
}

function stripComment(line) {
  let inString = false;
  let escape = false;
  let out = '';
  for (let i = 0; i < line.length; i += 1) {
    const ch = line[i];
    if (escape) {
      out += ch;
      escape = false;
      continue;
    }
    if (ch === '\\' && inString) {
      out += ch;
      escape = true;
      continue;
    }
    if (ch === '"') {
      inString = !inString;
      out += ch;
      continue;
    }
    if (ch === '#' && !inString) break;
    out += ch;
  }
  return out;
}

function bracketBalance(line) {
  // Counts `{`/`[` and `}`/`]` ignoring strings.
  let inString = false;
  let escape = false;
  let depth = 0;
  for (let i = 0; i < line.length; i += 1) {
    const ch = line[i];
    if (escape) { escape = false; continue; }
    if (inString) {
      if (ch === '\\') escape = true;
      else if (ch === '"') inString = false;
      continue;
    }
    if (ch === '"') { inString = true; continue; }
    if (ch === '[' || ch === '{') depth += 1;
    else if (ch === ']' || ch === '}') depth -= 1;
  }
  return depth;
}

function foldContinuations(rawLines) {
  const out = [];
  let buffer = '';
  let bufferOpen = false;
  for (const raw of rawLines) {
    const stripped = stripComment(raw);
    // `[table]` / `[[table]]` headers must be evaluated on their own line.
    // The bracket-balance counter cannot distinguish a header `[x]` from
    // an inline array `[1,2]`, so we only fold when there's already an
    // unbalanced opener queued OR when the current line itself opens
    // (and does not close) an inline structure that starts after an `=`.
    if (!bufferOpen) {
      // Only consider folding lines that contain an `=` (key/value lines).
      // Headers and section starts do not participate.
      const eq = findTopLevelEquals(stripped);
      const balance = bracketBalance(stripped);
      if (eq >= 0 && balance > 0) {
        buffer = stripped;
        bufferOpen = true;
        continue;
      }
      out.push(stripped);
      continue;
    }
    buffer += ' ' + stripped;
    if (bracketBalance(buffer) <= 0) {
      out.push(buffer);
      buffer = '';
      bufferOpen = false;
    }
  }
  if (bufferOpen) out.push(buffer);
  return out;
}

function findTopLevelEquals(line) {
  let inString = false;
  let escape = false;
  let depth = 0;
  for (let i = 0; i < line.length; i += 1) {
    const ch = line[i];
    if (escape) {
      escape = false;
      continue;
    }
    if (inString) {
      if (ch === '\\') escape = true;
      else if (ch === '"') inString = false;
      continue;
    }
    if (ch === '"') {
      inString = true;
      continue;
    }
    if (ch === '[' || ch === '{') depth += 1;
    else if (ch === ']' || ch === '}') depth -= 1;
    else if (ch === '=' && depth === 0) return i;
  }
  return -1;
}

function splitDottedKey(key) {
  return key.split('.').map((segment) => {
    const s = segment.trim();
    if (s.startsWith('"') && s.endsWith('"')) return s.slice(1, -1);
    return s;
  });
}

function enterTable(root, key) {
  const parts = splitDottedKey(key);
  let cursor = root;
  for (const part of parts) {
    if (!Object.prototype.hasOwnProperty.call(cursor, part)) {
      cursor[part] = {};
    }
    if (Array.isArray(cursor[part])) {
      cursor = cursor[part][cursor[part].length - 1];
    } else {
      cursor = cursor[part];
    }
  }
  return cursor;
}

function enterArrayOfTables(root, key) {
  const parts = splitDottedKey(key);
  let cursor = root;
  for (let i = 0; i < parts.length - 1; i += 1) {
    const part = parts[i];
    if (!Object.prototype.hasOwnProperty.call(cursor, part)) {
      cursor[part] = {};
    }
    if (Array.isArray(cursor[part])) {
      cursor = cursor[part][cursor[part].length - 1];
    } else {
      cursor = cursor[part];
    }
  }
  const last = parts[parts.length - 1];
  if (!Object.prototype.hasOwnProperty.call(cursor, last)) {
    cursor[last] = [];
  }
  if (!Array.isArray(cursor[last])) {
    throw new Error(`TOML parse error: [[${key}]] conflicts with existing table`);
  }
  const entry = {};
  cursor[last].push(entry);
  return entry;
}

function assignKey(cursor, keyPart, value, lineNo) {
  const parts = splitDottedKey(keyPart);
  let target = cursor;
  for (let i = 0; i < parts.length - 1; i += 1) {
    const part = parts[i];
    if (!Object.prototype.hasOwnProperty.call(target, part)) {
      target[part] = {};
    }
    target = target[part];
  }
  const last = parts[parts.length - 1];
  if (Object.prototype.hasOwnProperty.call(target, last)) {
    throw new Error(`TOML parse error: duplicate key '${keyPart}' on line ${lineNo}`);
  }
  target[last] = value;
}

function parseValue(input, lineNo) {
  const text = input.trim();
  if (text === '') {
    throw new Error(`TOML parse error: empty value on line ${lineNo}`);
  }
  if (text[0] === '"') {
    const { value, rest } = parseString(text);
    if (rest.trim() !== '') {
      throw new Error(`TOML parse error: trailing content after string on line ${lineNo}`);
    }
    return value;
  }
  if (text[0] === '{') return parseInlineTable(text, lineNo).value;
  if (text[0] === '[') return parseInlineArray(text, lineNo).value;
  if (text === 'true') return true;
  if (text === 'false') return false;
  if (/^-?\d+$/.test(text)) return Number.parseInt(text, 10);
  if (/^-?\d+\.\d+$/.test(text)) return Number.parseFloat(text);
  throw new Error(`TOML parse error: unrecognized value '${text}' on line ${lineNo}`);
}

function parseString(text) {
  if (text[0] !== '"') {
    throw new Error(`TOML parse error: expected string at '${text.slice(0, 16)}'`);
  }
  let i = 1;
  let value = '';
  while (i < text.length) {
    const ch = text[i];
    if (ch === '\\') {
      const next = text[i + 1];
      switch (next) {
        case 'n': value += '\n'; break;
        case 't': value += '\t'; break;
        case 'r': value += '\r'; break;
        case '"': value += '"'; break;
        case '\\': value += '\\'; break;
        case '/': value += '/'; break;
        default:
          throw new Error(`TOML parse error: unsupported escape \\${next}`);
      }
      i += 2;
      continue;
    }
    if (ch === '"') {
      return { value, rest: text.slice(i + 1) };
    }
    value += ch;
    i += 1;
  }
  throw new Error('TOML parse error: unterminated string');
}

function parseInlineArray(text, lineNo) {
  if (text[0] !== '[') {
    throw new Error(`TOML parse error: expected '[' at '${text.slice(0, 16)}'`);
  }
  let i = 1;
  const out = [];
  while (i < text.length) {
    while (i < text.length && /\s/.test(text[i])) i += 1;
    if (text[i] === ']') {
      return { value: out, rest: text.slice(i + 1) };
    }
    const slice = text.slice(i);
    const item = parseInlineValue(slice, lineNo);
    out.push(item.value);
    i += slice.length - item.rest.length;
    while (i < text.length && /\s/.test(text[i])) i += 1;
    if (text[i] === ',') {
      i += 1;
      continue;
    }
    if (text[i] === ']') {
      return { value: out, rest: text.slice(i + 1) };
    }
  }
  throw new Error(`TOML parse error: unterminated array on line ${lineNo}`);
}

function parseInlineTable(text, lineNo) {
  if (text[0] !== '{') {
    throw new Error(`TOML parse error: expected '{' at '${text.slice(0, 16)}'`);
  }
  let i = 1;
  const obj = {};
  while (i < text.length) {
    while (i < text.length && /\s/.test(text[i])) i += 1;
    if (text[i] === '}') {
      return { value: obj, rest: text.slice(i + 1) };
    }
    // Read key (bare or quoted).
    let key;
    if (text[i] === '"') {
      const { value, rest } = parseString(text.slice(i));
      key = value;
      i += text.slice(i).length - rest.length;
    } else {
      let end = i;
      while (end < text.length && /[A-Za-z0-9_\-.]/.test(text[end])) end += 1;
      key = text.slice(i, end);
      i = end;
    }
    if (!key) {
      throw new Error(`TOML parse error: expected key in inline table on line ${lineNo}`);
    }
    while (i < text.length && /\s/.test(text[i])) i += 1;
    if (text[i] !== '=') {
      throw new Error(`TOML parse error: expected '=' after '${key}' in inline table on line ${lineNo}`);
    }
    i += 1;
    while (i < text.length && /\s/.test(text[i])) i += 1;
    const valueSlice = text.slice(i);
    const valueParsed = parseInlineValue(valueSlice, lineNo);
    assignKey(obj, key, valueParsed.value, lineNo);
    i += valueSlice.length - valueParsed.rest.length;
    while (i < text.length && /\s/.test(text[i])) i += 1;
    if (text[i] === ',') {
      i += 1;
      continue;
    }
    if (text[i] === '}') {
      return { value: obj, rest: text.slice(i + 1) };
    }
  }
  throw new Error(`TOML parse error: unterminated inline table on line ${lineNo}`);
}

function parseInlineValue(text, lineNo) {
  const t = text.replace(/^\s+/, '');
  const consumed = text.length - t.length;
  if (t[0] === '"') {
    const { value, rest } = parseString(t);
    return { value, rest };
  }
  if (t[0] === '{') {
    const parsed = parseInlineTable(t, lineNo);
    return parsed;
  }
  if (t[0] === '[') {
    const parsed = parseInlineArray(t, lineNo);
    return parsed;
  }
  // Bare literal: number / bool. Consume until comma/closing bracket.
  let end = 0;
  let depth = 0;
  while (end < t.length) {
    const ch = t[end];
    if ((ch === ',' || ch === ']' || ch === '}') && depth === 0) break;
    if (ch === '[' || ch === '{') depth += 1;
    if (ch === ']' || ch === '}') depth -= 1;
    end += 1;
  }
  const literal = t.slice(0, end).trim();
  if (literal === 'true') return { value: true, rest: t.slice(end) };
  if (literal === 'false') return { value: false, rest: t.slice(end) };
  if (/^-?\d+$/.test(literal)) return { value: Number.parseInt(literal, 10), rest: t.slice(end) };
  if (/^-?\d+\.\d+$/.test(literal)) return { value: Number.parseFloat(literal), rest: t.slice(end) };
  throw new Error(`TOML parse error: unrecognized inline value '${literal}' on line ${lineNo} (consumed ${consumed} ws chars)`);
}

// ---------------------------------------------------------------------------
// Placeholder substitution.
// ---------------------------------------------------------------------------

function substitutePlaceholders(value, ctx) {
  if (typeof value === 'string') {
    return value
      .replaceAll('${workspace}', ctx.workspace)
      .replaceAll('${missing_path}', ctx.missingPath)
      .replaceAll('${root_escape_path}', ctx.rootEscapePath)
      .replaceAll('${session_id}', ctx.sessionId)
      .replaceAll('${profile_id}', ctx.profileId)
      .replaceAll('${email}', ctx.email);
  }
  if (Array.isArray(value)) return value.map((entry) => substitutePlaceholders(entry, ctx));
  if (value && typeof value === 'object') {
    const out = {};
    for (const [k, v] of Object.entries(value)) {
      out[k] = substitutePlaceholders(v, ctx);
    }
    return out;
  }
  return value;
}

// ---------------------------------------------------------------------------
// Stdio JSON-RPC client.
// ---------------------------------------------------------------------------

function appendJsonl(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.appendFileSync(file, `${JSON.stringify(redactSecrets(value))}\n`);
}

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(redactSecrets(value), null, 2)}\n`);
}

function sensitiveKey(key) {
  return /(?:api[_-]?key|secret|token|password|authorization|auth[_-]?header)$/i.test(key)
    || /(?:API_KEY|SECRET|TOKEN|PASSWORD)$/i.test(key);
}

function redactSecrets(value) {
  if (Array.isArray(value)) return value.map(redactSecrets);
  if (!value || typeof value !== 'object') return value;
  const out = {};
  for (const [key, child] of Object.entries(value)) {
    out[key] = sensitiveKey(key) && typeof child === 'string' ? '<redacted>' : redactSecrets(child);
  }
  return out;
}

function getByPath(obj, dottedPath) {
  const parts = dottedPath.split('.');
  let cursor = obj;
  for (const part of parts) {
    if (cursor == null || typeof cursor !== 'object') return undefined;
    cursor = cursor[part];
  }
  return cursor;
}

export class StdioClient {
  constructor({ octosBin, dataDir, workspace, repoRoot, stderrLog, transcriptLog, timeoutMs }) {
    this.timeoutMs = timeoutMs;
    this.transcriptLog = transcriptLog;
    this.stderrText = '';
    this.pending = new Map();
    this.notifications = [];
    this.nextSeq = 0;
    this.child = spawn(
      octosBin,
      ['serve', '--stdio', '--data-dir', dataDir, '--cwd', workspace],
      {
        cwd: repoRoot,
        env: {
          ...process.env,
          RUST_BACKTRACE: process.env.RUST_BACKTRACE || '1',
          // Defensive: block accidental OTP wiring during onboarding probes.
          OCTOS_DISABLE_SMTP: process.env.OCTOS_DISABLE_SMTP || '1',
        },
        stdio: ['pipe', 'pipe', 'pipe'],
      },
    );
    this.child.stderr.on('data', (chunk) => {
      const text = chunk.toString();
      this.stderrText += text;
      fs.appendFileSync(stderrLog, text);
    });
    this.rl = readline.createInterface({ input: this.child.stdout });
    this.rl.on('line', (line) => this._onLine(line));
    // Codex P2 follow-up: when the spawned `octos serve` crashes,
    // panics, or the wrong binary is at OCTOS_BIN, the child can
    // exit with pending RPCs still in flight. Track exit so we can
    // reject pending requests instead of waiting for them to time
    // out (or worse, propagating an unhandled EPIPE on stdin —
    // which would terminate Node and leave no scenario.json or
    // summary.json artifact behind).
    this.exitInfo = null;
    this.exited = new Promise((resolve) => {
      this.child.once('exit', (code, signal) => {
        this.exitInfo = { code, signal };
        this._failPending(new Error(
          `octos serve exited unexpectedly (code=${code}, signal=${signal})`,
        ));
        resolve(this.exitInfo);
      });
    });
    // Defensive: stdin EPIPE arrives as an `error` event. Without a
    // handler this terminates Node. With one we just record it; the
    // child-exit handler above does the actual cleanup.
    this.child.stdin.on('error', (err) => {
      appendJsonl(this.transcriptLog, {
        direction: 'stdin_error',
        code: err.code,
        message: err.message,
      });
    });
    this.spawned = new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('octos serve --stdio spawn timeout')), 10_000);
      this.child.once('spawn', () => { clearTimeout(timer); resolve(); });
      this.child.once('error', reject);
    });
  }

  _failPending(err) {
    const pending = this.pending;
    this.pending = new Map();
    for (const [, request] of pending) {
      try { request.resolve({ jsonrpc: '2.0', id: null, error: {
        code: -32000,
        message: err.message,
        data: { kind: 'backend_exited' },
      } }); } catch { /* ignore */ }
    }
  }

  _onLine(line) {
    let frame;
    try {
      frame = JSON.parse(line);
    } catch {
      appendJsonl(this.transcriptLog, { direction: 'server_to_client_non_json', line });
      return;
    }
    appendJsonl(this.transcriptLog, { direction: 'server_to_client', frame });
    if (frame && Object.prototype.hasOwnProperty.call(frame, 'id') && frame.id != null) {
      const request = this.pending.get(String(frame.id));
      if (request) {
        this.pending.delete(String(frame.id));
        request.resolve(frame);
      }
      return;
    }
    if (frame?.method) this.notifications.push(frame);
  }

  rpc(method, params = {}) {
    const id = `m22-matrix-${++this.nextSeq}-${crypto.randomBytes(3).toString('hex')}`;
    const frame = { jsonrpc: '2.0', id, method, params };
    appendJsonl(this.transcriptLog, { direction: 'client_to_server', frame });
    // Codex P2 follow-up: if the backend already exited, don't write
    // to a dead stdin — emit a typed `backend_exited` error instead
    // so the scenario records a failed step and the run summary is
    // still written.
    if (this.exitInfo) {
      return Promise.resolve({ jsonrpc: '2.0', id, error: {
        code: -32000,
        message: `octos serve has exited (code=${this.exitInfo.code}, signal=${this.exitInfo.signal})`,
        data: { kind: 'backend_exited' },
      } });
    }
    try {
      this.child.stdin.write(`${JSON.stringify(frame)}\n`);
    } catch (err) {
      // EPIPE between the exit-event firing and us reaching this
      // line: synthesize the same typed error so the scenario doesn't
      // hang waiting for a response that will never come.
      return Promise.resolve({ jsonrpc: '2.0', id, error: {
        code: -32000,
        message: `stdin write failed (${err.code || err.message})`,
        data: { kind: 'backend_exited' },
      } });
    }
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`RPC timeout for ${method}`));
      }, this.timeoutMs);
      this.pending.set(id, {
        method,
        resolve: (response) => {
          clearTimeout(timer);
          resolve(response);
        },
      });
    });
  }

  async close() {
    try { this.child.stdin.end(); } catch { /* ignore */ }
    setTimeout(() => {
      if (!this.child.killed) this.child.kill('SIGTERM');
    }, 200);
    await this.exited.catch(() => undefined);
  }
}

// ---------------------------------------------------------------------------
// Step expectations (light, shape-only). The full validator suite lands
// in PR #2 — see follow-up issues filed alongside this PR.
// ---------------------------------------------------------------------------

function evaluateExpectations(expect, frame) {
  const errors = [];
  if (!expect) {
    if (frame.error) {
      errors.push({ kind: 'unexpected_error', error: frame.error });
    }
    return errors;
  }
  const expectedOk = expect.ok !== false;
  const observedOk = !frame.error;
  if (expectedOk !== observedOk) {
    errors.push({
      kind: 'ok_mismatch',
      expected_ok: expectedOk,
      observed_ok: observedOk,
      error: frame.error || null,
    });
  }
  if (!expectedOk && expect.error_kind) {
    const observedKind = frame.error?.data?.kind || null;
    if (observedKind !== expect.error_kind) {
      errors.push({
        kind: 'error_kind_mismatch',
        expected: expect.error_kind,
        observed: observedKind,
        full_error: frame.error,
      });
    }
  }
  if (observedOk && Array.isArray(expect.result_has)) {
    for (const dotted of expect.result_has) {
      const value = getByPath(frame.result, dotted);
      if (value === undefined) {
        errors.push({ kind: 'missing_result_key', path: dotted });
      }
    }
  }
  return errors;
}

// ---------------------------------------------------------------------------
// Scenario execution.
// ---------------------------------------------------------------------------

async function runScenario(scenario, ctx, repoRoot, octosBin, runTimeoutMs) {
  const scenarioDir = path.join(ctx.runRoot, scenario.name);
  const dataDir = path.join(scenarioDir, 'data');
  const workspace = path.join(scenarioDir, 'workspace');
  const stderrLog = path.join(scenarioDir, 'server-stderr.log');
  const transcriptLog = path.join(scenarioDir, 'rpc-transcript.jsonl');
  const resultPath = path.join(scenarioDir, 'result.json');

  fs.mkdirSync(workspace, { recursive: true });
  fs.mkdirSync(dataDir, { recursive: true });
  // Seed a workspace fixture so directory_is_writable() probes succeed.
  fs.writeFileSync(path.join(workspace, '.matrix-fixture'), 'm22 matrix workspace probe seed\n');
  // Pre-create a closed log file so artifact listings are stable even if
  // the scenario never produces stderr or transcript output.
  for (const log of [stderrLog, transcriptLog]) fs.closeSync(fs.openSync(log, 'a'));

  if (scenario.skip_reason) {
    const summary = {
      name: scenario.name,
      tier: scenario.tier,
      transport: scenario.transport,
      status: 'skipped',
      skip_reason: scenario.skip_reason,
      validators: scenario.validators || [],
      steps: [],
    };
    writeJson(resultPath, summary);
    return summary;
  }

  const localCtx = {
    workspace,
    missingPath: path.join(scenarioDir, 'does-not-exist-matrix-fast'),
    rootEscapePath: '/etc/octos-matrix-fast-not-a-real-dir-1056',
    sessionId: `${scenario.name}:local:m22-matrix-fast-${ctx.runStamp}`,
    profileId: `m22-${scenario.name}`.toLowerCase().replace(/[^a-z0-9]+/g, '-').slice(0, 40),
    email: `${scenario.name}@m22-matrix.test`,
  };

  const client = new StdioClient({
    octosBin,
    dataDir,
    workspace,
    repoRoot,
    stderrLog,
    transcriptLog,
    timeoutMs: runTimeoutMs,
  });

  const stepResults = [];
  let scenarioOk = true;
  let fatal = null;

  try {
    await client.spawned;
    for (const step of scenario.steps || []) {
      const params = substitutePlaceholders(step.params || {}, localCtx);
      let frame;
      try {
        frame = await client.rpc(step.rpc, params);
      } catch (rpcErr) {
        scenarioOk = false;
        stepResults.push({
          id: step.id,
          rpc: step.rpc,
          ok: false,
          rpc_failure: String(rpcErr?.message || rpcErr),
        });
        continue;
      }
      const errors = evaluateExpectations(step.expect, frame);
      const ok = errors.length === 0;
      if (!ok) scenarioOk = false;
      stepResults.push({
        id: step.id,
        rpc: step.rpc,
        ok,
        observed_ok: !frame.error,
        result_keys: frame.result && typeof frame.result === 'object' ? Object.keys(frame.result) : null,
        error: frame.error || null,
        expectation_errors: errors,
      });
    }
  } catch (err) {
    fatal = String(err?.stack || err);
    scenarioOk = false;
  } finally {
    await client.close();
  }

  const summary = {
    name: scenario.name,
    tier: scenario.tier,
    transport: scenario.transport,
    status: scenarioOk && !fatal ? 'passed' : 'failed',
    fatal,
    validators: scenario.validators || [],
    steps: stepResults,
    artifacts: {
      data_dir: dataDir,
      workspace,
      stderr_log: stderrLog,
      transcript_log: transcriptLog,
      result_json: resultPath,
    },
  };
  writeJson(resultPath, summary);
  return summary;
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

async function main() {
  const args = parseCli(process.argv.slice(2));
  if (!args.pack || !args.tier) {
    console.error('--pack and --tier are required.');
    printUsageAndExit(2);
  }
  const validTiers = ['fast', 'local', 'release'];
  if (!validTiers.includes(args.tier)) {
    console.error(`Invalid --tier ${args.tier}. Expected one of ${validTiers.join(', ')}.`);
    process.exit(2);
  }

  const repoRoot = path.resolve(import.meta.dirname, '..', '..');
  const manifestPath = args.manifest
    ? path.resolve(args.manifest)
    : path.join(repoRoot, 'e2e', 'matrix', `${args.pack}.toml`);
  if (!fs.existsSync(manifestPath)) {
    console.error(`Manifest not found: ${manifestPath}`);
    process.exit(2);
  }

  const manifestSource = fs.readFileSync(manifestPath, 'utf8');
  const manifest = parseToml(manifestSource);
  const packName = manifest?.pack?.name || args.pack;
  if (packName !== args.pack) {
    console.error(
      `Manifest declares pack "${packName}" but --pack was "${args.pack}".`,
    );
    process.exit(2);
  }
  const scenarios = (manifest.scenarios || []).filter(
    (scenario) => scenario.tier === args.tier,
  );
  if (scenarios.length === 0) {
    console.error(`No scenarios match pack=${args.pack} tier=${args.tier}.`);
    process.exit(2);
  }

  const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..+/, 'Z');
  const runRoot = process.env.OCTOS_MATRIX_DIR
    || path.join(repoRoot, 'e2e', 'test-results-matrix', `${args.pack}-${args.tier}`, stamp);
  fs.mkdirSync(runRoot, { recursive: true });
  const octosBin = process.env.OCTOS_BIN || path.join(repoRoot, 'target', 'debug', 'octos');
  if (!fs.existsSync(octosBin)) {
    const failure = {
      ok: false,
      pack: args.pack,
      tier: args.tier,
      run_root: runRoot,
      error: `octos binary not found at ${octosBin}. Build it (\`cargo build -p octos-cli --features api\`) or set OCTOS_BIN.`,
    };
    writeJson(path.join(runRoot, 'summary.json'), failure);
    console.error(JSON.stringify(failure, null, 2));
    process.exit(2);
  }

  const rpcTimeoutMs = Number(process.env.OCTOS_MATRIX_RPC_TIMEOUT_MS || 10_000);
  const ctx = { runRoot, runStamp: stamp };

  const startedAt = new Date().toISOString();
  const scenarioSummaries = [];
  let passed = 0;
  let failed = 0;
  let skipped = 0;
  for (const scenario of scenarios) {
    const summary = await runScenario(scenario, ctx, repoRoot, octosBin, rpcTimeoutMs);
    scenarioSummaries.push(summary);
    if (summary.status === 'passed') passed += 1;
    else if (summary.status === 'skipped') skipped += 1;
    else failed += 1;
  }
  const finishedAt = new Date().toISOString();

  const summary = {
    ok: failed === 0,
    pack: args.pack,
    tier: args.tier,
    transport: 'stdio',
    contract: manifest?.pack?.contract || null,
    issue: manifest?.pack?.issue || null,
    started_at: startedAt,
    finished_at: finishedAt,
    run_root: runRoot,
    manifest_path: manifestPath,
    host: os.hostname(),
    counts: { passed, failed, skipped, total: scenarios.length },
    scenarios: scenarioSummaries,
  };
  writeJson(path.join(runRoot, 'summary.json'), summary);
  console.log(JSON.stringify(summary, null, 2));
  if (!summary.ok) process.exitCode = 1;
}

// Codex P2 follow-up: only run `main()` when this file is invoked
// as a CLI script. Importing it from a regression test (to exercise
// StdioClient against a fake backend) must not kick off the full
// runner pipeline.
const __thisFile = fileURLToPath(import.meta.url);
if (process.argv[1] && path.resolve(process.argv[1]) === __thisFile) {
  main().catch((err) => {
    console.error(err?.stack || String(err));
    process.exit(1);
  });
}
