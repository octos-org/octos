# Open Issue Closure Contract Workstreams

Status: active closure contract
Date: 2026-05-23
Repository: octos-org/octos
Input snapshot: 70 open GitHub issues from `gh issue list --state open`

## Goal

Close the remaining open issue backlog without losing the distinction between
implemented code, missing code, missing tests, missing live proof, and parent
tracker dependencies.

This contract is intentionally operational. Every row below names the issue
nature, current closure status, and the concrete path to close it. Do not close
an issue just because code appears nearby. Close only when the row's closure
evidence exists and has been linked in a GitHub comment or closing PR.

## Status Vocabulary

| Status | Meaning |
|---|---|
| Closed | GitHub issue is already closed; the row is retained for historical traceability and should not be selected for new closure work. |
| Closure PR open - review gate | A PR that claims closure exists and CI is green/mergeable, but repository policy still requires review. Do not manually close; let the PR close the issue after review and merge. |
| Support PR open - review gate | A non-closing or partial PR exists and is green/mergeable, but repository policy still requires review. Keep the issue open until the remaining acceptance is covered. |
| Complete - pending closure audit | Merged work appears to cover the issue, but the issue was not auto-closed. Verify acceptance against the issue body, comment with evidence, then close manually if satisfied. |
| Partial - pending coding | Some implementation exists, but the issue acceptance is incomplete or only present in dirty/local work. Finish and merge a clean PR. |
| Partial - pending docs | Some documentation cleanup exists, but the issue acceptance is incomplete or only covered by review-gated PRs. Finish the docs audit after dependencies merge. |
| Pending coding | No accepted implementation is known. Build the feature or fix. |
| Pending tests | Runtime behavior may exist, but the requested regression/property/e2e coverage is missing. |
| Pending validation | Code may exist, but live soak, fleet proof, artifact evidence, or manual verification is missing. |
| Pending dependencies | Parent tracker or later slice blocked by child issues, milestone contracts, or cross-repo work. |
| Pending docs | Documentation or runbook update is the main closure requirement. |

## Closure Rules

1. Every code issue closes through a PR that says `Closes #NNN` unless the issue
   is validation-only and no code changed.
2. Every validation issue needs an evidence comment with command, commit SHA,
   environment, artifact path, pass/fail summary, and remaining gaps.
3. Parent trackers close only after all child rows are either closed or
   explicitly deferred in a new successor issue.
4. Feature slices close only when the public API, tests, docs, and compatibility
   story match the issue body.
5. Do not close an issue because a partial PR mentions it. "Partial #NNN" is not
   closure.
6. If a row is marked "Complete - pending closure audit", do a direct audit
   before closing: inspect merged PRs, run focused tests, and paste the exact
   evidence into the issue.

## Workstream Summary

| Workstream | Count | Issues |
|---|---:|---|
| WS-1 TUI, UX gate, onboarding | 8 | #1068 #1067 #1066 #1065 #1062 #1056 #918 #837 |
| WS-2 Agent, orchestration, context, swarm, hardening | 13 | #1023 #897 #890 #706 #654 #511 #413 #412 #297 #296 #295 #294 #293 |
| WS-3 Pipeline, budgets, metrics, workflow | 6 | #964 #615 #321 #228 #227 #11 |
| WS-4 Skills, media, channels | 10 | #1142 #1041 #895 #893 #889 #652 #336 #261 #257 #87 |
| WS-5 Admin, dashboard, setup, users, auth, config | 20 | #907 #904 #626 #512 #431 #429 #428 #427 #426 #425 #424 #423 #422 #420 #290 #289 #288 #121 #120 #117 |
| WS-6 UI protocol, web chat, web client, document UX | 8 | #716 #573 #383 #334 #333 #332 #323 #77 |
| WS-7 Deployment, platform, sandbox, robotics, gateway | 5 | #455 #381 #239 #237 #235 |

## WS-1: TUI, UX Gate, Onboarding

Intent: finish the real tmux UX gate and onboarding proof so TUI-facing work can
be closed from evidence rather than manual inspection.

