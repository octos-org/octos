/**
 * M7.8 live swarm dispatch release gate (issue #511).
 *
 * This spec is intentionally gated out of default e2e runs. It only talks to
 * a real canary when OCTOS_M7_SWARM_LIVE=1 is present and the supervisor has
 * supplied OCTOS_TEST_URL plus OCTOS_AUTH_TOKEN. The companion script
 * `scripts/validate-m7-swarm-live.sh` sets those variables.
 */
import fs from 'node:fs';
import path from 'node:path';
import { expect, test, type Page, type TestInfo } from '@playwright/test';

import { M9WsClient, chatWS, type ChatWsEvent } from '../lib/m9-ws-client';
import { login, SEL } from './live-browser-helpers';

interface SwarmFixture {
  schema: string;
  issue: number;
  workflow: string;
  required_test_count: number;
  required_subagents: number;
  required_progress_events: number;
  required_artifacts: number;
  required_matrix_rooms: number;
  required_matrix_puppets: number;
  required_validator_aggregates: number;
  disallowed_hosts: string[];
  prompts: {
    dispatch: string;
  };
  markers: Record<string, string[]>;
  limits: {
    dispatch_timeout_seconds: number;
    spawn_timeout_seconds: number;
    artifact_timeout_seconds: number;
    reload_timeout_seconds: number;
    poll_interval_seconds: number;
    diagnostic_sample_limit: number;
  };
}

interface BackgroundTaskRow {
  id?: string;
  task_id?: string;
  tool_name?: string;
  tool_call_id?: string;
  parent_task_id?: string | null;
  child_session_key?: string | null;
  status?: string;
  lifecycle_state?: string;
  runtime_state?: string;
  runtime_detail?: string | Record<string, unknown> | null;
  progress?: number | null;
  current_phase?: string | null;
  progress_message?: string | null;
  output_files?: string[];
  cost?: {
    tokens_in?: number;
    tokens_out?: number;
    usd_used?: number;
    usd_reserved?: number;
  } | null;
  started_at?: string;
  updated_at?: string;
}

interface GateState {
  sessionId: string;
  events: ChatWsEvent[];
  content: string;
  doneEvent?: ChatWsEvent;
  tasks: BackgroundTaskRow[];
  subagents: BackgroundTaskRow[];
  messages: unknown[];
}

const FIXTURE_PATH = path.join(__dirname, '..', 'fixtures', 'm7-swarm-expected.json');
const FIXTURE = loadFixture();
const RUN_LIVE = process.env.OCTOS_M7_SWARM_LIVE === '1';
const BASE = (process.env.OCTOS_TEST_URL || '').replace(/\/+$/, '');
const TOKEN = process.env.OCTOS_AUTH_TOKEN || '';
const PROFILE = process.env.OCTOS_PROFILE || 'dspfac';
const OUTPUT_DIR =
  process.env.OCTOS_M7_SWARM_OUTPUT_DIR ||
  path.join(process.cwd(), 'test-results-m7-swarm-live');
const DIAGNOSTIC_JSON =
  process.env.OCTOS_M7_SWARM_DIAGNOSTICS ||
  path.join(OUTPUT_DIR, 'diagnostic.json');
const POLL_INTERVAL_MS = FIXTURE.limits.poll_interval_seconds * 1_000;
const DISPATCH_TIMEOUT_MS = FIXTURE.limits.dispatch_timeout_seconds * 1_000;
const SPAWN_TIMEOUT_MS = FIXTURE.limits.spawn_timeout_seconds * 1_000;
const ARTIFACT_TIMEOUT_MS = FIXTURE.limits.artifact_timeout_seconds * 1_000;
const RELOAD_TIMEOUT_MS = FIXTURE.limits.reload_timeout_seconds * 1_000;
const SAMPLE_LIMIT = FIXTURE.limits.diagnostic_sample_limit;

const state: GateState = {
  sessionId: `m7-swarm-live-${Date.now().toString(36)}`,
  events: [],
  content: '',
  tasks: [],
  subagents: [],
  messages: [],
};

function loadFixture(): SwarmFixture {
  const parsed = JSON.parse(fs.readFileSync(FIXTURE_PATH, 'utf8')) as SwarmFixture;
  if (!parsed?.schema || parsed.issue !== 511 || parsed.required_test_count !== 5) {
    throw new Error(`Invalid M7 swarm live fixture at ${FIXTURE_PATH}`);
  }
  return parsed;
}

