# ADR: Human Approval Flow for Matrix/Gateway Channels (Robrix Phase 4)

> Status: **Accepted & implemented** (2026-06-13) | Date: 2026-06-12 | Workstream: [robrix-integration](../workstreams/robrix-integration.md)
> Decision owners: @AlexZ

## Context

Closed PR #345 included a ~1,900-line config-driven approval flow for Matrix
bots (reference commits `73313637` + `53fb5c87`, reachable via
`git fetch origin pull/345/head:pr-345-head`). Phases 1–3 of the salvage
workstream re-implemented the PR's other features; this ADR decides how to
land approvals, because **main has since grown its own approval
infrastructure** and porting the PR as-is would create a second, semantically
divergent approval system.

## What main already has (investigated 2026-06-12)

### A. The synchronous in-turn approval bridge

- `ToolApprovalRequester` / `TOOL_APPROVAL_CTX` task-local
  (`octos-agent/src/tools/mod.rs:329-354`). A tool that hits
  `Decision::Ask` (SafePolicy patterns like `sudo`, `rm -rf`,
  `git push --force` — `policy.rs:277-300`) fetches the per-turn requester
  and **blocks on it inside the running turn** until a decision arrives.
- Sole production implementor: `UiProtocolApprovalRequester`
  (`octos-cli/src/api/ui_protocol.rs:3071`), wired only on the serve/WebSocket
  path. It sends `approval/requested` over the socket and blocks on a oneshot
  until the client calls `approval/respond`.
- Rich supporting machinery, all UI-protocol-side:
  - `PendingApprovalStore` with Pending/Responded/Cancelled states and
    turn-interrupt cancellation (`api/ui_protocol_approvals.rs`)
  - scope policies — approve-for-turn / -session / -tool auto-resolution
    (`api/ui_protocol_scope.rs`)
  - durable audit: append-only JSONL `approvals-*.log` with rotation and
    retention (`api/ui_protocol_audit.rs`)
- Gate config: `ApprovalPolicy` enum `Ask | Never` (`octos-agent/src/policy.rs:24`),
  derived from `PermissionProfile`.

### B. What main does NOT have

Confirmed absent (grep + file review):

| Capability | Status on main |
|---|---|
| Approval available on gateway channels (Matrix/Telegram/…) | **No** — `session_actor.rs` never scopes `TOOL_APPROVAL_CTX`; `Ask` tools are auto-denied ("no interactive approval available") |
| Config-driven *per-tool* "always require human approval" rules | No — only SafePolicy pattern matching triggers Ask |
| Named authorized approvers | No — whoever holds the WebSocket can approve |
| Configurable approval expiry / timeout behavior | No — pending approvals live until response or turn interrupt |
| Risk levels attached to approvals | No (dispatch metadata carries an informational `risk` string, not wired to approvals) |

### C. The hooks deny path (orthogonal)

`before_tool_call` hooks (exit 1 = deny, exit 2 = modified) short-circuit
before any approval logic (`hooks.rs:589`, `execution.rs:415-461`). They are
policy automation, not human escalation; no change proposed here.

## What PR #345 built (reference semantics)

The PR's model is **suspend-and-resume**, not block-in-turn:

1. `Agent::execute_tools` checks a config-driven rule set
   (`ApprovalRule { tools, risk_level, authorized_approvers, expires_in_secs, on_timeout }`)
   *before* executing a matching tool, and returns early with
   `ToolExecutionOutcome::ApprovalRequested(PendingApprovalDraft)`. The agent
   loop **terminates the turn** with `ConversationResponse.pending_approval`.
2. `session_actor` converts the draft to a `PendingApproval`
   (bound to room + requester), sends a Matrix message carrying
   `org.octos.approval_request` + `org.octos.actions` (Approve/Deny buttons,
   rendered natively by Robrix), stores it in an in-memory
   `PendingApprovalStore`, and spawns an expiry timer.
3. The human's response arrives later as a **new inbound message** carrying
   `org.octos.approval_response`. The actor validates it — unknown/consumed
   request, wrong room, expired, unauthorized sender, SHA-256
   `tool_args_digest` mismatch — re-runs `before_tool_call` hooks
   (`revalidate_pending_approval`, policy may have changed mid-wait), then
   executes the tool directly via `execute_approved_tool` (no LLM round trip).
4. Hook exit code **3** lets a hook dynamically demand approval
   (`HookResult::ApprovalRequested(spec)`).

Strong properties worth keeping regardless of architecture: digest-bound
approvals (no arg-swap), consumed-set (no double execution), authorized
approver lists, post-wait revalidation, room binding.

Weaknesses: pending approvals are in-memory only (lost on restart), audit is
tracing-only, and it duplicates concepts main now owns (a second
`ApprovalPolicy` type name colliding with `policy::ApprovalPolicy`, a second
`PendingApprovalStore` distinct from `api/ui_protocol_approvals.rs`).

## The actual decision: which waiting model for gateway channels

The two systems differ fundamentally in **where the wait lives**:

- **Block-in-turn** (main): the tool future parks on a oneshot; fine when a
  client is attached over a live socket and answers in seconds.
- **Suspend-and-resume** (PR): the turn ends; the approval response is a new
  inbound event. Fits store-and-forward channels where the human may answer
  in minutes or hours.

