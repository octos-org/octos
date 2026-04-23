/**
 * M7.8 — Live fleet swarm dispatch gate.
 *
 * Browser-side companion to scripts/validate-m7-swarm-live.sh. Uses the M7.6
 * SwarmPage UI (once merged) to author the contract, dispatch, watch live
 * progress, and accept the supervisor review at the completion gate.
 *
 * The spec is deliberately scaffolded: individual assertions are wrapped in
 * `test.fixme()` for now because M7.6's UI endpoints are still landing. The
 * discovery shape matters: CI can `--list` this spec to confirm the harness
 * wiring without exercising the canary.
 *
 * Gated by OCTOS_SWARM_GATE — when unset (CI without canary), the suite
 * no-ops via `test.describe.skip()`.
 *
 * Run against live canary:
 *   OCTOS_SWARM_GATE=1 \
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=$OCTOS_AUTH_TOKEN \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/swarm-dispatch-gate.spec.ts
 *
 * List-only (no network):
 *   npx playwright test tests/swarm-dispatch-gate.spec.ts --list
 */
import { expect, test, type Page } from '@playwright/test';
import { login, SEL } from './live-browser-helpers';

const SWARM_GATE_ENABLED = process.env.OCTOS_SWARM_GATE === '1';

const SWARM_SELECTORS = {
  navLink: "[data-testid='nav-swarm']",
  contractEditor: "[data-testid='swarm-contract-editor']",
  contractLabel: "[data-testid='swarm-contract-label']",
  dispatchButton: "[data-testid='swarm-dispatch-button']",
  subtaskCard: "[data-testid='swarm-subtask-card']",
  outcomeBadge: "[data-testid='swarm-outcome-badge']",
  progressEvents: "[data-testid='swarm-progress-events']",
  validatorPanel: "[data-testid='swarm-validator-panel']",
  reviewAcceptButton: "[data-testid='swarm-review-accept']",
  reviewGatedBanner: "[data-testid='swarm-review-gated-banner']",
  artifactLink: "[data-testid='swarm-artifact-link']",
  costPanel: "[data-testid='swarm-cost-panel']",
} as const;

/**
 * Fixture payload — the three-variant fibonacci dispatch the validator
 * script also consumes. Kept inline so `--list` never needs to read the
 * filesystem.
 */
const DISPATCH_FIXTURE = {
  label: 'm7.8-live-gate-fibonacci',
  topology: 'parallel',
  maxConcurrency: 3,
  contracts: [
    { contractId: 'fib-iter', label: 'iterative fibonacci' },
    { contractId: 'fib-rec', label: 'recursive fibonacci' },
    { contractId: 'fib-memo', label: 'memoized fibonacci' },
  ],
} as const;

async function openSwarmPage(page: Page): Promise<void> {
  await page.locator(SWARM_SELECTORS.navLink).click();
  await expect(page.locator(SWARM_SELECTORS.contractEditor)).toBeVisible({
    timeout: 15_000,
  });
}

async function authorContract(page: Page, label: string): Promise<void> {
  await page.locator(SWARM_SELECTORS.contractLabel).fill(label);
  await page.locator(SWARM_SELECTORS.contractEditor).click();
  await page.keyboard.type(
    JSON.stringify(
      {
        label,
        topology: { kind: 'parallel', max_concurrency: 3 },
        contracts: DISPATCH_FIXTURE.contracts.map((c) => ({
          contract_id: c.contractId,
          tool_name: 'claude_code/run_task',
          label: c.label,
          task: { prompt: `Write a fibonacci variant labeled ${c.label}` },
        })),
      },
      null,
      2,
    ),
  );
}

async function triggerDispatch(page: Page): Promise<string> {
  const dispatchButton = page.locator(SWARM_SELECTORS.dispatchButton);
  await dispatchButton.click();
  // The SwarmPage surfaces the dispatch_id in a data attribute on the
  // outcome badge once the API round-trips. Polled rather than awaited so
  // the spec fails fast with a clear message if the endpoint regresses.
  const outcomeBadge = page.locator(SWARM_SELECTORS.outcomeBadge);
  await expect(outcomeBadge).toBeVisible({ timeout: 30_000 });
  const dispatchId = await outcomeBadge.getAttribute('data-dispatch-id');
  if (!dispatchId) {
    throw new Error('dispatch_id missing from outcome badge');
  }
  return dispatchId;
}

async function waitForTerminalOutcome(
  page: Page,
  timeoutMs: number,
): Promise<string> {
  const outcomeBadge = page.locator(SWARM_SELECTORS.outcomeBadge);
  let outcome: string | null = null;
  await expect
    .poll(
      async () => {
        outcome = await outcomeBadge.getAttribute('data-outcome');
        return outcome;
      },
      { timeout: timeoutMs, intervals: [2_000, 5_000] },
    )
    .toMatch(/^(success|partial|failed|aborted)$/);
  return outcome ?? 'unknown';
}