| Issue | Nature | Status | How to close |
|---|---|---|---|
| #1062 | Testing contract, parent tracker | Pending dependencies | Treat as M19 parent. Audit PR #1155 and PR #1075, then close only after #1065, #1066, #1067, and #1068 are closed or explicitly deferred to successor issues. Evidence must include `ux:scenario:list`, `ux:tmux:run`, and `ux:tmux:validate` passing for the required scenario pack. |
| #1065 | Harness coding, e2e testing | Closure PR open - review gate | PR #1338 carries `Closes #1065` and has current-main real tmux evidence, but is blocked by `REVIEW_REQUIRED`. PR #1307 and PR #1217 are older superseded/supporting runner-contract work. Do not manually close; let #1338 close the issue after required review and merge. |
| #1066 | Harness coding, validator tests | Closure PR open - review gate | PR #1232 claims closure and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let #1232 close the issue after required review and merge. |
| #1067 | E2E scenario migration | Partial - pending validation | PR #1075 says several M19 scenarios are represented with blocked reasons. Successor #1313 and #1314 have review-gated closure PRs; #1312 remains blocked pending task capabilities. Close only after the initial UX scenario pack is migrated under the manifest and runner, or remaining blocked scenarios are explicitly deferred. |
| #1068 | CI reporting, docs | Closure PR open - review gate | PR #1264 claims closure and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let #1264 close the issue after required review and merge. |
| #1056 | Onboarding matrix, validators | Pending dependencies | PR #1304 and PR #1321 are non-closing onboarding hardening/validator slices with green checks but `REVIEW_REQUIRED`. Keep the parent open until the full onboarding matrix and live proof are present. |
| #918 | Documentation | Closed | Closed on 2026-05-24. Do not select for new closure work. |
| #837 | Fleet deployment, live validation | Pending validation | Deploy the WS-only binary to mini1, mini2, mini3, and mini5, leaving mini4 on its intended line. Verify `/api/status`, `/api/chat?stream=true` 404, `/api/ui-protocol/ws` upgrade, and 9/9 soak on each mini. Close with host matrix and artifact paths. |

## WS-2: Agent, Orchestration, Context, Swarm, Hardening

Intent: finish the backend-owned orchestration and context-management work that
supports full subagent orchestration, durable context, and operator hardening.

| Issue | Nature | Status | How to close |
|---|---|---|---|
| #1023 | Live validation | Pending validation | Remaining proof is explicit budget exhaustion soak, explicit `spawn_agent` soak with at least three native children, and M16 TUI tmux soak. Run all against the intended live backend, link artifacts, and close only when all pass or failures are split into new issues. |
| #897 | Agent context coding | Closure PR open - review gate | PR #1266 carries `Closes #897` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #890 | Production soak | Pending validation | Run SOAK-3, SOAK-5, SOAK-7, and SOAK-8 from `workstreams/M11-SOAK-TESTS.md` against the live fleet. Close with per-scenario artifact paths and observed runtime counters. |
| #706 | Preservation and compaction coding/tests | Closure PR open - review gate | PR #1248 carries `Closes #706` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #654 | Property testing | Closed | Closed by merged PR #1200 on 2026-05-24. A 2026-05-26 post-closure audit re-ran the Rust property test and Playwright property script on current main. Do not select for new closure work. |
| #511 | Live gate coding, e2e | Support PR open - review gate | PR #1299 adds the M7 swarm live gate and is mergeable with green checks, but it only `Refs #511`. Keep the issue open until canary gate logs and artifact evidence satisfy the closure requirements. |
| #413 | Canary soak, regression triage | Pending validation | Run sustained canary traffic using Phase 2 observability counters. File separate bugs for regressions. Close this issue only when the soak period completes and counters show no unexplained regressions. |
| #412 | Hardening parent tracker | Pending dependencies | Parent for Phase 3 coding-loop exploitation and operator hardening. Close only after #413 and any hardening child issues are closed or explicitly superseded. |
| #297 | Swarm mailbox coding | Pending dependencies | Blocked by #295 / mailbox backend primitives. PR #1206 is open for mailbox backend primitives and is review-gated; do not start `IdleNotification` closure until that dependency lands. |
| #296 | Bridge/session ingress coding | Closure PR open - review gate | PR #1303 carries `Closes #296` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #295 | Swarm durability coding | Support PR open - review gate | PR #1206 provides mailbox backend primitives and is review-gated, but it is non-closing for #295. Finish restart/recovery semantics in a later closure PR after that dependency lands. |
| #294 | Spawn isolation coding | Closure PR open - review gate | PR #1250 carries `Closes #294` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #293 | Async approval coding | Closure PR open - review gate | PR #1272 carries `Closes #293` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |

## WS-3: Pipeline, Budgets, Metrics, Workflow

Intent: make pipeline execution observable, budget-aware, rate-limit-safe, and
accounted for across sessions.

