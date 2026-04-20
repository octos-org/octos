/**
 * Phase 3 coding hard-case acceptance coverage.
 *
 * These cases stay deliberately close to current main behavior:
 * - repo edit yields a bounded, reviewable diff
 * - failing repo check is repaired in a single foreground turn
 * - coding fanout creates bounded child sessions and joins them cleanly
 * - reload during a long coding reply preserves the same turn
 * - concurrent coding sessions stay isolated under load
 *
 * Run listing only:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   npx playwright test tests/coding-hardcases.spec.ts --list
 */
import { expect, test } from '@playwright/test';

import {
  SEL,
  createNewSession,
  expectSingleTurn,
  getChatThreadText,
  getInput,
  getSendButton,
  login,
  sendAndWait,
} from './live-browser-helpers';
import {
  findSessionIdByMessageText,
  getActiveSessionId,
  getSessionMessagesText,
  openAuthedChat,
  uniqueRepoName,
  waitForAssistantTextProgress,
  waitForChildSessionTasksToSettle,
  waitForSingleSettledTurn,
  waitForStreamingAssistantTurn,
} from './coding-hardcases-helpers';

const FANOUT_CHILD_SESSION_LIMIT = 3;

function buildBoundedDiffPrompt(repoName: string, returnToken?: string) {
  const tail = returnToken
    ? `Return only the unified diff, then a final line exactly ${returnToken}.`
    : 'Return only the unified diff, nothing else.';

  return [
    'Use shell tool only.',
    'If shell is not already active, activate it first.',
    `Run \`mkdir -p ./${repoName} && cd ./${repoName} && git init\` to create a temporary git repo inside the current workspace.`,
    'Stay inside the current workspace; do not use /tmp or any other absolute temp directory.',
    'All subsequent shell commands must run from that repo root unless a step says otherwise.',
    'Inside it, create notes.txt with exactly two lines: alpha and beta.',
    'Run git add notes.txt so the file is tracked before editing it.',
    'Make exactly one edit: change beta to gamma.',
    'Then run git diff -- notes.txt.',
    tail,
    'Do not start background work.',
  ].join(' ');
}

function buildRepairPrompt(repoName: string, returnToken?: string) {
  const tail = returnToken
    ? `Return only the stdout from that final command, then a final line exactly ${returnToken}.`
    : 'Return only the stdout from that final command, nothing else.';

  return [
    'Use shell tool only.',
    'If shell is not already active, activate it first.',
    `Run \`mkdir -p ./${repoName} && cd ./${repoName} && git init\` to create a temporary git repo inside the current workspace.`,
    'Stay inside the current workspace; do not use /tmp or any other absolute temp directory.',
    'All subsequent shell commands must run from that repo root unless a step says otherwise.',
    'Inside it, create notes.txt with exactly two lines: alpha and beta.',
    'Create check.sh that exits 0 only when the second line of notes.txt is gamma and exits 1 otherwise.',
    'Make check.sh executable.',
    'Run git add notes.txt check.sh so both files are tracked before editing.',
    'Run ./check.sh once and confirm it exits non-zero.',
    'Repair only notes.txt by changing the second line from beta to gamma without modifying the first line or check.sh.',
    'Your final shell command from the repo root must be `./check.sh && git diff -- notes.txt`.',
    'If that final command exits non-zero, keep fixing notes.txt and rerun it until it succeeds.',
    tail,
    'Do not start background work.',
  ].join(' ');
}

function buildFanoutPrompt(labelPrefix: string) {
  const labels = Array.from(
    { length: FANOUT_CHILD_SESSION_LIMIT },
    (_, index) => `${labelPrefix}-${String.fromCharCode(97 + index)}`,
  );

  return {
    labels,
    prompt: [
      'Use the spawn tool in background mode for coding reconnaissance.',
      `Attempt exactly ${labels.length} coding child sessions.`,
      'Each child must set allowed_tools to ["shell"] and no other tools.',
      `Use labels ${labels.join(', ')}.`,
      'Each child should only run a tiny shell command that prints its label, then stop.',
      'The parent must not run shell directly.',
      'After dispatching what is allowed, briefly say delegation started and stop.',
    ].join(' '),
  };
}

function buildLongRepairPrompt(repoName: string, resumeMarker: string) {
  return [
    'Use shell for every repo operation in this task.',
    'If shell is not already active, activate it first.',
    `Run \`mkdir -p ./${repoName} && cd ./${repoName} && git init\` to create a temporary git repo inside the current workspace.`,
    'Stay inside the current workspace; do not use /tmp or any other absolute temp directory.',
    'All subsequent shell commands must run from that repo root unless a step says otherwise.',
    'Inside it, create notes.txt with exactly two lines: alpha and beta.',
    'Run git add notes.txt so it is tracked before editing.',
    'Repair only notes.txt by changing beta to gamma.',
    'Then run git diff -- notes.txt.',
    'Run sleep 12 once from the repo root before writing the final answer.',
    `After the shell work completes, return exactly two parts: first a line containing ${resumeMarker}, then the unified diff for notes.txt and nothing else.`,
    'Do not start background work.',
  ].join(' ');
}

