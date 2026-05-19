#!/usr/bin/env node

import fs from 'node:fs';
import path from 'node:path';

const VALIDATION_SCHEMA = 'octos.ux.validation.v1';
const ARTIFACT_ABI = 'octos.ux.artifacts.v1';
const REQUIRED_ARTIFACTS = [
  'scenario.json',
  'summary.json',
  'launch-command.txt',
  'terminal-size.json',
  'input-replay.log',
  'appui-transcript.jsonl',
  'server.log',
  'tui-capture.txt',
  'runtime-policy-stamp.json',
  'validation.json',
];
const INPUT_ARTIFACTS = REQUIRED_ARTIFACTS.filter((name) => name !== 'validation.json');
const JSON_ARTIFACTS = new Set([
  'scenario.json',
  'summary.json',
  'terminal-size.json',
  'runtime-policy-stamp.json',
]);
const VALID_SUMMARY_STATUSES = new Set(['passed', 'failed', 'blocked', 'skipped', 'quarantined']);
const VALID_DIRECTIONS = new Set([
  'client_to_server',
  'server_to_client',
  'server_to_client_non_json',
  'tx',
  'rx',
]);
const ANSI_RE = /\x1b\[[0-9;?]*[ -/]*[@-~]/g;

const KNOWN_CAPTURE_BUG_PATTERNS = [
  {
    id: 'split_work_progress_pane',
    regex: /^\u250c(Work|Progress)/,
    detail: 'split Work/Progress pane rendered in normal chat layout',
  },
  {
    id: 'turn_plan_or_workspace_clarifier_leak',
    regex: /Plan rounds|Current round:|Is this a path within the current project\/workspace|Or is it a system path outside the workspace|Did you mean a different directory/,
    detail: 'turn planning or workspace clarifier rows leaked into the chat surface',
  },
  {
    id: 'bottom_state_spinner',
    regex: /^ state .*[\u25d0\u25d1\u25d2\u25d3]/,
    detail: 'bottom state line rendered an animated spinner',
  },
  {
    id: 'removed_pane_border_overlap',
    regex: /^\u250c(Work|Progress).*\u203a|^\u250cWor \u203a|^\u250cProgress.*\u203a/,
    detail: 'input text overlapped a removed Work/Progress pane border',
  },
  {
    id: 'markdown_control_text_leak',
    regex: /\u2022 ####|What I \*can\* access|\[x\] Point me|\[x\] Or share/,
    detail: 'markdown control text leaked into rendered assistant text',
  },
  {
    id: 'appui_error_text_visible',
    regex: /malformed_json|session\.workspace_cwd|requires protocol|provider is unavailable|Task Error|app-ui error|unavailable: AppUI capabilities/,
    detail: 'AppUI or onboarding error text is visible in the capture',
  },
  {
    id: 'tmux_session_missing',
    regex: /tmux session not running:/,
    detail: 'tmux capture shows the real TUI pane was missing',
  },
  {
    id: 'octos_tui_exited',
    regex: /octos-tui exited with status/,
    detail: 'tmux capture shows the real octos-tui process exited before validation',
  },
  {
    id: 'ui_protocol_connect_failed',
    regex: /failed to connect UI protocol endpoint|Connection refused \(os error/,
    detail: 'TUI failed to connect to the AppUI protocol endpoint',
  },
];

const SERVER_DROPPED_TURN_PATTERN =
  /lifecycle notification not delivered.*turn\/completed|writer channel full for lifecycle frame|lifecycle ws send failed; aborting connection/;
const CAPTURE_STUCK_RUNNING_PATTERN = /Task Working|Progress .*Thinking|state .*Working/;
const ACCEPTABLE_IDEMPOTENT_ERROR_CODES = new Set([
  'profile_exists',
  'already_exists',
  'conflict',
  'profile_local_collision',
]);

function usage() {
  return [
    'Usage: ux:tmux:validate <artifact-dir>',
    '',
    'Equivalent command without a package script:',
    '  node e2e/scripts/ux-tmux-validate.mjs <artifact-dir>',
  ].join('\n');
}

function stripAnsi(text) {
  return text.replace(ANSI_RE, '').replace(/\r/g, '');
}

function readText(file) {
  try {
    return { ok: true, text: stripAnsi(fs.readFileSync(file, 'utf8')) };
  } catch (error) {
    return { ok: false, text: '', error: error.message };
  }
}

function readJson(file) {
  const text = readText(file);
  if (!text.ok) return { ok: false, error: text.error };
  try {
    const value = JSON.parse(text.text);
    if (!isPlainObject(value)) {
      return { ok: false, error: 'expected top-level JSON object' };
    }
    return { ok: true, value };
  } catch (error) {
    return { ok: false, error: error.message };
  }
}

function isPlainObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function jsonRpcErrorCode(error) {
  if (!isPlainObject(error)) return '';
  return String(error.data?.kind || error.data?.code || error.code || error.message || '');
}

function isAcceptableJsonRpcError(frame) {
  if (!isPlainObject(frame) || !isPlainObject(frame.error)) return false;
  const code = jsonRpcErrorCode(frame.error);
  if (ACCEPTABLE_IDEMPOTENT_ERROR_CODES.has(code)) return true;
  const message = String(frame.error.message || '').toLowerCase();
  return [...ACCEPTABLE_IDEMPOTENT_ERROR_CODES].some((known) => message.includes(known));
}

function artifactPath(artifactDir, name) {
  return path.join(artifactDir, name);
}

function lineMatches(text, regex) {
  const lines = text.split('\n');
  for (let index = 0; index < lines.length; index += 1) {
    if (regex.test(lines[index])) {
      return {
        line: index + 1,
        preview: lines[index].trim().slice(0, 160),
      };
    }
  }
  return null;
}

function captureLines(text) {
  const lines = text.split('\n');
  while (lines.length > 0 && lines[lines.length - 1] === '') {
    lines.pop();
  }
  return lines;
}

function codepointLength(text) {
  return [...text].length;
}

function sortedStrings(values) {
  return [...new Set(values)].sort();
}

function makeCheck(id, passed, detail, evidence) {
  return {
    id,
    status: passed ? 'passed' : 'failed',
    detail,
    evidence,
  };
}

function validateScenarioJson(value) {
  const problems = [];
  if (typeof value.schema !== 'string' || value.schema.length === 0) {
    problems.push('scenario.json schema must be a non-empty string');
  }
  if (typeof value.id !== 'string' && typeof value.name !== 'string') {
    problems.push('scenario.json must include string id or name');
  }
  if (value.artifact_abi !== undefined && value.artifact_abi !== ARTIFACT_ABI) {
    problems.push(`scenario.json artifact_abi must be ${ARTIFACT_ABI}`);
  }
  if (value.required_artifacts !== undefined) {
    if (!Array.isArray(value.required_artifacts)) {
      problems.push('scenario.json required_artifacts must be an array when present');
    } else {
      const missingNames = REQUIRED_ARTIFACTS.filter((name) => !value.required_artifacts.includes(name));
      if (missingNames.length > 0) {
        problems.push(`scenario.json required_artifacts omits ${missingNames.join(', ')}`);
      }
    }
  }
  return problems;
}

function validateSummaryJson(value) {
  const problems = [];
  if (typeof value.schema !== 'string' || value.schema.length === 0) {
    problems.push('summary.json schema must be a non-empty string');
  }
  if (!VALID_SUMMARY_STATUSES.has(value.status)) {
    problems.push('summary.json status must be passed, failed, blocked, skipped, or quarantined');
  }
  if (value.artifacts !== undefined && !isPlainObject(value.artifacts)) {
    problems.push('summary.json artifacts must be an object when present');
  } else if (isPlainObject(value.artifacts)) {
    const missing = REQUIRED_ARTIFACTS.filter((name) => !Object.prototype.hasOwnProperty.call(value.artifacts, name));
    if (missing.length > 0) {
      problems.push(`summary.json artifacts omits ${missing.join(', ')}`);
    }
  }
  return problems;
}

function validateRuntimePolicyStampJson(value) {
  const problems = [];
  if (typeof value.schema !== 'string' || value.schema.length === 0) {
    problems.push('runtime-policy-stamp.json schema must be a non-empty string');
  }
  if (!isPlainObject(value.stamp) && !isPlainObject(value.runtime_policy_stamp)) {
    problems.push('runtime-policy-stamp.json must include stamp or runtime_policy_stamp object');
  }
  return problems;
}

function validateTerminalSizeJson(value) {
  const problems = [];
  if (value.schema !== 'octos.ux.terminal_size.v1') {
    problems.push('terminal-size.json schema must be octos.ux.terminal_size.v1');
  }
  if (!Number.isInteger(value.cols) || value.cols <= 0) {
    problems.push('terminal-size.json cols must be a positive integer');
  }
  if (!Number.isInteger(value.rows) || value.rows <= 0) {
    problems.push('terminal-size.json rows must be a positive integer');
  }
  return problems;
}

function validateExistingValidationJson(file) {
  if (!fs.existsSync(file)) return [];
  const parsed = readJson(file);
  if (!parsed.ok) return [`validation.json must be parseable JSON object: ${parsed.error}`];
  const value = parsed.value;
  const problems = [];
  if (value.schema !== VALIDATION_SCHEMA) {
    problems.push(`validation.json schema must be ${VALIDATION_SCHEMA}`);
  }
  if (!['passed', 'failed'].includes(value.status)) {
    problems.push('validation.json status must be passed or failed');
  }
  if (!Array.isArray(value.checks)) {
    problems.push('validation.json checks must be an array');
  }
  return problems;
}

function validateJsonArtifact(name, value) {
  if (name === 'scenario.json') return validateScenarioJson(value);
  if (name === 'summary.json') return validateSummaryJson(value);
  if (name === 'terminal-size.json') return validateTerminalSizeJson(value);
  if (name === 'runtime-policy-stamp.json') return validateRuntimePolicyStampJson(value);
  return [];
}

function checkArtifactAbi(artifactDir) {
  const problems = [];
  const directoryExists = fs.existsSync(artifactDir) && fs.statSync(artifactDir).isDirectory();
  if (!directoryExists) {
    return makeCheck(
      'artifact_abi',
      false,
      'artifact directory does not exist or is not a directory',
      REQUIRED_ARTIFACTS,
    );
  }

  for (const name of INPUT_ARTIFACTS) {
    const file = artifactPath(artifactDir, name);
    if (!fs.existsSync(file)) {
      problems.push(`${name} is missing`);
      continue;
    }
    const stat = fs.statSync(file);
    if (!stat.isFile()) {
      problems.push(`${name} is not a regular file`);
      continue;
    }
    if (stat.size === 0 && name !== 'server.log') {
      problems.push(`${name} is empty`);
    }
    if (JSON_ARTIFACTS.has(name)) {
      const parsed = readJson(file);
      if (!parsed.ok) {
        problems.push(`${name} must be parseable JSON object: ${parsed.error}`);
      } else {
        problems.push(...validateJsonArtifact(name, parsed.value));
      }
    }
  }
  problems.push(...validateExistingValidationJson(artifactPath(artifactDir, 'validation.json')));

  return makeCheck(
    'artifact_abi',
    problems.length === 0,
    problems.length === 0
      ? `required ${ARTIFACT_ABI} artifacts are present and schema-shaped; validation.json uses ${VALIDATION_SCHEMA}`
      : `artifact ABI problems: ${problems.join('; ')}`,
    REQUIRED_ARTIFACTS,
  );
}

function parseJsonl(file) {
  const text = readText(file);
  if (!text.ok) return { ok: false, rows: [], errors: [{ line: 0, error: text.error }] };
  const rows = [];
  const errors = [];
  const lines = text.text.split('\n');
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index].trim();
    if (!line) continue;
    try {
      const value = JSON.parse(line);
      if (!isPlainObject(value)) {
        errors.push({ line: index + 1, error: 'expected JSON object' });
      } else {
        rows.push({ line: index + 1, value });
      }
    } catch (error) {
      errors.push({ line: index + 1, error: error.message });
    }
  }
  return { ok: errors.length === 0, rows, errors };
}

