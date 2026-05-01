# Octos UI Protocol Change Request: SessionOpened Capability Advertisement

## Header

- Request id: `UPCR-2026-007`
- Title: Emit `UiProtocolCapabilities` on `SessionOpened` so clients can
  discover server features in-band
- Author: M9 harness audit follow-up (coding-green)
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related M issue: `#720` (M9 audit gap — `SessionOpened` had no
  `capabilities` payload, so clients had to read the spec doc to know
  which `X-Octos-Ui-Features` tokens the server actually honours)

## Summary

This change request adds a required `capabilities` field to the
`SessionOpened` payload (shared by the `session/open` RPC result and the
`session/open` notification) so clients can discover the server's
negotiated method set, notification set, schema versions, and the
honoured subset of their `X-Octos-Ui-Features` request without an
out-of-band spec lookup.

The shape is the existing `UiProtocolCapabilities` value, already
constructed for the runtime advertisement APIs in `octos-core`. The
field is **always emitted** by the server; the wire payload itself is a
strict superset of the previous one. Older clients that ignore unknown
fields per spec § 4 continue to work; older serialized payloads (e.g.
ledger replays from before the field existed) decode successfully
because the field carries `serde(default = "first_server_slice")`.

## Motivation

Spec § 4 already defines capability negotiation as part of the protocol
contract, and the WebSocket connection parser in
`crates/octos-cli/src/api/ui_protocol.rs` reads
`X-Octos-Ui-Features` (and the `ui_feature` / `ui_features` query
parameters) on every connection. But the `SessionOpened` payload — the
first lifecycle frame after `session/open` — never echoes back which
features the server understood. As a result:

- New clients have to hard-code feature tokens taken from the spec doc.
  They cannot ask the server "what are you actually willing to honour
  for this session?" until they probe each method individually and
  observe an `unsupported_capability` error.
- Servers that drop a feature flag (e.g. due to a missing dependency in
  a slim build) cannot signal that to the client until a downstream RPC
  fails.
- Replays of historical sessions cannot be inspected for "which
  capability slice did this client and server land on?" because the
  ledger event is silent on it.

Since `SessionOpened` is already on the critical path for every
session, attaching the negotiated capability snapshot to it closes the
gap with one additive field instead of an extra round-trip RPC.

## Change Type

Additive required field on an existing payload.

`SessionOpened.capabilities: UiProtocolCapabilities` is added to the
shared payload used by:

- the `session/open` RPC `SessionOpenResult.opened`
- the `session/open` notification (same `SessionOpened` shape, sent
  through the event ledger)

No method, notification, enum variant, capability flag, or protocol
identifier is changed. No existing field is removed or repurposed. The
`UiProtocolCapabilities` struct itself is unchanged.

## Wire Contract

Affected existing wire surface:

- Payload: `SessionOpened`
  - new field: `capabilities` (required, always emitted)

The added field is a `UiProtocolCapabilities` object with the existing
shape:

```json
{
  "version": {
    "protocol": "octos-ui/v1alpha1",
    "schema_version": 1,
    "jsonrpc": "2.0"
  },
  "capabilities_schema_version": 2,
  "supported_methods": [
    "session/open",
    "turn/start",
    "turn/interrupt",
    "approval/respond",
    "approval/scopes/list",
    "diff/preview/get",
    "task/list",
    "task/cancel",
    "task/restart_from_node",
    "task/output/read"
  ],
  "supported_notifications": [
    "session/open",
    "turn/started",
    "turn/completed",
    "turn/error",
    "message/delta",
    "tool/started",
    "tool/progress",
    "tool/completed",
    "approval/requested",
    "approval/auto_resolved",
    "approval/decided",
    "approval/cancelled",
    "task/updated",
    "task/output/delta",
    "progress/updated",
    "warning",
    "protocol/replay_lossy"
  ],
  "supported_features": [
    "pane.snapshots.v1"
  ]
}
```

### Negotiation Semantics

`capabilities.supported_methods` and
`capabilities.supported_notifications` are the server's first-slice
baseline so a discovery-aware client can learn the surface in-band even
when it never sent `X-Octos-Ui-Features`.

`capabilities.supported_features` is computed from the client's
`X-Octos-Ui-Features` header (or `ui_feature` / `ui_features` query
param):

1. **No header sent** → server returns
   `UiProtocolCapabilities::first_server_slice()` (full known feature
   set). Existing clients that don't yet know to negotiate still see
   what the server can do.
2. **Header sent with feature tokens** → server returns the
   intersection of requested features with the server-known feature
   registry (`UI_PROTOCOL_KNOWN_FEATURES`). The server **never** leaks a
   feature flag the client did not ask for; clients see exactly which
   of their requests were honoured.
