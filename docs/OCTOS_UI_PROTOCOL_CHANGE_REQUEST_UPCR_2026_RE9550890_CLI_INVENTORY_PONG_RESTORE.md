# UPCR-2026-RE9550890 — CLI inventory closure and healthy Pong test restoration

Status: proposed
PR: TBD

## Decision record

T-0589 closes 28 existing `octos-cli` test-inventory decisions in
`TESTS-KEPT.md` (25 retained rows and three assertion-level equivalent mappings),
restores the real WebSocket healthy-idle Ping/Pong test removed by #3144, and adds
a focused profile-factory constructor-failure assertion. The three equivalent
duplicates are removed only where the stronger retained chain preserves every
behavioral assertion. The restored client answers each
protocol Ping with Pong and must remain connected past the liveness deadline;
fully-silent close and pipelined request-id tests remain separate.

Shared test setup for memory profiles/files, direct-memory unchanged-byte
assertions, and real serve process construction/cleanup is consolidated to keep
the change line-neutral. No RPC method, envelope, event, error mapping,
permission, capability advertisement, persistence format, or client/server wire
behavior changes. Production interfaces are not added or modified.

The independent profile-factory cross-provider fallback `api_key_env` inheritance
divergence found during audit is explicitly outside this record and is reported
for separate production analysis; no production fix or secret-bearing test is
included here. Validation covers all affected unit tests, the three real
WebSocket liveness scenarios, strict test Clippy, exact retained-name discovery,
and normal CI. Cite this record as `UPCR-2026-RE9550890`.