| Issue | Nature | Status | How to close |
|---|---|---|---|
| #964 | Pipeline bug, recovery | Closure PR open - review gate | PR #1274 carries `Closes #964` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #615 | Budget/cost validation | Support PR open - review gate | PR #1275 references #615 and is mergeable with green checks, but it is non-closing. Keep the issue open until the two-sequential-pipeline validation evidence is posted or a closing PR lands. |
| #321 | Metrics storage coding | Pending coding | Persist session usage totals and provider/model consumption analytics. Define schema, migration/backfill behavior, and dashboard/API read path. Close with tests over restart. |
| #228 | Pipeline hook feature | Closure PR open - review gate | PR #1268 carries `Closes #228` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #227 | Rate-limit coordination | Closure PR open - review gate | PR #1269 carries `Closes #227` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #11 | Billing/time-series storage | Pending coding | Design and implement persistent billing/time-series metrics storage. Close only with schema, retention policy, write/read tests, and migration notes. |

## WS-4: Skills, Media, Channels

Intent: stabilize skill outputs, media generation, channel integrations, and
profile-scoped skill behavior.

| Issue | Nature | Status | How to close |
|---|---|---|---|
| #1142 | Email channel bug | Pending coding | Reproduce email channel context bloat, identify retained context source, add cap/summarization/pruning, and test multi-message thread growth. Close with before/after context-size evidence. |
| #1041 | Plugin output contract | Closure PR open - review gate | PR #1257 carries `Closes #1041` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #895 | Skill documentation | Pending validation | Upstream mofa-skills PR #64 is merged, but closure still requires deployment plus a live generation transcript showing no preemptive `fm_voice_list` call. |
| #893 | Podcast parser bug | Closed | Closed on 2026-05-24. Do not select for new closure work. |
| #889 | TTS persistence bug | Closure PR open - review gate | PR #1237 carries `Closes #889` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. PR #1261 is related AppUI regression coverage only. Do not manually close; let the closing PR merge after required review. |
| #652 | TTS clone persistence bug | Closed | Closed on 2026-05-25. Do not select for new closure work. |
| #336 | DingTalk channel feature | Closure PR open - review gate | PR #1285 carries `Closes #336` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #261 | Output naming feature | Closure PR open - review gate | PR #1255 carries `Closes #261` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #257 | Feishu tests | Closure PR open - review gate | PRs #1256 and #1286 cover Feishu reply-threading tests and are review-gated. #1256 has the stronger current-main validation because it also passes strict Feishu clippy with `-D warnings`. Merge only one accepted PR after required review. |
| #87 | Skill permission bug | Closure PR open - review gate | PR #1271 carries `Closes #87` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |

## WS-5: Admin, Dashboard, Setup, Users, Auth, Config

Intent: clean up operator-facing admin flows, setup safety, user/profile
correctness, and config-path correctness.

| Issue | Nature | Status | How to close |
|---|---|---|---|
| #907 | Daemon restart bug | Closure PR open - review gate | PR #1225 carries `Closes #907` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #904 | Admin API feature | Closure PR open - review gate | PR #1226 carries `Closes #904` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #626 | Profile isolation bug | Closure PR open - review gate | PR #1222 carries `Closes #626` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #512 | E2E test coverage | Closure PR open - review gate | PR #1243 carries `Closes #512` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #431 | API client typing | Closure PR open - review gate | PR #1223 carries `Closes #431` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #429 | Admin error response bug | Closure PR open - review gate | PRs #1212 and #1294 both carry `Closes #429` and are review-gated; merge only one accepted closure path and retarget or close the duplicate. |
| #428 | Users UI feature | Closure PR open - review gate | PR #1246 carries `Closes #428` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #427 | Metrics UI feature | Closure PR open - review gate | PR #1240 carries `Closes #427` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #426 | Settings scoping feature | Closure PR open - review gate | PR #1241 carries `Closes #426` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #425 | AdminBot UI feature | Closure PR open - review gate | PR #1213 carries `Closes #425` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #424 | Security/audit feature | Closure PR open - review gate | PR #1249 carries `Closes #424` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #423 | User role feature | Closure PR open - review gate | PR #1244 carries `Closes #423` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #422 | User deletion bug | Closure PR open - review gate | PR #1224 carries `Closes #422` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #420 | UI correctness | Closure PR open - review gate | PR #1239 carries `Closes #420` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Duplicate PR #1296 also closes #420 but currently has a red `test-octos-cli` flake; merge only one accepted closure path. |
| #290 | CLI safety bug | Closure PR open - review gate | PRs #1234 and #1288 both carry `Closes #290` and are review-gated; merge only one accepted closure path and retarget or close the duplicate. |
| #289 | CLI init feature | Closure PR open - review gate | PR #1247 carries `Closes #289` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #288 | CLI init feature | Closure PR open - review gate | PR #1238 carries `Closes #288` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #121 | Stale error cleanup | Partial - pending docs | PRs #1208 and #1245 cover separate stale-message slices and are review-gated. After they merge, rerun a final user-facing stale-message grep/audit before resolving #121. |
| #120 | Auth config bug | Closure PR open - review gate | PR #1262 carries `Closes #120` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #117 | Stale docs cleanup | Closure PR open - review gate | PR #1259 refreshes stale skill/API references and is review-gated. Do not duplicate the docs cleanup; merge after required review, then close only if grep evidence satisfies the issue. |

