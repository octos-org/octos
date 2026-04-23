/**
 * Coding-loop dashboard acceptance (issue #495 / M6.8).
 *
 * Exercises the `/admin/harness` SPA route with mocked backend responses so
 * the five named acceptance cases from #495 each drive a specific panel on
 * the operator dashboard:
 *
 *   1. success case                  — task finishes clean, no loop warnings
 *   2. validator failure             — validator_failed row surfaces a badge
 *   3. missing artifact              — ready row with no output files
 *   4. retry escalation              — a variant with exhausted_share > 50%
 *   5. credential rotation           — credential pool panel shows rotations
 *
 * Invariants (from #495):
 *   - dashboard reads existing + new backend APIs only
 *   - adding a new HarnessError variant auto-propagates (backend supplies
 *     the list; UI renders what's present)
 *   - delegation panel stub is clearly labeled "Pending M6.7 merge"
 *   - card counts match backend counter deltas within one scrape interval
 */

import { test, expect, type Page } from '@playwright/test';

const ADMIN_TOKEN = process.env.OCTOS_ADMIN_TOKEN || 'coding-loop-dashboard-test';
const FIXTURE_NOW = '2026-04-19T12:10:00Z';

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

interface BreakdownRow {
  [dim: string]: string | number;
  count: number;
}

interface SummaryFixture {
  totals?: Partial<Record<string, number>>;
  breakdowns?: Partial<Record<string, BreakdownRow[]>>;
}

function successTask(): TaskFixture {
  return {
    profile_id: 'alpha',
    session_id: 'loop-success-1',
    task_id: 'task-alpha-success',
    tool_name: 'code_run',
    lifecycle_state: 'ready',
    runtime_state: 'completed',
    workflow_kind: 'coding',
    current_phase: 'deliver_result',
    child_session_key: 'alpha:api:loop-success-1#child-ok',
    child_terminal_state: 'completed',
    child_join_state: 'joined',
    output_files: ['pf/alpha/coding/diff.patch'],
    started_at: '2026-04-19T12:00:00Z',
    updated_at: '2026-04-19T12:09:00Z',
    completed_at: '2026-04-19T12:09:00Z',
    derived: { stale: false, missing_artifact: false, validator_failed: false },
  };
}

function validatorFailedTask(): TaskFixture {
  return {
    profile_id: 'beta',
    session_id: 'loop-val-1',
    task_id: 'task-beta-validator',
    tool_name: 'code_run',
    lifecycle_state: 'failed',
    runtime_state: 'failed',
    workflow_kind: 'coding',
    current_phase: 'verify_contract',
    child_session_key: 'beta:api:loop-val-1#child-fail',
    child_terminal_state: 'terminal_failed',
    child_join_state: 'joined',
    child_failure_action: 'escalate',
    output_files: [],
    error: 'validator denied output: cargo test failed',
    started_at: '2026-04-19T12:00:00Z',
    updated_at: '2026-04-19T12:08:30Z',
    derived: { stale: false, missing_artifact: false, validator_failed: true },
  };
}

function missingArtifactTask(): TaskFixture {
  return {
    profile_id: 'alpha',
    session_id: 'loop-missing-1',
    task_id: 'task-alpha-missing',
    tool_name: 'code_run',
    lifecycle_state: 'ready',
    runtime_state: 'completed',
    workflow_kind: 'coding',
    current_phase: 'deliver_result',
    child_session_key: 'alpha:api:loop-missing-1#child-leak',
    child_terminal_state: 'completed',
    child_join_state: 'joined',
    output_files: [],
    started_at: '2026-04-19T12:00:00Z',
    updated_at: '2026-04-19T12:07:00Z',
    completed_at: '2026-04-19T12:07:00Z',
    derived: { stale: false, missing_artifact: true, validator_failed: false },
  };
}

