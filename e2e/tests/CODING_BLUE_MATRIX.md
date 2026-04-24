# coding-blue fragile-case e2e matrix

One spec file per fragile scenario uncovered across FA-5 through FA-11,
so coding-blue-r1 (post FA-12 + M7.9) and future releases have strong
regression coverage.

Run against any mini by setting `OCTOS_TEST_URL`:

```bash
OCTOS_TEST_URL=https://mini1.crew.ominix.io \
OCTOS_AUTH_TOKEN=octos-admin-2026 \
OCTOS_PROFILE=dspfac \
OCTOS_TEST_EMAIL=dspfac@gmail.com \
npx playwright test tests/<spec>.spec.ts
```

If `OCTOS_TEST_URL` is not set, every spec in this matrix is a no-op
(`test.skip`) so the suite can be wired into CI without pinning a
target. To list only the matrix specs:

```bash
npx playwright test tests/queue-mode-*.spec.ts \
  tests/rapid-consecutive-chats.spec.ts \
  tests/mixed-long-short-*.spec.ts \
  tests/out-of-order-response.spec.ts \
  tests/reload-mid-background.spec.ts \
  tests/session-switch-race.spec.ts \
  tests/file-attachment-identity-live.spec.ts \
  tests/queue-mode-switch-midsession.spec.ts \
  tests/task-anchor-lifecycle.spec.ts \
  tests/swarm-review-gate.spec.ts \
  --list
```

## Suites

### Suite 1 — Queue mode matrix (5 specs)

| spec | mode | contract |
| --- | --- | --- |
| `queue-mode-followup.spec.ts` | followup | strict U1→A1→U2→A2 DOM interleave |
| `queue-mode-collect.spec.ts` | collect | ONE assistant bubble answers both |
| `queue-mode-steer.spec.ts` | steer | Q2 overrides Q1 mid-flight; A2 wins |
| `queue-mode-interrupt.spec.ts` | interrupt | Q2 aborts Q1; A2 lands fresh |
| `queue-mode-speculative.spec.ts` | speculative | both run concurrent; both bubbles land |

### Suite 2 — Rapid consecutive chats

| spec | notes |
| --- | --- |
| `rapid-consecutive-chats.spec.ts` | 5 prompts over ~2.5s; followup; >=3 of 5 reply |

### Suite 3 — Long-running + one-shot mix

| spec | queue mode | notes |
| --- | --- | --- |
| `mixed-long-short-followup.spec.ts` | followup | FA-10 rewrite; content-based assertions |
| `mixed-long-short-speculative.spec.ts` | speculative | concurrent mix; depends on FA-12 |

### Suite 4 — Out-of-order response race (mocked)

| spec | notes |
| --- | --- |
| `out-of-order-response.spec.ts` | `page.route` intercept; 3s vs 100ms; FA-8 correlation |

### Suite 5 — Reload mid-background

| spec | notes |
| --- | --- |
| `reload-mid-background.spec.ts` | deep research + reload + snapshot + idempotent reload |

### Suite 6 — Session-switch race

| spec | notes |
| --- | --- |
| `session-switch-race.spec.ts` | A starts long task; switch to B; return to A; no bleed |

### Suite 7 — File-attachment identity race (live)

| spec | notes |
| --- | --- |
| `file-attachment-identity-live.spec.ts` | file attaches to originating turn, not latest |

### Suite 8 — Queue mode switching mid-session

| spec | notes |
| --- | --- |
| `queue-mode-switch-midsession.spec.ts` | followup → speculative → followup across Q1/Q2/Q3 |

### Suite 9 — Task-anchor lifecycle trace

| spec | notes |
| --- | --- |
| `task-anchor-lifecycle.spec.ts` | spinner on, label transitions, spinner off |

### Suite 10 — Swarm review gate

| spec | notes |
| --- | --- |
| `swarm-review-gate.spec.ts` | scaffold only; targets `/swarm/`; fixme'd pending M7.9 |

## Expected status vs releases

| spec | pre-FA-12 (coding-blue) | post FA-12 + M7.9 (coding-blue-r1) |
| --- | --- | --- |
| queue-mode-followup | PASS | PASS |
| queue-mode-collect | PASS | PASS |
| queue-mode-steer | `test.fixme` pending M7.9 | remove fixme, should PASS |
| queue-mode-interrupt | PASS (structural) | PASS + tighter labelling |
| queue-mode-speculative | `test.fixme` (FA-11) | remove fixme, PASS |
| rapid-consecutive-chats | PASS | PASS |
| mixed-long-short-followup | PASS | PASS |
| mixed-long-short-speculative | `test.fixme` (FA-11) | remove fixme, PASS |
| out-of-order-response | `test.fixme` (FA-8 envelope) | remove fixme, PASS |
| reload-mid-background | PASS | PASS |
| session-switch-race | PASS | PASS |
| file-attachment-identity-live | `test.fixme` (FA-12) | remove fixme, PASS |
| queue-mode-switch-midsession | `test.fixme` (FA-12/M7.9) | remove fixme, PASS |
| task-anchor-lifecycle | PASS (basic) | tighten with phase/progress testids |
| swarm-review-gate | `test.fixme` (M7.9 scaffold) | remove fixme, flesh out |

## Dependencies

- **FA-12 landing**: queue-mode-speculative, mixed-long-short-speculative,
  file-attachment-identity-live, queue-mode-switch-midsession.
- **M7.9 landing**: queue-mode-steer (semantics), queue-mode-interrupt
  (stricter labelling), task-anchor-lifecycle (phase/progress testids),
  swarm-review-gate.
- **FA-8 streamId envelope stabilization**: out-of-order-response.

## Guardrails

- Test-only; never modifies production code.
- Every spec creates its own fresh session via `createNewSession`
  so specs don't interfere.
- Failing specs use `test.fixme(true, 'reason — pointer to follow-up')`
  with an explicit comment, never silent skips.
- `test.skip(() => !TEST_URL)` at describe-block level so the suite
  is a no-op without a target.

## Shared helpers

All matrix helpers live in `tests/matrix-helpers.ts`:

- `setQueueMode(page, mode)`
- `fireRapidPrompts(page, prompts, intervalMs)`
- `waitForAllAssistantsContent(page, expectedCount, timeoutMs)`
- `snapshotTaskStore(page)`
- `waitForTerminalTask(request, baseURL, token, profile, sessionId, timeoutMs)`
- `getActiveSessionIdOrNull(page)`
- `countBubbles(page)`
- `buildEchoShellPrompt(marker)`
- `resetQueueMode(page)`
- `expectAssistantsContainAll(page, markers)`

These stack on top of `live-browser-helpers.ts` so DOM selectors stay
in one place.
