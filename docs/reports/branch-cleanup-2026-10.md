# Branch cleanup — 2026-10

One-off cleanup of the 732-branch backlog, then a standing policy
([`docs/CI_POLICY.md`](../CI_POLICY.md)) so it cannot grow back.

## Result

| | before | after |
| --- | --- | --- |
| Remote branches | 732 | **285** |
| Deleted | — | 447 |

- **443** branches deleted because every change they carry is already in
  `main` (ancestor of `main`, or patch-equivalent — squash/rebase-merged,
  verified with `git cherry`).
- **4** `backup/` + `archive/` snapshots deleted (explicitly disposable by
  naming; kept in a separate recovery bundle).

Everything with unmerged work was **left alone**. The count did not reach the
original target of <50, and it should not have: getting there requires deleting
branches whose work is *not* in `main`, which is a human decision, not a
script's. Those are listed below.

## What was preserved

- `main`
- `release/*`, `hotfix*`, `chore/release*`, `appcard/*`
- every branch with an **open PR** (13 same-repo; 3 more are fork PRs with no
  branch in this repo)
- every branch with unmerged, un-superseded commits

## Recovery

Nothing was deleted without a recovery path:

```
/tmp/octos-branch-cleanup/branch-cleanup-*.manifest   branch -> tip SHA (443)
/tmp/octos-branch-cleanup/deleted-branches-*.bundle   self-contained git bundle (112M)
/tmp/octos-branch-cleanup/backup-archive.manifest     the 4 snapshots
/tmp/octos-branch-cleanup/backup-archive.bundle       their bundle
```

Restore one branch:

```bash
git push origin <sha>:refs/heads/<branch>
```

Restore everything from a bundle:

```bash
git fetch /tmp/octos-branch-cleanup/deleted-branches-*.bundle \
  'refs/heads/*:refs/heads/restored/*'
```

## Remaining: needs a human decision

### Idle > 3 months, unmerged — 70 branches

These may still hold unmerged work. Close them (delete) if superseded, or open a
PR if they are still wanted.

