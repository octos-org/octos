# Octos UI Protocol Change Request: Typed Approval Payloads

## Header

- Request id: `UPCR-2026-001`
- Title: Add typed approval metadata and scoped approval responses
- Author: M9 protocol review
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related M issue: `M9` review

## Summary

This change request records the accepted typed-approval extension that the
protocol spec already references. It keeps the fallback approval text contract
intact while adding structured fields that let clients render command, diff,
filesystem, network, and sandbox-escalation approvals without parsing prose.

## Motivation

The first AppUI approval surface only guaranteed generic text fields. That was
enough for a minimal modal, but it made clients infer risk, command previews,
filesystem paths, and scope semantics from human-readable strings. The shipped
server and golden protocol tests expose these fields directly, so the protocol
documentation needs a durable UPCR for the wire-visible contract.

## Change Type

Additive optional fields on an existing notification and command.

## Wire Contract

Affected existing surface:

- Notification method: `approval/requested`
- Command method: `approval/respond`

Optional fields added to `approval/requested`:

- `approval_kind`: string registry. Initial values are `command`, `diff`,
  `filesystem`, `network`, and `sandbox_escalation`.
- `risk`: display/audit risk label.
- `typed_details`: tagged object. Its `kind` should match `approval_kind` when
  both fields are present.
- `render_hints`: optional UI hints such as default decision, danger state,
  labels, and monospace display groups.

Optional fields added to `approval/respond` params:

- `approval_scope`: string registry. Initial values are `request`, `turn`, and
  `session`.
- `client_note`: human-readable client audit note.

The required fallback `title` and `body` fields on `approval/requested` remain
mandatory so old clients can still render a correct prompt.

## Compatibility

This is additive. Old clients may ignore the typed fields and continue to use
`title` and `body`. Old servers that omit typed fields remain valid producers.
Servers must not require `approval_scope` or `client_note` from clients.

## Capability Negotiation

Feature token: `approval.typed.v1`.

Servers advertise the token in `UiProtocolCapabilities.supported_features`.
Clients request it through `X-Octos-Ui-Features` or the equivalent
`ui_feature` / `ui_features` query params.

## Tests

Coverage lives in the protocol golden tests and approval handler tests:

- `crates/octos-core/src/ui_protocol.rs` verifies typed approval payload
  serialization and the advertised method/notification sets.
- `crates/octos-cli/src/api/ui_protocol.rs` verifies typed approval requests,
  responses, and durable approval lifecycle notifications.

## Decision

Accepted. The typed fields are optional, backward-compatible, and required for
clients that need deterministic approval rendering without text scraping.
