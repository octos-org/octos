/**
 * Focused playwright replay of the mini3 kimi-k2.5 tool-loop failure
 * (session 8w2ime, 2026-05-29 01:27 UTC).
 *
 * Production failure: kimi asked about a Chinese-language slides deck
 * with a manifest-content question ("是你的 manifest 文件生成问题，
 * 图片都存在") after several mofa_slides iterations. The LLM picked
 * `check_workspace_contract` (which answers artifact PRESENCE, not
 * file CONTENT), got the same 4 KB result 5 times, and was terminated
 * by the loop detector.
 *
 * This spec drives a minimal version of the same pattern against the
 * deployed binary (PR #1363, commit c55f9b21):
 *
 *   1. Open a fresh slides session.
 *   2. Request a tiny 3-slide deck so the LLM produces a deck.pptx +
 *      images that `check_workspace_contract` will report as present.
 *   3. Send the ambiguous trigger message in Chinese.
 *   4. Observe the assistant's response and the server log.
 *
 * Pass criteria (one of):
 *   - The LLM responds with TEXT (broke out of the wrong-tool attractor).
 *   - The log shows `NO PROGRESS` marker firing AND the assistant
 *     subsequently responded (the new runtime fix engaged and worked).
 *
 * Fail criterion:
 *   - The log shows `LOOP DETECTED ... terminating turn` (the existing
 *     iter-4 hard cycle detector had to fire to clean up — meaning the
 *     new runtime guard didn't catch it in time).
 *
 * Run:
 *   OCTOS_TEST_URL=https://dspfac.octos.ominix.io \
 *   OCTOS_AUTH_TOKEN=octos-admin-2026 \
 *   OCTOS_PROFILE=dspfac \
 *   OCTOS_TEST_EMAIL=dspfac@gmail.com \
 *   npx playwright test tests/kimi-loop-replay.spec.ts
 */
import { expect, test } from '@playwright/test';
import {
  createNewSession,
  getAssistantMessageText,
  login,
  sendAndWait,
} from './live-browser-helpers';

// Full slides cycle is expensive; budget high so we don't false-fail on
// LLM latency during a real run.
test.setTimeout(20 * 60 * 1000);

test('kimi loop replay: ambiguous manifest question does not terminate via loop detector', async ({
  page,
}) => {
  await login(page);
  await createNewSession(page);

  // Step 1 — produce a small deck so a workspace contract exists.
  //
  // Keep it small (3 slides, 1K image_size) to cap real spend at < $1
  // per trial while still producing all the artifacts that
  // `check_workspace_contract` will report as present.
  const deckPrompt = [
    '生成一个 3 张幻灯片的普洱茶介绍。',
    '使用 nb-pro 样式（不要用自定义样式）。',
    '使用 image_size: "1K" 以节省时间和费用。',
    '内容简单即可，不需要详细。',
    '生成完成后告诉我。',
  ].join(' ');
  await sendAndWait(page, deckPrompt);

  // Snapshot the first response so we know the deck pipeline kicked off.
  const initialResponse = await getAssistantMessageText(page);
  expect(initialResponse.length, 'initial response should have content').toBeGreaterThan(20);

  // Step 2 — the ambiguous trigger message (verbatim from production
  // session 8w2ime, line 90 in the JSONL).
  //
  // This is the message that caused 5 consecutive check_workspace_contract
  // calls in production. With the new no-progress detector (PR #1363) +
  // the generic prompt block (PR #1362) + the tightened tool description
  // (PR #1361), the LLM should either pick a different tool (read_file /
  // list_dir) or respond with text.
  const triggerMessage = '是你的 manifest 文件生成问题，图片都存在';
  await sendAndWait(page, triggerMessage);

  const finalResponse = await getAssistantMessageText(page);
  console.log('\n=== assistant response to trigger ===\n');
  console.log(finalResponse.slice(0, 800));
  console.log('\n=====================================\n');

  // Pass: response is non-empty AND does not look like the loop
  // detector's terminal message.
  //
  // The hard loop detector emits a fixed terminal phrasing — checking for
  // the substring is sufficient (see loop_runner.rs::loop_detected_terminal_message).
  expect(finalResponse.length, 'assistant must produce a non-trivial response').toBeGreaterThan(30);
  expect(
    finalResponse,
    'response must NOT be the loop detector terminal message',
  ).not.toMatch(/keeps calling the same tool|stopping the turn|LOOP DETECTED/i);
});
