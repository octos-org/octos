# Octos UI Protocol Change Request: Session Workspace Cwd Binding

## Header

- Request id: `UPCR-2026-003`
- Title: Bind a session to a server-approved workspace cwd
- Author: M9 protocol review
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related M issue: `M9` review

## Summary

This change request records the accepted per-session workspace cwd extension
referenced by the protocol spec. It lets a client request a cwd during
`session/open` and lets the server return the canonical approved
`workspace_root` used by cwd-scoped tools.

## Motivation

Coding and workspace tools need a deterministic server-approved root. Letting
clients infer tool scope from their requested path is unsafe because the server
may canonicalize, reject, or narrow the workspace. The wire contract therefore
separates the requested `cwd` from the returned `workspace_root`.

## Change Type

Additive optional command param and additive optional result/notification
field.

## Wire Contract

Affected existing surface:

- Command method: `session/open`
- Params field: `cwd`
- Result shape: `SessionOpenResult.opened`
- Notification shape: `session/open` payload, which shares `SessionOpened`

Optional param:

- `cwd`: client-requested workspace path.

Optional result/notification field:

- `workspace_root`: canonical server-approved root used to bind cwd-scoped
  tools for the session.

Servers must canonicalize and approve `cwd` against runtime filesystem roots
before binding it. A client must not treat its requested `cwd` as approved
until it receives `workspace_root`.

## Compatibility

This is additive. Old clients omit `cwd` and ignore `workspace_root`. Old
servers omit `workspace_root`. Servers that receive `cwd` without negotiated
support must reject it with `invalid_params` and `kind: feature_required`.

## Capability Negotiation

Feature token: `session.workspace_cwd.v1`.

Clients request the feature through `X-Octos-Ui-Features` or equivalent query
params. A server may include `workspace_root` when it has an approved workspace
for the session, including sessions whose workspace was already known.

## Tests

Coverage lives in:

- `crates/octos-core/src/ui_protocol.rs` capability and golden payload tests.
- `crates/octos-cli/src/api/ui_protocol.rs` session-open tests covering cwd
  acceptance, rejection without feature negotiation, and returned
  `workspace_root`.

## Decision

Accepted. The extension makes workspace binding explicit and preserves server
authority over filesystem scope.
