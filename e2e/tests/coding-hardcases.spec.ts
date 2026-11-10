/**
 * Phase 3 coding hard-case acceptance coverage.
 *
 * These cases stay deliberately close to current main behavior:
 * - failing repo check is repaired in a single foreground turn
 * - coding fanout completes child sessions and delivers their results
 * - reload during a long coding reply preserves the same turn
 *
 * Run listing only:
 *   OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   npx playwright test tests/coding-hardcases.spec.ts --list
 */
import { expect, test } from "@playwright/test";

import {
  SEL,
  createNewSession,
  expectSingleTurn,
  getChatThreadText,
  getInput,
  getSendButton,
  login,
  sendAndWait,
} from "./live-browser-helpers";
import {
  findSessionIdByMessageText,
  getActiveSessionId,
  getSessionMessagesText,
  uniqueRepoName,
  waitForAssistantTextProgress,
  waitForChildSessionTasksToSettle,
  waitForSingleSettledTurn,
  waitForStreamingAssistantTurn,
} from "./coding-hardcases-helpers";

const FANOUT_CHILD_SESSION_LIMIT = 3;

function buildRepairPrompt(repoName: string, returnToken?: string) {
  const tail = returnToken
    ? `Return only the stdout from that final command, then a final line exactly ${returnToken}.`
    : "Return only the stdout from that final command, nothing else.";

  return [
    "Use shell tool only.",
    "If shell is not already active, activate it first.",
    `Run \`mkdir -p ./${repoName} && cd ./${repoName} && git init\` to create a temporary git repo inside the current workspace.`,
    "Stay inside the current workspace; do not use /tmp or any other absolute temp directory.",
    "All subsequent shell commands must run from that repo root unless a step says otherwise.",
    "Inside it, create notes.txt with exactly two lines: alpha and beta.",
    "Create check.sh that exits 0 only when the second line of notes.txt is gamma and exits 1 otherwise.",
    "Make check.sh executable.",
    "Run git add notes.txt check.sh so both files are tracked before editing.",
    "Run ./check.sh once and confirm it exits non-zero.",
    "Repair only notes.txt by changing the second line from beta to gamma without modifying the first line or check.sh.",
    "Your final shell command from the repo root must be `./check.sh && git diff -- notes.txt`.",
    "If that final command exits non-zero, keep fixing notes.txt and rerun it until it succeeds.",
    tail,
    "Do not start background work.",
  ].join(" ");
}

function buildFanoutPrompt(labelPrefix: string) {
  const labels = Array.from(
    { length: FANOUT_CHILD_SESSION_LIMIT },
    (_, index) => `${labelPrefix}-${String.fromCharCode(97 + index)}`,
  );

  return {
    labels,
    prompt: [
      "Use the spawn tool in background mode for coding reconnaissance.",
      `Attempt exactly ${labels.length} coding child sessions.`,
      'Each child must set allowed_tools to ["shell"] and no other tools.',
      `Use labels ${labels.join(", ")}.`,
      "Each child should only run a tiny shell command that prints its label, then stop.",
      "The parent must not run shell directly.",
      "After dispatching what is allowed, briefly say delegation started and stop.",
    ].join(" "),
  };
}

function buildLongRepairPrompt(repoName: string, resumeMarker: string) {
  return [
    "Use shell for every repo operation in this task.",
    "If shell is not already active, activate it first.",
    `Run \`mkdir -p ./${repoName} && cd ./${repoName} && git init\` to create a temporary git repo inside the current workspace.`,
    "Stay inside the current workspace; do not use /tmp or any other absolute temp directory.",
    "All subsequent shell commands must run from that repo root unless a step says otherwise.",
    "Inside it, create notes.txt with exactly two lines: alpha and beta.",
    "Run git add notes.txt so it is tracked before editing.",
    "Repair only notes.txt by changing beta to gamma.",
    "Then run git diff -- notes.txt.",
    "Run sleep 12 once from the repo root before writing the final answer.",
    `After the shell work completes, return exactly two parts: first a line containing ${resumeMarker}, then the unified diff for notes.txt and nothing else.`,
    "Do not start background work.",
  ].join(" ");
}

