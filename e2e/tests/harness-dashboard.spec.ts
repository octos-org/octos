/**
 * Operator harness dashboard acceptance (issue #468 / M4.5).
 *
 * Exercises the `/admin/harness` SPA route end-to-end with mocked backend
 * responses so the test can reliably cover:
 *   1. one success case (ready + output files)
 *   2. one validator-failure case (failed + terminal_failed child)
 *   3. one missing-artifact case (ready but zero output files)
 *
 * The dashboard is a read-only view of backend truth. The invariants asserted
 * here are the same ones spelled out in the M4.5 issue:
 *   - dashboard row count per lifecycle state matches the totals_by_lifecycle
 *     carried by `/api/admin/operator/tasks`
 *   - stale-task and missing-artifact conditions surface as visible signals
 *   - the per-gateway sources panel exposes partial-collection conditions
 */

import { test, expect, type Page } from '@playwright/test';

const ADMIN_TOKEN = process.env.OCTOS_ADMIN_TOKEN || 'harness-dashboard-test';

type LifecycleState = 'queued' | 'running' | 'verifying' | 'ready' | 'failed';

interface TaskFixture {
  profile_id: string;
  session_id: string;
  task_id: string;
  tool_name: string;
  lifecycle_state: LifecycleState;
  runtime_state?: string;
  workflow_kind?: string;
  current_phase?: string;
  child_session_key?: string;
  child_terminal_state?: string;
  child_join_state?: string;
  child_failure_action?: string;
  output_files: string[];
  error?: string;
  started_at: string;
  updated_at: string;
  completed_at?: string | null;
  derived: {
    stale: boolean;
    missing_artifact: boolean;
    validator_failed: boolean;
  };
}

interface SourceFixture {
  profile_id: string;
  status: 'ok' | 'failed' | 'missing_api_port';
  error?: string;
  api_port: number | null;
  session_count: number;
  task_count: number;
}

const FIXTURE_NOW = '2026-04-19T12:10:00Z';

function successTask(): TaskFixture {
  return {
    profile_id: 'alpha',
    session_id: 'slides-happy-1',
    task_id: 'task-alpha-success',
    tool_name: 'mofa_slides',
    lifecycle_state: 'ready',
    runtime_state: 'completed',
    workflow_kind: 'slides',
    current_phase: 'deliver_result',
    child_session_key: 'alpha:api:slides-happy-1#child-ok',
    child_terminal_state: 'completed',
    child_join_state: 'joined',
    output_files: ['pf/alpha/slides/deck.pptx'],
    started_at: '2026-04-19T12:00:00Z',
    updated_at: '2026-04-19T12:09:00Z',
    completed_at: '2026-04-19T12:09:00Z',
    derived: { stale: false, missing_artifact: false, validator_failed: false },
  };
}

function validatorFailedTask(): TaskFixture {
  return {
    profile_id: 'beta',
    session_id: 'podcast-deny-1',
    task_id: 'task-beta-validator',
    tool_name: 'podcast_generate',
    lifecycle_state: 'failed',
    runtime_state: 'failed',
    workflow_kind: 'research_podcast',
    current_phase: 'verify_contract',
    child_session_key: 'beta:api:podcast-deny-1#child-fail',
    child_terminal_state: 'terminal_failed',
    child_join_state: 'joined',
    child_failure_action: 'escalate',
    output_files: [],
    error: 'BeforeSpawnVerify denied output: missing transcript.txt',
    started_at: '2026-04-19T12:00:00Z',
    updated_at: '2026-04-19T12:08:30Z',
    derived: { stale: false, missing_artifact: false, validator_failed: true },
  };
}

function missingArtifactTask(): TaskFixture {
  return {
    profile_id: 'alpha',
    session_id: 'site-leak-1',
    task_id: 'task-alpha-missing',
    tool_name: 'site_build',
    lifecycle_state: 'ready',
    runtime_state: 'completed',
    workflow_kind: 'site',
    current_phase: 'deliver_result',
    child_session_key: 'alpha:api:site-leak-1#child-leak',
    child_terminal_state: 'completed',
    child_join_state: 'joined',
    output_files: [],
    started_at: '2026-04-19T12:00:00Z',
    updated_at: '2026-04-19T12:07:00Z',
    completed_at: '2026-04-19T12:07:00Z',
    derived: { stale: false, missing_artifact: true, validator_failed: false },
  };
}

