# RP06 — octos-dora-mcp Disposition Note

**Issue:** #452 (RP06 — octos-dora-mcp: real forwarding OR removal)
**Decision:** Option B (removal)
**Status:** No-op on main.

## Summary

The `octos-dora-mcp` crate proposed in PR #270 was **never merged to `main`**.
Pre-merge inspection of `origin/main` (and of this stacked branch
`robotics-rp/06-dora-mcp`, which is `origin/main` + RP01) shows:

- No `crates/octos-dora-mcp/` directory.
- No `examples/dora-bridge-config/` directory.
- No `octos_dora_mcp` imports or `octos-dora-mcp` Cargo entries anywhere.

Therefore the RP06 "deletion" contract is vacuously satisfied: there is
nothing to delete on this branch. This note records the decision and the
rationale so future contributors understand why the crate is absent.

## Why removal over real forwarding (Option A)

The PR #270 `DoraToolBridge::execute` implementation is a placeholder that
returns a formatted string instead of forwarding a tool invocation into a
dora-rs dataflow. Shipping that stub in `main` would mislead LLM tool
registration: the tool spec advertises real forwarding while `execute`
fabricates a response.

Delivering real forwarding requires, at minimum:

- External deps: `dora-arrow`, `zenoh` (or an equivalent transport).
- A real dora-rs daemon fixture for integration testing.
- A trust/safety story for letting a dataflow node reply to an LLM-invoked
  tool call (timeouts, resource caps, signed node manifests, etc.).

None of that is in place in the current robotics release preparation
window, and demand from robotics integrators has not surfaced a concrete
driver for the surface area. Clean removal now keeps `main` honest; the
deferred work is tracked in a successor issue opened at the same time as
this note.

## Follow-up

The successor issue linked from #452 captures:

- The desired Option A surface (trait-backed `DoraToolBridge` with real
  dataflow forwarding).
- External dependencies (`dora-arrow`, `zenoh`) and the integration test
  harness they imply.
- The "unblocked by" condition: demand signal from at least one robotics
  integrator with a concrete dataflow we can forward to.

Until then, `main` intentionally does not carry an `octos-dora-mcp` crate.
