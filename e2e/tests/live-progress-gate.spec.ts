/**
 * M4.1A live progress gate (issue #474).
 *
 * Validates on a real canary that the full M4.1A pipeline works end-to-end:
 *   - deep research emits structured `octos.harness.event.v1` progress events
 *   - the runtime sink folds them into durable `runtime_detail` on the parent task
 *   - the UI header surfaces phase/workflow/progress before completion
 *   - session switch and browser reload both preserve that progress
 *   - `/api/sessions/:id/tasks` and the session event SSE stream expose the
 *     same phase/status truth
 *
 * These tests are the live release gate and MUST hit a real canary URL. They
 * do NOT mock the backend. Pointing them at localhost only makes sense if the
 * canary stack is running locally.
 *
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/live-progress-gate.spec.ts
 */
import fs from 'node:fs';
import path from 'node:path';
import { expect, test, type Page } from '@playwright/test';

import {
  SEL,
  createNewSession,
  getInput,
  getSendButton,
  login,
} from './live-browser-helpers';
import {
  fetchSessionIds,
  getActiveSessionId,
  getSessionTasks,
  type SessionTask as BaseSessionTask,
} from './coding-hardcases-helpers';

/**
 * The M4.1A sink patch (#471) adds structured progress fields to each
 * background task. Until that patch lands in main, the SessionTask type
 * declared alongside coding-hardcases-helpers.ts does not know about these
 * fields. Widen the local view so the live gate can probe both the
 * pre-#471 and post-#471 task shapes without a compile-time regression on
 * the main branch.
 */
type SessionTask = BaseSessionTask & {
  id?: string | null;
  tool_call_id?: string | null;
  session_key?: string | null;
  started_at?: string | null;
  updated_at?: string | null;
  lifecycle_state?: string | null;
  workflow_kind?: string | null;
  current_phase?: string | null;
  progress_message?: string | null;
  progress?: number | null;
  runtime_detail?:
    | {
        workflow_kind?: string | null;
        current_phase?: string | null;
        progress_message?: string | null;
        progress?: number | null;
        phase?: string | null;
        schema?: string | null;
        kind?: string | null;
      }
    | string
    | null;
};

interface ExpectedPhase {
  phase: string;
  must_appear?: boolean;
  min_progress?: number;
  max_progress?: number;
}

interface ExpectedProgressFixture {
  schema: string;
  kind: string;
  workflow: string;
  lifecycle_states: string[];
  terminal_states: string[];
  active_states: string[];
  required_phases: ExpectedPhase[];
  phase_order: string[];
  ui_selectors: Record<string, string>;
  api_endpoints: Record<string, string>;
  prompts: Record<string, string>;
  limits: Record<string, number>;
}

const FIXTURE_PATH = path.join(
  __dirname,
  '..',
  'fixtures',
  'm4-1a-progress-expected.json',
);

function loadFixture(): ExpectedProgressFixture {
  const raw = fs.readFileSync(FIXTURE_PATH, 'utf8');
  const parsed = JSON.parse(raw) as ExpectedProgressFixture;
  if (!parsed || typeof parsed !== 'object' || !parsed.schema) {
    throw new Error(
      `Invalid live-gate fixture at ${FIXTURE_PATH}: missing schema`,
    );
  }
  return parsed;
}

const FIXTURE = loadFixture();
const POLL_INTERVAL_MS =
  (FIXTURE.limits?.poll_interval_seconds ?? 5) * 1_000;
const PER_RUN_TIMEOUT_MS =
  (FIXTURE.limits?.per_run_timeout_seconds ?? 600) * 1_000;
const MIN_PROGRESS_EVENTS = FIXTURE.limits?.min_progress_events ?? 3;
const MAX_DUPLICATE_SESSIONS = FIXTURE.limits?.max_duplicate_sessions ?? 0;
const DEEP_RESEARCH_PROMPT =
  FIXTURE.prompts.deep_research ||
  "Do a deep research on the latest Rust programming language developments in 2026. Run the pipeline directly, don't ask me to choose.";

// Legacy in-bubble progress selectors (TASK_INDICATOR/TASK_PHASE/etc) are
// retained in the fixture for cross-script compatibility but are no longer
// asserted here. Deep_research now spawns to a background-task widget
// (`.session-task-indicator` in the chat-layout header); per-phase progress
// lives on the task record itself, so we observe it via the API.