origin/allow_insecure (最后更新: 2026-07-08, 作者: alanpoon, 落后 base 1075 commits, 最后一次提交: "insecure")
origin/chore/homebrew-in-repo (最后更新: 2026-07-08, 作者: ymote, 落后 base 1056 commits, 最后一次提交: "fix(update): retarget in-app Homebrew upgrade hint to the in-repo tap")
origin/codex/1065-stdio-final-marker-current-65d6 (最后更新: 2026-06-21, 作者: Yue Chen, 落后 base 1127 commits, 最后一次提交: "test(e2e): opt stdio tmux solo scenarios into solo serve")
origin/codex/cli-agent-test-script-flake (最后更新: 2026-06-02, 作者: Yue Chen, 落后 base 1331 commits, 最后一次提交: "test(cli): avoid temp script exec race")
origin/codex/home-profile-config (最后更新: 2026-06-14, 作者: BH3GEI, 落后 base 1170 commits, 最后一次提交: "Add profile-backed home dashboard config")
origin/codex/issue-1314-backpressure-coverage-gate (最后更新: 2026-06-21, 作者: Yue Chen, 落后 base 1111 commits, 最后一次提交: "fix(memory): flush daily note appends")
origin/codex/issue-1344-email-outbound (最后更新: 2026-06-21, 作者: ymote, 落后 base 1132 commits, 最后一次提交: "fix(email): set origin on make_inbound test helper (drift vs main InboundMessage)")
origin/codex/issue-235-linux-container-sandbox (最后更新: 2026-06-22, 作者: ymote, 落后 base 1102 commits, 最后一次提交: "ci: reduce octos-agent test thread pressure")
origin/codex/issue-420-users-confirm-dialog (最后更新: 2026-06-01, 作者: Yue Chen, 落后 base 1334 commits, 最后一次提交: "test(dashboard): cover confirm dialog cancel behavior")
origin/codex/live-e2e-default-gate (最后更新: 2026-06-21, 作者: Yue Chen, 落后 base 1127 commits, 最后一次提交: "test(e2e): gate documented live specs by default")
origin/codex/m19-evidence-jsonl-append-lock (最后更新: 2026-06-02, 作者: Yue Chen, 落后 base 1331 commits, 最后一次提交: "test(cli): isolate skill compat cache home")
origin/codex/m19-scenario-capability-fixture (最后更新: 2026-06-21, 作者: Yue Chen, 落后 base 1127 commits, 最后一次提交: "test(swarm): avoid wall-clock fanout assertion")
origin/codex/profile-config-home-runtime (最后更新: 2026-06-17, 作者: ymote, 落后 base 1167 commits, 最后一次提交: "fix(profile): scope env_vars wholesale-replace to self-service paths (#1467 review round 2)")
origin/docs/book-feature-refresh (最后更新: 2026-07-08, 作者: ymote, 落后 base 1056 commits, 最后一次提交: "docs(book): codex round 14 (P3s) — memory IDs, comment XML-escape, CI-set wording")
origin/docs/issue-117-stale-docs-cleanup (最后更新: 2026-06-02, 作者: Yue Chen, 落后 base 1331 commits, 最后一次提交: "docs: finish serve port reference refresh")
origin/docs/newbie-first (最后更新: 2026-07-07, 作者: ymote, 落后 base 1072 commits, 最后一次提交: "docs: codex folds — auth login step, honest data-locality claim, platform list")
origin/docs/readme-refresh (最后更新: 2026-07-07, 作者: ymote, 落后 base 1075 commits, 最后一次提交: "docs: codex review folds — bundle-accurate skill list, admin SPA separation")
origin/docs/ui-protocol-spec-consistency (最后更新: 2026-05-30, 作者: ymote, 落后 base 1349 commits, 最后一次提交: "docs(ui-protocol): resync v1 spec catalog with code + reconcile REST surface")
origin/feat/config-home-dotconfig (最后更新: 2026-06-06, 作者: ymote, 落后 base 1212 commits, 最后一次提交: "docs+ci: document config home (~/.config/octos) + normalize OCTOS_CONFIG_DIR in service env")
origin/feat/config-slim-server (最后更新: 2026-07-12, 作者: ymote, 落后 base 966 commits, 最后一次提交: "fix(config): list only compiled commands in the overview (codex P3)")
origin/feat/danger-full-access-flag (最后更新: 2026-07-11, 作者: ymote, 落后 base 982 commits, 最后一次提交: "fix(serve): every permission-bearing cache caller pairs its snapshot with a pre-resolution epoch")
origin/feat/embed-octos-web (最后更新: 2026-06-06, 作者: ymote, 落后 base 1215 commits, 最后一次提交: "ci: one command per line in SPA-build steps so set -e catches failures (codex)")
origin/feat/embedding-model-config (最后更新: 2026-07-08, 作者: ymote, 落后 base 1052 commits, 最后一次提交: "fix(embedding): round-8 finding — non-openai providers fall back to the registry base URL")
origin/feat/home-integrations (最后更新: 2026-06-11, 作者: hzeng412, 落后 base 1206 commits, 最后一次提交: "feat(skills): Home Assistant + NAS integrations and MCP-per-profile")
origin/feat/init-key-capture (最后更新: 2026-07-07, 作者: ymote, 落后 base 1069 commits, 最后一次提交: "fix(init): honor --defaults + treat expired credentials as missing (codex folds)")
origin/feat/mcp-rmcp-oauth (最后更新: 2026-07-09, 作者: ymote, 落后 base 1025 commits, 最后一次提交: "Merge remote-tracking branch 'origin/main' into feat/mcp-rmcp-oauth")
origin/feat/memory-pr1-foundations (最后更新: 2026-07-08, 作者: ymote, 落后 base 1066 commits, 最后一次提交: "Merge branch 'main' into feat/memory-pr1-foundations")
origin/feat/memory-pr2-capture (最后更新: 2026-07-08, 作者: ymote, 落后 base 1064 commits, 最后一次提交: "Merge branch 'main' into feat/memory-pr2-capture")
origin/feat/memory-pr3-extraction (最后更新: 2026-07-07, 作者: ymote, 落后 base 1062 commits, 最后一次提交: "fix(memory): distinguish flock contention from transient errors; de-flake release test")
origin/feat/octos-config-wizard (最后更新: 2026-07-12, 作者: ymote, 落后 base 970 commits, 最后一次提交: "fix(config): reject persisted full-access + keep explicit precedence (codex)")
origin/feat/octos-diagnostics-stage1 (最后更新: 2026-06-08, 作者: ymote, 落后 base 1210 commits, 最后一次提交: "fix(diagnostics): Unknown install is UpdateAvailable, not package-manager owned (codex)")
origin/feat/profile-requested-id (最后更新: 2026-07-12, 作者: ymote, 落后 base 966 commits, 最后一次提交: "fix(profile): pre-save email revalidation for cross-writer race")
origin/feat/reasoning-stream (最后更新: 2026-07-09, 作者: ymote, 落后 base 1007 commits, 最后一次提交: "Merge remote-tracking branch 'origin/main' into feat/reasoning-stream")
origin/feat/rfc1-mofa-make-dispatcher (最后更新: 2026-06-22, 作者: ymote, 落后 base 1106 commits, 最后一次提交: "test(agent): refresh mofa_make fixtures for current main")
origin/feat/server-brew-npm-install (最后更新: 2026-06-06, 作者: ymote, 落后 base 1217 commits, 最后一次提交: "fix(packaging): exec wrapper (macOS), drop broken proxy, file:// + asset-race (codex)")
origin/feat/sessions-in-cwd (最后更新: 2026-07-13, 作者: ymote, 落后 base 964 commits, 最后一次提交: "fix(sessions): 3 codex P1s in the appui.sessions_in_cwd (flag-ON) path")
origin/feat/sse (最后更新: 2026-06-16, 作者: alanpoon, 落后 base 1170 commits, 最后一次提交: "extract_runsplash_block")
origin/feat/ui-protocol-session-rollback (最后更新: 2026-07-03, 作者: alanpoon, 落后 base 1090 commits, 最后一次提交: "feat(ui-protocol): add SESSION_ROLLBACK method + SessionRollbackParams/Result")
origin/feat/voice-llm-error-fail-fast (最后更新: 2026-06-25, 作者: alan0x, 落后 base 1094 commits, 最后一次提交: "feat(agent): FailFast LLM bail emits TurnFailure (classify once), no 2nd call, hook-deny excluded")
origin/feat/xdg-config-home (最后更新: 2026-06-06, 作者: ymote, 落后 base 1216 commits, 最后一次提交: "test(auth): gate at_tightens_loose_permissions_on_open behind #[cfg(unix)] (codex)")
origin/feature/ominix-runtime-install-repair (最后更新: 2026-06-23, 作者: ymote, 落后 base 1098 commits, 最后一次提交: "fix(api): drop needless borrow of octos_home in platform_runtime_bootstrap")
origin/fix/1477-voice-envelope-deltas (最后更新: 2026-06-23, 作者: ymote, 落后 base 1158 commits, 最后一次提交: "fix(voice): dual-emit projection envelope for voice message deltas")
origin/fix/1607-sandbox-project-validators (最后更新: 2026-07-10, 作者: ymote, 落后 base 1005 commits, 最后一次提交: "fix(security): don't re-add policy-denied run_pipeline on session rebind (codex r5)")
origin/fix/bundled-skill-preflight-staleness (最后更新: 2026-06-06, 作者: ymote, 落后 base 1219 commits, 最后一次提交: "fix(bootstrap): exempt skill-evolve from preflight instead of forcing it into bundles (codex)")
origin/fix/cpu_max (最后更新: 2026-06-16, 作者: alanpoon, 落后 base 1170 commits, 最后一次提交: "fix cpu max")
origin/fix/diff-preview-render-untracked (最后更新: 2026-07-11, 作者: ymote, 落后 base 976 commits, 最后一次提交: "fix(ui): harden untracked diff fallback (codex review)")
origin/fix/inloop-compaction-ux (最后更新: 2026-07-10, 作者: ymote, 落后 base 992 commits, 最后一次提交: "fix(api): emit in-loop compaction events after releasing the scratch lock (codex R2)")
origin/fix/issue-120-auth-store-config-path (最后更新: 2026-06-01, 作者: Yue Chen, 落后 base 1338 commits, 最后一次提交: "fix(cli): make auth store path configurable")
origin/fix/issue-228-pipeline-guards (最后更新: 2026-06-21, 作者: ymote, 落后 base 1119 commits, 最后一次提交: "test(pipeline): add guards field to ExecutorConfig test/example literals")
origin/fix/issue-87-subaccount-tool-regression (最后更新: 2026-06-01, 作者: Yue Chen, 落后 base 1338 commits, 最后一次提交: "fix(cli): isolate profile plugin verified cache")
origin/fix/issue-897-spawn-only-auto-summary (最后更新: 2026-05-28, 作者: Yue Chen, 落后 base 1360 commits, 最后一次提交: "test(agent): refresh auto-summary fixtures")
origin/fix/ledger-orphan-reconciliation (最后更新: 2026-07-04, 作者: ymote, 落后 base 1085 commits, 最后一次提交: "review: preserve turn topic on synthesized terminal; treat interrupted agents as terminal")
origin/fix/llm-transport-send-retry (最后更新: 2026-07-11, 作者: ymote, 落后 base 978 commits, 最后一次提交: "fix(llm): document at-least-once retry tradeoff + hermetic caching tests")
origin/fix/orphan-sweep-liveness-gate (最后更新: 2026-06-04, 作者: ymote, 落后 base 1252 commits, 最后一次提交: "docs(agent): correct stale guard-arming comments to foreground (codex round-6 nit)")
origin/fix/p1-tenant-isolation (最后更新: 2026-07-05, 作者: ymote, 落后 base 1078 commits, 最后一次提交: "fix(serve): router scope gate must honor authorized routed profile (codex P2)")
origin/fix/p2-batch (最后更新: 2026-07-06, 作者: ymote, 落后 base 1076 commits, 最后一次提交: "fix(autonomy): defer before draining an already-in-flight session (codex fold 4)")
origin/fix/rollback-followups (最后更新: 2026-07-03, 作者: ymote, 落后 base 1089 commits, 最后一次提交: "fix(session): keep the richer (canonical) row on cross-layout dedup (codex follow-up)")
origin/fix/runtime-p1-batch2 (最后更新: 2026-07-05, 作者: ymote, 落后 base 1079 commits, 最后一次提交: "review(codex): live-set counts in fan-out cap; no orphan tool_result blocks")
origin/fix/session-list-solo-profile (最后更新: 2026-07-04, 作者: ymote, 落后 base 1082 commits, 最后一次提交: "fix(session): last_prompt honors rollback markers; drop checked-in lock (codex)")
origin/fix/trim-openrouter-kimi-thinking (最后更新: 2026-06-05, 作者: ymote, 落后 base 1235 commits, 最后一次提交: "fix(llm): 1M context-window fallback for deepseek-v4 + minimax-m3")
origin/fix/tts-final-syllable-truncation (最后更新: 2026-06-13, 作者: alan0x, 落后 base 1170 commits, 最后一次提交: "fix(voice): normalise TTS tail to a full stop so the final syllable isn't clipped")
origin/fix/ux-validator-registry-1066 (最后更新: 2026-06-22, 作者: Yue Chen, 落后 base 1107 commits, 最后一次提交: "test(e2e): expose UX validator registry output")
origin/homebrew/octos-v2.0.1 (最后更新: 2026-07-12, 作者: octos-release-bot, 落后 base 968 commits, 最后一次提交: "octos v2.0.1: update Homebrew formula")
origin/octos_docker_to_use_serve (最后更新: 2026-06-29, 作者: alanpoon, 落后 base 2824 commits, 最后一次提交: "Wire real stdin approval into octos chat for Decision::Ask commands")
origin/perf/voice-cloud-tts-shared-client (最后更新: 2026-06-23, 作者: alan0x, 落后 base 1101 commits, 最后一次提交: "perf(voice): reuse a process-wide HTTP client for Volcano cloud TTS")
origin/perf/voice-tts-latency (最后更新: 2026-06-23, 作者: alan0x, 落后 base 1101 commits, 最后一次提交: "chore(voice): sync Cargo.lock for ws/rustls deps")
origin/rfc0/delete-lru-tool-deferral (最后更新: 2026-07-09, 作者: ymote, 落后 base 1036 commits, 最后一次提交: "docs: scrub stale activate_tools references in doc comments (RFC-0)")
origin/test/round12-slides-validator-v2 (最后更新: 2026-05-25, 作者: ymote, 落后 base 1385 commits, 最后一次提交: "test(e2e): two-phase outline + generate/go for round-12 slides validator (round-13)")
origin/test/slides-validator-disk-check-fallback (最后更新: 2026-05-25, 作者: ymote, 落后 base 1380 commits, 最后一次提交: "chore(typos): allowlist BRE (Basic Regular Expression — used in spec comments)")
origin/test/voice-perf-plus-selection (最后更新: 2026-06-16, 作者: alan0x, 落后 base 1170 commits, 最后一次提交: "Merge branch 'perf/voice-turn-lean-prompt' into test/voice-perf-plus-selection")

### Recent (< 30 days) and close to main, unmerged — 36

Review for merge.