3. **Unknown tokens in the header** → silently dropped from the
   response (server does not advertise capabilities it cannot honour).

The intersection logic lives behind the new
`UiProtocolCapabilities::for_negotiated_features` builder in
`octos-core` and behind a `ConnectionUiFeatures::negotiated_capabilities`
helper in `octos-cli` so it stays in one place across handlers.

## Compatibility

- Old clients that ignore unknown fields per spec § 4 continue to work
  unchanged. The TS interface in `e2e/lib/m9-ws-client.ts` already uses
  `unknown` for the pane field and will treat `capabilities` the same
  way.
- Old serialized payloads (ledger replays from before the field
  existed) decode successfully because the field carries
  `serde(default = "UiProtocolCapabilities::first_server_slice")`. A
  replayed `SessionOpened` from an older binary surfaces the
  first-slice default for the missing field, which is the same payload
  a fresh open with no header would receive — preserving the
  "unspecified ⇒ default" invariant.
- Old servers that have not adopted the field continue to emit a
  `SessionOpened` without `capabilities`. Clients deserializing such a
  payload through the new schema get the `first_server_slice` default;
  no breakage.
- Existing `UiProtocolLedger` replays continue to work because the
  ledger stores the JSON wire form, and the new schema decodes both old
  and new shapes.
- The `ConnectionUiFeatures` struct gains two private fields
  (`harness_task_control: bool`, `header_present: bool`) used by the
  negotiation helper. Both fields default to `false` (matches the
  pre-UPCR "no header sent" semantics) so existing tests that build the
  struct via `Default::default()` continue to work.
- No new protocol identifier is required because the change is
  additive.

## Capability Negotiation

None. UPCR-2026-007 *is the in-band capability negotiation surface*; it
does not introduce a new feature flag of its own. The field is
unconditionally present so a client can rely on its existence (and on
the `serde` default for legacy replays) without first negotiating
anything.

The set of feature flags advertised through
`capabilities.supported_features` is governed by the existing per-flag
UPCRs (`UPCR-2026-001`, `UPCR-2026-002`, `UPCR-2026-003`,
`UPCR-2026-005`).

## Tests

- `crates/octos-core/src/ui_protocol.rs`:
  - `session_open_result_includes_capabilities_field` — golden: the
    serialized `SessionOpened` carries a `capabilities` payload with
    `version`, `capabilities_schema_version`, `supported_methods`, and
    every flag in `UI_PROTOCOL_KNOWN_FEATURES`. Also asserts a legacy
    payload without the field decodes via the `serde` default.
  - `negotiated_capabilities_advertise_full_protocol_when_no_features_requested`
    — empty request returns the first-slice baseline with an empty
    `supported_features`.
  - `negotiated_capabilities_intersect_requested_with_known_features` —
    a request containing a known feature plus an unknown token returns
    only the known feature; never leaks unrequested flags.
- `crates/octos-cli/src/api/ui_protocol.rs`:
  - `session_open_result_advertises_full_protocol_when_no_header` —
    `open_session_result()` with `ConnectionUiFeatures::default()`
    returns `first_server_slice()` capabilities.
  - `session_open_result_advertises_intersection_when_header_subset` —
    a client requesting only `pane.snapshots.v1` receives a session
    payload whose `supported_features` is exactly that one entry, and
    the method/notification surface stays intact for in-band
    discovery.

## Rollout Plan

This UPCR ships in the same PR that:

1. Adds `UI_PROTOCOL_KNOWN_FEATURES` and the
   `UiProtocolCapabilities::for_negotiated_features` builder to
   `octos-core`.
2. Adds the required `capabilities` field to `SessionOpened` with
   `serde(default = "first_server_slice")` for backward compatibility.
3. Wires `ConnectionUiFeatures` to track `header_present` and
   `harness_task_control`, and exposes `negotiated_capabilities()`.
4. Populates `SessionOpened.capabilities` in `open_session_result`.
5. Updates `api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md` § 4 and § 7 to
   reference UPCR-2026-007 and document the field semantics.

No client renegotiation is required. Clients that want to gate UI on
the negotiated set can read the field on their next protocol-types
update.

## Decision

Accepted by: M9 harness audit follow-up (coding-green).

Decision notes: Accepted as the minimum additive change to close audit
issue #720. The field ships the existing `UiProtocolCapabilities`
shape, reuses the existing `X-Octos-Ui-Features` header as the
negotiation channel, and never leaks a feature the client did not
request. The required-field-with-`serde(default)` choice keeps wire
compatibility with older binaries and ledger replays while pinning the
field as a stable surface new clients can rely on.