function redact(value: unknown): unknown {
  if (typeof value === 'string') {
    return TOKEN ? value.split(TOKEN).join('[redacted-token]') : value;
  }
  if (Array.isArray(value)) return value.map(redact);
  if (value && typeof value === 'object') {
    const out: Record<string, unknown> = {};
    for (const [key, raw] of Object.entries(value)) {
      out[key] = /token|authorization|secret/i.test(key) ? '[redacted]' : redact(raw);
    }
    return out;
  }
  return value;
}

function writeDiagnostic(
  status: 'failed' | 'passed',
  kind: string,
  detail: string,
  extra: Record<string, unknown> = {},
): void {
  fs.mkdirSync(path.dirname(DIAGNOSTIC_JSON), { recursive: true });
  const diagnostic = {
    schema: 'octos.swarm.m7.live_gate.diagnostic.v1',
    status,
    kind,
    detail,
    issue: FIXTURE.issue,
    base_url: BASE || null,
    profile: PROFILE,
    session_id: state.sessionId || null,
    timestamp: new Date().toISOString(),
    ...redact(extra),
  };
  fs.writeFileSync(DIAGNOSTIC_JSON, `${JSON.stringify(diagnostic, null, 2)}\n`);
}

async function failWithDiagnostic(
  kind: string,
  detail: string,
  extra: Record<string, unknown> = {},
): Promise<never> {
  writeDiagnostic('failed', kind, detail, extra);
  throw new Error(`[${kind}] ${detail}`);
}

async function attachDiagnostic(testInfo: TestInfo): Promise<void> {
  if (!fs.existsSync(DIAGNOSTIC_JSON)) return;
  await testInfo.attach('m7-swarm-diagnostic', {
    path: DIAGNOSTIC_JSON,
    contentType: 'application/json',
  });
}

function asRecord(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  return value as Record<string, unknown>;
}

function parseRuntimeDetail(task: BackgroundTaskRow): Record<string, unknown> {
  const detail = task.runtime_detail;
  if (!detail) return {};
  if (typeof detail === 'string') {
    try {
      return asRecord(JSON.parse(detail)) ?? {};
    } catch {
      return {};
    }
  }
  return asRecord(detail) ?? {};
}

function haystack(value: unknown): string {
  try {
    return JSON.stringify(value).toLowerCase();
  } catch {
    return String(value ?? '').toLowerCase();
  }
}

function markerHit(value: unknown, markerGroup: string): boolean {
  const markers = FIXTURE.markers[markerGroup] ?? [];
  const text = haystack(value);
  return markers.some((marker) => text.includes(marker.toLowerCase()));
}

function markerOccurrenceCount(value: unknown, markerGroup: string): number {
  const markers = FIXTURE.markers[markerGroup] ?? [];
  const text = haystack(value);
  return markers.reduce((total, marker) => {
    const needle = marker.toLowerCase();
    if (!needle) return total;
    return total + text.split(needle).length - 1;
  }, 0);
}

function taskKey(task: BackgroundTaskRow): string {
  return (
    task.id ||
    task.task_id ||
    task.child_session_key ||
    task.tool_call_id ||
    `${task.tool_name || ''}:${task.started_at || ''}:${task.updated_at || ''}`
  );
}

function uniqueTasks(tasks: BackgroundTaskRow[]): BackgroundTaskRow[] {
  const seen = new Set<string>();
  const out: BackgroundTaskRow[] = [];
  for (const task of tasks) {
    const key = taskKey(task);
    if (!key || seen.has(key)) continue;
    seen.add(key);
    out.push(task);
  }
  return out;
}

function selectSubagents(tasks: BackgroundTaskRow[]): BackgroundTaskRow[] {
  return uniqueTasks(
    tasks.filter((task) => {
      if (task.child_session_key) return true;
      const detail = parseRuntimeDetail(task);
      return markerHit(task, 'subagent') || markerHit(detail, 'subagent');
    }),
  );
}

function progressEvidenceCount(events: ChatWsEvent[], tasks: BackgroundTaskRow[]): number {
  const eventCount = events.filter((event) => markerHit(event, 'progress')).length;
  const taskCount = tasks.filter((task) => {
    if (typeof task.progress === 'number') return true;
    if (task.current_phase || task.progress_message) return true;
    return markerHit(parseRuntimeDetail(task), 'progress');
  }).length;
  return eventCount + taskCount;
}