## WS-6: UI Protocol, Web Chat, Web Client, Document UX

Intent: finish protocol documentation consistency and close web/chat UX gaps
that sit above the backend protocol.

| Issue | Nature | Status | How to close |
|---|---|---|---|
| #716 | Protocol docs/testing | Closure PR open - review gate | PR #1228 carries `Closes #716` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. PR #1302 is related non-closing UPCR coverage. Merge only the accepted closure path after required review. |
| #573 | Web client migration | Pending coding | Complete web client adoption of UI Protocol v1. Close with protocol fixture tests and manual/web smoke showing no legacy dependency. |
| #383 | Web task tracker bug | Pending coding | Rehydrate octos-web cross-session background task tracker on page load. Add reconnect/reload test and close. |
| #334 | Chat title UX | Closed | Closed on 2026-05-26. Do not select for new closure work. |
| #333 | Chat layout UX | Pending coding | Improve chat sidebar and file panel layout behavior. Add responsive tests or screenshots for narrow/wide layouts. |
| #332 | Chat shell redesign | Pending coding | Redesign web chat shell with intentional motion and glass-panel style only if still desired. Close with screenshots and accessibility pass. |
| #323 | PDF ingestion feature | Closure PR open - review gate | PR #1270 carries `Closes #323` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #77 | Web site builder feature | Pending coding | Implement local site builder with tunnel publishing. Define scope, security model, artifact handling, and end-to-end smoke. |

## WS-7: Deployment, Platform, Sandbox, Robotics, Gateway

Intent: make deployment targets and platform-specific runtime behavior explicit,
tested, and documented.

| Issue | Nature | Status | How to close |
|---|---|---|---|
| #455 | Robotics integration | Pending coding | Add real dora-rs forwarding for octos-dora-mcp. Include integration test or documented hardware/simulator proof. |
| #381 | Gateway shutdown bug | Closure PR open - review gate | PR #1215 carries `Closes #381` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #239 | Windows deploy feature | Closure PR open - review gate | PR #1254 carries `Closes #239` and is mergeable with green checks, but is blocked by `REVIEW_REQUIRED`. Do not manually close; let the PR close the issue after required review and merge. |
| #237 | Linux deploy feature | Pending validation | Verify current deploy scripts against Linux bare-metal. If support is incomplete, finish it; if complete, close with command transcript and target OS matrix. |
| #235 | Linux sandbox feature | Pending coding | Add Landlock and seccomp sandbox support for Linux containers. Include capability detection, fallback behavior, and tests or container proof. |

## Recommended Closure Order

1. Clear approved review-gated PRs as soon as eligible approvals arrive. Current
   no-duplicate priorities include #1256 for #257, #1338 for #1065, #1232 for
   #1066, #1264 for #1068, #1303 for #296, and #1259 for #117.
2. Resolve duplicate closure paths before merge: #1256/#1286 for #257,
   #1212/#1294 for #429, #1234/#1288 for #290, and #1239/#1296 for #420.
3. Continue partial orchestration work where no closure PR exists yet: #295 and
   #297 remain blocked on mailbox primitives; #511 still needs live gate proof.
4. Run validation-only soaks after code has landed: #1023, #890, #413, #837,
   #615.
5. Then close parent trackers: #1062 and #412.

## Evidence Comment Template

Use this when closing validation or audit issues:

```markdown
Closure evidence:

- Commit / PR:
- Environment:
- Commands:
- Artifacts:
- Result:
- Remaining gaps:

Decision: closing as completed because the issue acceptance is satisfied.
```
