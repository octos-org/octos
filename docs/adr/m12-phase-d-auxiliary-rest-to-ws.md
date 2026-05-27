# M12 Phase D — Auxiliary REST → WS UI Protocol v1

- Date: 2026-05-12 (proposal) / 2026-05-27 (backfilled to repo)
- Status: **Accepted**. Implementation landed across `#912` (D-1 server frames),
  the D-3 client cutover sequence in `octos-web`, and `#914` (D-5 REST
  retirement). This ADR backfills the contract record that was missed during
  the original landing (tracked as issue `#1330`).
- Branch (target at proposal time): `main`. The ADR is now part of the
  steady-state contract record; future amendments go through UPCRs.

## Context

### What Phase C did

M9-α-5/α-6 (PRs `#855`, `#908`, `#909`) deleted the SSE foreground chat
transport. The sole chat transport is now `/api/ui-protocol/ws` (see
`crates/octos-cli/src/api/router.rs` and `docs/M9-ALPHA-SOLE-TRANSPORT-ADR.md`).
On the client, ingest goes through `octos-web/src/runtime/ui-protocol-bridge.ts`
exclusively.

### What Phase C did NOT do

Everything the web UI did **outside** the assistant streaming lifecycle still
went over REST. Concretely, every endpoint in `my_api` and the non-chat half of
`chat_api` was REST-only. The web client called them via the helper at
`octos-web/src/api/client.ts` (the `request<T>()` function), which carried a
global 401/403 interceptor that wiped the bearer token from `localStorage` and
redirected to `/login`.

### The incident this ADR responds to

On 2026-05-11, a fresh OTP-authenticated user landed on a `mini5` build whose
global agent was momentarily misconfigured. The browser bootstrap pipeline ran
~6 REST calls in parallel:

- `GET /api/auth/me`
- `GET /api/my/profile`
- `GET /api/status`
- `GET /api/sessions`
- `GET /api/sessions/{just-created-id}/messages` (404, harmless)
- `GET /api/sessions/{just-created-id}/files`

Any one of those returning 401 — for ANY reason on ANY of the six endpoints —
triggered `clearToken()`. The user's freshly-stored OTP token got wiped. The
login redirect created an infinite loop: authenticate → bootstrap → one of six
401s → wipe → login.

The WS chat session was independent of REST auth — but it read its bearer token
from `localStorage`. So even the WS chat died the next time the bridge had to
reconnect, because its credential just got wiped.

### Why the fix is architectural, not a one-line guard

Narrowing the 401 reaper to specific paths worked as a hotfix and shipped as
step D-4 below. But it did not address the underlying split: **two transports,
two auth surfaces, one combined kill switch**. Every new REST endpoint added
against the auxiliary panels (sessions / files / tasks / status / content)
re-opened the same failure mode. The structural fix is to finish the migration
the M9 plan started: move auxiliary REST onto the same WS UI Protocol v1 that
already carries chat. REST then survives only for **auth** (where it actually
belongs) and **blob I/O** (where HTTP fits the shape).

## Decision

The **data plane** for the octos web client is WebSocket UI Protocol v1
(`/api/ui-protocol/ws`). REST survives only for two carve-outs:

1. **AUTH** — `/api/auth/*` and the bootstrap helper `GET /api/my/profile` used
   to learn `selected_profile`. REST is correct here because:
   - OTP login establishes the bearer token used by every other transport.
   - Pre-session cookie/profile resolution must happen before any WS handshake
     can succeed.
   - These calls are scoped to a tiny, well-known prefix and can be guarded by
     an explicit auth-only 401 interceptor.

2. **BLOB** — `POST /api/upload`, `POST /api/site-files/upload`,
   `GET /api/files/{path}`, `GET /api/my/content/{id}/thumbnail`,
   `GET /api/files/list` (small JSON, but driven by the same blob-shaped use
   case). Multi-megabyte bodies belong on HTTP, not on the WS text-frame budget
   (`MAX_TEXT_FRAME_BYTES = 1 MiB` per `octos-core/src/ui_protocol.rs`).

