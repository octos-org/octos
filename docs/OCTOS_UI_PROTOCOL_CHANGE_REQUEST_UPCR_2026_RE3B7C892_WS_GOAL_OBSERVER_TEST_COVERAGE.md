# UPCR-2026-RE3B7C892 — WS goal observer test coverage (test-only)

Status: proposed
PR: #3315

## Decision record

This PR adds four test-only regressions for existing goal task-row observer installation in the standalone turn, background skill-action invoke, cached skill-action job restore, and native-review snapshot paths. It also records those exact test names in `crates/octos-cli/TESTS-KEPT.md`.

**No production code or UI Protocol contract changes.** No RPC method, payload schema, notification variant, capability, feature token, serialization, replay envelope, dispatcher behavior, or client-visible compatibility rule is added, removed, or altered. The OUP specification and client codecs need no corresponding change. The protocol-visible gate is triggered only because the tests live in UI-protocol test source files.

Each regression drives the real request/turn and checks the actual SQLite effect. Independently deleting the corresponding production installer makes the test fail: an owed restore row remains running, no registration row appears, or the native-specialist row is absent. This adds deletion-fail coverage of existing behavior; it does not implement the separate cleared/replaced-goal global sweep tracked by #2089.

Cite this record as `UPCR-2026-RE3B7C892`.