function validateFrameShape(row) {
  const { value } = row;
  const errors = [];
  if (value.direction !== undefined && !VALID_DIRECTIONS.has(value.direction)) {
    errors.push(`line ${row.line}: invalid direction ${value.direction}`);
  }
  if (value.direction === 'server_to_client_non_json') {
    if (typeof value.line !== 'string') {
      errors.push(`line ${row.line}: non-json transcript entry must include line string`);
    }
    return errors;
  }
  const frame = normalizeTranscriptFrame(value);
  if (!isPlainObject(frame)) {
    errors.push(`line ${row.line}: frame must be an object`);
    return errors;
  }
  if (frame.jsonrpc !== undefined && frame.jsonrpc !== '2.0') {
    errors.push(`line ${row.line}: frame jsonrpc must be 2.0 when present`);
  }
  const hasMethod = typeof frame.method === 'string' && frame.method.length > 0;
  const hasResult = Object.prototype.hasOwnProperty.call(frame, 'result');
  const hasError = Object.prototype.hasOwnProperty.call(frame, 'error');
  if (!hasMethod && !hasResult && !hasError) {
    errors.push(`line ${row.line}: frame must contain method, result, or error`);
  }
  if ((hasResult || hasError) && !Object.prototype.hasOwnProperty.call(frame, 'id')) {
    errors.push(`line ${row.line}: response frame must include id`);
  }
  return errors;
}

