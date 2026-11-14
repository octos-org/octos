# UPCR-2026-RB85220B5 — Host-scoped execution and active-turn steering

Status: proposed (implemented for review)
PR: #3260

## Decision record

Hagency needs to embed an Octos process for one owner-selected profile and
workspace while keeping turn control tied to the original authorized run.
This record covers the additive capability advertisements and steering
precondition in UPCR-2026-RB85220B5.

### Active-turn-only steering

The optional `turn.steer_active_only.v1` feature lets a client send
`require_active: true` with an `expected_turn_id` to `turn/steer`. A missing
expected ID is invalid; a mismatched or completed turn is rejected. If the
turn becomes terminal while the request is arriving, the request cannot
start a replacement turn. A successfully accepted input remains subject to
the existing steer-buffer and dropped-input settlement rules.

Omitting `require_active` preserves the existing idle fallback, including its
ability to start a new turn. The new feature is not inferred for legacy
clients: a client must negotiate it before relying on the precondition,
because older servers may ignore unknown request fields. The change adds no
new notification or persisted ledger format.

### Host-scoped capability advertisements

When started with `serve --stdio --host-scoped-profile ID`, the process
advertises `runtime.host_scoped_profile.v1`,
`runtime.host_scoped_workspace_io.v1`, `runtime.host_patch_worker.v1`, and
`runtime.host_scoped_web.v1`. On Unix it also advertises
`runtime.host_scoped_search.v1` and `runtime.host_scoped_vision.v1`. Their
individual semantics and platform constraints are specified in the OUP v1
specification.

The scoped process uses a separate instance directory and requires
`--no-network`; its bootstrap is limited to the selected profile and does not
load automatic hooks, MCP servers, plugins, or saved background work. Scoped
workspace operations fail closed without an authorized workspace. These
capabilities do not themselves authorize tools or provide OS process
isolation. The embedding host remains responsible for process confinement,
tool authorization, patch-worker supervision, and clearing ambient web-search
configuration as specified by the protocol.

Legacy execution is unchanged unless the new startup mode is selected. Older
binaries reject the startup flag, so a host must verify the advertised
capabilities before relying on this mode. Focused tests cover profile and
workspace confinement, active-only steering, and the completion race.

## Test evidence follow-up (T-0606)

`crates/octos-cli/tests/serve_host_scoped.rs` drives the real stdio executable
with private profile/instance/workspace directories and a local scripted model.
It checks startup prerequisites, advertised capabilities, foreign-profile
rejection, actual workspace read/list/write dispatch, and permission widening
at session bootstrap. Tool calls run separately so a failed read cannot cancel
a write before its boundary is exercised. The ancestor alias inside the
workspace passes canonical containment, independently testing the descriptor
walk; an outside alias verifies the public containment boundary as well.

The CLI unit tests pin exact idle/terminal active-only errors, legacy capability
omission, an already-loaded foreign runtime, and actual non-macOS sandbox
refusal. The process-global profile pin runs in an isolated test subprocess.
These tests change no runtime policy or wire format.
