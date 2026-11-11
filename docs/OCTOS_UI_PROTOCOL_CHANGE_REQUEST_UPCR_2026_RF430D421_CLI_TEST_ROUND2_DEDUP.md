# UPCR-2026-RF430D421 — CLI second-round test deduplication, no protocol change

Status: proposed
PR: TBD

## Decision record

T-0580 consolidates four same-chain retained-test scenarios and extracts shared
setup in `crates/octos-cli`, including files with `ui_protocol_*` names. The raw
and archived signed-install successes become one test, six registry/review/platform
refusals become labeled cases in one test, rollback and fresh review-bound reinstall
share one lifecycle test, and the work-secret raw denial joins its typed round trip.
Every original assertion, error kind, cursor, capability negotiation, cancellation,
recovery, durability, and platform condition remains covered.

Shared helpers for ACP initialization/prompts, `SessionOpenParams`, recovery drains,
process cleanup, readiness, and RPC frame decoding change test scaffolding only.
No RPC method, envelope, event, error mapping, permission, capability advertisement,
persistence format, or client/server wire behavior changes. Production interfaces
are not added or modified to support this test cleanup.

Validation includes execution of all affected unit and real-process tests, the full
114-test `octos-cli --lib` suite, strict test Clippy, exact retained-name mapping,
five security-subclass checks, and normal CI. The 28 pre-existing tests absent from
`TESTS-KEPT.md` and the independently reported WS Pong-liveness restoration gap are
not silently changed or restored by this record. Cite this record as
`UPCR-2026-RF430D421`.