function normalizeTranscriptFrame(value) {
  if (isPlainObject(value.frame)) return value.frame;
  if (value.direction === 'tx' && typeof value.method === 'string') {
    return {
      jsonrpc: '2.0',
      id: value.id,
      method: value.method,
      params: value.params,
    };
  }
  if (value.direction === 'rx') {
    if (value.notification === true && typeof value.method === 'string') {
      return {
        jsonrpc: value.jsonrpc ?? '2.0',
        method: value.method,
        params: value.params,
      };
    }
    if (isPlainObject(value.error)) {
      return {
        jsonrpc: '2.0',
        id: value.id,
        error: value.error,
      };
    }
    if (Object.prototype.hasOwnProperty.call(value, 'result') || Object.prototype.hasOwnProperty.call(value, 'ok')) {
      return {
        jsonrpc: '2.0',
        id: value.id,
        result: Object.prototype.hasOwnProperty.call(value, 'result')
          ? value.result
          : { ok: value.ok },
      };
    }
  }
  return null;
}

function checkAppuiTranscriptParseable(artifactDir) {
  const parsed = parseJsonl(artifactPath(artifactDir, 'appui-transcript.jsonl'));
  const shapeErrors = parsed.rows.flatMap((row) => validateFrameShape(row));
  const jsonErrors = parsed.errors.map((entry) => `line ${entry.line}: ${entry.error}`);
  const frameRows = parsed.rows
    .map((row) => ({ row, frame: normalizeTranscriptFrame(row.value) }))
    .filter((entry) => isPlainObject(entry.frame));
  const methodNames = sortedStrings(
    frameRows
      .map((entry) => entry.frame.method)
      .filter((method) => typeof method === 'string' && method.length > 0),
  );
  const responseCount = frameRows.filter((entry) => (
    Object.prototype.hasOwnProperty.call(entry.frame, 'result')
      || Object.prototype.hasOwnProperty.call(entry.frame, 'error')
  )).length;
  const problems = [
    ...jsonErrors,
    ...shapeErrors,
  ];
  if (parsed.rows.length === 0) {
    problems.push('appui-transcript.jsonl has no JSONL entries');
  }
  if (methodNames.length === 0) {
    problems.push('appui-transcript.jsonl has no AppUI method frames');
  }
  if (responseCount === 0) {
    problems.push('appui-transcript.jsonl has no JSON-RPC response frames');
  }
  return makeCheck(
    'appui_transcript_parseable',
    problems.length === 0,
    problems.length === 0
      ? `parsed ${parsed.rows.length} JSONL entries; methods=${methodNames.join(', ')}; responses=${responseCount}`
      : `transcript parse problems: ${problems.join('; ')}`,
    ['appui-transcript.jsonl'],
  );
}

function checkRealTmuxEvidence(artifactDir) {
  const parsed = readJson(artifactPath(artifactDir, 'summary.json'));
  if (!parsed.ok) {
    return makeCheck(
      'real_tmux_evidence',
      false,
      `summary.json is not parseable JSON: ${parsed.error}`,
      ['summary.json'],
    );
  }

  const summary = parsed.value;
  if (summary.mode !== 'run' || ['blocked', 'skipped', 'quarantined'].includes(summary.status)) {
    return makeCheck(
      'real_tmux_evidence',
      true,
      `real tmux evidence is not required for mode=${summary.mode ?? '<unset>'} status=${summary.status ?? '<unset>'}`,
      ['summary.json'],
    );
  }

  const problems = [];
  if (summary.status !== 'passed') {
    problems.push(`summary.status must be passed for a real UX gate, got ${summary.status ?? '<unset>'}`);
  }
  if (summary.placeholder_artifacts !== false) {
    problems.push('summary.placeholder_artifacts must be false for a real UX gate');
  }
  if (summary.real_tmux_launched !== true) {
    problems.push('summary.real_tmux_launched must be true for a real UX gate');
  }

  return makeCheck(
    'real_tmux_evidence',
    problems.length === 0,
    problems.length === 0
      ? 'summary confirms this artifact set came from a completed real tmux run'
      : `real tmux evidence problems: ${problems.join('; ')}`,
    ['summary.json'],
  );
}

function checkAppuiTranscriptSemantic(artifactDir) {
  const summary = readJson(artifactPath(artifactDir, 'summary.json'));
  if (!summary.ok) {
    return makeCheck(
      'appui_transcript_semantic',
      false,
      `summary.json is not parseable JSON: ${summary.error}`,
      ['summary.json', 'appui-transcript.jsonl'],
    );
  }
  if (summary.value.mode !== 'run' || ['blocked', 'skipped', 'quarantined'].includes(summary.value.status)) {
    return makeCheck(
      'appui_transcript_semantic',
      true,
      `semantic transcript checks are not required for mode=${summary.value.mode ?? '<unset>'} status=${summary.value.status ?? '<unset>'}`,
      ['summary.json', 'appui-transcript.jsonl'],
    );
  }

  const parsed = parseJsonl(artifactPath(artifactDir, 'appui-transcript.jsonl'));
  const frameRows = parsed.rows
    .map((row) => ({ row, frame: normalizeTranscriptFrame(row.value) }))
    .filter((entry) => isPlainObject(entry.frame));
  const methodNames = new Set(
    frameRows
      .map((entry) => entry.frame.method)
      .filter((method) => typeof method === 'string' && method.length > 0),
  );
  const requiredMethods = [
    'config/capabilities/list',
    'profile/local/create',
    'permission/profile/list',
    'permission/profile/set',
    'session/open',
    'session/status/read',
    'tool/status/list',
  ];
  const requestIds = new Map();
  const responseIds = [];
  const problems = [];

  for (const entry of frameRows) {
    const { frame } = entry;
    if (typeof frame.method === 'string' && Object.prototype.hasOwnProperty.call(frame, 'id')) {
      requestIds.set(String(frame.id), frame.method);
    }
    if (Object.prototype.hasOwnProperty.call(frame, 'result') || Object.prototype.hasOwnProperty.call(frame, 'error')) {
      responseIds.push({
        id: frame.id,
        row: entry.row,
        frame,
        hasError: Object.prototype.hasOwnProperty.call(frame, 'error'),
      });
    }
  }

  for (const method of requiredMethods) {
    if (!methodNames.has(method)) {
      problems.push(`missing required AppUI method ${method}`);
    }
  }
  for (const response of responseIds) {
    const id = String(response.id);
    if (requestIds.size > 0 && !requestIds.has(id)) {
      problems.push(`line ${response.row.line}: response id ${id} has no matching request`);
    }
    if (response.hasError && !isAcceptableJsonRpcError(response.frame)) {
      problems.push(`line ${response.row.line}: transcript contains JSON-RPC error response id ${id}`);
    }
  }
  if (responseIds.length === 0) {
    problems.push('appui-transcript.jsonl has no JSON-RPC response frames');
  }

  return makeCheck(
    'appui_transcript_semantic',
    problems.length === 0,
    problems.length === 0
      ? `transcript includes required onboarding/session/permission methods and ${responseIds.length} correlated response(s)`
      : `transcript semantic problems: ${problems.join('; ')}`,
    ['appui-transcript.jsonl'],
  );
}