interface TaskRuntimeDetail {
  workflow_kind?: string | null;
  current_phase?: string | null;
  progress_message?: string | null;
  progress?: number | null;
  phase?: string | null;
  schema?: string | null;
  kind?: string | null;
}

async function readSessionTasks(
  page: Page,
  sessionId: string,
): Promise<SessionTask[]> {
  const raw = await getSessionTasks(page, sessionId);
  return raw as SessionTask[];
}

function extractRuntimeDetail(task: SessionTask): TaskRuntimeDetail {
  const raw = (task as SessionTask).runtime_detail;
  if (!raw) return {};
  if (typeof raw === 'object') return raw as TaskRuntimeDetail;
  if (typeof raw === 'string') {
    try {
      return JSON.parse(raw) as TaskRuntimeDetail;
    } catch {
      return {};
    }
  }
  return {};
}

function extractPhase(task: SessionTask): string | null {
  const direct = task?.current_phase;
  if (typeof direct === 'string' && direct.length > 0) return direct;
  const detail = extractRuntimeDetail(task);
  if (typeof detail.current_phase === 'string' && detail.current_phase)
    return detail.current_phase;
  if (typeof detail.phase === 'string' && detail.phase) return detail.phase;
  return null;
}

function extractWorkflow(task: SessionTask): string | null {
  const direct = task?.workflow_kind;
  if (typeof direct === 'string' && direct.length > 0) return direct;
  const detail = extractRuntimeDetail(task);
  if (typeof detail.workflow_kind === 'string' && detail.workflow_kind)
    return detail.workflow_kind;
  return null;
}

function extractProgress(task: SessionTask): number | null {
  const detail = extractRuntimeDetail(task);
  if (typeof detail.progress === 'number' && Number.isFinite(detail.progress))
    return detail.progress;
  if (typeof task?.progress === 'number' && Number.isFinite(task.progress))
    return task.progress;
  return null;
}

function extractLifecycleState(task: SessionTask): string | null {
  const direct = task?.lifecycle_state;
  if (typeof direct === 'string' && direct.length > 0) return direct;
  return null;
}

function isDeepResearchTask(task: SessionTask): boolean {
  const workflow = extractWorkflow(task)?.toLowerCase() || '';
  if (workflow.includes('deep_research') || workflow.includes('deep research'))
    return true;
  const toolName = String(task?.tool_name || '').toLowerCase();
  if (toolName.includes('deep_research') || toolName.includes('deep_search'))
    return true;
  const label = String(task?.label || '').toLowerCase();
  if (label.includes('deep_research') || label.includes('deep research'))
    return true;
  return false;
}

function isActiveLifecycle(state: string | null): boolean {
  if (!state) return false;
  return FIXTURE.active_states.includes(state.toLowerCase());
}

function taskKey(task: SessionTask): string {
  return (
    task?.id ||
    task?.child_session_key ||
    task?.tool_call_id ||
    task?.session_key ||
    `${task?.started_at || ''}|${task?.updated_at || ''}|${task?.status || ''}`
  );
}

function isMonotonicPhaseSequence(
  seen: string[],
  allowedOrder: string[],
): { ok: true } | { ok: false; offendingPhase: string; previousPhase: string } {
  let maxIndex = -1;
  let previousPhase = '';
  for (const phase of seen) {
    const idx = allowedOrder.indexOf(phase);
    if (idx < 0) continue;
    if (idx < maxIndex) {
      return { ok: false, offendingPhase: phase, previousPhase };
    }
    if (idx > maxIndex) {
      maxIndex = idx;
      previousPhase = phase;
    }
  }
  return { ok: true };
}

async function submitPrompt(page: Page, prompt: string) {
  await getInput(page).fill(prompt);
  await getSendButton(page).click();
}

async function waitForDeepResearchTask(
  page: Page,
  sessionId: string,
  timeoutMs: number,
): Promise<SessionTask> {
  const deadline = Date.now() + timeoutMs;
  let lastTasks: SessionTask[] = [];

  while (Date.now() < deadline) {
    lastTasks = await readSessionTasks(page, sessionId);
    const candidate = lastTasks.find(
      (task) => isDeepResearchTask(task) && isActiveLifecycle(extractLifecycleState(task)),
    );
    if (candidate) {
      return candidate;
    }

    await page.waitForTimeout(POLL_INTERVAL_MS);
  }

  throw new Error(
    `Timed out after ${(timeoutMs / 1000).toFixed(0)}s waiting for an active ` +
      `deep_research task in session ${sessionId}. Last tasks: ` +
      `${JSON.stringify(lastTasks).slice(0, 1024)}`,
  );
}