function retryEscalationTask(): TaskFixture {
  return {
    profile_id: 'gamma',
    session_id: 'loop-retry-1',
    task_id: 'task-gamma-retry',
    tool_name: 'code_run',
    lifecycle_state: 'failed',
    runtime_state: 'failed',
    workflow_kind: 'coding',
    current_phase: 'plan_step',
    child_session_key: 'gamma:api:loop-retry-1#child-retry',
    output_files: [],
    error: 'bucket exhausted: rate_limited (5/5)',
    started_at: '2026-04-19T11:50:00Z',
    updated_at: '2026-04-19T12:05:00Z',
    derived: { stale: false, missing_artifact: false, validator_failed: false },
  };
}

function credentialRotationTask(): TaskFixture {
  return {
    profile_id: 'delta',
    session_id: 'loop-cred-1',
    task_id: 'task-delta-cred',
    tool_name: 'code_run',
    lifecycle_state: 'running',
    runtime_state: 'executing_tool',
    workflow_kind: 'coding',
    current_phase: 'call_llm',
    child_session_key: 'delta:api:loop-cred-1#child-cred',
    output_files: [],
    started_at: '2026-04-19T12:00:00Z',
    updated_at: '2026-04-19T12:09:30Z',
    derived: { stale: false, missing_artifact: false, validator_failed: false },
  };
}

function tasksResponse(
  tasks: TaskFixture[],
  sources: SourceFixture[],
  partial: boolean,
) {
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

function defaultSummary(extra: SummaryFixture = {}) {
  const totals: Record<string, number> = {
    retries: 0,
    timeouts: 0,
    result_deliveries: 0,
    workflow_phase_transitions: 0,
    duplicate_suppressions: 0,
    orphaned_child_sessions: 0,
    session_replays: 0,
    session_persists: 0,
    session_rewrites: 0,
    child_session_lifecycle: 0,
    loop_errors: 0,
    loop_retries: 0,
    compaction_preservation_violations: 0,
    credential_rotations: 0,
    routing_decisions: 0,
    ...extra.totals,
  };
  const breakdowns: Record<string, BreakdownRow[]> = {
    workflow_phase_transitions: [],
    retry_reasons: [],
    timeout_reasons: [],
    duplicate_suppressions: [],
    child_session_orphans: [],
    result_delivery: [],
    session_replay: [],
    session_persist: [],
    session_rewrite: [],
    child_session_lifecycle: [],
    workspace_validator_runs: [],
    loop_errors: [],
    loop_retries: [],
    compaction_preservation_violations: [],
    credential_rotations: [],
    routing_decisions: [],
    ...extra.breakdowns,
  };
  return {
    available: true,
    collection: {
      running_gateways: 1,
      gateways_with_api_port: 1,
      gateways_missing_api_port: 0,
      scrape_failures: 0,
      sources_observed: 1,
      sources_with_metrics: 1,
      sources_without_metrics: 0,
      partial: false,
    },
    totals,
    breakdowns,
    sources: [],
  };
}

async function installMocks(
  page: Page,
  opts: {
    tasks: TaskFixture[];
    sources: SourceFixture[];
    partial?: boolean;
    summary?: SummaryFixture;
  },
) {
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
      body: JSON.stringify(defaultSummary(opts.summary ?? {})),
    });
  });

  await page.route('**/api/admin/operator/tasks', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(tasksResponse(opts.tasks, opts.sources, opts.partial ?? false)),
    });
  });

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

