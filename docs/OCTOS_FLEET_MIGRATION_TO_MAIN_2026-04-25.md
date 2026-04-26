# Octos Fleet Migration to release/coding-purple — 2026-04-25

This document captures the two-phase migration that consolidated 11
release-branch-only production fixes onto `main`, cut a fresh
`release/coding-purple`, and rolled it across the production fleet.

## Phase A — Backport 9 LOW+MEDIUM commits (PR #587)

PR <https://github.com/octos-org/octos/pull/587> merged at
`05c114a59f7a51d9275328c944aefd8b0d53c78a`.

| release-branch SHA | main SHA after backport | Outcome |
|---|---|---|
| b166ba83 | a8b4dec1 | clean cherry-pick — voice skill port probing |
| b24d04dd | 368e94f0 | clean — fail-loud Qwen3-TTS rejection |
| ff538f7f | 1b2f1e97 | clean — SMTP/Feishu password UI |
| 521e7fd6 | e874e9e0 | clean — Gmail app-password docs link |
| ac3848ed | 1963541d | conflict-resolved — task_supervisor artifact size validation; surgical execution.rs merge |
| 7f138f0a | d8ac3860 | clean (after ac3848ed) — audio content validation |
| c35d458d | 9a24a4d8 | conflict-resolved — overflow history snapshot refresh; merged with PR #586 cmid plumbing |
| bf3378ea | 5b1069df | conflict-resolved — per-mini base_domain config |
| a6d0575f | 04af74c9 | conflict-resolved — deploy/SMTP/daemon logging adapted to main's structure (94acc954 helpers absent) |
| 449ad4dc | 3078c55e | partial — "Earlier task completed" prefix drop production change landed; FA-11 + FA-12d tests dropped (rely on release-only test infra) |

Three follow-up fixup commits also rode the PR:
- `bb7b6254` — added `base_domain: None` to 4 inline test AppState constructors and dropped the FA-12d test (depends on `StreamingMockProvider` / `FakeSseChannel` / `setup_speculative_actor_with_indicator` / `make_inbound_api` helpers that exist only on release/coding-blue).
- `a66092f1` — dropped FA-11 test that asserts release-branch-only `_session_result` metadata emission on the overflow path.
- `df5a756e` — `cargo fmt` cleanup on `router.rs`.

## Phase B — Backport M8.9 (PR #588)

PR <https://github.com/octos-org/octos/pull/588> merged at
`82ab50be13cfe95f70949614408d17b164dca182`.