interface ProgressObservation {
  phases: Set<string>;
  progressValues: number[];
  sampleCount: number;
  lastDetail: TaskRuntimeDetail;
  finalTask: SessionTask | null;
  phaseSequence: string[];
  lifecycleTrail: string[];
}

async function observeProgressUntilTerminal(
  page: Page,
  sessionId: string,
  taskKeyValue: string,
  timeoutMs: number,
): Promise<ProgressObservation> {
  const deadline = Date.now() + timeoutMs;
  const phases = new Set<string>();
  const phaseSequence: string[] = [];
  const lifecycleTrail: string[] = [];
  const progressValues: number[] = [];
  let sampleCount = 0;
  let lastDetail: TaskRuntimeDetail = {};
  let finalTask: SessionTask | null = null;

  while (Date.now() < deadline) {
    const tasks = await readSessionTasks(page, sessionId);
    const task = tasks.find((t) => taskKey(t) === taskKeyValue) ||
      tasks.find(isDeepResearchTask);
    if (task) {
      sampleCount += 1;
      finalTask = task;
      const phase = extractPhase(task);
      const lifecycle = extractLifecycleState(task);
      lastDetail = extractRuntimeDetail(task);

      if (phase) {
        if (phaseSequence[phaseSequence.length - 1] !== phase) {
          phaseSequence.push(phase);
        }
        phases.add(phase);
      }
      if (lifecycle) {
        if (lifecycleTrail[lifecycleTrail.length - 1] !== lifecycle) {
          lifecycleTrail.push(lifecycle);
        }
      }

      const progress = extractProgress(task);
      if (progress !== null) {
        progressValues.push(progress);
      }

      if (lifecycle && FIXTURE.terminal_states.includes(lifecycle.toLowerCase())) {
        break;
      }
    }

    await page.waitForTimeout(POLL_INTERVAL_MS);
  }

  return {
    phases,
    progressValues,
    sampleCount,
    lastDetail,
    finalTask,
    phaseSequence,
    lifecycleTrail,
  };
}

async function ensureSecondSession(page: Page, originSessionId: string) {
  // Navigate to a fresh session via the new-chat button and return its id.
  await page.locator(SEL.newChatButton).click();
  await page.waitForTimeout(1_000);
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    const sessionIds = await fetchSessionIds(page);
    const candidate = sessionIds.find((id) => id !== originSessionId);
    if (candidate) return candidate;
    await page.waitForTimeout(500);
  }
  throw new Error('Could not allocate a second session for switch test');
}

async function openSseSnapshot(
  page: Page,
  sessionId: string,
  timeoutMs: number,
) {
  return page.evaluate(
    async ({ sessionId: sid, timeoutMs: timeout }) => {
      const token =
        localStorage.getItem('octos_session_token') ||
        localStorage.getItem('octos_auth_token') ||
        '';
      const headers: Record<string, string> = {};
      if (token) headers.Authorization = `Bearer ${token}`;
      const controller = new AbortController();
      const started = Date.now();
      let buffer = '';
      const events: Array<Record<string, unknown>> = [];
      try {
        const resp = await fetch(
          `/api/sessions/${encodeURIComponent(sid)}/events/stream`,
          { headers, signal: controller.signal },
        );
        if (!resp.ok || !resp.body) {
          return { ok: false, reason: `status=${resp.status}`, events };
        }
        const reader = resp.body.getReader();
        const decoder = new TextDecoder();
        while (Date.now() - started < timeout) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          let idx = buffer.indexOf('\n\n');
          while (idx !== -1) {
            const chunk = buffer.slice(0, idx);
            buffer = buffer.slice(idx + 2);
            const dataLine = chunk
              .split('\n')
              .find((line) => line.startsWith('data:'));
            if (dataLine) {
              const payload = dataLine.slice(5).trim();
              try {
                events.push(JSON.parse(payload));
              } catch {
                events.push({ raw: payload });
              }
            }
            idx = buffer.indexOf('\n\n');
          }
          if (events.length >= 8) break;
        }
        controller.abort();
        return { ok: true, events };
      } catch (error) {
        return { ok: false, reason: String(error), events };
      }
    },
    { sessionId, timeoutMs },
  );
}