Everything else — session list, snapshot, messages, files panel, tasks panel,
status, content panel, content delete — became a JSON-RPC method on the
existing WS connection. The same connection already carried `session/open`,
`session/hydrate`, `turn/start`, `turn/interrupt`, `task/cancel`,
`task/output/read`, etc. Adding auxiliary methods was additive and did not
introduce a second transport.

After the client migration completed, the 401 reaper collapsed to:

```ts
if (resp.status === 401 && path.startsWith("/api/auth/")) {
  clearToken();
  // redirect ...
}
```

A 401 on auxiliary REST during the deprecation window became a typed error
surfaced by the panel that called it — not a session detonation.

## Endpoint inventory

Sourced from the pre-D-5 REST router and the `octos-web` API clients. One row
per URL the web client actually called.

| URL | Method | Current client caller (pre-cutover) | Category | Shipped WS frame |
| --- | --- | --- | --- | --- |
| `/api/auth/send-code` | POST | `api/auth.ts` | AUTH | — (stays REST) |
| `/api/auth/verify` | POST | `api/auth.ts` | AUTH | — (stays REST) |
| `/api/auth/me` | GET | `api/auth.ts` | AUTH | — (stays REST) |
| `/api/auth/status` | GET | `api/auth.ts` | AUTH | — (stays REST) |
| `/api/auth/logout` | POST | `api/auth.ts` | AUTH | — (stays REST) |
| `/api/my/profile` | GET | `api/client.ts` (bootstrap) | AUTH | — (stays REST; pre-WS bootstrap) |
| `/api/status` | GET | `api/sessions.ts` | MIGRATED | `system/status.get` |
| `/api/sessions` | GET | `api/sessions.ts` | MIGRATED | `session/list` |
| `/api/sessions/{id}` | DELETE | `api/sessions.ts` | MIGRATED | `session/delete` |
| `/api/sessions/{id}/messages` | GET | `api/sessions.ts` | MIGRATED | `session/messages_page` |
| `/api/sessions/{id}/status` | GET | `api/sessions.ts` | MIGRATED | `session/status.get` |
| `/api/sessions/{id}/files` | GET | `api/sessions.ts` | MIGRATED | `session/files.list` |
| `/api/sessions/{id}/tasks` | GET | `api/sessions.ts` | MIGRATED | `session/tasks.list` |
| `/api/sessions/{id}/workspace-contract` | GET | `api/sessions.ts` | MIGRATED | `session/workspace.get` |
| `/api/sessions/{id}/title` | PATCH | server-side title flow | MIGRATED | `session/title.set` |
| (combined snapshot helper) | — | new bootstrap consolidation | MIGRATED | `session/snapshot` |
| `/api/my/content` | GET | `api/content.ts` | MIGRATED | `content/list` |
| `/api/my/content/{id}` | DELETE | `api/content.ts` | MIGRATED | `content/delete` |
| `/api/my/content/bulk-delete` | POST | `api/content.ts` | MIGRATED | `content/bulk_delete` |
| `/api/my/content/{id}/thumbnail` | GET | `api/content.ts` (URL only) | BLOB | — (stays REST; image source) |
| `/api/my/content/{id}/body` | GET | download path | BLOB | — (stays REST) |
| `/api/upload` | POST | `api/chat.ts` | BLOB | — (stays REST; multipart) |
| `/api/site-files/upload` | POST | `sites/api.ts` | BLOB | — (stays REST; multipart) |
| `/api/files/list` | GET | `store/file-store.ts`, `slides/api.ts`, `sites/api.ts` | BLOB | — (stays REST; directory listing for blob URLs) |
| `/api/files/{path}` | GET | many sites/slides/file-delivery surfaces | BLOB | — (stays REST; binary download) |
| `/api/site-preview/...` | GET | served as raw HTML preview | BLOB | — (stays REST; HTML/asset preview) |
| `/api/preview/{profile}/{session}/{slug}/...` | GET | public site preview | BLOB | — (stays REST; public asset) |
| `/api/admin/*` | various | admin SPA (out of scope) | OUT-OF-SCOPE | tracked separately |

