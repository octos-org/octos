# OUP/backend completion and validation — 2026-09-14

Status: local code, CI suite and functional validation complete.

This record covers the current checkout's incomplete-response handling,
shared chat/ACP/OUP runtime, semantic context/cache behavior, and native
bindings. It supplements the historical acceptance entries in
[the context/cache design record](oup-semantic-boundary-context-cache.md).
The September 5 `/tmp` evidence is not used as proof of the current checkout.

## Runtime correction found by functional validation

A real `serve --stdio --solo` process accepted profile creation, LLM selection
and `session/open`, then rejected `peer/prepare` with “no bootstrapped runtime.”
The peer handlers consulted the startup profile map, while session bootstrap
had installed the runtime in the dynamic profile map.

Resource-only peer preparation/gathering now resolve a persisted profile's
data root without bootstrapping a model. After bootstrap, peer result writes,
parent wake-ups, fleet synthesis, consumed-result checks and snapshot access
resolve the same active profile runtime as the session/turn handlers. The
existing skill-job data-root fallback uses this shared resolver too.

The Rust regression first failed with the same error and then passed. Actual
OUP subprocess tests additionally verify peer result persistence, cold restart,
reuse without replaying the brief, and a dynamically created peer waking its
parent to gather and synthesize its result.

## Repeatable CI entry points

- `./scripts/milestone-ci.sh oup-runtime` builds the CLI and both native
  libraries, exercises real chat/ACP/OUP processes, checks generated Python
  binding parity, compiles the C header contract and calls both native ABIs.
- `./scripts/milestone-ci.sh oup-minimal` tests, strictly lints and builds the
  CLI without default features, then checks the actual chat/ACP unsupported
  runtime contracts.
- `.github/workflows/ci.yml` runs both suites in the CLI job and uploads
  `target/oup-functional/` even when a step fails.

The runtime fixture uses only isolated data/workspace directories and a
localhost HTTP provider. It covers JSON/text incomplete output, actual tool
execution, exact partial identity and usage, generic bootstrap failures, ACP
multi-turn history, OUP cold replay, manual and automatic compaction, cache
epoch continuity and prompt-free diagnostics, interruption/reuse, and peers.
Recorded evidence includes RPC traffic, stderr, provider requests, result
summaries, per-RPC timings and binary/test-source SHA-256 hashes. Each run rejects a changed
binary or test source.

The first packaged run hit three cold-start readiness deadlines. It loaded
the full workspace's ten debug skill binaries; loader timestamps show their
verification during the wait, and the compaction/replay traces contain a late
`server_hello`. The harness now gives startup and lazy profile bootstrap
120 seconds while keeping ordinary RPC/turn waits at 45 seconds and shutdown
at 15 seconds. Recovery assertions are unchanged. The failed run remains at
`target/oup-functional/20260914T234504.302955Z/`; only the harness changed after
the completed Rust matrix, recorded in `harness-input-changes.json`.

## Current validation

Final check results and source manifests are retained under
`target/oup-validation/20260914-final/`. The earlier baseline and the peer
regression's failure/success logs are under
`target/oup-validation/20260914T225312Z/`.

| Check | Result |
| --- | --- |
| Workspace tests, all targets, Rust 1.97.1 | 9,189 passed; 96 ignored; 169 suites |
| Workspace doctests, Rust 1.97.1 | 3 passed; 7 ignored; 26 suites |
| Workspace/all-targets Clippy, Rust 1.98.0, `-D warnings` | Passed |
| Expanded CLI feature Clippy, Rust 1.98.0, `-D warnings` | Passed |
| `oup-minimal` CI suite | 1,625 Rust tests passed; 6 ignored; strict lint and both actual frontend checks passed |
| `oup-runtime` CI suite | 11 functional tests passed; generated Python parity, C declarations and 4 actual native ABI calls passed |
| Format and whitespace checks | Passed |
| Tool-description lint and its self-tests | Passed |
| Protocol UPCR guard against `HEAD` | Passed |
| Workflow YAML and suite/artifact wiring | Passed |

Expanded lint features are
`api,telegram,discord,dingtalk,slack,whatsapp,feishu,email,matrix,audio_mp3`,
matching the existing CI feature job. The final source manifests cover tracked
and newly added crate files; the test manifest also includes the protocol
specification, workflow and new CI scripts. Validation checks these inputs
again at completion.

The completed evidence index is
`target/oup-validation/20260914-final/summary.json`. It references the earlier
Rust/minimal results and the successful final runtime rerun in
`completion-results.json`; the failed startup-deadline run remains in
`test-results.json` and `oup-runtime.log` for diagnosis. No Rust input changed
between the successful matrix and final runtime validation.

Final functional traces are in
`target/oup-functional/20260914T235324.739104Z/`; minimal frontend evidence is in
`target/oup-functional/20260914T234305.327831Z/`. `artifacts.json` records the
rebuilt CLI, C library, UniFFI library and binding generator hashes, and
`final-inputs.json` records the final source/script inputs. All recorded final
inputs remained unchanged during the successful run.

## Scope of the evidence

These are local macOS arm64 checks. The functional fixtures exercise real
runtime/transport/tool/native-binding code against deterministic local model
responses; they do not establish paid-provider availability or measured remote
KV-cache hit rates. Historical cloud/tmux acceptance is not rerun here.
GitHub workflow wiring is checked locally; this record does not claim a
remote GitHub Actions run. Optional llama/CUDA/Metal feature combinations and
historical split-media migration remain outside this validation matrix.
