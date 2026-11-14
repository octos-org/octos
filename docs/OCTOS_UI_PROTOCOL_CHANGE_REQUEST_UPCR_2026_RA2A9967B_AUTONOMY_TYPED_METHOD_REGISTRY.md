# UPCR-2026-RA2A9967B — Autonomy Typed Method Registry

Status: proposed (implemented for review)
PR: #3269

## Decision record

UPCR-2026-RA2A9967B records an internal contract refactor with no UI
Protocol wire, capability, method-name, or gate change.

The 24 coding-autonomy methods (`agent/*`, `session/goal/*`, `loop/*`,
`monitor/*`) previously kept their param DTOs as CLI-private `Raw*Params`,
their capability gate as a hand-maintained match, and their dispatcher as a
parallel hand-maintained match. They are now driven by a single typed
registry in `octos-core` (`autonomy_rpc::AUTONOMY_METHODS`): each entry
carries the wire method name, the capability feature group, and the typed
param decoder, and yields a typed `AutonomyCommand` the transport maps to
orchestrator calls. Rust clients can now reuse the published param schema.

Compatibility contract: param JSON shapes are byte-identical to the
previous raw DTOs (same field names, defaults, the `loop/create`
`command`/`input` aliases, and no `deny_unknown_fields`); the decode error
format remains `"{method} params: {err}"`; feature gating
(`coding.agent_control.v1`, `coding.goal_runtime.v1`,
`coding.loop_runtime.v1`, `coding.monitor_runtime.v1`) and the
`method_not_supported` / `method_not_found` split are unchanged; result
shapes were already typed orchestrator responses and are untouched.