**Thirteen MIGRATED rows** — twelve direct REST replacements plus the
consolidated `session/snapshot` helper that collapses three bootstrap calls
into one round trip. Eight BLOB rows, six AUTH rows, one out-of-scope row.

## Method index — `auxiliary.rest_to_ws.v1`

All thirteen methods are gated on
`UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1` (`auxiliary.rest_to_ws.v1`). The
gate is strict opt-in: a client that does not negotiate the feature receives
`method_not_supported` (`-32004`) on every call below, even when no feature
header was sent. This is what makes Phase D-1 truly additive — pre-existing
clients cannot trip into the new methods without explicit negotiation.

| Method | Purpose | Replaces |
| --- | --- | --- |
| `session/list` | Sidebar session list. | `GET /api/sessions` |
| `session/snapshot` | Combined bootstrap fetch (status + files + tasks in one round trip). | `GET /api/sessions/{id}/status` + `/files` + `/tasks` |
| `session/messages_page` | Paginated chat history scroll. | `GET /api/sessions/{id}/messages` |
| `session/status.get` | Status-pill poller; standalone (not folded into snapshot) for periodic polling. | `GET /api/sessions/{id}/status` |
| `session/files.list` | Files panel listing. | `GET /api/sessions/{id}/files` |
| `session/tasks.list` | Background-tasks panel listing. | `GET /api/sessions/{id}/tasks` |
| `session/workspace.get` | Workspace-contract panel. | `GET /api/sessions/{id}/workspace-contract` |
| `session/title.set` | Manual session-title rename. | `PATCH /api/sessions/{id}/title` |
| `session/delete` | Session deletion. | `DELETE /api/sessions/{id}` |
| `system/status.get` | Agent/server status. Distinct from `auth/status` (which stays REST). | `GET /api/status` |
| `content/list` | Content-gallery listing. | `GET /api/my/content` |
| `content/delete` | Single-content deletion. | `DELETE /api/my/content/{id}` |
| `content/bulk_delete` | Bulk-content deletion. | `POST /api/my/content/bulk-delete` |

