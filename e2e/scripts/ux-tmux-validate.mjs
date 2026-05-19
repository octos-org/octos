#!/usr/bin/env node

import fs from 'node:fs';
import path from 'node:path';

const VALIDATION_SCHEMA = 'octos.ux.validation.v1';
const ARTIFACT_ABI = 'octos.ux.artifacts.v1';
const REQUIRED_ARTIFACTS = [
  'scenario.json',
  'summary.json',
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
    regex: /tmux session not running:|octos-tui exited with status/,
    detail: 'tmux capture shows the real TUI pane exited or was missing',
  },
];

const SERVER_DROPPED_TURN_PATTERN =
  /lifecycle notification not delivered.*turn\/completed|writer channel full for lifecycle frame|lifecycle ws send failed; aborting connection/;
const CAPTURE_STUCK_RUNNING_PATTERN = /Task Working|Progress .*Thinking|state .*Working/;

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

function checkRenderedScreenNoKnownBugPatterns(artifactDir) {
  const capture = readText(artifactPath(artifactDir, 'tui-capture.txt'));
  const serverLog = readText(artifactPath(artifactDir, 'server.log'));
  const problems = [];
  if (!capture.ok) {
    problems.push(`tui-capture.txt could not be read: ${capture.error}`);
  } else if (capture.text.trim().length === 0) {
    problems.push('tui-capture.txt is empty after stripping ANSI escapes');
  } else {
    for (const pattern of KNOWN_CAPTURE_BUG_PATTERNS) {
      const match = lineMatches(capture.text, pattern.regex);
      if (match) {
        problems.push(`${pattern.id} at line ${match.line}: ${pattern.detail}`);
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
    if (capture.ok && CAPTURE_STUCK_RUNNING_PATTERN.test(capture.text) && serverDrop) {
      problems.push('capture_stuck_running_after_server_drop: capture still shows running state after dropped completion evidence');
    }
  }
  return makeCheck(
    'rendered_screen_no_known_bug_patterns',
    problems.length === 0,
    problems.length === 0
      ? 'tui-capture.txt and server.log do not match known tmux UX bug patterns'
      : `known rendered-screen bug patterns found: ${problems.join('; ')}`,
    ['tui-capture.txt', 'server.log'],
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
    checkRenderedScreenNoKnownBugPatterns(artifactDir),
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