function checkRenderedScreenNoKnownBugPatterns(artifactDir) {
  const scenario = readJson(artifactPath(artifactDir, 'scenario.json'));
  const scenarioId = scenario.ok
    ? (scenario.value.id || scenario.value.scenario_id || '')
    : '';
  const captureNames = fs.existsSync(artifactDir)
    ? fs
      .readdirSync(artifactDir)
      .filter((name) => /^tui-capture.*\.txt$/.test(name))
      .sort()
    : ['tui-capture.txt'];
  if (!captureNames.includes('tui-capture.txt')) captureNames.unshift('tui-capture.txt');
  const serverLog = readText(artifactPath(artifactDir, 'server.log'));
  const problems = [];
  let mainCaptureText = '';
  for (const captureName of captureNames) {
    const capture = readText(artifactPath(artifactDir, captureName));
    if (!capture.ok) {
      problems.push(`${captureName} could not be read: ${capture.error}`);
      continue;
    }
    if (captureName === 'tui-capture.txt') mainCaptureText = capture.text;
    if (capture.text.trim().length === 0) {
      problems.push(`${captureName} is empty after stripping ANSI escapes`);
      continue;
    }
    for (const pattern of KNOWN_CAPTURE_BUG_PATTERNS) {
      if (
        scenarioId === 'restart-reconnect'
        && pattern.id === 'appui_error_text_visible'
        && /UI protocol disconnected[\s\S]*UI protocol reconnected/.test(capture.text)
      ) {
        continue;
      }
      const match = lineMatches(capture.text, pattern.regex);
      if (match) {
        problems.push(`${pattern.id} in ${captureName} at line ${match.line}: ${pattern.detail}`);
      }
    }
  }
  if (!serverLog.ok) {
    problems.push(`server.log could not be read: ${serverLog.error}`);
  } else {
    const serverDrop = lineMatches(serverLog.text, SERVER_DROPPED_TURN_PATTERN);
    if (serverDrop) {
      problems.push(`server_dropped_turn_completed at line ${serverDrop.line}: server log contains dropped turn/completed lifecycle evidence`);
    }
    if (mainCaptureText && CAPTURE_STUCK_RUNNING_PATTERN.test(mainCaptureText) && serverDrop) {
      problems.push('capture_stuck_running_after_server_drop: capture still shows running state after dropped completion evidence');
    }
  }
  return makeCheck(
    'rendered_screen_no_known_bug_patterns',
    problems.length === 0,
    problems.length === 0
      ? 'tui-capture*.txt and server.log do not match known tmux UX bug patterns'
      : `known rendered-screen bug patterns found: ${problems.join('; ')}`,
    [...captureNames, 'server.log'],
  );
}

function checkScreenGeometryConsistent(artifactDir) {
  const summary = readJson(artifactPath(artifactDir, 'summary.json'));
  if (!summary.ok) {
    return makeCheck(
      'screen_geometry_consistent',
      false,
      `summary.json is not parseable JSON: ${summary.error}`,
      ['summary.json', 'terminal-size.json', 'tui-capture.txt'],
    );
  }
  if (summary.value.mode !== 'run' || ['blocked', 'skipped', 'quarantined'].includes(summary.value.status)) {
    return makeCheck(
      'screen_geometry_consistent',
      true,
      `screen geometry checks are not required for mode=${summary.value.mode ?? '<unset>'} status=${summary.value.status ?? '<unset>'}`,
      ['summary.json', 'terminal-size.json', 'tui-capture.txt'],
    );
  }

  const terminal = readJson(artifactPath(artifactDir, 'terminal-size.json'));
  const capture = readText(artifactPath(artifactDir, 'tui-capture.txt'));
  const problems = [];
  if (!terminal.ok) {
    problems.push(`terminal-size.json is not parseable JSON: ${terminal.error}`);
  }
  if (!capture.ok) {
    problems.push(`tui-capture.txt could not be read: ${capture.error}`);
  }
  if (problems.length > 0) {
    return makeCheck(
      'screen_geometry_consistent',
      false,
      `screen geometry problems: ${problems.join('; ')}`,
      ['terminal-size.json', 'tui-capture.txt'],
    );
  }

  const { cols, rows } = terminal.value;
  const lines = captureLines(capture.text);
  const maxWidth = lines.reduce((max, line) => Math.max(max, codepointLength(line)), 0);
  if (lines.length > rows) {
    problems.push(`capture has ${lines.length} rendered row(s), expected at most ${rows}`);
  }
  if (maxWidth > cols) {
    problems.push(`capture has a ${maxWidth}-column line, expected at most ${cols}`);
  }
  if (!capture.text.includes('Composer')) {
    problems.push('capture is missing the Composer control row');
  }
  if (!/(^|\n) state /.test(capture.text)) {
    problems.push('capture is missing the bottom state row');
  }

  return makeCheck(
    'screen_geometry_consistent',
    problems.length === 0,
    problems.length === 0
      ? `capture fits declared terminal size ${cols}x${rows} with max width ${maxWidth} and ${lines.length} row(s)`
      : `screen geometry problems: ${problems.join('; ')}`,
    ['terminal-size.json', 'tui-capture.txt'],
  );
}

