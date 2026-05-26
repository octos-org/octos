# Octos UI Protocol Change Request: Pane Snapshots On Session Open

## Header

- Request id: `UPCR-2026-002`
- Title: Add optional workspace, artifact, and git pane snapshots
- Author: M9 protocol review
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related M issue: `M9` review

## Summary

This change request records the accepted `session/open` pane snapshot payloads
referenced by the protocol spec. The extension lets a server return initial
workspace, artifact, and git panel state during session open so clients can
hydrate panes without immediate follow-up REST calls.

## Motivation

The AppUI shell needs enough initial state to render the session workspace
panels immediately after reconnect or reload. Without an in-band snapshot,
clients either show empty panes until secondary fetches complete or rebuild
state from unrelated stream events. The accepted pane snapshot contract gives
clients an explicit, bounded bootstrap payload.

## Change Type

Additive optional field on an existing command result.

## Wire Contract

Affected existing surface:

- Command method: `session/open`
- Result shape: `SessionOpenResult.opened`
- Notification shape: `session/open` payload, which shares `SessionOpened`

Optional field:

- `panes`: object containing optional `workspace`, `artifacts`, and `git`
  snapshots plus non-fatal `limitations`.

Initial workspace entry kinds are string values:

- `directory`
- `file`
- `symlink`
- `other`

Servers must keep each snapshot bounded. If a pane cannot be collected, the
server should omit that pane or include a limitation rather than failing
`session/open`.

## Compatibility

This is additive. Old clients ignore `panes`; old servers omit it. Clients must
keep fallback pane rendering for absent snapshots.

## Capability Negotiation

Feature token: `pane.snapshots.v1`.

Servers may include `panes` only when the feature is negotiated. Clients
request it through `X-Octos-Ui-Features` or equivalent query params.

## Tests

Coverage lives in:

- `crates/octos-core/src/ui_protocol.rs` golden payload tests for
  `SessionOpened` and capability advertisement.
- `crates/octos-cli/src/api/ui_protocol.rs` session-open tests that verify pane
  snapshot gating and fallback behavior.

## Decision

Accepted. Pane snapshots are a bounded bootstrap aid and do not change the
canonical session lifecycle or message stream.
