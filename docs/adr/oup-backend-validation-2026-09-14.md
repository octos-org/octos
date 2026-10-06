# OUP/backend integration validation — 2026-09-14

## Scope

The shared OUP runtime, incomplete-result handling and C/UniFFI contracts
already landed in [PR #2265](https://github.com/octos-org/octos/pull/2265).
This follow-up is based on `f1fc20c1e` and preserves subsequent upstream work.
It ports the remaining fixes and repeatable validation from local commit
`3cf3ebda1`; that older checkout's test results are not evidence for this tree.

Peer staging and gathering previously required a runtime in the startup map.
Solo stdio profiles are persisted first and bootstrapped into a separate
dynamic map, so those operations failed even when session turns worked.
Resource-only operations now resolve the active runtime's data directory or
the persisted profile without starting a model. Peer result persistence,
parent continuations and snapshot lookup use the same runtime resolver as
session execution.

## Repeatable checks

```sh
cargo test --locked -p octos-cli --lib peer_resources_follow_cold_and_dynamic_profile_runtime
./scripts/milestone-ci.sh oup-runtime
./scripts/milestone-ci.sh oup-minimal
```

`oup-runtime` builds the CLI and native libraries. Its localhost provider
fixture drives actual chat, ACP and OUP subprocesses through successful and
truncated turns, tool execution, cancellation/reuse, compaction, cold replay,
peer result persistence and parent synthesis. It also checks generated Python
binding parity, C declarations, and actual C/Python runtime calls.

`oup-minimal` runs the CLI's tests and strict Clippy checks without default
features, then verifies that chat and ACP explain their OUP feature requirement.

Process evidence is retained under `target/oup-functional/` (or
`OUP_TEST_OUTPUT_DIR`): wire frames, RPC timings, stderr, fixture provider
requests and result summaries with binary/script hashes. Fixtures use isolated
profiles, workspaces and fake localhost credentials. CI uploads the runtime
evidence even when a test fails.

## Integration results

On macOS with Rust 1.97.1:

- The new regression failed on the integration base with “profile lazy-peer
  has no bootstrapped runtime”, then passed after the runtime lookup fix.
- All 11 real-process tests passed, including peer restart/reuse and one parent
  synthesis, exact incomplete-result identity/usage, and both compaction modes.
- Generated Python parity, C header compilation, and all four actual C/Python
  success/incomplete calls passed.
- Formatting, Python/shell syntax, workflow YAML, tool-description lint and
  the UPCR coverage guard passed.

Local process evidence is in
`target/oup-functional/20260915T022813.098303Z/`; validation logs are in
`target/oup-integration-validation/` in the integration worktree. The fixture
uses a short private socket directory so the newer goal-control endpoint fits
Unix socket path limits even under a deeply nested checkout.

The pull request records subsequent workspace, minimal-feature and GitHub CI
results. Those broader checks were still running at this validation snapshot.

These deterministic fixtures do not establish live DeepSeek, GLM or Kimi
workload acceptance. No new live-provider workload was supplied for this
follow-up.