function checkPermissionSelectionScenario(artifactDir) {
  const scenario = readJson(artifactPath(artifactDir, 'scenario.json'));
  if (!scenario.ok) {
    return makeCheck(
      'permission_selection_visible_contract',
      false,
      `scenario.json is not parseable JSON: ${scenario.error}`,
      ['scenario.json'],
    );
  }
  if (scenario.value.id !== 'permission-selection' && scenario.value.scenario_id !== 'permission-selection') {
    return makeCheck(
      'permission_selection_visible_contract',
      true,
      'permission-selection visible contract is not required for this scenario',
      ['scenario.json'],
    );
  }

  const openCapture = readText(artifactPath(artifactDir, 'tui-capture-permissions-open.txt'));
  const appliedCapture = readText(artifactPath(artifactDir, 'tui-capture-permissions-applied.txt'));
  const problems = [];
  if (!openCapture.ok) {
    problems.push(`tui-capture-permissions-open.txt could not be read: ${openCapture.error}`);
  } else {
    for (const expected of ['Update Model Permissions', 'Workspace Write, Never Ask', 'Full Access']) {
      if (!openCapture.text.includes(expected)) {
        problems.push(`open permission capture is missing ${expected}`);
      }
    }
  }
  if (!appliedCapture.ok) {
    problems.push(`tui-capture-permissions-applied.txt could not be read: ${appliedCapture.error}`);
  } else {
    for (const expected of [
      'Permissions updated: Workspace Write',
      'Workspace Write',
      'network blocked',
    ]) {
      if (!appliedCapture.text.includes(expected)) {
        problems.push(`applied permission capture is missing ${expected}`);
      }
    }
  }

  return makeCheck(
    'permission_selection_visible_contract',
    problems.length === 0,
    problems.length === 0
      ? 'permission menu opened and applied-state capture shows server-confirmed permission state'
      : `permission selection visible contract problems: ${problems.join('; ')}`,
    ['tui-capture-permissions-open.txt', 'tui-capture-permissions-applied.txt'],
  );
}

function checkProviderMissingScenario(artifactDir) {
  const scenario = readJson(artifactPath(artifactDir, 'scenario.json'));
  if (!scenario.ok) {
    return makeCheck(
      'provider_missing_visible_contract',
      false,
      `scenario.json is not parseable JSON: ${scenario.error}`,
      ['scenario.json'],
    );
  }
  if (
    scenario.value.id !== 'provider-missing-recoverable'
    && scenario.value.scenario_id !== 'provider-missing-recoverable'
  ) {
    return makeCheck(
      'provider_missing_visible_contract',
      true,
      'provider-missing visible contract is not required for this scenario',
      ['scenario.json'],
    );
  }

  const capture = readText(artifactPath(artifactDir, 'tui-capture-provider-missing.txt'));
  const problems = [];
  if (!capture.ok) {
    problems.push(`tui-capture-provider-missing.txt could not be read: ${capture.error}`);
  } else {
    for (const expected of [
      'Set Up LLM Provider',
      'Load provider catalog',
      'Selected provider: not selected',
      'Saved provider: none',
      'API key: not set',
      'Open coding session',
      'save provider first',
    ]) {
      if (!capture.text.includes(expected)) {
        problems.push(`provider-missing capture is missing ${expected}`);
      }
    }
  }

  return makeCheck(
    'provider_missing_visible_contract',
    problems.length === 0,
    problems.length === 0
      ? 'provider setup surface shows recoverable missing-provider state and setup actions'
      : `provider-missing visible contract problems: ${problems.join('; ')}`,
    ['tui-capture-provider-missing.txt'],
  );
}

function checkApprovalDenialScenario(artifactDir) {
  const scenario = readJson(artifactPath(artifactDir, 'scenario.json'));
  if (!scenario.ok) {
    return makeCheck(
      'approval_denial_visible_contract',
      false,
      `scenario.json is not parseable JSON: ${scenario.error}`,
      ['scenario.json'],
    );
  }
  if (scenario.value.id !== 'approval-denial' && scenario.value.scenario_id !== 'approval-denial') {
    return makeCheck(
      'approval_denial_visible_contract',
      true,
      'approval-denial visible contract is not required for this scenario',
      ['scenario.json'],
    );
  }

  const requestCapture = readText(artifactPath(artifactDir, 'tui-capture-approval-request.txt'));
  const deniedCapture = readText(artifactPath(artifactDir, 'tui-capture-approval-denied.txt'));
  const problems = [];
  if (!requestCapture.ok) {
    problems.push(`tui-capture-approval-request.txt could not be read: ${requestCapture.error}`);
  } else {
    for (const expected of ['Approval Requested', 'n = deny it']) {
      if (!requestCapture.text.includes(expected)) {
        problems.push(`approval request capture is missing ${expected}`);
      }
    }
  }
  if (!deniedCapture.ok) {
    problems.push(`tui-capture-approval-denied.txt could not be read: ${deniedCapture.error}`);
  } else if (!/Approval denied|denied by client|without sudo|continuing without sudo/i.test(deniedCapture.text)) {
    problems.push('approval denied capture is missing a denial acknowledgement');
  }

  return makeCheck(
    'approval_denial_visible_contract',
    problems.length === 0,
    problems.length === 0
      ? 'approval request surfaced in the TUI and denial acknowledgement was rendered after pressing n'
      : `approval denial visible contract problems: ${problems.join('; ')}`,
    ['tui-capture-approval-request.txt', 'tui-capture-approval-denied.txt'],
  );
}

