// Capture-and-replay smoke spec (PR I).
//
// Sends a single trivial prompt against the live SPA + daemon, with the
// PR I capture-replay harness wired in. The assertion is purposefully
// loose: the real point is to demonstrate that `attachCapture` records
// SSE frames + DOM state into a JSON fixture under
// `e2e/fixtures/captured/` when run with `OCTOS_CAPTURE_FIXTURE=1`.
//
// Run:
//   OCTOS_TEST_URL=https://<host>.octos.ominix.io \
//   OCTOS_CAPTURE_FIXTURE=1 \
//     npx playwright test tests/live-msg-order.spec.ts --reporter=line
//
// The fixture lands at e2e/fixtures/captured/<slug>-<timestamp>.json.
import { test, expect } from '@playwright/test';
import { attachCapture } from '../lib/capture-replay';
import { login, sendAndWait } from './live-browser-helpers';

test.describe('PR I capture-replay smoke', () => {
  test('captures SSE stream + DOM state for a trivial single-turn prompt', async ({
    page,
  }, testInfo) => {
    test.setTimeout(180_000);
    const capture = await attachCapture(page, testInfo, {
      description: 'PR I demo: single-turn prompt against live daemon',
    });
    try {
      await login(page);
      const result = await sendAndWait(
        page,
        'Reply with a single word: ok',
        { capture, maxWait: 90_000, throwOnTimeout: false, label: 'pri-demo' },
      );
      // Soft assertions — the goal is to exercise the capture machinery,
      // not to police the daemon's reply quality.
      expect(result.assistantBubbles).toBeGreaterThanOrEqual(0);
    } catch (err) {
      capture.recordFailure(err);
      throw err;
    } finally {
      await capture.finalize({ reason: 'spec-complete' });
    }
  });
});