function artifactEvidenceCount(tasks: BackgroundTaskRow[], messages: unknown[]): number {
  const fromTasks = tasks.flatMap((task) => task.output_files ?? []).length;
  const text = haystack({ tasks, messages });
  const fileRefs = new Set(
    text.match(/[a-z0-9_./-]+\.(?:md|json|txt|html|pdf|png|jpg|jpeg|mp3|wav|zip)/g) ?? [],
  );
  return (
    fromTasks +
    fileRefs.size +
    markerOccurrenceCount(tasks, 'artifact') +
    markerOccurrenceCount(messages, 'artifact')
  );
}

function costEvidenceRows(subagents: BackgroundTaskRow[]): BackgroundTaskRow[] {
  return subagents.filter((task) => {
    const cost = task.cost;
    if (cost) {
      const tokensIn = Number(cost.tokens_in ?? 0);
      const tokensOut = Number(cost.tokens_out ?? 0);
      const usdUsed = Number(cost.usd_used ?? 0);
      if (tokensIn > 0 || tokensOut > 0 || usdUsed > 0) return true;
    }
    return markerHit(task, 'cost') || markerHit(parseRuntimeDetail(task), 'cost');
  });
}

function matrixRoomEvidenceCount(tasks: BackgroundTaskRow[], messages: unknown[]): number {
  const text = haystack({ tasks, messages });
  const roomIds = new Set(text.match(/![a-z0-9._=-]+:[a-z0-9.-]+/g) ?? []);
  return Math.max(roomIds.size, markerOccurrenceCount({ tasks, messages }, 'matrix_room'));
}

function matrixPuppetEvidenceCount(tasks: BackgroundTaskRow[], messages: unknown[]): number {
  const text = haystack({ tasks, messages });
  const userIds = new Set(text.match(/@[a-z0-9._=-]+:[a-z0-9.-]+/g) ?? []);
  return Math.max(userIds.size, markerOccurrenceCount({ tasks, messages }, 'matrix_puppet'));
}

function validatorEvidenceCount(tasks: BackgroundTaskRow[], messages: unknown[]): number {
  return (
    tasks.filter((task) => markerHit(task, 'validator')).length +
    messages.filter((message) => markerHit(message, 'validator')).length
  );
}

async function getTasks(sessionId: string): Promise<BackgroundTaskRow[]> {
  const resp = await fetch(`${BASE}/api/sessions/${encodeURIComponent(sessionId)}/tasks`, {
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'X-Profile-Id': PROFILE,
    },
  });
  if (!resp.ok) {
    await failWithDiagnostic('tasks_api_failed', `GET /api/sessions/:id/tasks returned ${resp.status}`, {
      response_body: await resp.text().catch(() => ''),
    });
  }
  const rows = (await resp.json().catch(() => [])) as unknown;
  return Array.isArray(rows) ? (rows as BackgroundTaskRow[]) : [];
}

async function getMessages(sessionId: string): Promise<unknown[]> {
  const resp = await fetch(`${BASE}/api/sessions/${encodeURIComponent(sessionId)}/messages`, {
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'X-Profile-Id': PROFILE,
    },
  });
  if (!resp.ok) return [];
  const rows = (await resp.json().catch(() => [])) as unknown;
  return Array.isArray(rows) ? rows : [];
}

async function refreshState(): Promise<void> {
  state.tasks = await getTasks(state.sessionId);
  state.subagents = selectSubagents(state.tasks);
  state.messages = await getMessages(state.sessionId);
}

async function pollUntil(
  predicate: () => Promise<boolean>,
  timeoutMs: number,
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return true;
    await new Promise((resolve) => setTimeout(resolve, POLL_INTERVAL_MS));
  }
  return predicate();
}

async function ensureLiveConfig(): Promise<void> {
  if (!BASE) {
    await failWithDiagnostic('missing_base_url', 'OCTOS_TEST_URL is required when OCTOS_M7_SWARM_LIVE=1');
  }
  if (!TOKEN) {
    await failWithDiagnostic('missing_auth_token', 'OCTOS_AUTH_TOKEN is required when OCTOS_M7_SWARM_LIVE=1');
  }
  let host = '';
  try {
    host = new URL(BASE).hostname;
  } catch {
    await failWithDiagnostic('invalid_base_url', `Invalid OCTOS_TEST_URL: ${BASE}`);
  }
  if (FIXTURE.disallowed_hosts.includes(host)) {
    await failWithDiagnostic('disallowed_host', `M7 swarm live gate refuses to run on ${host}`, {
      disallowed_hosts: FIXTURE.disallowed_hosts,
    });
  }
}