function checkTaskSubagentTreeScenario(artifactDir) {
  const scenario = readJson(artifactPath(artifactDir, 'scenario.json'));
  if (!scenario.ok) {
    return makeCheck(
      'task_subagent_tree_visible_contract',
      false,
      `scenario.json is not parseable JSON: ${scenario.error}`,
      ['scenario.json'],
    );
  }
  if (scenario.value.id !== 'task-subagent-tree' && scenario.value.scenario_id !== 'task-subagent-tree') {
    return makeCheck(
      'task_subagent_tree_visible_contract',
      true,
      'task-subagent-tree visible contract is not required for this scenario',
      ['scenario.json'],
    );
  }
  const summary = readJson(artifactPath(artifactDir, 'summary.json'));
  if (summary.ok && ['blocked', 'skipped', 'quarantined'].includes(summary.value.status)) {
    return makeCheck(
      'task_subagent_tree_visible_contract',
      true,
      `task-subagent-tree visible contract is not required for status=${summary.value.status}`,
      ['scenario.json', 'summary.json'],
    );
  }

  const runningCapture = readText(artifactPath(artifactDir, 'tui-capture-task-subagent-tree-running.txt'));
  const finalCapture = readText(artifactPath(artifactDir, 'tui-capture-task-subagent-tree-final.txt'));
  const summaryCapture = readText(artifactPath(artifactDir, 'tui-capture-task-subagent-tree-summary.txt'));
  const launchCommand = readText(artifactPath(artifactDir, 'launch-command.txt'));
  const transcriptPath = artifactPath(artifactDir, path.join('m15-evidence', 'appui-transcript.jsonl'));
  const codeReviewReportPath = artifactPath(
    artifactDir,
    path.join('m15-evidence', 'agent-artifacts', 'code-review-report.md'),
  );
  const codeReviewReport = readText(codeReviewReportPath);
  const finalMarker = typeof scenario.value.final_marker === 'string' && scenario.value.final_marker.length > 0
    ? scenario.value.final_marker
    : 'M15CODEREVIEWFINALLINE';
  const parsed = parseJsonl(transcriptPath);
  const frameRows = parsed.rows
    .map((row) => ({ row, frame: normalizeTranscriptFrame(row.value) }))
    .filter((entry) => isPlainObject(entry.frame));
  const methodNames = new Set(
    frameRows
      .map((entry) => entry.frame.method)
      .filter((method) => typeof method === 'string' && method.length > 0),
  );
  const requiredMethods = [
    'turn/start',
    'turn/started',
    'task/updated',
    'task/output/delta',
    'agent/updated',
    'message/delta',
    'turn/completed',
  ];
  const problems = [];
  if (!runningCapture.ok) {
    problems.push(`tui-capture-task-subagent-tree-running.txt could not be read: ${runningCapture.error}`);
  } else if (!/Live code review subagent swarm|Agent task|fixture turn running/.test(runningCapture.text)) {
    problems.push('running task/subagent capture is missing a visible task or fixture-running marker');
  }
  if (!finalCapture.ok) {
    problems.push(`tui-capture-task-subagent-tree-final.txt could not be read: ${finalCapture.error}`);
  } else {
    for (const expected of [
      'Scatter-join complete',
      'Subagents',
      'Ada Lovelace',
      'Hypatia',
      'Socrates',
      'Artifacts',
      finalMarker,
      'Done',
    ]) {
      if (!finalCapture.text.includes(expected)) problems.push(`final task/subagent capture is missing ${expected}`);
    }
  }
  if (!summaryCapture.ok) {
    problems.push(`tui-capture-task-subagent-tree-summary.txt could not be read: ${summaryCapture.error}`);
  } else {
    if (!/Code\s+Review Summary|Review Summary/.test(summaryCapture.text)) {
      problems.push('scrolled task/subagent summary capture is missing Code Review Summary');
    }
    for (const expected of ['Findings', 'High:']) {
      if (!summaryCapture.text.includes(expected)) {
        problems.push(`scrolled task/subagent summary capture is missing ${expected}`);
      }
    }
  }
  if (!fs.existsSync(transcriptPath)) {
    problems.push('m15-evidence/appui-transcript.jsonl is missing');
  }
  if (!codeReviewReport.ok) {
    problems.push(`m15-evidence/agent-artifacts/code-review-report.md could not be read: ${codeReviewReport.error}`);
  } else if (!/Code Review Summary|Review Summary/.test(codeReviewReport.text)) {
    problems.push('m15-evidence/agent-artifacts/code-review-report.md is missing review summary heading');
  }
  if (!launchCommand.ok) {
    problems.push(`launch-command.txt could not be read: ${launchCommand.error}`);
  } else {
    for (const expected of [
      'OCTOS_M15_LIVE_SUBAGENT_FIXTURE=',
      'OCTOS_TUI_M15_UX_OUTPUT_DIR=',
      'OCTOS_TUI_M15_UX_WORKDIR=',
      'OCTOS_M15_LIVE_SUBAGENT_DELAY_SCALE=',
    ]) {
      if (!launchCommand.text.includes(expected)) {
        problems.push(`launch-command.txt is missing ${expected}`);
      }
    }
  }
  const fixtureEnv = isPlainObject(scenario.value.fixture_env) ? scenario.value.fixture_env : {};
  if (fixtureEnv.OCTOS_M15_LIVE_SUBAGENT_FIXTURE !== '1') {
    problems.push('scenario fixture_env missing OCTOS_M15_LIVE_SUBAGENT_FIXTURE=1');
  }
  for (const expected of [
    'OCTOS_TUI_M15_UX_OUTPUT_DIR',
    'OCTOS_TUI_M15_UX_WORKDIR',
    'OCTOS_M15_LIVE_SUBAGENT_DELAY_SCALE',
  ]) {
    if (typeof fixtureEnv[expected] !== 'string' || fixtureEnv[expected].length === 0) {
      problems.push(`scenario fixture_env missing ${expected}`);
    }
  }
  for (const error of parsed.errors) {
    problems.push(`m15 evidence transcript line ${error.line}: ${error.error}`);
  }
  for (const method of requiredMethods) {
    if (!methodNames.has(method)) {
      problems.push(`m15 evidence transcript missing ${method}`);
    }
  }

  return makeCheck(
    'task_subagent_tree_visible_contract',
    problems.length === 0,
    problems.length === 0
      ? 'task/subagent fixture rendered running and final TUI states with M15 AppUI transcript evidence'
      : `task/subagent tree visible contract problems: ${problems.join('; ')}`,
    [
      'tui-capture-task-subagent-tree-running.txt',
      'tui-capture-task-subagent-tree-final.txt',
      'tui-capture-task-subagent-tree-summary.txt',
      'launch-command.txt',
      'm15-evidence/appui-transcript.jsonl',
      'm15-evidence/agent-artifacts/code-review-report.md',
    ],
  );
}