origin/branding/octos-core-family-logo (最后更新: 2026-10-06, 新增: 5 commits, 落后: 64, 最后: "Apply selected orchid purple and shell pink logo palette")
origin/codex/trim-tool-descriptions (最后更新: 2026-10-09, 新增: 3 commits, 落后: 42, 最后: "test(agent): match compact model context description")
origin/feat/workspace-peer-teams (最后更新: 2026-10-08, 新增: 4 commits, 落后: 46, 最后: "test: cover workspace peer methods in AppUI dispatch probes")
origin/fix/2315-keychain-release-on-delete (最后更新: 2026-10-06, 新增: 2 commits, 落后: 72, 最后: "fix(api): release keychain items on the purge and user-delete paths too")
origin/fix/2328-api-channel-origin-host (最后更新: 2026-10-10, 新增: 5 commits, 落后: 36, 最后: "security(bus): harden the loopback parser and pin the fallback coverage")
origin/fix/2423-upcr-registry (最后更新: 2026-10-11, 新增: 2 commits, 落后: 26, 最后: "ci: run the UPCR filename registry lint in the check job")
origin/fix/2426-as-channel-hardening (最后更新: 2026-10-08, 新增: 2 commits, 落后: 47, 最后: "Address review: trim bind override, warn on port divergence, docs coverage")
origin/fix/2426-as-media-hardening (最后更新: 2026-10-13, 新增: 3 commits, 落后: 1, 最后: "fix(bus): address review — byte-cap filenames, tighten loose media dirs, cover whatsapp bridge media")
origin/fix/2453-book-zh-parity (最后更新: 2026-10-11, 新增: 3 commits, 落后: 20, 最后: "docs(book-zh): review round 2 — dated baseline exemptions, degenerate-input guards, ci wiring")
origin/fix/2661-upcr-gate-ci-wiring (最后更新: 2026-10-12, 新增: 2 commits, 落后: 13, 最后: "ci: wire the UPCR coverage gate and its self-test into the PR check job")
origin/fix/2678-windows-residuals (最后更新: 2026-10-05, 新增: 2 commits, 落后: 99, 最后: "test: assert git log status and use the abs() idiom")
origin/fix/2684-refresh-lock-retry (最后更新: 2026-10-11, 新增: 2 commits, 落后: 28, 最后: "test(memory-refresh): pin the retry recovery and cost note per review")
origin/fix/2696-purge-dir-removal-gate (最后更新: 2026-10-06, 新增: 3 commits, 落后: 66, 最后: "review follow-ups: root-guard the injection test, legacy-only tombstone errors comment, idempotency note (#2696)")
origin/fix/2702-cli-gateway-keychain-release (最后更新: 2026-10-06, 新增: 3 commits, 落后: 69, 最后: "test: pin the shared-account contract on the CLI/gateway keychain release paths")
origin/fix/2722-legacy-root-convergence (最后更新: 2026-10-07, 新增: 3 commits, 落后: 54, 最后: "fix(config): route status and resolve_config_file_path tests through the shared legacy root — review round 2")
origin/fix/2729-admin-auth-no-leak (最后更新: 2026-10-08, 新增: 4 commits, 落后: 45, 最后: "fix(bus): make the admin shell token compare constant-time and pin the rejection log")
origin/fix/2736-bus-token-surface (最后更新: 2026-10-09, 新增: 7 commits, 落后: 38, 最后: "review: pin the chat gate in CI and doc honesty fixes (#2736)")
origin/fix/2741-team-cap-gc (最后更新: 2026-10-10, 新增: 4 commits, 落后: 28, 最后: "fix(peers): guard the team eviction against turns in flight")
origin/fix/2751-api-channel-auth-surface (最后更新: 2026-10-09, 新增: 4 commits, 落后: 37, 最后: "fix(bus): review round — constant-time feishu signature compare, honest gate docs")
origin/fix/2756-feishu-missing-token (最后更新: 2026-10-10, 新增: 3 commits, 落后: 30, 最后: "fix(bus,cli): accept the verification token at header.token on v2.0 envelopes")
origin/fix/2758-host-allowed-duplicate-host (最后更新: 2026-10-10, 新增: 2 commits, 落后: 33, 最后: "fix(serve): fail the host-managed Host matcher closed on duplicates")
origin/fix/2798-distribution-alignment (最后更新: 2026-10-13, 新增: 2 commits, 落后: 2, 最后: "release: gate the workflow on npm/Homebrew distribution alignment")
origin/fix/context-management-journal (最后更新: 2026-10-11, 新增: 7 commits, 落后: 21, 最后: "style(api): simplify obsolete journal cleanup condition")
origin/fix/host-route-ownership-gates (最后更新: 2026-10-09, 新增: 7 commits, 落后: 54, 最后: "test(peers): apply BH3GEI's verified deadlock fix — the re-declaration before the drop(ws)")
origin/fix/ledger-bounded-session-paths (最后更新: 2026-10-07, 新增: 4 commits, 落后: 64, 最后: "fix(peers): serialize peer file writes with purge removal")
origin/fix/main-windows-frame-wait-budgets (最后更新: 2026-10-05, 新增: 2 commits, 落后: 97, 最后: "test(cli): convert the remaining loaded-shard waits around the composer")
origin/fix/main-windows-loopback-refused-budget (最后更新: 2026-10-07, 新增: 4 commits, 落后: 53, 最后: "test(octos-llm): soften the retry helper comment's refusal-latency claim")
origin/fix/main-windows-oup-acp-budgets (最后更新: 2026-10-09, 新增: 5 commits, 落后: 40, 最后: "Merge remote-tracking branch 'origin/main' into fix/main-windows-oup-acp-budgets")
origin/fix/main-windows-peer-turn-arrival-budget (最后更新: 2026-10-05, 新增: 2 commits, 落后: 98, 最后: "test(peers): converge the remaining arrival polls on the same budget")
origin/fix/nightly-windows-fleet-unix-gates (最后更新: 2026-10-06, 新增: 2 commits, 落后: 70, 最后: "test(fleet-worker): name the decisive contract in the gate comments")
origin/fix/oup-spec-consistency-20261007 (最后更新: 2026-10-07, 新增: 2 commits, 落后: 52, 最后: "test(oup): handle Windows line endings in spec fixtures")
origin/fix/persistent-session-history (最后更新: 2026-10-06, 新增: 6 commits, 落后: 68, 最后: "test(runtime): normalize Windows paths in workspace history assertion")
origin/fix/remove-dot-runtime (最后更新: 2026-10-05, 新增: 7 commits, 落后: 74, 最后: "fix(runtime): release refresh locks explicitly and update DOT retirement tests")
origin/fix/setup-token-constant-time (最后更新: 2026-10-06, 新增: 2 commits, 落后: 67, 最后: "style: rustfmt")
origin/fix/skill-install-containment (最后更新: 2026-10-06, 新增: 2 commits, 落后: 67, 最后: "style: rustfmt")
origin/fix/workspace-join-degrade (最后更新: 2026-10-10, 新增: 2 commits, 落后: 43, 最后: "docs(adr): record the join-failure degrade and the 1024 cap in the workspace-teams ADR")

## Appendix — deleted branches

Full list (branch + tip SHA at deletion). Recoverable individually from the
manifest/bundle above.