async function openSessionAfterReload(): Promise<void> {
  const client = new M9WsClient({
    url: BASE,
    token: TOKEN,
    profileId: PROFILE,
    requestTimeoutMs: 20_000,
  });
  try {
    await client.openSession({ session_id: state.sessionId }, 20_000);
  } finally {
    await client.close();
  }
}

async function pointBrowserAtSession(page: Page): Promise<void> {
  await login(page);
  await page.evaluate((sessionId) => {
    localStorage.setItem('octos_current_session', sessionId);
  }, state.sessionId);
  await page.goto('/chat', { waitUntil: 'domcontentloaded' });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForSelector(SEL.chatInput, { timeout: RELOAD_TIMEOUT_MS });
}

test.describe('M7.8 live swarm dispatch gate', () => {
  test.describe.configure({ mode: 'serial' });
  test.setTimeout(DISPATCH_TIMEOUT_MS + 120_000);
  test.skip(!RUN_LIVE, 'Set OCTOS_M7_SWARM_LIVE=1 to run the M7.8 canary-only swarm gate.');

  test.beforeAll(async () => {
    await ensureLiveConfig();
  });

  test.afterEach(async ({}, testInfo) => {
    if (testInfo.status !== testInfo.expectedStatus && !fs.existsSync(DIAGNOSTIC_JSON)) {
      writeDiagnostic('failed', 'playwright_failure', testInfo.error?.message || 'Playwright test failed');
    }
    await attachDiagnostic(testInfo);
  });

  test('dispatches three sub-agents and emits progress', async () => {
    const result = await chatWS({
      baseUrl: BASE,
      token: TOKEN,
      profileId: PROFILE,
      sessionId: state.sessionId,
      message: FIXTURE.prompts.dispatch,
      maxWait: DISPATCH_TIMEOUT_MS,
      requestTimeoutMs: 60_000,
    });
    state.events = result.events;
    state.content = result.content;
    state.doneEvent = result.doneEvent;

    if (!result.doneEvent) {
      await failWithDiagnostic('dispatch_turn_timeout', 'Swarm dispatch turn did not reach a terminal WS event', {
        events: result.events.slice(-SAMPLE_LIMIT),
        content: result.content.slice(-1000),
      });
    }
    const doneType = String(result.doneEvent.type || '');
    if (doneType === 'error') {
      await failWithDiagnostic('dispatch_turn_error', 'Swarm dispatch returned a turn/error event', {
        done_event: result.doneEvent,
        events: result.events.slice(-SAMPLE_LIMIT),
      });
    }

    const spawned = await pollUntil(async () => {
      await refreshState();
      return state.subagents.length >= FIXTURE.required_subagents;
    }, SPAWN_TIMEOUT_MS);
    if (!spawned) {
      await failWithDiagnostic('subagents_missing', 'Expected at least three M7 sub-agents to be visible in task state', {
        required_subagents: FIXTURE.required_subagents,
        observed_subagents: state.subagents.length,
        tasks: state.tasks.slice(0, SAMPLE_LIMIT),
        events: state.events.slice(-SAMPLE_LIMIT),
      });
    }

    const progressCount = progressEvidenceCount(state.events, state.tasks);
    if (progressCount < FIXTURE.required_progress_events) {
      await failWithDiagnostic('progress_events_missing', 'Swarm dispatch did not expose enough progress evidence', {
        required_progress_events: FIXTURE.required_progress_events,
        observed_progress_events: progressCount,
        tasks: state.tasks.slice(0, SAMPLE_LIMIT),
        events: state.events.slice(-SAMPLE_LIMIT),
      });
    }

    expect(state.subagents.length).toBeGreaterThanOrEqual(FIXTURE.required_subagents);
  });

  test('records per-subagent task and cost attribution', async () => {
    await refreshState();
    if (state.subagents.length < FIXTURE.required_subagents) {
      await failWithDiagnostic('subagents_missing_for_cost', 'Cannot verify cost attribution without three sub-agents', {
        observed_subagents: state.subagents.length,
        tasks: state.tasks.slice(0, SAMPLE_LIMIT),
      });
    }

    const costRows = costEvidenceRows(state.subagents);
    if (costRows.length < FIXTURE.required_subagents) {
      await failWithDiagnostic('subagent_cost_missing', 'Each M7 sub-agent must have cost ledger attribution', {
        required_cost_rows: FIXTURE.required_subagents,
        observed_cost_rows: costRows.length,
        subagents: state.subagents.slice(0, SAMPLE_LIMIT),
      });
    }

    expect(costRows.length).toBeGreaterThanOrEqual(FIXTURE.required_subagents);
  });

  test('delivers artifacts and aggregate validator evidence', async () => {
    const complete = await pollUntil(async () => {
      await refreshState();
      const artifacts = artifactEvidenceCount(state.tasks, state.messages);
      const validators = validatorEvidenceCount(state.tasks, state.messages);
      return (
        artifacts >= FIXTURE.required_artifacts &&
        validators >= FIXTURE.required_validator_aggregates
      );
    }, ARTIFACT_TIMEOUT_MS);

    const artifacts = artifactEvidenceCount(state.tasks, state.messages);
    const validators = validatorEvidenceCount(state.tasks, state.messages);
    if (!complete) {
      await failWithDiagnostic('artifact_or_validator_missing', 'Swarm artifacts or M4.3 aggregate validator evidence did not appear', {
        required_artifacts: FIXTURE.required_artifacts,
        observed_artifacts: artifacts,
        required_validator_aggregates: FIXTURE.required_validator_aggregates,
        observed_validator_aggregates: validators,
        tasks: state.tasks.slice(0, SAMPLE_LIMIT),
        messages: state.messages.slice(-SAMPLE_LIMIT),
      });
    }

    expect(artifacts).toBeGreaterThanOrEqual(FIXTURE.required_artifacts);
    expect(validators).toBeGreaterThanOrEqual(FIXTURE.required_validator_aggregates);
  });

  test('creates Matrix room and puppet evidence', async () => {
    await refreshState();
    const roomEvidence = matrixRoomEvidenceCount(state.tasks, state.messages);
    const puppetEvidence = matrixPuppetEvidenceCount(state.tasks, state.messages);

    if (roomEvidence < FIXTURE.required_matrix_rooms) {
      await failWithDiagnostic('matrix_room_missing', 'M7.3 Matrix room evidence was not observed for the swarm run', {
        required_matrix_rooms: FIXTURE.required_matrix_rooms,
        observed_matrix_rooms: roomEvidence,
        tasks: state.tasks.slice(0, SAMPLE_LIMIT),
        messages: state.messages.slice(-SAMPLE_LIMIT),
      });
    }
    if (puppetEvidence < FIXTURE.required_matrix_puppets) {
      await failWithDiagnostic('matrix_puppets_missing', 'M7.3 puppet registration evidence was not observed for all sub-agents', {
        required_matrix_puppets: FIXTURE.required_matrix_puppets,
        observed_matrix_puppets: puppetEvidence,
        tasks: state.tasks.slice(0, SAMPLE_LIMIT),
        messages: state.messages.slice(-SAMPLE_LIMIT),
      });
    }

    expect(roomEvidence).toBeGreaterThanOrEqual(FIXTURE.required_matrix_rooms);
    expect(puppetEvidence).toBeGreaterThanOrEqual(FIXTURE.required_matrix_puppets);
  });

  test('preserves swarm state after protocol and browser reload', async ({ page }) => {
    await openSessionAfterReload();
    await refreshState();
    const beforeReloadKeys = new Set(state.subagents.map(taskKey));

    if (beforeReloadKeys.size < FIXTURE.required_subagents) {
      await failWithDiagnostic('pre_reload_swarm_state_missing', 'Swarm state is incomplete before reload verification', {
        observed_subagents: beforeReloadKeys.size,
        subagents: state.subagents.slice(0, SAMPLE_LIMIT),
      });
    }

    await pointBrowserAtSession(page);
    await refreshState();
    const afterReloadKeys = new Set(state.subagents.map(taskKey));
    const missing = [...beforeReloadKeys].filter((key) => !afterReloadKeys.has(key));

    if (missing.length > 0) {
      await failWithDiagnostic('reload_lost_swarm_state', 'Reload lost one or more M7 sub-agent task records', {
        missing_task_keys: missing,
        before_subagents: [...beforeReloadKeys],
        after_subagents: [...afterReloadKeys],
        tasks: state.tasks.slice(0, SAMPLE_LIMIT),
      });
    }

    writeDiagnostic('passed', 'm7_swarm_gate_passed', 'M7.8 live swarm gate passed', {
      subagent_count: afterReloadKeys.size,
      artifact_count: artifactEvidenceCount(state.tasks, state.messages),
      matrix_room_evidence: matrixRoomEvidenceCount(state.tasks, state.messages),
      matrix_puppet_evidence: matrixPuppetEvidenceCount(state.tasks, state.messages),
      validator_evidence: validatorEvidenceCount(state.tasks, state.messages),
    });

    expect(afterReloadKeys.size).toBeGreaterThanOrEqual(FIXTURE.required_subagents);
  });
});