test.describe("Phase 3 coding hard cases", () => {
  test.describe.configure({ mode: "serial" });
  test.setTimeout(600_000);

  test("failing test is repaired without starting a second ghost turn", async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    let repoName = uniqueRepoName("phase3-repair");
    let result = await sendAndWait(page, buildRepairPrompt(repoName), {
      maxWait: 420_000,
      label: "repair-pass",
    });
    if (!result.responseText.includes("diff --git")) {
      await createNewSession(page);
      repoName = uniqueRepoName("phase3-repair-retry");
      result = await sendAndWait(page, buildRepairPrompt(repoName), {
        maxWait: 420_000,
        label: "repair-pass-retry",
      });
    }

    await expectSingleTurn(page);
    expect(result.responseText).toContain("diff --git");
    expect(result.responseText).toContain("notes.txt");
    expect(result.responseText).toContain("-beta");
    expect(result.responseText).toContain("+gamma");
  });

  test("coding fanout completes bounded child sessions and delivers their results", async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    const priorSessionId = await getActiveSessionId(page, {
      timeoutMs: 5_000,
    }).catch(() => null);
    const { labels, prompt } = buildFanoutPrompt(
      uniqueRepoName("phase3-fanout"),
    );

    await sendAndWait(page, prompt, {
      maxWait: 120_000,
      label: "coding-fanout",
    });

    const sessionId = await getActiveSessionId(page, {
      ignoreSessionIds: priorSessionId ? [priorSessionId] : [],
    });
    const childTasks = await waitForChildSessionTasksToSettle(
      page,
      sessionId,
      120_000,
    );
    expect(childTasks.length).toBeGreaterThan(0);
    expect(childTasks.length).toBeLessThanOrEqual(FANOUT_CHILD_SESSION_LIMIT);
    for (const label of labels) {
      // Require assistant-side delivery, not the labels in the user prompt.
      await expect
        .poll(() => getSessionMessagesText(page, sessionId, "assistant"), {
          timeout: 30_000,
        })
        .toContain(label);
    }

    for (const task of childTasks) {
      expect(task.status).toBe("completed");
      expect(task.child_terminal_state).toBe("completed");
      // The optional contract channel is independent of result delivery.
      // Unobserved is valid; an observed orphan is not a clean completion.
      expect([undefined, null, "joined"]).toContain(task.child_join_state);
    }
  });

  test("long idle resume keeps the same coding turn after reconnect", async ({
    page,
  }) => {
    await login(page);
    await createNewSession(page);

    const repoName = uniqueRepoName("phase3-resume");
    const resumeMarker = `RESUME-${Date.now()}`;
    await getInput(page).fill(buildLongRepairPrompt(repoName, resumeMarker));
    await getSendButton(page).click();

    await waitForStreamingAssistantTurn(page, 120_000);
    await waitForAssistantTextProgress(page, {
      timeoutMs: 30_000,
      minGrowthEvents: 2,
      minLength: 120,
    });
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.waitForSelector(SEL.chatInput, { timeout: 15_000 });

    const sessionId = await findSessionIdByMessageText(
      page,
      resumeMarker,
      60_000,
    );
    const finalText = await waitForSingleSettledTurn(page, 240_000);
    await expectSingleTurn(page);
    const deadline = Date.now() + 60_000;
    let combinedText = finalText;
    while (Date.now() < deadline) {
      const persistedText = await getSessionMessagesText(page, sessionId);
      combinedText = `${await getChatThreadText(page)}\n${persistedText}`;
      if (
        combinedText.includes(resumeMarker) &&
        combinedText.includes("diff --git") &&
        combinedText.includes("notes.txt") &&
        combinedText.includes("-beta") &&
        combinedText.includes("+gamma")
      ) {
        break;
      }
      await page.waitForTimeout(2_000);
    }

    expect(combinedText).toContain(resumeMarker);
    expect(combinedText).toContain("diff --git");
    expect(combinedText).toContain("notes.txt");
    expect(combinedText).toContain("-beta");
    expect(combinedText).toContain("+gamma");
  });
});