Per-method request and response shapes are documented in
`api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md` § 7 ("M12 Phase D: Auxiliary
RPC"). Request/response Rust types live in `crates/octos-core/src/ui_protocol.rs`
(`SessionListParams`/`SessionListResult`, …, `ContentBulkDeleteParams`/
`ContentBulkDeleteResult`). Implementation dispatchers live in
`crates/octos-cli/src/api/ui_protocol.rs::handle_session_list` and siblings.

### Per-method error envelopes

Every method uses the standard JSON-RPC 2.0 error envelope documented in
`octos-core/src/ui_protocol.rs`. The Phase D dispatchers map REST status codes
to typed errors with `data.kind` keys so clients can branch on the kind, not
the HTTP status:

- `unknown_session` (`-32100`) — REST 404 on a session-scoped method
  (`session/snapshot`, `session/messages_page`, `session/status.get`,
  `session/files.list`, `session/tasks.list`, `session/workspace.get`,
  `session/title.set`, `session/delete`). `data.session_id` carries the
  addressed id so clients can reconcile against their session table.
- `resource_not_found` (`-32170`) — REST 404 on a non-session method
  (`content/delete` against a missing content id, `content/list` /
  `content/bulk_delete` collection endpoints). `data.resource_type` ("content")
  and `data.identifier` echo the addressed id.
- `invalid_params` (`-32602`) — schema validation failure, including the
  `content/bulk_delete` over-cap guard (`data.max_ids`, `data.requested_ids`)
  and the `session/title.set` empty/over-length checks
  (`SESSION_TITLE_SET_MAX_CHARS`).
- `runtime_not_ready` (`-32???`) — REST 503 (handler not configured;
  gateway-proxied methods on a standalone server).
- `auth_unavailable` (`-32120`) — content methods called without a usable
  identity. The WS connection is also closed with code `1008 auth_expired` so
  the client's `crew:auth_expired` flow can clear the token and route to
  `/login`. See `close_ws_with_code` in `crates/octos-cli/src/api/ui_protocol.rs`.
- `method_not_supported` (`-32004`) — capability not negotiated.
- `internal_error` (`-32603`) — REST 5xx other than 503; non-JSON REST body.

The dispatcher additionally surfaces `rest_status` (REST status code) and an
optional `detail` field inside `data` so panels can render the REST handler's
human-readable error text. `detail` is capped at 2 KiB so error frames stay
small.

### Backward compatibility

- REST endpoints remained live through D-1 → D-4. Clients that did not
  negotiate `auxiliary.rest_to_ws.v1` continued to receive identical REST
  responses byte-for-byte (the WS dispatchers invoke the same REST handler
  functions internally).
- D-5 retired the twelve direct-replacement REST routes. Clients that have not
  upgraded to the WS surface receive 404 from the legacy URLs after D-5.
  `/api/auth/*`, `GET /api/my/profile`, and all BLOB endpoints are unchanged.
- Result body wrappers (e.g. `SessionListResult.sessions` wraps the REST array)
  are intentional: they preserve the WS JSON-RPC envelope's object-result
  convention and let future fields (`has_more`, `next_offset` on
  `session/messages_page`) be added without breaking the wire shape.
- `session/snapshot` is net-new (no REST analogue) — it is the consolidation
  helper that lets a panel bootstrap a session without three round trips. The
  three constituent methods (`session/status.get`, `session/files.list`,
  `session/tasks.list`) remain callable for fine-grained pollers and cache
  invalidation.

## Consequences

- The web data plane has one transport (WS UI Protocol v1) plus two
  carve-outs (AUTH REST, BLOB REST). A 401 on an auxiliary endpoint no longer
  detonates the session; it surfaces as a typed RPC error on a connected,
  authenticated WS socket.
- Clients that historically used REST for the thirteen surfaces above MUST
  negotiate `auxiliary.rest_to_ws.v1` to access the new wire. The `octos-tui`
  client speaks UI Protocol v1 natively and consumes the new methods as soon
  as the gate is set in its capability handshake.
- The capability is strictly opt-in: even with the header absent, the gate
  fires for these methods, so a client that has not been updated to declare
  the feature falls back cleanly (after D-5, that means the REST URL itself
  returns 404).
- Future REST additions on the auxiliary surface are explicitly out of scope.
  New session/content controls must land as WS RPC methods (gated by their
  own capability if they are not part of the core surface).

## Migration plan (as shipped)

Five phases. Each shipped as an independent PR. The server-side phases were
additive and reversible; the client cutover was gated by a feature flag that
defaulted to OFF until soak passed.

### D-1 — Server: add WS frames (additive)

Shipped as `#912`. Added the thirteen `UiCommand` variants in
`crates/octos-cli/src/api/ui_protocol.rs`, their typed
params/results in `crates/octos-core/src/ui_protocol.rs`, and the
`UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1` capability flag. Each WS
dispatcher delegates to the corresponding REST handler function so business
logic stays single-sourced. Codex review on the original PR landed as a
follow-up fixup (`5b982203`) that split session-scoped 404 from generic 404,
added the `RESOURCE_NOT_FOUND` slot, capped `content/bulk_delete` ids at
`CONTENT_BULK_DELETE_MAX_IDS`, and tightened the empty-title /
over-length-title validation.

### D-2 — Client: WS bridge methods (additive, behind flag)

Shipped in `octos-web` behind the `aux_rest_to_ws_v1` feature flag (default
OFF). Added typed wrappers in `octos-web/src/runtime/ui-protocol-bridge.ts`
that mirror the existing `request<T>()` private-method pattern.

### D-3 — Client: panel-by-panel cutover

Shipped one panel per PR for blast-radius control: status pill → sidebar list →
right-rail snapshot (the consolidating `session/snapshot` call) → messages
history scroll → workspace contract panel → content panel → title editor.

### D-4 — Default flag ON; tighten 401 reaper

Flipped the flag default to ON in `octos-web/src/lib/feature-flags.ts` after
the fleet soaked clean. Narrowed `octos-web/src/api/client.ts:128-136` to fire
only for `/api/auth/*`. Removed the duplicate reaper in
`octos-web/src/api/chat.ts:45-52`.

### D-5 — Retire REST endpoints (cleanup)

Shipped as `#914` plus the `cleanup/m12-phase-d5-retire-rest-routes` follow-up
commits (`161bbf36`, `36000807`). Retired routes:

- `/api/sessions` family (list / get / delete / title / messages / status /
  files / tasks / workspace-contract)
- `/api/my/content` family (list / delete / bulk-delete)
- `/api/status`

BLOB endpoints (`/api/upload`, `/api/site-files/upload`, `/api/files/*`,
`/api/my/content/{id}/thumbnail|body`, `/api/site-preview/*`,
`/api/preview/*`) and AUTH endpoints (`/api/auth/*`, `GET /api/my/profile`)
were **not** retired and stay REST.

## Acceptance criteria (closed)

1. `git grep -E "/api/sessions|/api/status|/api/my/content" octos-web/src`
   returns ZERO matches in non-test, non-BLOB files.
2. The 401 reaper in `octos-web/src/api/client.ts` triggers only on paths
   starting with `/api/auth/`. Unit test asserts this.
3. The mini5 incident reproduction passes: with a misconfigured global agent
   that 401s the data plane during bootstrap, the user stays logged in and
   the WS chat continues working.
4. Server emits `auxiliary.rest_to_ws.v1` in `session/open` capabilities;
   clients gate calls on this feature.
5. Soak gate: marathon-thirty-messages + thread-interleave + overflow-stress
   + content-grid pass 9/9 on mini1, mini2, mini3, mini5 with the flag
   default ON.
6. The thirteen retired REST routes referenced in D-5 return `404`
   uniformly. Existing healthchecks and curl probes that don't use them keep
   passing.

## Out of scope

- **Auth flow.** `/api/auth/*` stays REST. PKCE/OTP/session token exchange is
  the right shape for HTTP, and decoupling auth from the data plane is half
  the point of this ADR.
- **File blob endpoints.** `/api/files/*`, `/api/upload`,
  `/api/site-files/upload`, `/api/my/content/{id}/thumbnail`,
  `/api/my/content/{id}/body`, `/api/site-preview/*`, `/api/preview/*` stay
  REST. Multi-MiB bodies and direct `<img>` / `<video>` sourcing belong on
  HTTP.
- **Admin SPA.** `/api/admin/*` is consumed by a separate SPA, not the user
  web app, and is not driven by the 401-reaper code path this ADR fixes.
  Migrating the admin surface is a separate decision tracked elsewhere.
- **Server-side session TTL, refresh, and rotation logic.** The existing
  auth manager keeps owning that. This ADR only moved what the data plane
  sends over.
- **`octos-tui`.** The TUI already speaks UI Protocol v1 natively — no
  auxiliary REST surface to migrate. It consumes the new methods
  opportunistically but was not a release gate.
- **Slides / sites sub-apps.** `slides/api.ts`, `sites/api.ts` still call
  `/api/chat` (legacy) and `/api/files/list` (blob). Tracked separately.

## References

- M9-α Sole Transport ADR: `docs/M9-ALPHA-SOLE-TRANSPORT-ADR.md`
- M9-γ Server Projection ADR: `docs/M9-GAMMA-SERVER-PROJECTION-ADR.md`
- M11 Profile/Session Runtime ADR: `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md`
- Phase C cleanup PRs: `#855`, `#908`, `#909`
- Phase D landing PRs: `#912` (D-1), `#914` (D-5)
- Capability flag: `UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1` in
  `crates/octos-core/src/ui_protocol.rs`
- WS dispatchers: `crates/octos-cli/src/api/ui_protocol.rs::handle_session_list`
  and siblings
- Tracking issue: `#1330` (this backfill)