function staleTask(): TaskFixture {
  return {
    profile_id: 'gamma',
    session_id: 'research-stale-1',
    task_id: 'task-gamma-stale',
    tool_name: 'deep_research',
    lifecycle_state: 'running',
    runtime_state: 'executing_tool',
    workflow_kind: 'deep_research',
    current_phase: 'fetch_sources',
    child_session_key: 'gamma:api:research-stale-1#child-stuck',
    output_files: [],
    started_at: '2026-04-19T11:30:00Z',
    updated_at: '2026-04-19T11:40:00Z',
    derived: { stale: true, missing_artifact: false, validator_failed: false },
  };
}

function tasksResponse(tasks: TaskFixture[], sources: SourceFixture[], partial: boolean) {
  const totals: Record<LifecycleState, number> = {
    queued: 0,
    running: 0,
    verifying: 0,
    ready: 0,
    failed: 0,
  };
  for (const t of tasks) totals[t.lifecycle_state] = (totals[t.lifecycle_state] ?? 0) + 1;
  return {
    generated_at: FIXTURE_NOW,
    stale_threshold_secs: 300,
    tasks,
    totals_by_lifecycle: totals,
    stale_count: tasks.filter((t) => t.derived.stale).length,
    missing_artifact_count: tasks.filter((t) => t.derived.missing_artifact).length,
    validator_failed_count: tasks.filter((t) => t.derived.validator_failed).length,
    sources,
    partial,
  };
}

function summaryResponse() {
  return {
    available: true,
    collection: {
      running_gateways: 3,
      gateways_with_api_port: 3,
      gateways_missing_api_port: 0,
      scrape_failures: 0,
      sources_observed: 3,
      sources_with_metrics: 3,
      sources_without_metrics: 0,
      partial: false,
    },
    totals: {
      retries: 4,
      timeouts: 1,
      result_deliveries: 2,
      workflow_phase_transitions: 6,
      duplicate_suppressions: 0,
      orphaned_child_sessions: 0,
      session_replays: 3,
      session_persists: 12,
      session_rewrites: 0,
      child_session_lifecycle: 3,
    },
    breakdowns: {
      workflow_phase_transitions: [
        { workflow_kind: 'slides', from_phase: 'queued', to_phase: 'render', count: 2 },
        {
          workflow_kind: 'research_podcast',
          from_phase: 'render',
          to_phase: 'verify_contract',
          count: 2,
        },
        { workflow_kind: 'site', from_phase: 'render', to_phase: 'deliver_result', count: 2 },
      ],
      retry_reasons: [{ reason: 'background_result_ack_timeout', count: 4 }],
      timeout_reasons: [{ reason: 'session_turn', count: 1 }],
      duplicate_suppressions: [],
      child_session_orphans: [],
      result_delivery: [],
      session_replay: [],
      session_persist: [],
      session_rewrite: [],
      child_session_lifecycle: [],
    },
    sources: [],
  };
}

async function installMocks(
  page: Page,
  opts: { tasks: TaskFixture[]; sources: SourceFixture[]; partial?: boolean },
) {
  // Mock auth so the dashboard renders the admin guard
  await page.route('**/api/auth/me', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        user: {
          id: 'admin-1',
          email: 'admin@example.com',
          name: 'Admin',
          role: 'admin',
          created_at: '2026-01-01T00:00:00Z',
          last_login_at: FIXTURE_NOW,
        },
        profile: null,
      }),
    });
  });

  await page.route('**/api/admin/operator/summary', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(summaryResponse()),
    });
  });

  await page.route('**/api/admin/operator/tasks', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(tasksResponse(opts.tasks, opts.sources, opts.partial ?? false)),
    });
  });

  // Every other admin call used by the rendered page/chrome — return an empty
  // shape so no navigation or layout code crashes.
  await page.route('**/api/admin/overview', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ total_profiles: 0, running: 0, stopped: 0, profiles: [] }),
    });
  });
}

async function authenticate(page: Page) {
  await page.addInitScript((token) => {
    localStorage.setItem('octos_session_token', token);
    localStorage.setItem('octos_auth_token', token);
  }, ADMIN_TOKEN);
}