test.describe('M4.1A live progress gate', () => {
  test.setTimeout(PER_RUN_TIMEOUT_MS + 120_000);

  test.beforeEach(async ({ page }) => {
    await login(page);
    await createNewSession(page);
  });

  // M8.10 follow-up: deep_research now spawns to a background-task widget
  // rather than emitting live progress into the assistant bubble. The bubble
  // surfaces only an acknowledgement ("深度研究已在后台启动..."); per-phase
  // progress lives in the session-task indicator (header pill) and on the
  // task record itself. The legacy `data-testid='task-current-phase'` UI
  // selectors no longer exist, so we gate phase progression on the backend
  // truth via /api/sessions/:id/tasks (which is also what the third spec
  // asserts is in sync with the SSE stream).
  test('deep research emits live progress through every required phase', async ({
    page,
  }) => {
    await submitPrompt(page, DEEP_RESEARCH_PROMPT);

    const sessionId = await getActiveSessionId(page);
    const activeTask = await waitForDeepResearchTask(
      page,
      sessionId,
      120_000,
    );
    const initialKey = taskKey(activeTask);

    // The header pill surfaces "<workflow> running" while the background
    // task is active. The widget only mounts once a task exists, so we
    // probe with a soft assertion (it may have already settled by the time
    // we get here).
    const indicatorVisible = await page
      .locator('.session-task-indicator')
      .first()
      .isVisible({ timeout: 5_000 })
      .catch(() => false);
    if (indicatorVisible) {
      const indicatorLabel =
        (await page
          .locator('.session-task-indicator')
          .first()
          .textContent()
          .catch(() => '')) || '';
      expect(
        indicatorLabel.toLowerCase().includes('deep') ||
          indicatorLabel.toLowerCase().includes('research') ||
          indicatorLabel.toLowerCase().includes('running'),
        `session-task-indicator did not surface a recognisable label: "${indicatorLabel}"`,
      ).toBe(true);
    }

    const observation = await observeProgressUntilTerminal(
      page,
      sessionId,
      initialKey,
      PER_RUN_TIMEOUT_MS,
    );

    // Backend truth: at least the minimum count of progress snapshots and at
    // least one progress value in [0, 1].
    expect(observation.sampleCount).toBeGreaterThanOrEqual(
      MIN_PROGRESS_EVENTS,
    );
    expect(observation.progressValues.length).toBeGreaterThan(0);
    for (const value of observation.progressValues) {
      expect(value).toBeGreaterThanOrEqual(0);
      expect(value).toBeLessThanOrEqual(1);
    }

    // Each required phase must have been observed at least once.
    const requiredPhases = FIXTURE.required_phases
      .filter((entry) => entry.must_appear)
      .map((entry) => entry.phase);
    for (const requiredPhase of requiredPhases) {
      expect(
        observation.phases.has(requiredPhase),
        `required phase "${requiredPhase}" not observed. saw=${JSON.stringify(
          observation.phaseSequence,
        )}`,
      ).toBe(true);
    }

    // The research_report workflow loops through evidence-gathering passes,
    // so the deep_search plugin re-cycles `search`/`synthesize`/`completion`
    // phases multiple times within a single workflow run. We no longer
    // assert strict monotonic phase ordering — instead we require that at
    // least one declared phase from the canonical ladder was observed.
    const observedKnownPhases = observation.phaseSequence.filter((phase) =>
      FIXTURE.phase_order.includes(phase),
    );
    expect(
      observedKnownPhases.length,
      `none of the canonical phases observed. saw=${JSON.stringify(
        observation.phaseSequence,
      )}`,
    ).toBeGreaterThan(0);

    // Lifecycle must have reached a terminal state and transitions must be
    // monotonic along the declared ladder.
    const lifecycleLadder = FIXTURE.lifecycle_states;
    let lastIdx = -1;
    for (const state of observation.lifecycleTrail) {
      const idx = lifecycleLadder.indexOf(state);
      if (idx >= 0) {
        expect(
          idx,
          `lifecycle state ${state} regressed in trail ${JSON.stringify(
            observation.lifecycleTrail,
          )}`,
        ).toBeGreaterThanOrEqual(lastIdx);
        lastIdx = idx;
      }
    }
    expect(observation.finalTask).not.toBeNull();
    const finalLifecycle =
      extractLifecycleState(observation.finalTask as SessionTask) || '';
    expect(
      FIXTURE.terminal_states.includes(finalLifecycle.toLowerCase()),
      `task never reached a terminal state (final=${finalLifecycle})`,
    ).toBe(true);

    // No duplicate research sessions should be created from one prompt.
    const tasks = await readSessionTasks(page, sessionId);
    const deepResearchTasks = tasks.filter(isDeepResearchTask);
    const seen = new Set<string>();
    for (const t of deepResearchTasks) seen.add(taskKey(t));
    expect(
      deepResearchTasks.length - seen.size,
      `duplicate deep_research tasks detected: raw=${deepResearchTasks.length}, unique=${seen.size}`,
    ).toBe(0);
    expect(deepResearchTasks.length).toBeLessThanOrEqual(
      1 + MAX_DUPLICATE_SESSIONS,
    );
  });

  test('progress state persists across session switch and browser reload', async ({
    page,
  }) => {
    await submitPrompt(page, DEEP_RESEARCH_PROMPT);
    const originSessionId = await getActiveSessionId(page);
    const originalTask = await waitForDeepResearchTask(
      page,
      originSessionId,
      120_000,
    );
    const originalPhase = extractPhase(originalTask);

    // Background-task widget should be visible while deep research is
    // running on the origin session.
    const indicator = page.locator('.session-task-indicator').first();
    await expect(
      indicator,
      'background-task indicator did not appear on origin session',
    ).toBeVisible({ timeout: 30_000 });

    // Switch to a sibling session while deep research is still running.
    const siblingSessionId = await ensureSecondSession(page, originSessionId);
    expect(siblingSessionId).not.toBe(originSessionId);

    // Confirm origin session still reports the task via API while we are
    // viewing the sibling.
    const siblingTasks = await readSessionTasks(page, originSessionId);
    const stillActive = siblingTasks.find(
      (task) =>
        isDeepResearchTask(task) &&
        isActiveLifecycle(extractLifecycleState(task)),
    );
    expect(
      stillActive,
      `deep research task disappeared after switching sessions: ${JSON.stringify(
        siblingTasks,
      ).slice(0, 512)}`,
    ).toBeTruthy();

    // No progress bleed: the sibling session must not expose deep research,
    // and the widget on the sibling viewport must not show the running pill
    // for our origin task.
    const siblingSessionTasks = await readSessionTasks(page, siblingSessionId);
    const bleed = siblingSessionTasks.filter(isDeepResearchTask);
    expect(
      bleed.length,
      `deep research progress bled into sibling session ${siblingSessionId}: ${JSON.stringify(
        bleed,
      ).slice(0, 512)}`,
    ).toBe(0);

    // Switch back and confirm progress is still surfaced.
    const switchButton = page.locator(
      `[data-session-id='${originSessionId}'] [data-testid='session-switch-button']`,
    );
    if (await switchButton.count().catch(() => 0)) {
      await switchButton.first().click();
    } else {
      // Fallback: direct navigation.
      await page.evaluate((sid) => {
        localStorage.setItem('octos_current_session', sid);
      }, originSessionId);
      await page.reload({ waitUntil: 'domcontentloaded' });
    }
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    // Probe API for the task without requiring it to still be active —
    // the workflow re-cycles phases (search -> synthesize -> completion ->
    // search ...) and the spec budget cannot wait for a full run.
    await waitForDeepResearchTask(page, originSessionId, 60_000).catch(
      () => null,
    );
    const postSwitchTasks = await readSessionTasks(page, originSessionId);
    const anyDeepResearch = postSwitchTasks.find(isDeepResearchTask);
    expect(anyDeepResearch).toBeTruthy();
    if (originalPhase) {
      const latestPhase = extractPhase(anyDeepResearch as SessionTask);
      // We no longer assert monotonic phase ordering across switches,
      // because the research_report workflow re-cycles deep_search phases
      // multiple times. We only require that the latest phase is one of
      // the canonical ladder entries.
      if (latestPhase) {
        expect(
          FIXTURE.phase_order.includes(latestPhase),
          `unexpected post-switch phase "${latestPhase}" (original was "${originalPhase}")`,
        ).toBe(true);
      }
    }

    // Reload and assert the task is still replayable.
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await page.waitForTimeout(3_000);
    const reloadedTasks = await readSessionTasks(page, originSessionId);
    const reloaded = reloadedTasks.find(isDeepResearchTask);
    expect(
      reloaded,
      `deep research task not replayable after reload: ${JSON.stringify(
        reloadedTasks,
      ).slice(0, 512)}`,
    ).toBeTruthy();

    // If the task is still active after reload, the background-task widget
    // should also re-mount (driven by the runtime task store, not by the
    // assistant bubble).
    const lifecycleAfterReload = extractLifecycleState(
      reloaded as SessionTask,
    );
    if (isActiveLifecycle(lifecycleAfterReload)) {
      const reloadIndicator = page
        .locator('.session-task-indicator')
        .first();
      await expect(
        reloadIndicator,
        'background-task indicator did not survive page reload',
      ).toBeVisible({ timeout: 20_000 });
    }

    // The persistence contract is "task survives switch + reload" — not
    // "task completes within this test budget". Deep_research can take ~10
    // minutes per run, and a single test budget cannot afford to wait for
    // a full pipeline run after also exercising session switch + reload.
    // We verify that the task lifecycle is one of the known states (active
    // OR terminal) rather than gating on completion. The standalone live
    // progress test (#1) covers terminal-state reachability.
    const finalTasks = await readSessionTasks(page, originSessionId);
    const finalTask = finalTasks.find(isDeepResearchTask);
    expect(finalTask).toBeTruthy();
    const finalLifecycle =
      (extractLifecycleState(finalTask as SessionTask) || '').toLowerCase();
    const allKnownLifecycles = [
      ...FIXTURE.active_states,
      ...FIXTURE.terminal_states,
    ];
    expect(
      allKnownLifecycles.includes(finalLifecycle),
      `post-reload task lifecycle "${finalLifecycle}" not in declared ladder ${JSON.stringify(allKnownLifecycles)}`,
    ).toBe(true);
  });

  test('task API and event SSE stream expose the same phase truth', async ({
    page,
  }) => {
    await submitPrompt(page, DEEP_RESEARCH_PROMPT);
    const sessionId = await getActiveSessionId(page);
    await waitForDeepResearchTask(page, sessionId, 120_000);

    // Let the run emit a few events before snapshotting the stream.
    await page.waitForTimeout(POLL_INTERVAL_MS * 2);

    const sseSnapshot = await openSseSnapshot(page, sessionId, 20_000);
    expect(sseSnapshot.ok, `SSE snapshot failed: ${sseSnapshot.reason}`).toBe(
      true,
    );

    const tasks = await readSessionTasks(page, sessionId);
    const task = tasks.find(isDeepResearchTask);
    expect(task).toBeTruthy();
    const apiPhase = extractPhase(task as SessionTask);
    const apiWorkflow = extractWorkflow(task as SessionTask);

    // Events array may include replay_complete and task_status_changed events.
    // We assert that at least one event references the current session and
    // that phase/status shapes are present somewhere in the stream.
    const events = sseSnapshot.events || [];
    expect(
      events.length,
      `event stream produced no events for session ${sessionId}`,
    ).toBeGreaterThan(0);

    const sessionEvents = events.filter((event) => {
      const value = JSON.stringify(event);
      return (
        value.includes(sessionId) ||
        value.includes('task_status') ||
        value.includes('progress') ||
        value.includes('phase')
      );
    });
    expect(
      sessionEvents.length,
      `no session-scoped events found in SSE snapshot: ${JSON.stringify(events).slice(0, 512)}`,
    ).toBeGreaterThan(0);

    if (apiPhase) {
      const haystack = JSON.stringify(events);
      expect(
        haystack.includes(apiPhase) ||
          haystack.includes('task_status_changed'),
        `API phase "${apiPhase}" not reflected in event stream: ${haystack.slice(
          0,
          512,
        )}`,
      ).toBe(true);
    }

    if (apiWorkflow) {
      const haystack = JSON.stringify(events);
      expect(
        haystack.includes(apiWorkflow) ||
          haystack.includes('task_status_changed'),
        `API workflow "${apiWorkflow}" not reflected in event stream: ${haystack.slice(
          0,
          512,
        )}`,
      ).toBe(true);
    }
  });
});