function checkRestartReconnectScenario(artifactDir) {
  const scenario = readJson(artifactPath(artifactDir, 'scenario.json'));
  if (!scenario.ok) {
    return makeCheck(
      'restart_reconnect_visible_contract',
      false,
      `scenario.json is not parseable JSON: ${scenario.error}`,
      ['scenario.json'],
    );
  }
  if (scenario.value.id !== 'restart-reconnect' && scenario.value.scenario_id !== 'restart-reconnect') {
    return makeCheck(
      'restart_reconnect_visible_contract',
      true,
      'restart-reconnect visible contract is not required for this scenario',
      ['scenario.json'],
    );
  }

  const preCapture = readText(artifactPath(artifactDir, 'tui-capture-pre-restart.txt'));
  const postCapture = readText(artifactPath(artifactDir, 'tui-capture-post-reconnect.txt'));
  const preSnapshot = readJson(artifactPath(artifactDir, 'pre-restart-snapshot.json'));
  const postSnapshot = readJson(artifactPath(artifactDir, 'post-reconnect-snapshot.json'));
  const parsed = parseJsonl(artifactPath(artifactDir, 'websocket-transcript.jsonl'));
  const frameRows = parsed.rows
    .map((row) => ({ row, frame: normalizeTranscriptFrame(row.value) }))
    .filter((entry) => isPlainObject(entry.frame));
  const methodNames = new Set(
    frameRows
      .map((entry) => entry.frame.method)
      .filter((method) => typeof method === 'string' && method.length > 0),
  );
  const requiredMethods = [
    'client_hello',
    'config/capabilities/list',
    'profile/local/create',
    'permission/profile/list',
    'permission/profile/set',
    'session/open',
    'session/status/read',
    'tool/status/list',
    'session/snapshot',
    'session/hydrate',
  ];
  const problems = [];
  if (!preCapture.ok) {
    problems.push(`tui-capture-pre-restart.txt could not be read: ${preCapture.error}`);
  } else if (!/Before backend restart|Done|Protocol backend connected|Ask Octos to change code/.test(preCapture.text)) {
    problems.push('pre-restart TUI capture is missing visible active-session state');
  }
  if (!postCapture.ok) {
    problems.push(`tui-capture-post-reconnect.txt could not be read: ${postCapture.error}`);
  } else {
    const normalizedPostCapture = postCapture.text.replaceAll('_', '');
    if (!/Backend connection reconnected|UI protocol reconnected|M19_RESTART_RECONNECT_FINAL_LINE|M19RESTARTRECONNECTFINALLINE|reconnected/i.test(postCapture.text)) {
      problems.push('post-reconnect TUI capture is missing reconnect or final-marker evidence');
    }
    if (
      !postCapture.text.includes('M19_RESTART_RECONNECT_FINAL_LINE')
      && !normalizedPostCapture.includes('M19RESTARTRECONNECTFINALLINE')
    ) {
      problems.push('post-reconnect TUI capture is missing the restart reconnect final marker');
    }
  }
  if (!preSnapshot.ok) {
    problems.push(`pre-restart-snapshot.json is not parseable JSON: ${preSnapshot.error}`);
  }
  if (!postSnapshot.ok) {
    problems.push(`post-reconnect-snapshot.json is not parseable JSON: ${postSnapshot.error}`);
  }
  if (preSnapshot.ok && postSnapshot.ok) {
    if (preSnapshot.value.session_id !== postSnapshot.value.session_id) {
      problems.push('pre/post snapshots refer to different session_id values');
    }
    const preCursorValue = preSnapshot.value.cursor?.seq ?? preSnapshot.value.hydrate?.cursor?.seq;
    const postCursorValue = postSnapshot.value.cursor?.seq ?? postSnapshot.value.hydrate?.cursor?.seq;
    const preSeq = Number(preCursorValue);
    const postSeq = Number(postCursorValue);
    if (preCursorValue === undefined || postCursorValue === undefined) {
      problems.push('pre/post snapshots must both include cursor.seq or hydrate.cursor.seq');
    } else if (!Number.isFinite(preSeq) || !Number.isFinite(postSeq) || postSeq < preSeq) {
      problems.push(`post reconnect cursor seq ${postSeq} is behind pre restart cursor seq ${preSeq}`);
    }
  }
  for (const error of parsed.errors) {
    problems.push(`websocket transcript line ${error.line}: ${error.error}`);
  }
  for (const method of requiredMethods) {
    if (!methodNames.has(method)) {
      problems.push(`websocket transcript missing ${method}`);
    }
  }

  return makeCheck(
    'restart_reconnect_visible_contract',
    problems.length === 0,
    problems.length === 0
      ? 'backend restart, TUI reconnect, and session hydrate snapshots are visible and cursor-consistent'
      : `restart/reconnect visible contract problems: ${problems.join('; ')}`,
    [
      'tui-capture-pre-restart.txt',
      'tui-capture-post-reconnect.txt',
      'pre-restart-snapshot.json',
      'post-reconnect-snapshot.json',
      'websocket-transcript.jsonl',
    ],
  );
}