A Matrix approval can take hours. Holding an agent turn open that long is not
viable: the tool-registry timeout (default 1800 s) would expire the wait, the
session actor would be stuck mid-turn while the approval reply queues behind
the very turn that is waiting for it (deadlock-shaped), and an LLM
turn held open across a restart is unrecoverable. **Block-in-turn cannot serve
gateway channels; suspend-and-resume is structurally required there.**

## Options considered

### Option 1 — Port PR #345 as-is
Two parallel approval systems with different config, audit, and semantics.
Rejected: this is the divergence the ADR exists to prevent.

### Option 2 — Pure reuse: a Matrix `ToolApprovalRequester`
Implement the existing trait over Matrix send/receive and block the turn.
Rejected on the latency/deadlock/restart grounds above. (It would also need
inbound approval replies to bypass the busy session actor — a deeper change
than the PR's model.)

### Option 3 — Suspend-and-resume transport, unified semantics (CHOSEN)
Adopt the PR's suspend/resume shape for gateway channels, but converge every
shared concept onto main's existing vocabulary instead of duplicating it:

1. **One rule schema.** Land `ApprovalRuleConfig` (tools / authorized_approvers /
   expires_in_secs / risk_level / on_timeout) in `octos-cli/src/config.rs` +
   `profiles.rs` as *the* config surface for human-approval rules. Rename the
   runtime type from the PR's `ApprovalPolicy` to **`HumanApprovalRules`**
   (avoids colliding with `policy::ApprovalPolicy`). The UI-protocol path may
   later consume the same rules ("require approval for tool X even over
   WebSocket") — the schema must not be Matrix-flavored.
2. **One audit trail.** Gateway approval decisions emit the same
   `ApprovalDecidedEvent` record shape into the existing JSONL audit log
   (`api/ui_protocol_audit.rs`) — not tracing-only. `decided_by` carries the
   Matrix user ID.
3. **Keep both waiting models, with a documented boundary.**
   `ToolApprovalRequester` remains the mechanism for live-socket clients;
   `ToolExecutionOutcome::ApprovalRequested` + session-actor resume becomes
   the mechanism for store-and-forward channels. The agent-side trigger is
   shared: one rule-match in `execute_tools` feeds whichever wait mechanism
   the host wired.
4. **Port the PR's security invariants verbatim**: args digest binding,
   consumed-set, room binding, authorized-approver validation, post-wait
   `revalidate_pending_approval`.
5. **Channel projection stays generic.** `org.octos.approval_request` /
   `org.octos.actions` / `org.octos.approval_response` follow the Phase 1
   app-card pattern (Robrix renders buttons; other clients show the text
   fallback). Telegram/Discord can implement the same metadata contract later
   without touching the agent.
6. **Defer hook exit code 3.** It widens the hook contract
   (CLAUDE.md documents 0/1/2+ semantics) for a use case config rules already
   cover. Revisit only with a concrete need.
7. **v1 limitations, accepted and documented:** pending approvals are
   in-memory (a restart drops them — the requester sees no reply and the
   expiry notice never fires; mitigated by short default expiry, e.g. 600 s);
   `on_timeout` supports `notify` only; no approve-for-session scopes on the
   gateway path in v1.

## Consequences

- Gateway channels gain human approval without forking the concept space:
  one config schema, one audit log, two clearly-bounded wait mechanisms.
- The agent crate gains a small generic module (`approval.rs` ≈ the PR's
  model minus the name collision) plus the `ToolExecutionOutcome` /
  `ConversationResponse.pending_approval` plumbing — reusable by any
  suspend-capable host, not Matrix-specific.
- The UI-protocol path is untouched in v1; later unification (config rules
  triggering WebSocket approvals) is additive.
- Restart durability and richer timeout behaviors are explicitly deferred;
  the audit log gives operators the paper trail in the meantime.

## Implementation sketch (single PR, est. ~1,200 lines incl. tests)

| Step | Where | Notes |
|---|---|---|
| 1. `HumanApprovalRules` model + drafts + pending store + validation errors | `octos-agent/src/approval.rs` (new) | Port from `pr-345-head`, rename types, keep digest/consume/revalidate |
| 2. `ToolExecutionOutcome::ApprovalRequested` + early return + `execute_approved_tool` / `revalidate_pending_approval` | `agent/execution.rs`, `loop_runner.rs`, `agent/mod.rs` | Adapt to current loop (post-M11) |
| 3. Config schema + validation + profile passthrough | `octos-cli/src/config.rs`, `profiles.rs` | Include `53fb5c87`'s post-expansion validation |
| 4. Session-actor bridge: emit request card, pending store, expiry timer, response handling, audit emission | `octos-cli/src/session_actor.rs` | Reuse `ApprovalDecidedEvent` + audit log from `api/ui_protocol_audit.rs` |
| 5. Matrix projection in/out | `octos-bus/src/matrix_channel.rs` | ~140 lines, mirrors Phase 1 app-card projection |
| 6. Docs | `book/src/configuration.md`, `channels.md` | Rule schema + Robrix card behavior |

TDD per repo convention; mock-homeserver tests for the projection, actor
tests for validate/consume/expiry, agent tests for rule-match interception.

## References

- Main approval infra: `octos-agent/src/tools/mod.rs:329-354`,
  `octos-agent/src/tools/shell.rs:310-369`, `octos-agent/src/policy.rs:24-40`,
  `octos-cli/src/api/ui_protocol_approvals.rs`, `ui_protocol_scope.rs`,
  `ui_protocol_audit.rs`
- PR reference: commits `73313637`, `53fb5c87` on `pr-345-head`
- Related ADRs: `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md`