test.describe('Coding-loop dashboard (#495 / M6.8)', () => {
  test.setTimeout(60_000);

  test('success case: clean ready task with no loop warnings', async ({ page }) => {
    await authenticate(page);
    await installMocks(page, {
      tasks: [successTask()],
      sources: [
        { profile_id: 'alpha', status: 'ok', api_port: 51001, session_count: 1, task_count: 1 },
      ],
      summary: {
        totals: { routing_decisions: 4, credential_rotations: 1 },
        breakdowns: {
          routing_decisions: [
            { tier: 'cheap', lane: 'default', count: 3 },
            { tier: 'strong', lane: 'default', count: 1 },
          ],
          credential_rotations: [
            { reason: 'initial_acquire', strategy: 'fill_first', count: 1 },
          ],
        },
      },
    });
    await page.goto('/admin/harness', { waitUntil: 'domcontentloaded' });
    await page.waitForSelector("[data-testid='harness-page']");

    const rows = page.locator("[data-testid='harness-task-row']");
    await expect(rows).toHaveCount(1);
    await expect(rows.first()).toHaveAttribute('data-lifecycle', 'ready');

    // No loop warnings summoned from backend data.
    await expect(page.locator("[data-testid='count-loop-warnings']")).toContainText('0');
    await expect(page.locator("[data-testid='count-loop-errors']")).toContainText('0');
    await expect(page.locator("[data-testid='count-retry-decisions']")).toContainText('0');

    // Routing panel shows cheap + strong counts and saved-budget estimate.
    await expect(page.locator("[data-testid='routing-cheap']")).toContainText('3');
    await expect(page.locator("[data-testid='routing-strong']")).toContainText('1');
    await expect(page.locator("[data-testid='routing-saved']")).toContainText('3');
    await expect(page.locator("[data-testid='routing-cheap-share']")).toContainText('75%');

    // Delegation stub is visible and labeled per invariant #6.
    const stub = page.locator("[data-testid='delegation-stub']");
    await expect(stub).toBeVisible();
    await expect(stub).toContainText('Pending M6.7 merge');
  });

  test('validator failure: row surfaces validator badge and routes through filter', async ({
    page,
  }) => {
    await authenticate(page);
    await installMocks(page, {
      tasks: [validatorFailedTask(), successTask()],
      sources: [
        { profile_id: 'alpha', status: 'ok', api_port: 51001, session_count: 1, task_count: 1 },
        { profile_id: 'beta', status: 'ok', api_port: 51002, session_count: 1, task_count: 1 },
      ],
      summary: {
        totals: { loop_errors: 1, compaction_preservation_violations: 0 },
        breakdowns: {
          loop_errors: [
            { variant: 'tool_execution', recovery: 'fail_fast', count: 1 },
          ],
        },
      },
    });
    await page.goto('/admin/harness', { waitUntil: 'domcontentloaded' });
    await page.waitForSelector("[data-testid='harness-page']");

    const failRow = page.locator(
      "[data-testid='harness-task-row'][data-validator-failed='true']",
    );
    await expect(failRow).toHaveCount(1);
    await expect(
      failRow.locator("[data-testid='badge-validator-failed']"),
    ).toBeVisible();

    // Loop warning count reflects the failed task.
    await expect(page.locator("[data-testid='count-loop-warnings']")).toContainText('1');

    // Filter "sessions with loop warnings" narrows to the failed task only.
    await page.locator("[data-testid='count-loop-warnings']").click();
    const filtered = page.locator("[data-testid='harness-task-row']");
    await expect(filtered).toHaveCount(1);
    await expect(filtered.first()).toHaveAttribute('data-validator-failed', 'true');

    // Error taxonomy breakdown surfaces the variant + recovery hint.
    const errorRow = page.locator(
      "[data-testid='harness-error-row'][data-variant='tool_execution']",
    );
    await expect(errorRow).toHaveCount(1);
    await expect(errorRow).toHaveAttribute('data-recovery', 'fail_fast');
  });

  test('missing artifact: ready row with zero output_files is flagged', async ({ page }) => {
    await authenticate(page);
    await installMocks(page, {
      tasks: [missingArtifactTask(), successTask()],
      sources: [
        { profile_id: 'alpha', status: 'ok', api_port: 51001, session_count: 2, task_count: 2 },
      ],
      summary: {
        totals: { compaction_preservation_violations: 1 },
        breakdowns: {
          compaction_preservation_violations: [
            { phase: 'deliver_result', count: 1 },
          ],
        },
      },
    });
    await page.goto('/admin/harness', { waitUntil: 'domcontentloaded' });
    await page.waitForSelector("[data-testid='harness-page']");

    const missing = page.locator(
      "[data-testid='harness-task-row'][data-missing-artifact='true']",
    );
    await expect(missing).toHaveCount(1);
    await expect(
      missing.locator("[data-testid='badge-missing-artifact']"),
    ).toBeVisible();

    // Per-card count and table count agree (invariant #4).
    await expect(page.locator("[data-testid='count-missing-artifact']")).toContainText('1');
    await expect(page.locator("[data-testid='count-loop-warnings']")).toContainText('1');

    // Compaction panel surfaces the preservation violation.
    await expect(page.locator("[data-testid='compaction-violation-total']")).toContainText(
      '1',
    );
    const compRow = page.locator(
      "[data-testid='compaction-row'][data-phase='deliver_result']",
    );
    await expect(compRow).toHaveCount(1);
  });

  test('retry escalation: exhausted bucket highlights at > 50% share', async ({ page }) => {
    await authenticate(page);
    await installMocks(page, {
      tasks: [retryEscalationTask(), successTask()],
      sources: [
        { profile_id: 'gamma', status: 'ok', api_port: 51003, session_count: 1, task_count: 1 },
      ],
      summary: {
        totals: { loop_errors: 6, loop_retries: 8 },
        breakdowns: {
          loop_errors: [
            { variant: 'rate_limited', recovery: 'backoff_retry', count: 6 },
          ],
          loop_retries: [
            { variant: 'rate_limited', decision: 'continue', count: 1 },
            { variant: 'rate_limited', decision: 'exhausted', count: 5 },
            { variant: 'network', decision: 'continue', count: 2 },
          ],
        },
      },
    });
    await page.goto('/admin/harness', { waitUntil: 'domcontentloaded' });
    await page.waitForSelector("[data-testid='harness-page']");

    const hot = page.locator(
      "[data-testid='retry-bucket-row'][data-variant='rate_limited']",
    );
    await expect(hot).toHaveCount(1);
    await expect(hot).toHaveAttribute('data-exhausting', 'true');
    // Exhausted share is 5/6 ≈ 0.83.
    const share = await hot.getAttribute('data-exhausted-share');
    expect(share).not.toBeNull();
    expect(parseFloat(share as string)).toBeGreaterThan(0.5);

    // Non-exhausting bucket still renders but is not flagged.
    const cool = page.locator(
      "[data-testid='retry-bucket-row'][data-variant='network']",
    );
    await expect(cool).toHaveAttribute('data-exhausting', 'false');

    // Card aggregate count matches backend counter total (invariant #4).
    await expect(page.locator("[data-testid='count-retry-decisions']")).toContainText('8');
  });

  test('credential rotation: pool panel shows cooldown and strategy rows', async ({ page }) => {
    await authenticate(page);
    await installMocks(page, {
      tasks: [credentialRotationTask()],
      sources: [
        { profile_id: 'delta', status: 'ok', api_port: 51004, session_count: 1, task_count: 1 },
      ],
      summary: {
        totals: { credential_rotations: 5, routing_decisions: 7 },
        breakdowns: {
          credential_rotations: [
            { reason: 'rate_limit_cooldown', strategy: 'round_robin', count: 2 },
            { reason: 'auth_failure', strategy: 'round_robin', count: 1 },
            { reason: 'initial_acquire', strategy: 'fill_first', count: 2 },
          ],
          routing_decisions: [
            { tier: 'cheap', lane: 'budget', count: 5 },
            { tier: 'strong', lane: 'premium', count: 2 },
          ],
        },
      },
    });
    await page.goto('/admin/harness', { waitUntil: 'domcontentloaded' });
    await page.waitForSelector("[data-testid='harness-page']");

    // Totals and derived splits.
    await expect(page.locator("[data-testid='credential-total']")).toContainText('5');
    await expect(page.locator("[data-testid='credential-active']")).toContainText('2');
    await expect(page.locator("[data-testid='credential-cooldown']")).toContainText('3');

    // Per-reason row rendered.
    const cooldownRow = page.locator(
      "[data-testid='credential-row'][data-reason='rate_limit_cooldown']",
    );
    await expect(cooldownRow).toHaveCount(1);
    await expect(cooldownRow).toHaveAttribute('data-strategy', 'round_robin');

    // Routing panel reflects the accompanying lane breakdown.
    const cheap = page.locator(
      "[data-testid='routing-row'][data-tier='cheap'][data-lane='budget']",
    );
    await expect(cheap).toHaveCount(1);

    // Aggregate card matches the backend total.
    await expect(
      page.locator("[data-testid='count-credential-rotations']"),
    ).toContainText('5');
  });
});