function checkDroppedCompletionBackpressureScenario(artifactDir) {
  const scenario = readJson(artifactPath(artifactDir, 'scenario.json'));
  if (!scenario.ok) {
    return makeCheck(
      'dropped_completion_backpressure_contract',
      false,
      `scenario.json is not parseable JSON: ${scenario.error}`,
      ['scenario.json'],
    );
  }
  if (
    scenario.value.id !== 'dropped-completion-backpressure'
    && scenario.value.scenario_id !== 'dropped-completion-backpressure'
  ) {
    return makeCheck(
      'dropped_completion_backpressure_contract',
      true,
      'dropped-completion/backpressure contract is not required for this scenario',
      ['scenario.json'],
    );
  }
  const summary = readJson(artifactPath(artifactDir, 'summary.json'));
  if (summary.ok && ['blocked', 'skipped', 'quarantined'].includes(summary.value.status)) {
    return makeCheck(
      'dropped_completion_backpressure_contract',
      true,
      `dropped-completion/backpressure contract is not required for status=${summary.value.status}`,
      ['scenario.json', 'summary.json'],
    );
  }

  const replayCapture = readText(artifactPath(artifactDir, 'tui-capture-replay-lossy.txt'));
  const finalCapture = readText(artifactPath(artifactDir, 'tui-capture-backpressure-final.txt'));
  const report = readJson(artifactPath(artifactDir, 'backpressure-report.json'));
  const parsedNotifications = parseJsonl(artifactPath(artifactDir, 'notification-log.jsonl'));
  const parsedWs = parseJsonl(artifactPath(artifactDir, 'websocket-transcript.jsonl'));
  const notificationFrames = parsedNotifications.rows
    .map((row) => ({ row, frame: normalizeTranscriptFrame(row.value) }))
    .filter((entry) => isPlainObject(entry.frame));
  const wsFrames = parsedWs.rows
    .map((row) => ({ row, frame: normalizeTranscriptFrame(row.value) }))
    .filter((entry) => isPlainObject(entry.frame));
  const wsMethods = new Set(
    wsFrames
      .map((entry) => entry.frame.method)
      .filter((method) => typeof method === 'string' && method.length > 0),
  );
  const problems = [];

  if (!replayCapture.ok) {
    problems.push(`tui-capture-replay-lossy.txt could not be read: ${replayCapture.error}`);
  } else if (!replayCapture.text.includes('Replay lossy')) {
    problems.push('replay-lossy TUI capture is missing Replay lossy status');
  }
  if (!finalCapture.ok) {
    problems.push(`tui-capture-backpressure-final.txt could not be read: ${finalCapture.error}`);
  } else if (CAPTURE_STUCK_RUNNING_PATTERN.test(finalCapture.text)) {
    problems.push('final backpressure capture still shows a running task state');
  }
  if (!report.ok) {
    problems.push(`backpressure-report.json is not parseable JSON: ${report.error}`);
  } else {
    const droppedCount = Number(report.value.replay_lossy?.dropped_count);
    if (!Number.isFinite(droppedCount) || droppedCount <= 0) {
      problems.push(`backpressure report replay_lossy.dropped_count must be > 0, got ${report.value.replay_lossy?.dropped_count ?? '<missing>'}`);
    }
    if (report.value.terminal?.method !== 'turn/completed') {
      problems.push(`backpressure report terminal method must be turn/completed, got ${report.value.terminal?.method ?? '<missing>'}`);
    }
    if (report.value.session_id !== scenario.value.session_id) {
      problems.push('backpressure report session_id does not match scenario session_id');
    }
    if (!isPlainObject(report.value.snapshot)) {
      problems.push('backpressure report is missing session snapshot object');
    }
  }
  for (const error of parsedNotifications.errors) {
    problems.push(`notification log line ${error.line}: ${error.error}`);
  }
  for (const error of parsedWs.errors) {
    problems.push(`websocket transcript line ${error.line}: ${error.error}`);
  }
  if (!notificationFrames.some((entry) => entry.frame.method === 'protocol/replay_lossy')) {
    problems.push('notification-log.jsonl is missing protocol/replay_lossy');
  }
  for (const method of [
    'client_hello',
    'config/capabilities/list',
    'profile/local/create',
    'permission/profile/list',
    'permission/profile/set',
    'session/open',
    'session/status/read',
    'tool/status/list',
    'turn/start',
    'session/snapshot',
  ]) {
    if (!wsMethods.has(method)) problems.push(`websocket transcript missing ${method}`);
  }

  return makeCheck(
    'dropped_completion_backpressure_contract',
    problems.length === 0,
    problems.length === 0
      ? 'fixture-backed protocol/replay_lossy recovery is visible, terminal, and snapshot-backed'
      : `dropped-completion/backpressure contract problems: ${problems.join('; ')}`,
    [
      'tui-capture-replay-lossy.txt',
      'tui-capture-backpressure-final.txt',
      'notification-log.jsonl',
      'backpressure-report.json',
      'websocket-transcript.jsonl',
    ],
  );
}

function checkLowerSoakSummary(artifactDir) {
  const file = artifactPath(artifactDir, 'soak-summary.json');
  if (!fs.existsSync(file)) {
    return makeCheck(
      'lower_soak_summary_semantic',
      true,
      'no lower soak summary artifact is present for this scenario',
      ['soak-summary.json'],
    );
  }

  const parsed = readJson(file);
  if (!parsed.ok) {
    return makeCheck(
      'lower_soak_summary_semantic',
      false,
      `soak-summary.json is not parseable JSON: ${parsed.error}`,
      ['soak-summary.json'],
    );
  }

  const cases = Array.isArray(parsed.value.cases) ? parsed.value.cases : [];
  const blockedOrFailed = cases.filter((entry) => (
    isPlainObject(entry)
      && ['blocked', 'failed'].includes(entry.status)
  ));
  return makeCheck(
    'lower_soak_summary_semantic',
    blockedOrFailed.length === 0,
    blockedOrFailed.length === 0
      ? `lower soak summary has ${cases.length} case(s) and no blocked/failed case`
      : `lower soak summary has blocked/failed case(s): ${blockedOrFailed
        .map((entry) => `${entry.name ?? '<unnamed>'}=${entry.status}`)
        .join(', ')}`,
    ['soak-summary.json'],
  );
}

function buildValidation(artifactDir) {
  const checks = [
    checkArtifactAbi(artifactDir),
    checkAppuiTranscriptParseable(artifactDir),
    checkRealTmuxEvidence(artifactDir),
    checkAppuiTranscriptSemantic(artifactDir),
    checkRenderedScreenNoKnownBugPatterns(artifactDir),
    checkScreenGeometryConsistent(artifactDir),
    checkPermissionSelectionScenario(artifactDir),
    checkProviderMissingScenario(artifactDir),
    checkApprovalDenialScenario(artifactDir),
    checkTaskSubagentTreeScenario(artifactDir),
    checkRestartReconnectScenario(artifactDir),
    checkDroppedCompletionBackpressureScenario(artifactDir),
    checkLowerSoakSummary(artifactDir),
  ];
  const failures = checks
    .filter((check) => check.status === 'failed')
    .map((check) => ({
      id: check.id,
      detail: check.detail,
      evidence: check.evidence,
    }));
  return {
    schema: VALIDATION_SCHEMA,
    status: failures.length === 0 ? 'passed' : 'failed',
    checks,
    failures,
  };
}

function main() {
  const args = process.argv.slice(2);
  if (args.length !== 1 || args.includes('--help') || args.includes('-h')) {
    console.error(usage());
    return args.length === 1 ? 0 : 2;
  }

  const artifactDir = path.resolve(args[0]);
  const result = buildValidation(artifactDir);
  const output = `${JSON.stringify(result, null, 2)}\n`;
  if (fs.existsSync(artifactDir) && fs.statSync(artifactDir).isDirectory()) {
    fs.writeFileSync(artifactPath(artifactDir, 'validation.json'), output, 'utf8');
  }
  process.stdout.write(output);
  return result.status === 'passed' ? 0 : 1;
}

process.exitCode = main();