```
origin/chore/deploy-install-tmux e25a114909e669fc36354f8dd5dc3f6f71b1e7f5
origin/chore/fmt-octos-pipeline 5c0fdcf3016f2c35568c6c86475d85e2415ffbbc
origin/chore/gitignore-scratchpad f38bc7b2c6697396f16249ace150c14364d4ed17
origin/chore/model-catalog-deepseek-v4-minimax-m3 ee6a9e4d6121c0d44b627fe9beb3e0a138583db8
origin/chore/remove-committed-runtime-cruft 57e6ec7a85e0728744481ec2a0a9e05d69d6c9dc
origin/chore/remove-stray-0 ddc144cd42bcd594406d50e06e456e173b7f41e1
origin/chore/rename-octoscode-refs 963a62e6ee68c091015d60026c96be6e3244aece
origin/chore/rename-queuemode-steer-latest 708602d0c717889a5d4077964bffabaa0daf2e16
origin/chore/retire-memory-cron-rest cc47d42b705872389a643ab16f283bbd4872c1d8
origin/chore/scrub-personal-details 2faadc1ddbd7857ba49582532178c07ba87eb6ca
origin/chore/smaller-dev-builds-v2 a12c215ae8e2925e4ab3f6f09258a5e5aadd7543
origin/ci/check-arm-preserve-logs a70e9143d3accffcd5c6597ecff8444933457399
origin/ci/check-arm-record-findings f1c894007a4f04edeacf51d4f4053ccc2adc0728
origin/ci/hardening-gates 71d548193f5af668cb42b774a371edd426b57708
origin/ci/nightly-no-fail-fast c307489dd23922b5eb6afa06f5db40a6c015da4f
origin/ci/platform-checks-on-prs 4902f7cd178c30e82561d745ee371bb64060356a
origin/ci/prerelease-aware-publish acbee6560a960ef7bdecf35d933c46b1564dcc87
origin/ci/release-prerelease-flag-and-dispatch f5d0be42d60fca58948fe6ae56415466d4b329f4
origin/ci/research-only-skip 5d0fa8cabeae0ad36c13b1e0cc2b07be2768492e
origin/ci/split-test-octos-cli c23f2f97595ef6d33d6bf9a907c0e1a7939e354a
origin/ci/windows-report-full-truth 3295c3fd6d5d54f9b50871404a0ecc7d96368095
origin/ci/windows-timeout c8119d42c91b677cb666f7ac3eacd57251a45010
origin/closure/tui-task-control-1380 f614ab8442b498522be77df529b4c888c51f8443
origin/codex/fix-1358-ledger-rustfmt 48349d0f8d3d44f0e772032b060689779eaf8642
origin/codex/issue-1056-fast-onboarding-validators 4938acfa704e58887cc74c63a410797693df6ad3
origin/codex/issue-1056-required-release-skips 5032d223868ef8cc3cbf8a4b0e5d5841c0b51e77
origin/codex/issue-1279-session-sandbox 24fd2a56647c2d901bbf53ea0cf8f67fbd03cbfc
origin/codex/issue-1280-post-eviction-replay afe55ade52da844113c6fce64c7fdb7ca72b4131
origin/codex/issue-1389-git-env-detection 1b6a121737e98bb948c0f43d91f3d099b05df895
origin/codex/issue-237-deploy-harness 54b6ddfcc4f903e30ca418a6e2e9253386b16cba
origin/codex/issue-290-init-defaults-safety 56d6016d1ac63714f27dddbdf9d19fe51f4ceceb
origin/codex/issue-296-work-secret-ingress 1b07ed66fc62f6c19a5fccbbcd2eafde515435bb
origin/codex/issue-321-usage-analytics 94835b8aeceb9f04e3a90718da293f659472fd8f
origin/codex/issue-336-dingtalk-channel 17bf652d5f38a2e08f97e65a67f45329c03b7f58
origin/codex/issue-427-metrics-freshness-20260528 274301ccd75638350eec916e636af8539a4cbb25
origin/codex/issue-429-admin-conflict-json 32289b1957740846deeda1b2949525a9e3488e27
origin/codex/issue-511-swarm-live-gate 4dbe7f5ebf1b3b1b14b955f9b5f41fd9ce8ff75c
origin/codex/issue-895-agent-schema-validation 411cf7fa1214eb14bf32be53c5a8bf5a23f7c616
origin/codex/refresh-closure-contract-status 3708e878b2fd616ddbf92e1381ee5f48def972ab
origin/deploy/full-bundle 589fd33173714efec06f6ab66e5e30e5688073bb
origin/docs/adr-fleet-runtime 63aedfb362f21a33693100b6d2ad251104b236be
origin/docs/agent-topologies 2c1a80cf90c582f309ad6302453fdddb7a882a53
origin/docs/api-key-provider 61b66ec27ace576ceba0449f3610e11ff0cbc7df
origin/docs/autonomy-goals-loops 7c989b81a56d1c152e0a535877d3fedc016b0799
origin/docs/cli-agent-mode-readme 52613a645664d32b7f5cbd6d3c41811831aa6160
origin/docs/cloud-signup-to-web ea2e92eabe3c8a30e0b525e9e18fbe9f065a8a97
origin/docs/embedding-provider-llamacpp ed2c32cd97d772b62b711e080a2b996243d1f299
origin/docs/fleet-kernel-spec-v2 e34c0226eb6262906af8055497efefe5c8a8e8c2
origin/docs/fleet-kernel-v1 c48e0c29a4c15f4d9647a3b6028418d48fcfd22e
origin/docs/goal-supported-surface 6564fd27ad3a1ec6129d52e4b84e7370b90c8f54
origin/docs/harness-kernel-oup-readme-20260910 6d585ea2f430d90fe318d6aebbf9470e54d20c38
origin/docs/headless-json-scripting 31217a0295b7a152f759f963b0e0e0fd9654db2e
origin/docs/headless-option-matrix cfe13acd17e65e7f6fdc0785ffefe0d65514c06f
origin/docs/mcp-client-readme d2ee63bf2725bea127764c4ac48c16dbddf4e14f
origin/docs/move-hosting-to-web a981131e36b90a3c1496fe8ad07cad4220a6465e
origin/docs/octos-diagnostics-scope 1865443f2f0b2de54707ba066bb8b8c19805f92b
origin/docs/quickstart-validated 29614a8aeb83f425339027f0402671a1303d1a73
origin/docs/readme-library-usage b54ddc65dec3177d62aca8534769e06c41a36a7f
origin/docs/restore-readme-ascii-20260910 2f02d058dfe104795c70988faf98debac277260d
origin/docs/stale-error-messages-121 6f4e47545e703d7a61b286944c67e7219d4ae60b
origin/docs/ui-protocol-inventory-716 69d189feff0d92dca17dd815a07a5a243fc12331
origin/drainmain 712ad354c0c86ed9b4ad7d1e7fc610d5cfe815bd
origin/feat/1976-per-path-write-grants b1251cd17e65a2d9eed6e2171b671e6a9665183c
origin/feat/1977-monitor-runtime b51bb3e88ec6ff2fd7c7ce4648bc3be443ef1e30
origin/feat/2026-peer-rounds 247535915e2448809c8177f6f3ee87d6f0085995
origin/feat/admin-audit-log-424 14ad61b5df6c3d9f0d492edc7aa3fdcc286ac9b6
origin/feat/agent-mention-gating 5b6a0f5fdcf52b2a2bdb04a79cf92d80ada44ef3
origin/feat/chat-api-type f003bc9bd382332005a61703952f929482c3f353
origin/feat/chat-concurrent-data-dir 126ed164af694d2f847f73510265bf2a08ea18e8
origin/feat/chat-headless-yolo 44a6bca303b41f9eb4d19ba76ad3722a8442b297
origin/feat/chat-json b3df155dc059d5b74d8201e9bd433324bca9f4eb
origin/feat/chat-read-serve-profile 495f306bee0a9b4eb7b7eb13cf43a662269b5b8e
origin/feat/cli-agent-backend bccc70572cac88b39aa71cf1bd63feaebf8a84b6
origin/feat/context-compaction-mode 99d70ef0bddbbc4f04da8dd081f7db4f4700f6dd
origin/feat/cron-provenance-and-notify-mode f111cbd607cb10506fab84f9f3635484be128f50
origin/feat/doctor-installations cf71d8111f5906505a3f9e37b125627966e5b232
origin/feat/durable-host-tool-list 4481e9fdcea2215ddd7f177bd941db0078f4ee03
origin/feat/goal-context-segment d08197c23fba0ba5f71c889bf4269f5781c5cbb8
origin/feat/goal-create-tool-and-prompt 6470b147cb1228f67e78e001de988a8961a25276
origin/feat/goal-default-budget-100m f7abeac379550b8805166a9f69f56199d1dd55b3
origin/feat/goal-interactive-token-charge 03bc2ae3983d404f7b3df046b2a29586149d9160
origin/feat/host-app-peers 5dad8f5f566da56d5ed3c0887bb25c4c5206b036
origin/feat/hydrate-tool-call-identity 7cfee8b184834e7f7a07f5abe28b89c81306d5e2
origin/feat/init-api-type-selection-288 e09eb76187149f0d90ed0225ab56a40a5d45de33
origin/feat/init-custom-provider-289 8053e944348bb2956a4d0815ff1c1618bb9e5d2a
origin/feat/llm-summarization-compaction d10800ab7b0924dd45e405cb9cec27a40e5f2b4b
origin/feat/local-network-default aa3ded3ea6951044b665814a9162e85eeadebc85
origin/feat/memory-reindex a5298168588b50b4dc369ed8eb0c81f3a4379af2
origin/feat/mid-turn-steering 3077de27df02022b5fd78bc00ae89771c46f36af
origin/feat/multi-instance-stdio 169082d2dc945d6b1a2c9c5e516a022caece789c
origin/feat/observable-prompt-cache-usage 73784d21992947cf436f0888d00f4a3d730d997d
origin/feat/octos-diagnostics-stage2 09d2e92bad822962999ef2677b856d75e1fa95c7
origin/feat/octos-fleet-boot-resume a762d4ad6256b45e1ed6a6c9f242f816ce12156c
origin/feat/octos-fleet-goal 60e6ae331c8de86cb5a7ca59fbd736e82b182a21
origin/feat/octos-fleet-plan cd3d8b9ba4974a9efbb41bec49ed2003d2e99922
origin/feat/octos-fleet-prA-worker-grant 2c835a895cad0b300cce124ce4e40030ec6887ef
origin/feat/octos-fleet-prB-escalate 5e1286d8a63a79acdcc0a8e0d0b91bfc0f8c9a7b
origin/feat/octos-fleet-prC-worktree 8d9996cda7b9261e4a7635bad7b2d650518b027f
origin/feat/octos-fleet-rehydrate 3f281b6813a508031a22b82ef567d2406e3b572e
origin/feat/octos-fleet-store 6f484c9e151085ee390ded0227fe3333aed6e703
origin/feat/octos-fleet-wake f927a7052e3f41373b8f6482ca5f4553d3a95fd0
origin/feat/octos-fleet-worker c9ab5db19e1d62be6aeb89b53e7805a3b6fd643e
origin/feat/octos-pyo3 7375decb71b39073e65cb6e90efca62ce8f59e49
origin/feat/octos-uniffi 836ac84bf77a2b83a0aed602eea3fd44ae8a5e0a
origin/feat/octos-wasm c359166ff8b9abec7df111aa6d12ce85c2d3e7aa
origin/feat/oup-peer-token-budget f1d2a58e90c08a4925abcd4bc85d03130b40713c
origin/feat/peer-awaiting-wake a7d5292c43864d7d9c721e559e19b2dea9de5fbb
origin/feat/peer-context-read-parent 080f6cfae65592c9d3222972d221e4f1629cfebe
origin/feat/peer-host-tools 4f9f73fafce1a276ffb6926440048efa9ce04abb
origin/feat/peer-purge 2a749b6d0db53f38ff017c54ea05cc4dc60fd908
origin/feat/peer-respond 29e15170906b45d6c7bd21ec51d6036c46af8a95
origin/feat/peer-tools-unregister 96118e9d2d7c612c53afdc305304184c1cd8b857
origin/feat/per-session-thinking-effort b0a654aa52792752f31135d397c6c3dc12120612
origin/feat/phase1-goal-in-chat 709f4330c1eb1cfedd4454e2b3d6b72b7c186862
origin/feat/plan-todos-agent 16eee4c4a0f31f8867efe2870884faa504e76c34
origin/feat/plan-todos-core 8629eb9def8f71b371ce686f47bcfbe156c655ea
origin/feat/plan-todos-serve e5d24b37242d0a09191fe8a3c86067d019d192b8
origin/feat/profile-config-inheritance 149207f02c4d582a59f850fa47ffde549877f48e
origin/feat/projection-envelope-v2-core-server d0a786782b66c92df6ee85cb08411e310b8391c8
origin/feat/projection-v2-stage4-telemetry 031507444cba56ecd1ac1ff58d6aca2e4fa4875a
origin/feat/reasoning-effort-chat-path dfcfc27eb6cf6ff37e607e9195e31daf50d7d562
origin/feat/research-menu-server 7bc1af16731ef81076cc269a34a8d4f21701cb90
origin/feat/research-multilingual fc43e5389e755fc43d94b6fd6afda8dcda5e3fd8
origin/feat/research-provider-lane 3b32ba85b1354086abc17f097d330e2343e03732
origin/feat/searxng-parity-google 6a56b9f23512d2a67d3230b8853e07adf198cce7
origin/feat/serve-host-managed 64a233c7483601907819a87e0a5c49ff905145c7
origin/feat/serve-llm-compaction-flag 3548b14ae1c6b42b34f05796a6a3bb293746dee2
origin/feat/server-stop 6289b774c4381e0167861f11ae82c0080c195394
origin/feat/session-compact-method 0ace04970c8c6992b738f3adfdc29bfa47fa65fb
origin/feat/session-keepalive 9d904f8a7002e9b9620f0c205f2aa7af4b9f79b8
origin/feat/session-resume-rewind e7e0ff72b2d00ceaf7dbeec93d8df30084fd503a
origin/feat/session-tool-cap da71a191ecce1260374da4e7c140996379500d1c
origin/feat/share-running-turns 13406ba6a6aa71929ea1b5b0b1d464f55bef7ec8
origin/feat/shared-peer-conversation b2e82b1f186de8e8a9c0c2fece4b8c6671979a64
origin/feat/shell-background-task-tracking 6501a6bc262134e7c8c12911ca9f556f1b29dff3
origin/feat/skill-layering-v1 9b1fc38f6c31ac48ea7124f5d92fb1824df33d70
origin/feat/smart-home-backend-migration 2d3c4fe85118b7a1493a11ada22828c76ab209ad
origin/feat/stage5-server-remove-legacy-2 7001a4726efed6df6a8c5a50c3d788ae8e3db27e
origin/feat/thinking-default-clears 14c8837f2da752e91a88aae6a3217a269d7596f0
origin/feat/tui-compaction-ui d75105d554a8b6d25809a4fe3524f954ebe653c7
origin/feat/user-prompt-submit-hook db10b184f9163b22c73eace7b3469065d4d5e310
origin/feat/user-role-promote-demote-423 080a7d372ea0499cdbf3fbf7842e15260e83f7e2
origin/feat/users-bulk-actions-428 8f5d564be0c72d46841ba5e3aa1bd309545843a3
origin/fix/1440-wechat-qr-admin-profile f3d206428eb4b8e62a3390ca3dd53a033e520dd9
origin/fix/1463-m9-interrupt-cancel-user-questions 60c5232620217a88d351173a64ce83461db78a1a
origin/fix/1470-partial-nested-config-patch e3138af06ea0d34d3a5ad508b3a78a8d6d953377
origin/fix/1472-subaccount-rollback 40141a47176ee8fb44682a7aa5917b22485ca2fa
origin/fix/1473-voice-resolver-exact-id baa3a86c0f07d3239c192a79ecdf89d8dde912c5
origin/fix/1498-ominix-home-discovery-err-log 328329d1ca46c8fbed696cbfd7aad207a9eabab9
origin/fix/1573-acp-concurrent-prompt-test d29df7ff38c9421a416d669967a03c13a8fe06db
origin/fix/1595-task-updated-started-at cd0334fe5c0a2521f61eb844bbcc6edcc8cf700d
origin/fix/1601-dispatch-env-gate-live-input d013abed7ca88903851d4c46cc84ee6f2c78f9ab
origin/fix/1655-retry-guard-dirty-check 827c33b77be057f368bfa288ef596681977fd272
origin/fix/1657-dispatcher-startup-check 327e7c2e9f90a698ca01dea63fdd63a13b0f4c79
origin/fix/1795-runtime-runs-gc 8fe71fc20ad7047c518e7aa309355a6da6dfb12e
origin/fix/1842-close-while-parked eaaf003308e1f332bafa427bab2aab53fa521a77
origin/fix/1865-eager-fleet-convergence f8a49b9d5761b861820ea3982a599e0bb618d43f
origin/fix/1901-planner-model-router-filter 8ea552711cd5628b4730f0d41930283c09a32dde
origin/fix/1912-services-compaction-framing 03aeae3bd77aa6fc432aa9ba054027a01ca73066
origin/fix/1923-nightly-tool-regression-canary 4d56e424cb9e366dd918e8c3380b814542fe215a
origin/fix/1929-release-bundle-sha256 b72a984b7ae222075ec7a6aa23c715ed1c0677b9
origin/fix/1930-reaper-weak-supervisor 814e8ae58f244b45171bdba499c04a7e8a57bf0e
origin/fix/1932-codergen-timeout-margin 717946607f002294e61aba6baf6ba9efeafe5b62
origin/fix/1935-verifier-lane f718390dce685a1986416e05c76af2d83e5db5b9
origin/fix/1945-ledger-read-path b5c93e322918d0bcd9e318a5488f2b113b9c082b
origin/fix/1965-peer-token-charge fc161588454f6a786ad4696f866aecee60063854
origin/fix/1967-escalation-lifecycle 97f2c0bf1685ef0aca4e3db1f988b1a71e732d7d
origin/fix/1969-interrupt-leg 06645984a3d56ce5e4873e8112122967b26a4955
origin/fix/1969-partial-turn-usage 3163f20c2d8ac0d836c52e59010697896c00c833
origin/fix/1973-goal-restart-durability f17267e3ea196ef6468cc6f38461e8bfe9be030d
origin/fix/1974-supervisor-store-scaling bc2ef92aa43c90087c109cf65e1946a89e1552b9
origin/fix/1975-user-goal-id-reuse 8ce072da9380ca854ffc80b377f5d85a833d13d7
origin/fix/1982-ledger-token-reconcile 6494639640e89d02891923ce575d3774051713de
origin/fix/2003-fleet-rearm-master-edge 54d9a006636d7341b99fc156249496eb7334bf90
origin/fix/2003-inflight-liveness-check 985160aac543dd12d74d3c8bcbb2affe38de8f4f
origin/fix/2003-inflight-marker-leak 85f271fed394029fa09c5865b47b773bc0a86cd1
origin/fix/2006-session-torn-tail-seal 32bf4f4d99037b6d34b8413b8a7395c2b7a4446f
origin/fix/2011-diff-edit-timeout-race 1ac2e4c46636e46d766f2d52beca95ceaed281e7
origin/fix/2011-master-continuation-flaky 5a51988835937d7b4fdafa929294e706819649f3
origin/fix/2013-active-sessions-quarantine 7eb4dc763077fd70f1f728701b151d5451534d15
origin/fix/2015-e2e-gate 3597d52f426c7a87a18563e71e19ab084f414418
origin/fix/2020-retire-recovery-hint 1b1c253c500aa98b41cde8d4849d7ad9fb807846
origin/fix/2029-agent-id-collision 81f3da343f79229fcdfcab05389d09c4a48fb572
origin/fix/2029-ci-filter-zero-match-guard 92a187e460b361eeb6175bfee930f91c47d12041
origin/fix/2029-stale-drain-guard 038c2148e74338f88007533f3471e02dc03e528b
origin/fix/2033-boot-synthesis-eval 20960343b5bae6833aae44f110194f0ebe7ce8e8
origin/fix/2036-inner-assertion-cap d6539925e63d3ce57c87e8c119511ae790a396b8
origin/fix/2036-peer-finding-evidence 61791b790d6cc5b7fd853bbc18d51ec8ea8d45f1
origin/fix/2053-windows-format-timeout 76a45068d98e89b26c3be7149c1673e6574461f2
origin/fix/2053-windows-timeout-waits d83db7b5e8ba1108e3905266f963f46b7896b6ec
origin/fix/2054-task-status-writer 9a956d1d4a2d0a9d4c28c2c159371ecbeaeecc17
origin/fix/2073-e2e-nightly-wiring c54f8a6fd091afa8fb96ecf32c4ffb53092dc9c5
origin/fix/2073-nightly-triage 025279e4c5781da0db76ebdbdd2ece9e6f0d6959
origin/fix/2076-get-goal-torn-read 9db518d53916fe455a6c66bdfefbe4b40fc4b7c3
origin/fix/2077-nested-spawn-row-race fc43a6835575394efcc4c6312a9c3dddb3cb392e
origin/fix/2078-dead-replay-profile-check 7f4e3bbde25b04943cb9fbfc23dfb4cc792e570a
origin/fix/2093-plugin-tool-test-flake eb1dbeaadfaf66e8084e4d7c35d541f65a1bb2c1
origin/fix/2102-gap1-replay-dedupe 71f7ce4cc45c33b737e8b343bb876b2df1e76fd3
origin/fix/2102-gap2-reclaim-window 2a381331efae3a7c78ca7d910ec178acc3cc6719
origin/fix/2102-gap3-join-epoch bb212b4f6e3a82a262ca754cff359913d7906cb8
origin/fix/2103-message-import 32bef79fa8d2d3308cc86090ac6b85be0e4bdc98
origin/fix/2162-session-open-identity 6e71e9497e498f238203e9b0a3afee0e8cdd928f
origin/fix/2164-profile-llm-runtime-invalidation 346dcbeacae2a06fece1b6406ed528dc72a280ce
origin/fix/2181-goal-offline-lock-hold 82200b2704264d871d1cf5e99489ffefd6e60f6b
origin/fix/2185-r9s-per-model-discovery 9daab368d237db5b89e6fa0d67540398030314d9
origin/fix/2186-skill-rebuild-generation-guard 93c191dd57761668a268694252c49e92321db290
origin/fix/2187-llm-upsert-rejection-gaps aae6bf56f122e75a00bcc7f6d24ff1bbdfcf8e72
origin/fix/2197-unarmed-blind-cut-cache 36d3c926634e9dc785532f97085c1cd92590dbed
origin/fix/2198-usage-cache-write-status ce2a5c42d432cce2de5b3587b7da04ed74b39cef
origin/fix/2217-tool-policy-reload ad94febba36513d6d0c497685e223d395de030e1
origin/fix/2221-retry-state-merge-guard 97eb1e480ad8a55b2920c0166049d8ca0b209e63
origin/fix/2233-responses-discovery-v2 12dab6a412a4b49506367c2311ea269c0b047830
origin/fix/2244-on-turn-end-oup b37a7e163965d8eae38bd68b672dbe7dd62cab20
origin/fix/2245-hook-argv-relative-paths 8a040efcf9cec768048bd944ed3c05fe34ecbb24
origin/fix/2246-hook-context-ids d3d5f2d24aef6faf4de16e333203aea52050a220
origin/fix/2249-hook-deny-policy-classification 31fdcbeeaa045f2c821a11b44f4b247b7b688fee
origin/fix/2250-skills-install-nested-docs f96b4d71cd03b08ad1abedd7e81bb9390876563f
origin/fix/2255-skill-mcp-args-resolve 163e9d6bcf737f36b024715c5421da19efe2fe4e
origin/fix/2259-ci-check-darwin a9d4711da088404f270c232539f0a4349c8c3de8
origin/fix/2316-sub-account-rollback-boundary aae6459f91916d36d548093f9dd975296fcb9764
origin/fix/2336-stdio-dispatch-count-race 4b6beae6e3498c62bff98850caf2f5b895925a8a
origin/fix/2338-cli-agent-env-scrub de04da0e2f97b5d2aa9398b1a923dee0b4973e72
origin/fix/2357-serve-lock-retry 3a303ed4d1e5be7d9ed822f1297b5a87078599e0
origin/fix/2359-toolless-budget-grace 853d94788770cfab7f93799cf0e8bf62e214524d
origin/fix/2364-bg-activity-recorder-crosstalk 1c94f12ea8679a232e9105854488631360aff9f4
origin/fix/2367-monitor-resume-expired c560cfcb7d0ccf3d44beb86e6ccd7603e64daed0
origin/fix/2371-auth-token-argv-warning 8aa1be36c1295815e126792f6b1c43aff2bda28c
origin/fix/2380-deploy-nssm-token-env 8cf5f1dfc215af316accfbcf74eb557681dea016
origin/fix/2384-doctor-empty-embeds bae0e7c8ab01c67c2eaeffd06f2ec189bf382572
origin/fix/2385-port-in-use-remediation 66a503d30bab3eb0819560a7279d798c746c7964
origin/fix/2390-recall-layout-hashset 230bd0204010320412954f0957b27fdedbd065ec
origin/fix/2391-usage-ledger-record-drop 70581acfd133a99ed7d2568ee21e80b7055bbd95
origin/fix/2392-sessions-listing-count-cache 5f82a2d9e4995dfb0da464ae08523ac01d0b6c28
origin/fix/2412-turn-admission-progression 76008cd2256a9c3b8a602083078bef96ef27c757
origin/fix/2413-completions-registry 1f85cadf40d71bd901328a8508d78d8bdc104eda
origin/fix/2414-cli-dead-end-errors 8db9998a74cffba6e18e5a53363e2117c2c23a77
origin/fix/2415-auth-help-platform-store bd2fdbfe70acf9bb613dc9460e84e467283409fd
origin/fix/2420-matrix-appservice-bind-wait 14356db4be973cb5f75ef496b86597881798b30f
origin/fix/2421-oup-minimal-own-leg e9e813137dfced0e114873a29dd3696cc950e120
origin/fix/2424-session-ingress-docs 549ee72a1304754be098b49d6dcfb57182d50bb5
origin/fix/2440-selfupdate-verification df6825a8572d70be43732be0fa5750d9625a3945
origin/fix/2444-list-work-secrets 75f4b2fb0ff563aeb19a6d93dc100a209a03085e
origin/fix/2450-protocol-error-code-contract 13cdfffc9654c639e7255848edae9efc85c2b513
origin/fix/2454-stop-server-docs e3a611945890a97b81db3cdc52fcdd816f8ac0df
origin/fix/2458-architecture-doc-dead-refs 19c0df256ce42a028887d11e7f4b54a177b2cdd7
origin/fix/2473-storage-contract-docs dc721a492a1ad24c8963b76c63c0278d04ff727c
origin/fix/2478-m9-envelope-terminal-frames 8fd019de3a7108c252c5cb6321b07ccd8289b7b8
origin/fix/2481-seal-guard-legacy-meta c4e0513e92f2bbc17d4c1700a16adcaf295f4be2
origin/fix/2483-fixture-lane-web-client-specs 4bcb6495a92d50051dfa0c30f301cfbad725c88c
origin/fix/2484-envelope-external-consumers c7761060baebde1af24b30821a59b6f22091abe9
origin/fix/2486-m18-solo-envelope 20435927b34af7f167e144bc5d259084a222d69e
origin/fix/2486-solo-gated-soaks deba863bff950cba0e84daa149e2fee06ecb0553
origin/fix/2486-ws-soaks-solo-envelope b3f388525991c88f3f07e3837657c17b74aa518b
origin/fix/2491-spec-10-error-code-parity 871a8fd34f84e261685772de3324631537a7aa87
origin/fix/2514-installer-checksum 9dfa5fca3548115ea1894f0a39cb4a545e1f3bde
origin/fix/2515-windows-append-sync-data 1ba28f3431c0487d5c5ec15cb5eb050c5b7bea40
origin/fix/2540-advertise-wire-notifications a26885b14c2d69bec28c27f83adb3fb6ce0e4d44
origin/fix/2548-matrix-pr-lane 0293c84b3638c584935c5f7a4e5720e601e5ab2a
origin/fix/2559-codesign-provenance 094c395ec3452ebd2ed029c228f491348e5b0f75
origin/fix/2561-embed-download-stall cac29dc2ed7bda112aaf26a2c01d3d8424810d80
origin/fix/2596-net-url-query-order 3f5bafebb62a89bb6e7cf5ba5605abe0528ee5f7
origin/fix/2629-peer-host-tool-digest-canonical 39309d3d54d66919752835ae1e908b43e1413504
origin/fix/2632-new-topic-self-heal-docs c6c8f72a022cdb84cbe688ca42c1efb539c63e71
origin/fix/2638-interrupt-cancel-frame-determinism 118107aff07bd3b8695a3829cda75849331894de
origin/fix/2659-purge-incomplete-erase 92a9f2bd8246b1c719b38a9723ebaa239a29bd16
origin/fix/2674-peer-primary-model-display 6001ba51af4cb7f4f2197ed290d38e8cdfe59a78
origin/fix/2688-sha-pin-ci-actions 5583c0850b77577e0c3b759d73112d6dcbfb35b8
origin/fix/2707-worktree-gate-posix-sh f16c2e2d077dcc3f05214f634cb1ebb2eeabf3be
origin/fix/2712-purge-debris-refusal 003c3961ab2cf70591f2dd46c5eb196249dfbbed
origin/fix/2718-capacity-warn-capture d5733695a99f0d319e337198629b0ae8bdb9ab53
origin/fix/2718-http-probe-deadline-budget e5e5df85ba9c09caa7b2a2c62d20c243cf7bd5b9
origin/fix/2719-windows-config-context-xdg-tests d3d49fce9592d65468671f9e087156ae4b4936ac
origin/fix/2765-security-md 3e4b77f9140c454e8596c342f2aa6928030ff361
origin/fix/2772-feishu-empty-encrypt-key 2caac30c77b99d10c3a407894b4597fb4e5597b8
origin/fix/2773-feishu-timestamp-freshness 229ff358f6a31f8835fe5c8ab574f4e2c7aa1d33
origin/fix/2776-upcr-registry-row-lint 3038ddf2d6fe1fb4426e8b50f6425241778fcde9
origin/fix/2781-critical-event-byte-cap 518ba976c8448b021ed9e0132b91c4133ec111ea
origin/fix/2784-double-launch-flake 561f4b2951cd8f5f0b6532a6c2198acfaf789a60
origin/fix/acp-1909 3360b75257a7f9af050d594ff281acfb157ec789
origin/fix/agent-runtime-p1s 510e9d48331f6c8a450f02ccf4c1550d99ab3a6b
origin/fix/anthropic-sampling-params d42470e4400afdaba8d51ce3bc65fbacbe4fbc73
origin/fix/auth-store-safety-2239 f39fe4ba7071c8591fd9d1412b1d0e0f51d6f781
origin/fix/background-result-ack-timeout-889 59c3fcb97bb80e9a5d207a74ae899ad434504150
origin/fix/bing-news-zh-market c5012dd1e0211f4484b43d7d9aadb9ba667617f2
origin/fix/bound-workspace-symlink-swap 085ba2526cb94967881028fa886966805f3791b0
origin/fix/build-system-prompt-clippy 8f8fae5cc0feb386a0bc166ebef3cb692a1f854a
origin/fix/chat-provider-clean-switch 63efb9ac9d478ab0455deb53dfee316aa062225e
origin/fix/check-arm-oom ef50ff1bd79482bfcb9892364ec3ff3703ea3080
origin/fix/check-windows-appui-continuation-budget 0645db5a1e147b03090b970f36f67d327cdcfcd8
origin/fix/check-windows-blocking 9c6e8319614e5316902cb17ae10f5e843b03b5de
origin/fix/check-windows-workspace-browse-tests 34c1c08abbf14af3cd39e65f4c2579308cc877f1
origin/fix/ci-and-peer-worktree 261b3b0fa226782cd412dc80de0cbac2a4eccbd7
origin/fix/ci-gates-that-run e1b1cac5f26e857bd4a1ad74571805209936bb82
origin/fix/ci-riscv-manual-only 29bff44a889de5c68e6615c1b0a474177fdb4915
origin/fix/ci-skip-serve-broken-pipe 623533099ba0cb3855be2e5feb8d28655e98119f
origin/fix/compaction-llm-max-output 8412defce1b1542dcd2e96979bcd808cee74b8da
origin/fix/cron-shortid-collision fbed4c6b421fac2f21dbe273c733f1d0852bfbd5
origin/fix/cve-citation-correction 5561e720d6eed59bed12768cb64391ffd94928a1
origin/fix/deep-research-worker-deliverable 426d6f696b93ddb6ffc7d88a92128204a4cbdb0b
origin/fix/deep-search-stop-deep-crawl 0458aae54b75fac445f09eca7fc7ea8bd091ce70
origin/fix/dep-triple-bump 5054462e72e4140b76dee919e85ce500911f5f0d
origin/fix/doctor-clippy-write-literal 20a0a71e7c54718feb8824c1f16219121b4503f5
origin/fix/drop-dead-ux-detector-branches 113fdde9af3ffdcb88ec3087802ce2c171adb3a9
origin/fix/e2e-playwright-install c9ee54c714e6e3cc1608755416d7c9e25557183f
origin/fix/email-thread-topic-isolation-1142 9f4acaa4d1e6c85575056ad02947cc25ebc191b4
origin/fix/embedding-vector-coverage 8edc070fe5dcb57d117284f57a4a59f5c261f2d3
origin/fix/episode-store-locked-error-message 9206c1454103e318c96465dbc9ff11fc16572faf
origin/fix/esc-orphans-child-process-group 98d48b0a8826d0dbf18851aabc4c7b68fc064565
origin/fix/evo-goal-verifier 8bebab9fd28dc5e65c860f50753c0b5137ad844f
origin/fix/evo-peer-turn-status 9c9e6f140d6350dc3beb71794d727a9833169a8f
origin/fix/exclude-restored-agents-from-list f30f6cb2f259bbbbb6454d64c3355ccdf72081af
origin/fix/exclude-sandbox-from-release f82cd8b2d3528f012cd4cfc7d3aa36a5ad47026e
origin/fix/git-snapshot-hardening e1722ccbc84ae82bbd76b8cae8305480d0edde1b
origin/fix/goal-budget-rearm e6b43a58fee5ad650ea98b8e9f26da8769e2f9c5
origin/fix/goal-hardening 7c2acf6424232b66543c04e057e23605911c6586
origin/fix/goal-ledger-escalation-resolve b42290452b521dd078b5da2af978f4dbaedaebd7
origin/fix/goal-plan-error-message 480a82687d7328befdc9960281c7413322c20b62
origin/fix/goal-scoped-charge fbaf52c56b2c26bff0b1b0df7ae3adf3fbebeef6
origin/fix/goal-store-cwd-scope 7dcf48fd2e10324b29cd8e1656378b68e72da643
origin/fix/homebrew-push-new-formula fc51ffbda928f320a407c1b1c63ab6cf4156a9b4
origin/fix/host-managed-followups c8ac8a6dcb6756f6c6543446c8a6ceb7f94755f2
origin/fix/host-peer-approvals-and-reused-call-ids 14139610f8354b33e6fc93575eed1b958b2499e4
origin/fix/host-peer-low-findings e9f67f488ab7d8843896600ca59afef8a4e3a98c
origin/fix/host-tool-claims-per-route ea862276af78c1ec6917796277c491db03507418
origin/fix/hydration-compaction 03e072315b3fc010fce9cc38d56dc6d045f300a3
origin/fix/inloop-context-journal f9c0df03feb653d44a604fc98d8041ad9184347d
origin/fix/install-path-test-no-tunnel 336736a378013f85711b5ef5eb206f5acd9301a4
origin/fix/install-ps1-ascii 5d94f7f55e0902a971f8e03ff493a1ce3a1b65b8
origin/fix/inventory-notification-names-soak-sentinel 8ab259acfb722e7ee17c08b003b91b84c770e2f9
origin/fix/issue-1041-mofa-cards-spawn-files 07b20baf69b4c3f7b9ccd5ae90ddfa4dd8a48540
origin/fix/issue-227-pipeline-llm-throttle 50c6e81e321afd5ff5509b05cd854c4b5dc4e580
origin/fix/issue-257-feishu-threading-tests 4f0b9e1d3a30a97b929b306a8d99c69697291220
origin/fix/issue-261-output-naming cd95f04cdacf1cb4c552d81f57cfd031e207c651
origin/fix/issue-293-approval-timeout 7b7c1d69439ab0253103ed8fedb1bf5424e63b28
origin/fix/issue-323-scanned-pdf-fallback 35318a5ee5b4dc30bed1cfa55d422b473f7c3923
origin/fix/issue-615-pipeline-cost-aggregate 4049b8d1deaa219b5412a223ceb0d784bde7dc84
origin/fix/issue-889-bg-persistence-warning 03ee8e2b997323f524f14ac392e26ce0f7c0a266
origin/fix/issue-964-pipeline-node-timeout 719a88c0282dd864daa652afa9111374239ffd65
origin/fix/k3-context-window-200k 9d33021873d5beb189d7678b66e7aa320a67bbf3
origin/fix/k3-glm-thinking-effort 69dbd982d5bbe24836ff617addfbdb62d24b5547
origin/fix/llm-reasoning-backfill 25b3bc28645110594fe8f92454023ddb2c50a7c8
origin/fix/loop-first-fire e68b855b0a7d86b19f1e00566b9a0e49616fd15a
origin/fix/main-check-windows-test-break 30966b64343fbc72250dad8239e2fbcf0dbf9ead
origin/fix/main-peer-host-deadlock f44afa6066ffb8b2ac7e7cee1c619ee0a9f6aceb
origin/fix/main-purge-test-concat 38fa80493d385d042aa60e009239eed8e24c4108
origin/fix/main-red-fmt-clippy-providers 3cc0c699b0af56079f8fa5e658cabb9d7512623e
origin/fix/main-windows-peer-workspace-assert 02daf9967c12792b9cc47c5fb67c1e926d144843
origin/fix/manual-compaction-session-model 2519871bafed58095b4263d34aeadb093ba7e876
origin/fix/mask-email-secrets c8cf15a5dec3a9505ec398bd355ac8a7c68de2bd
origin/fix/mcp-transport-lifetime 91fa3c1c304c57629ea4f50e5f5bde858021d445
origin/fix/merged-verifier-review-20260910 dd79aa0ac23cd3329924253865410b69fe385bc2
origin/fix/mofa-site-publish-rust-skills 8bdb702c8760b8c90558a046330b0c36240ac0cb
origin/fix/msg_not_dispatched 914e716e21533530af7cb9507bb1b4de68d4404a
origin/fix/nightly-windows-pool-posix-sh-gate 190936ec790d6d504f247b68cb0641e9e54ccd24
origin/fix/octos-agent-lib-ci-hang 73e548c24de7dfbe913b4f41ad7c4b62bbce57e9
origin/fix/open-compaction-events 59be2292dcf347d745909abe738ecc89a070ba7b
origin/fix/openrouter-tool-call-type 64dafe0bb429ef3743389e2c40e14ee5b4b80814
origin/fix/oup-single-projection-lane 36e55805a904a60a84d8ebe34f69a0b465890abc
origin/fix/oversized-hydrate ac264483720827e751c34d5bfe9928cf00c7b4d7
origin/fix/peer-clone-not-worktree 689dce6a2c46cac6637cca959117b0a796bf813a
origin/fix/peer-close-host-owned d72667e4802759ecc394f2442fe2dd6c7c5c0d8f
origin/fix/peer-close-interrupt-attribution 216e87c07a1da8b995d640bc9c975d871b62662c
origin/fix/peer-collect-every-turn 724308ccf751f3728aef671687d3222a16f42ade
origin/fix/peer-handoff-keep-working 304460707063a437991026cad820bc701080dc73
origin/fix/peer-input-claim-after-admission 08ac73dcef5505e43ee1b05998ce60768c688fc6
origin/fix/peer-key-silent-noop 3a9879d3ba03d797ec1e5d99cca23efdac54b94a
origin/fix/peer-result-turn-identity-20260913 a18c00c34e6523ff085104b694f680320aa8bac0
origin/fix/persist-unknown-host-tool-outcomes f02db1c38f73ad353e9ce3475b44d863f43a93f6
origin/fix/pipeline-max-iterations 34f41268006ffc942d3439ab07bec0578ed19ef1
origin/fix/pre-release-codex-p1 12f7ccf4b13c6cc72524a78835389b5d9627d026
origin/fix/prerelease-package-publisher b727204e437cdc9993acd1d0a91d236d2430fabd
origin/fix/preservation-fail-closed-706 b24be0658ecbe951b475c81f0ddd62faedf1fae6
origin/fix/profile-disabled-error-message 4fc13cfcda92778eea216e70f50fff7dcfeddcfa
origin/fix/profile-inheritance-hardening f130346f620c8061c38714aabb35977cf3e3ed11
origin/fix/profile-llm-delete-implementation a93964fd538670dad5e70ad3c01b5f7c5b4f89a4
origin/fix/profile-monitor-settings-426 9b3d5389613e9fa5d8f233d1d731fff17993d25f
origin/fix/purge-partial-erase 91fa177b5de6153ad8cda0722849a96591a168c3
origin/fix/release-drop-phantom-pipeline-guard f2af22f10a4d2676bf4772a84950c188d8dca70b
origin/fix/release-embed-swarm-app c4e93152a825b45ecb97a6c7e5b194b84a3287de
origin/fix/research-read-reasons a4b5d24146d5f8058744e147a516bec8ee07b655
origin/fix/research-review-pipeline 9c026e84b0e6358891789bb6ea84e7a6abc5c9e1
origin/fix/research-review-server de03863129278ff4e4dd6bbd88a8425cf06618c6
origin/fix/research-stub-feeds-gdelt 6ec6ff5460db44aec6b3b894fbea0ee31f4eed2d
origin/fix/reviewer-role-can-write 95ccb07e7f511083ee9c126d8803be006dcb469f
origin/fix/roster-cwd-scope a534ed062ca4cea3953bc6687cb6e34349ec7fdf
origin/fix/run-pipeline-dot-delivery 392db1e9fdc0c7777a124030a842f8aa7d601ba2
origin/fix/rustls-bump-and-hygiene 94f606004cc262729dccf5c06ceffd4d992651eb
origin/fix/scoped-worktree-prune bac9e9af3ec6b21eba379384ceb1ccadd8f6acb1
origin/fix/segment-recovery-unreadable-meta f1d2bbb6920f0cc9a8e88c421f02a83f54b7a7e1
origin/fix/send-file-delivery 62e4d50d7cb8ab0a745cfe70436e9b9554776ece
origin/fix/session-actor-type-complexity 86d8d6d145b01c0cca9972bd68b46b7b9e6d56d1
origin/fix/session-ingress-capability-filter 2d49fd8260d4dff3362cadf7229a44b6a1b11205
origin/fix/session-ingress-scope-before-raw 5e8fc02f2784c09d98d734814bd9cb7392255f4e
origin/fix/session-open-client-commands-init c8ec09aa851fd0ed8a6e37ef68c4864bf1d2ad8b
origin/fix/smart-home-conversation-path 36771f65fcd8178f357f56e93aed7289ce88e9bf
origin/fix/solo-boot-loop-pause bece0e502b748fa712ee49c2835375492e370779
origin/fix/solo-gateway-profile-lock 1514c61f6d449a01332339c758f207599505f6c3
origin/fix/spawn-allowed-tools-schema-hint 7d2302cb224c9d7131c0b51c2cab6c5c0be58062
origin/fix/stdio-no-envelope-default 793a59077a3389294d251fdeb2410c20d4afb34e
origin/fix/stdio-shutdown-turn-drain 4d8d849868cd3cbcfbcb9059fbbbb15e2106d014
origin/fix/stdio-task-cancel d5038e97d5aab7a01ba038857b88f586ab33392f
origin/fix/stdio-writer-failure-injection-order eb7eb86fce5375f501419893296bf970b6c2e63f
origin/fix/swarm-codex-followups cfc6e98bf5f085de0c4f63201b1d06f4e4de004e
origin/fix/swarm-p1s a4df4abfd055604df7bee9bd82bedbb6da78d60e
origin/fix/terminal-after-forwarded-envelopes 7bfc642dae31a3e793ff54dea454bc7b8c06748e
origin/fix/truncated-toolcall-recovery e3601dd75b91a2bf574f4d08553b7df0085a5c55
origin/fix/turn-state-lost-turns f12eb636ff3e2c27ac86ba6fea83e892b72c0370
origin/fix/twilio-wecom-constant-time 3d6184e321d73903410c50492ccb2dcd0579028c
origin/fix/typos-avances-spanish 6270e35e89128ef3cfdabb5af8a5beb1bd6f2df8
origin/fix/typos-wronly-false-positive 1cb814994584e34ff2227574479ff69aa83f542c
origin/fix/unbreak-main-ci 6787e78f90dd1f2f81929f32351e08f14a9693c1
origin/fix/upper-sidecar-digest-only 532978465028c9fcd86cfa157ab15d17a54c9a1d
origin/fix/validator-registry-first 8944e1818ca6ff318399363eb3c9cf45cbda02c9
origin/fix/vision-image-modality-fallback 52fa6a17ab2c77ec7c4802f8df650e9ef5f75a9d
origin/fix/web-search-general-web-tier 428eb9f93935ceb13b76fecc076fbb3a46912dfa
origin/fix/windows-rustfmt-timeout-flake cd1faad3d7a0e045a380071a5cf36659fd71bbdc
origin/fix/windows-server-deploy 561c8d9cccce1002f82ed749acf1ded20131c016
origin/fix/windows-unix-only-test-gating 427240e1a0d4efada5ac787fb3e66d9f1dde9fc7
origin/fix/worktree-isolation-preflight f8309c4f42afdf17d028977bd435de420a747da0
origin/fix/zai-openai-route-compaction-markers a24ede19fbcc9e2dd66f127c3b45ee634d3936e3
origin/fix2065-rebase cff7fa9b244733413438bf9f4164e7c851c8546b
origin/homebrew/octos-v2.0.0 3d59b694dc0ec36049a6751d5b2f59bb35bcb0c2
origin/perf/bm25-topk-dense-accumulator 88675981d8e55f705fe38ae7aed5522efcfb3ebf
origin/refactor/extract-config-context aadd2cb65fb882d277fd575b3cd97a24c0fb9d74
origin/refactor/extract-octos-workflows 674502ad613b61d87ab532dcfe820ef2b7403e0a
origin/refactor/extract-session-actor-tests 06d5502af2ccd5155ce5ac8011f32e43e99431c3
origin/refactor/extract-test-modules-batch1 4d8e45b444bc84b36034316462d2ae0c713bcd66
origin/refactor/extract-test-modules-batch2 e36ed7f91e80964e6a159591d2cac8577b8700af
origin/refactor/octos-server-stage1-scaffold da49898516d83f3d29f06117897010d4f4273f66
origin/refactor/phase0-autonomy-extraction bd155c8fc834b8a7fea25b00b5ecf4c83cb07abc
origin/refactor/r1-dead-code-cli-lib dd81ddea7229352678202869595a551195023442
origin/refactor/r1-orchestrator-dead-code 657117800465956972bf0124c62d22ccc1ada4ab
origin/refactor/r4-orchestrator-contract a1bf7a51187924e191cdcbead9f45d7490c295aa
origin/refactor/R6-llm-test-blind-spots f0f8243fd813b97f7b5c06e15142654cd994d798
origin/refactor/r6-plugin-types-wire-pins 33b98fc4da2b3490fdde78c41b2a41bf2dc351c7
origin/refactor/R6-sandbox-host-managed-tests abc637269f73a712620789f0866d9189e120d351
origin/refactor/R6-workflows-test-pins 7d1759f9b3e98b4f3ccc6ef175f728bdf2be3fd9
origin/refactor/rename-ui-protocol-transport 41ff317722cde69f60880eef564c3d308ef5a90a
origin/refactor/ui-protocol-extract-tests 812946043835817a1be7483891b44bfc9fc36922
origin/revert-1452-feat/atlascloud-provider a941d55f1fa854b0a0f9ec86ed47ad4c54791493
origin/serve-default-entry-app d852b6edb4cbc11e3c1c257990d273697fd601e9
origin/test/admin-setup-wizard-e2e-512 8a6a1ccedba12a29ed8d914d536d81c8a861bc13
origin/test/serp-challenge-fixtures 70effe2f29bb801c23666045f1369da254a97f7a
```