test.describe('Phase 3 coding hard cases', () => {
  test.describe.configure({ mode: 'serial' });
  test.setTimeout(600_000);

  test('repo edit task writes a bounded diff and exposes reviewable output', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    const repoName = uniqueRepoName('phase3-bounded-diff');
    const prompt = buildBoundedDiffPrompt(repoName);

    const result = await sendAndWait(page, prompt, {
      maxWait: 180_000,
      label: 'bounded-diff',
    });

    const response = result.responseText;
    if (!response) {
      throw new Error('Expected a reviewable diff response, got empty assistant output');
    }

    await expectSingleTurn(page);
    expect(response).toContain('diff --git');
    expect(response).toContain('notes.txt');
    expect(response).toContain('-beta');
    expect(response).toContain('+gamma');
    expect(response.length).toBeLessThan(4_000);
  });

  test('failing test is repaired without starting a second ghost turn', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    let repoName = uniqueRepoName('phase3-repair');
    let result = await sendAndWait(page, buildRepairPrompt(repoName), {
      maxWait: 240_000,
      label: 'repair-pass',
    });
    if (!result.responseText.includes('diff --git')) {
      await createNewSession(page);
      repoName = uniqueRepoName('phase3-repair-retry');
      result = await sendAndWait(page, buildRepairPrompt(repoName), {
        maxWait: 240_000,
        label: 'repair-pass-retry',
      });
    }

    await expectSingleTurn(page);
    expect(result.responseText).toContain('diff --git');
    expect(result.responseText).toContain('notes.txt');
    expect(result.responseText).toContain('-beta');
    expect(result.responseText).toContain('+gamma');
  });

  test('coding fanout creates bounded child sessions and joins them cleanly', async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    const priorSessionId = await getActiveSessionId(page, { timeoutMs: 5_000 }).catch(() => null);
    const { labels, prompt } = buildFanoutPrompt(uniqueRepoName('phase3-fanout'));

    await sendAndWait(page, prompt, {
      maxWait: 120_000,
      label: 'coding-fanout',
    });

    const sessionId = await getActiveSessionId(page, {
      ignoreSessionIds: priorSessionId ? [priorSessionId] : [],
    });
    const childTasks = await waitForChildSessionTasksToSettle(page, sessionId, 120_000);
    const threadText = await getChatThreadText(page);

    expect(childTasks.length).toBeGreaterThan(0);
    expect(childTasks.length).toBeLessThanOrEqual(FANOUT_CHILD_SESSION_LIMIT);
    for (const label of labels) {
      expect(threadText).toContain(label);
    }

    for (const task of childTasks) {
      expect(task.status).toBe('completed');
      expect(task.child_terminal_state).toBe('completed');
      expect(task.child_join_state).toBe('joined');
    }
  });

  test('long idle resume keeps the same coding turn after reconnect', async ({ page }) => {
    await login(page);
    await createNewSession(page);

    const repoName = uniqueRepoName('phase3-resume');
    const resumeMarker = `RESUME-${Date.now()}`;
    await getInput(page).fill(buildLongRepairPrompt(repoName, resumeMarker));
    await getSendButton(page).click();

    await waitForStreamingAssistantTurn(page, 120_000);
    await waitForAssistantTextProgress(page, {
      timeoutMs: 30_000,
      minGrowthEvents: 2,
      minLength: 120,
    });
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });

    const sessionId = await findSessionIdByMessageText(page, resumeMarker, 60_000);
    const finalText = await waitForSingleSettledTurn(page, 240_000);
    await expectSingleTurn(page);
    const deadline = Date.now() + 60_000;
    let combinedText = finalText;
    while (Date.now() < deadline) {
      const persistedText = await getSessionMessagesText(page, sessionId);
      combinedText = `${await getChatThreadText(page)}\n${persistedText}`;
      if (
        combinedText.includes(resumeMarker) &&
        combinedText.includes('diff --git') &&
        combinedText.includes('notes.txt') &&
        combinedText.includes('-beta') &&
        combinedText.includes('+gamma')
      ) {
        break;
      }
      await page.waitForTimeout(2_000);
    }

    expect(combinedText).toContain(resumeMarker);
    expect(combinedText).toContain('diff --git');
    expect(combinedText).toContain('notes.txt');
    expect(combinedText).toContain('-beta');
    expect(combinedText).toContain('+gamma');
  });

  test('concurrent coding sessions remain isolated under load', async ({ browser }) => {
    const first = await openAuthedChat(browser);
    const second = await openAuthedChat(browser);

    try {
      const alphaToken = `ALPHA-CODE-${Date.now()}`;
      const betaToken = `BRAVO-CODE-${Date.now()}`;
      const alphaPrompt = buildBoundedDiffPrompt(
        uniqueRepoName('phase3-concurrent-alpha'),
        alphaToken,
      );
      const betaPrompt = buildBoundedDiffPrompt(
        uniqueRepoName('phase3-concurrent-bravo'),
        betaToken,
      );

      const [alphaResult, betaResult] = await Promise.all([
        sendAndWait(first.page, alphaPrompt, {
          label: 'concurrent-alpha',
          maxWait: 180_000,
        }),
        sendAndWait(second.page, betaPrompt, {
          label: 'concurrent-beta',
          maxWait: 180_000,
        }),
      ]);

      const alphaText = await getChatThreadText(first.page);
      const betaText = await getChatThreadText(second.page);

      await expectSingleTurn(first.page);
      await expectSingleTurn(second.page);

      expect(alphaResult.responseText).toContain(alphaToken);
      expect(betaResult.responseText).toContain(betaToken);
      expect(alphaText).toContain(alphaToken);
      expect(alphaText).not.toContain(betaToken);
      expect(betaText).toContain(betaToken);
      expect(betaText).not.toContain(alphaToken);
    } finally {
      await Promise.all([first.context.close(), second.context.close()]);
    }
  });
});
