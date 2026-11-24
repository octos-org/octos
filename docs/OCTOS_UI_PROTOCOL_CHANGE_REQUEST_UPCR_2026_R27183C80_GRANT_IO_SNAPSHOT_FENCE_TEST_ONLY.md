# UPCR-2026-R27183C80 — Grant snapshot fence release test (test-only)

Status: proposed
PR: #3354

## Decision record

T-0660 changes only `crates/octos-cli/src/api/ui_protocol_grant_io_tests.rs`, a path covered by the protocol-visible gate. The post-admission check for the grant snapshot fence now polls with a five-second deadline instead of assuming that the completed task's frame teardown has already closed the lock file when its `JoinHandle` resolves. This removes the intermittent WouldBlock race seen on ARM64.

The PR changes **no production code, wire type, error code, frame shape, capability, dispatcher behavior, or client contract**. `bind_ingress_generation` still drops the snapshot fence synchronously before returning, revocation still follows the fence release, and a fence that is never released still fails the test at the deadline. No protocol revision or compatibility change is introduced.

`docs/upcr/README.md` intentionally has zero diff under the decentralized-id rule.