test.describe(SWARM_GATE_ENABLED ? 'M7.8 swarm dispatch gate' : 'M7.8 swarm dispatch gate (skipped)', () => {
  test.describe.configure({ mode: 'serial' });
  test.setTimeout(300_000);

  test.skip(
    !SWARM_GATE_ENABLED,
    'OCTOS_SWARM_GATE not set; run with OCTOS_SWARM_GATE=1 against a live canary',
  );

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test('supervisor authors a contract and dispatches a parallel-3 fanout', async ({
    page,
  }) => {
    test.fixme(
      !process.env.OCTOS_M7_6_READY,
      'M7.6 SwarmPage UI not yet merged on canary — set OCTOS_M7_6_READY=1 once merged',
    );

    await openSwarmPage(page);
    const label = `${DISPATCH_FIXTURE.label}-${Date.now().toString(36)}`;
    await authorContract(page, label);
    const dispatchId = await triggerDispatch(page);
    expect(dispatchId).toMatch(/^[a-zA-Z0-9_-]+$/);
    expect(dispatchId.length).toBeGreaterThan(3);
  });

  test('live progress panel surfaces one card per sub-agent', async ({ page }) => {
    test.fixme(
      !process.env.OCTOS_M7_6_READY,
      'M7.6 SwarmPage live progress wiring pending',
    );

    await openSwarmPage(page);
    const label = `${DISPATCH_FIXTURE.label}-progress-${Date.now().toString(36)}`;
    await authorContract(page, label);
    await triggerDispatch(page);

    const subtaskCards = page.locator(SWARM_SELECTORS.subtaskCard);
    await expect(subtaskCards).toHaveCount(DISPATCH_FIXTURE.contracts.length, {
      timeout: 60_000,
    });

    for (const contract of DISPATCH_FIXTURE.contracts) {
      await expect(
        page.locator(SWARM_SELECTORS.subtaskCard, { hasText: contract.label }),
      ).toBeVisible();
    }

    const progressEvents = page.locator(SWARM_SELECTORS.progressEvents);
    await expect(progressEvents).toBeVisible({ timeout: 30_000 });
  });

  test('dispatch reaches terminal state and exposes cost + validator panels', async ({
    page,
  }) => {
    test.fixme(
      !process.env.OCTOS_M7_6_READY,
      'M7.6 SwarmPage terminal transition pending',
    );

    await openSwarmPage(page);
    const label = `${DISPATCH_FIXTURE.label}-terminal-${Date.now().toString(36)}`;
    await authorContract(page, label);
    await triggerDispatch(page);

    const outcome = await waitForTerminalOutcome(page, 240_000);
    expect(['success', 'partial']).toContain(outcome);

    await expect(page.locator(SWARM_SELECTORS.validatorPanel)).toBeVisible();
    await expect(page.locator(SWARM_SELECTORS.costPanel)).toBeVisible();
  });

  test('artifact links open and the supervisor can accept the review gate', async ({
    page,
  }) => {
    test.fixme(
      !process.env.OCTOS_M7_6_READY,
      'M7.6 review gate UI pending',
    );

    await openSwarmPage(page);
    const label = `${DISPATCH_FIXTURE.label}-review-${Date.now().toString(36)}`;
    await authorContract(page, label);
    await triggerDispatch(page);
    await waitForTerminalOutcome(page, 240_000);

    const artifactLinks = page.locator(SWARM_SELECTORS.artifactLink);
    await expect(artifactLinks.first()).toBeVisible({ timeout: 15_000 });

    const reviewBanner = page.locator(SWARM_SELECTORS.reviewGatedBanner);
    if (await reviewBanner.isVisible().catch(() => false)) {
      await page.locator(SWARM_SELECTORS.reviewAcceptButton).click();
      await expect(reviewBanner).toBeHidden({ timeout: 15_000 });
    }
  });

  test('survives a reload without losing the dispatch record', async ({
    page,
  }) => {
    test.fixme(
      !process.env.OCTOS_M7_6_READY,
      'M7.6 SwarmPage reload persistence pending',
    );

    await openSwarmPage(page);
    const label = `${DISPATCH_FIXTURE.label}-reload-${Date.now().toString(36)}`;
    await authorContract(page, label);
    const dispatchId = await triggerDispatch(page);
    await waitForTerminalOutcome(page, 240_000);

    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });
    await openSwarmPage(page);

    const outcomeBadge = page.locator(
      `${SWARM_SELECTORS.outcomeBadge}[data-dispatch-id="${dispatchId}"]`,
    );
    await expect(outcomeBadge).toBeVisible({ timeout: 30_000 });
  });
});