Commit `4f8542b8` (M8.9 Runtime failure recovery) cherry-picked onto
the post-Phase-A main. Conflicts:
- `crates/octos-agent/src/agent/execution.rs`: main had restructured the spawn_only path into a `spawn_tool_task` method (post-M8.8). The cherry-pick's inline-closure form was removed and the M8.9 change (`register_task` → `register_task_with_input`) was applied surgically to the canonical site.
- `crates/octos-cli/src/session_actor.rs`: the cherry-pick brought in F-015 fields (`persistent_retry_state`, `retry_state_path`) that are NOT M8.9. Those fields were stripped from each of 7 SessionActor struct sites; only `recovered_tasks: Arc<StdMutex<HashSet>>` (the actual M8.9 field) is added. The `setup_speculative_actor_with_indicator` and `make_inbound_api` test helpers from the cherry-pick were dropped (they support FA-12d, already excluded from PR #587).

`cargo test -p octos-agent --lib` jumped from 1133 to 1146 passes (+13 M8.9 tests). `cargo test -p octos-cli --features api --lib` jumped from 573 to 580 (+7 M8.9 tests).

## Phase C — Cut release/coding-purple

`origin/main:refs/heads/release/coding-purple` pushed to
`82ab50be13cfe95f70949614408d17b164dca182`.

## Phase D — Build + validate

- Dashboard rebuilt via `scripts/build-dashboard.sh` (476 KB JS, 31 KB CSS).
- `cargo build --release -p octos-cli --features "telegram,whatsapp,feishu,twilio,wecom,api"` clean (1m 52s).
- `cargo test -p octos-bus --features api`: 0 failures.
- `cargo test -p octos-cli --features api --lib`: 580 passed; 3 pre-existing main failures unchanged (`api::admin_setup::tests::post_setup_step_accepts_boundary_values`, `api::router::tests::events_harness_route_with_bearer_auth_returns_200_sse`, `api::router::tests::unmatched_api_path_returns_json_404_not_redirect`).
- `cargo fmt --all -- --check`: clean.
- `cargo clippy -p octos-bus --features api -- -D warnings`: clean.

## Phase E — Fleet deploy

| Mini | Old SHA | New SHA | /api/version | /admin/ |
|---|---|---|---|---|
| mini4 (69.194.3.66) | n/a (pre-existing) | `82ab50be` | 200 | 200 |
| mini3 (69.194.3.203) | `b8e63761` | `82ab50be` | 200 | 200 |
| mini1 (69.194.3.128) | `b8e63761` | `82ab50be` | 200 | 200 |
| mini2 (69.194.3.129) | `b8e63761` | `82ab50be` | 200 | 200 |
| mini5 (69.194.3.19) | SKIPPED | SKIPPED | — | — |

Each binary backed up as `~/.octos/bin/octos.bak-pre-purple-migration` before swap. Daemons reloaded:
- mini1, mini2, mini4: root `/Library/LaunchDaemons/io.octos.serve.plist`.
- mini3: USER agent `~/Library/LaunchAgents/io.ominix.octos-serve.plist` (the pre-existing crash-loop on root daemon at port 8080 was left alone).

mini5 reserved per directive for coding-green canary work.

`macmini-31.octos.bot` (66.201.40.31) explicitly excluded — separate dev/test box, not in production fleet.

## Phase F — e2e validation on mini2

`tests/runtime-regression.spec.ts -g "Background task lifecycle"` ran 3/3 passing in 41.2 s against `https://dspfac.bot.ominix.io`. The middle test exercises the canonical spawn_only + audio file delivery path (closes #388 / #366) which IS the storage-unification probe — the session journal contract that PR #586 unified surfaces the audio file replay during the polling loop, so the test passing implies the contract holds end-to-end.

## Future policy

1. **Fixes go to main first.** Release branches are branch-cuts of main, not cherry-pick targets. The 11-commit drift that motivated this migration must not recur.
2. **Test infrastructure changes follow the same rule.** When a regression test depends on release-branch-only helpers (e.g., `StreamingMockProvider`, `FakeSseChannel`), the helpers go to main first.
3. **`release/coding-blue` and `release/coding-yellow` stay as rollback targets** for at least one week before deprecation. Do NOT delete those branches. mini5 stays untouched as a coding-green reservation per user directive.

## Rollback procedure

Each mini has its pre-purple binary at `~/.octos/bin/octos.bak-pre-purple-migration`. To roll back a single mini:

```bash
ssh cloud@<mini-ip>
sudo launchctl unload <plist>
sudo cp ~/.octos/bin/octos.bak-pre-purple-migration ~/.octos/bin/octos
sudo launchctl load <plist>
curl -sS http://127.0.0.1:50080/api/version  # should report b8e63761 again
```

For mini3 (USER agent), drop `sudo` from the launchctl calls.

## Surprises encountered

- The 449ad4dc commit was effectively a no-op on the first attempt (conflict marker straddled an empty HEAD region) but the production change DID need to land — the diff carries 3 tests and a rewrite of the `overflow_served` handling. The retry after the dependent commits resolved cleanly.
- The a6d0575f commit's `init_tracing` rewrite assumed helpers (`should_enable_console_logs`, `is_interactive_terminal`, `has_rolling_file_logs`) added in 94acc954 (a release-only commit). The rewrite was simplified to use direct `log_dir.is_none() || std::io::stderr().is_terminal()` instead.
- `git fetch` did not pick up `release/coding-purple` automatically because the remote's default refspec was `+refs/heads/main:refs/remotes/origin/main`. An explicit `git fetch origin 'refs/heads/release/coding-purple:refs/remotes/origin/release/coding-purple'` was required.