test.describe('Operator harness dashboard (#468)', () => {
  test.setTimeout(60_000);

  test('renders success, validator-failure, and missing-artifact rows', async ({ page }) => {
    await authenticate(page);
    await installMocks(page, {
      tasks: [successTask(), validatorFailedTask(), missingArtifactTask()],
      sources: [
        { profile_id: 'alpha', status: 'ok', api_port: 51001, session_count: 2, task_count: 2 },
        { profile_id: 'beta', status: 'ok', api_port: 51002, session_count: 1, task_count: 1 },
      ],
    });

    await page.goto('/admin/harness', { waitUntil: 'domcontentloaded' });
    await page.waitForSelector("[data-testid='harness-page']", { timeout: 10_000 });

    // 1. Success row
    const successRow = page.locator(
      "[data-testid='harness-task-row'][data-lifecycle='ready'][data-missing-artifact='false']",
    );
    await expect(successRow).toHaveCount(1);
    await expect(successRow).toContainText('mofa_slides');
    await expect(successRow).toContainText('deck.pptx');

    // 2. Validator failure row
    const failedRow = page.locator(
      "[data-testid='harness-task-row'][data-lifecycle='failed'][data-validator-failed='true']",
    );
    await expect(failedRow).toHaveCount(1);
    await expect(failedRow).toContainText('podcast_generate');
    await expect(failedRow.locator("[data-testid='badge-validator-failed']")).toBeVisible();

    // 3. Missing artifact row
    const missingRow = page.locator(
      "[data-testid='harness-task-row'][data-missing-artifact='true']",
    );
    await expect(missingRow).toHaveCount(1);
    await expect(missingRow.locator("[data-testid='badge-missing-artifact']")).toBeVisible();
    await expect(missingRow).toContainText('site_build');

    // Dashboard row count per state matches totals_by_lifecycle
    const readyCount = await page
      .locator("[data-testid='count-ready']")
      .getAttribute('data-active');
    expect(readyCount).not.toBeNull();
    await expect(page.locator("[data-testid='count-ready']")).toContainText('2');
    await expect(page.locator("[data-testid='count-failed']")).toContainText('1');
    await expect(page.locator("[data-testid='count-running']")).toContainText('0');
    await expect(page.locator("[data-testid='count-missing-artifact']")).toContainText('1');
    await expect(page.locator("[data-testid='count-validator-failed']")).toContainText('1');
    await expect(page.locator("[data-testid='count-stale']")).toContainText('0');

    // Per-state row count (table) matches the summary card
    const rows = page.locator("[data-testid='harness-task-row']");
    await expect(rows).toHaveCount(3);
  });

  test('filter by lifecycle state narrows the task list', async ({ page }) => {
    await authenticate(page);
    await installMocks(page, {
      tasks: [successTask(), validatorFailedTask(), missingArtifactTask()],
      sources: [
        { profile_id: 'alpha', status: 'ok', api_port: 51001, session_count: 2, task_count: 2 },
        { profile_id: 'beta', status: 'ok', api_port: 51002, session_count: 1, task_count: 1 },
      ],
    });
    await page.goto('/admin/harness', { waitUntil: 'domcontentloaded' });
    await page.waitForSelector("[data-testid='harness-page']");

    await page.locator("[data-testid='count-failed']").click();
    const rows = page.locator("[data-testid='harness-task-row']");
    await expect(rows).toHaveCount(1);
    await expect(rows.first()).toContainText('podcast_generate');

    await page.locator("[data-testid='count-all']").click();
    await expect(rows).toHaveCount(3);
  });

  test('surfaces partial-collection banner when a gateway fails to report', async ({ page }) => {
    await authenticate(page);
    await installMocks(page, {
      tasks: [successTask(), staleTask()],
      sources: [
        { profile_id: 'alpha', status: 'ok', api_port: 51001, session_count: 1, task_count: 1 },
        {
          profile_id: 'gamma',
          status: 'failed',
          error: 'http 502',
          api_port: 51003,
          session_count: 0,
          task_count: 0,
        },
      ],
      partial: true,
    });
    await page.goto('/admin/harness', { waitUntil: 'domcontentloaded' });
    await page.waitForSelector("[data-testid='harness-page']");

    await expect(page.locator("[data-testid='harness-partial-banner']")).toBeVisible();

    const gammaSource = page.locator(
      "[data-testid='harness-source-row'][data-profile-id='gamma']",
    );
    await expect(gammaSource).toHaveAttribute('data-status', 'failed');
    await expect(gammaSource).toContainText('http 502');

    // Stale row surfaces its stale badge even though the value lives under the
    // row's data-stale attribute.
    const staleRow = page.locator("[data-testid='harness-task-row'][data-stale='true']");
    await expect(staleRow).toHaveCount(1);
    await expect(staleRow.locator("[data-testid='badge-stale']")).toBeVisible();
  });
});
