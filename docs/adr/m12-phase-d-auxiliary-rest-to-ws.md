# M12 Phase D — Auxiliary REST → WS UI Protocol v1

Date: 2026-05-12
Branch (target): `main`
Status: PROPOSED — follow-up to M9-α-5/α-6 (Phase C) chat-transport
cutover, scoped against the 2026-05-11 mini5 401-reaper incident.

## Context

### What Phase C did

M9-α-5/α-6 (PRs `#855`, `#908`, `#909`) deleted the SSE foreground
chat transport. The sole **chat** transport is now
`/api/ui-protocol/ws` (see `crates/octos-cli/src/api/router.rs:103-108`
and `M9-ALPHA-SOLE-TRANSPORT-ADR.md`). On the client, ingest goes
through `octos-web/src/runtime/ui-protocol-bridge.ts` exclusively.

### What Phase C did NOT do

Everything the web UI does **outside** the assistant streaming
lifecycle still goes over REST. Concretely, every endpoint in
`my_api` and the non-chat half of `chat_api` in
`crates/octos-cli/src/api/router.rs:103-263` is REST-only. The web
client calls them via the helper at
`octos-web/src/api/client.ts:102-151` (the `request<T>()` function).

That helper has a global 401/403 interceptor at lines 128-136:

```ts
if (!resp.ok) {
  if (resp.status === 401 || resp.status === 403) {
    clearToken();
    if (!window.location.pathname.endsWith("/login")) {
      window.location.href = "/login?redirect=" + encodeURIComponent(window.location.pathname);
    }
  }
  ...
}
```

`clearToken()` (`octos-web/src/api/client.ts:35-38`) removes BOTH
`octos_session_token` and `octos_auth_token` from localStorage and
hard-redirects to `/login`. The same kill switch is duplicated in
`octos-web/src/api/chat.ts:45-52` (upload path).

### The incident this ADR responds to

On 2026-05-11, a fresh OTP-authenticated user landed on a mini5 build
whose global agent was momentarily misconfigured. The browser
bootstrap pipeline runs ~6 REST calls in parallel:

- `GET /api/auth/me`
- `GET /api/my/profile`
- `GET /api/status`
- `GET /api/sessions`
- `GET /api/sessions/{just-created-id}/messages` (404, harmless)
- `GET /api/sessions/{just-created-id}/files`

Any one of those returning 401 — for ANY reason on ANY of the six
endpoints — triggers `clearToken()`. The user's freshly-stored OTP
token gets wiped. The login redirect creates an infinite loop:
authenticate → bootstrap → one of six 401s → wipe → login.

The WS chat session is independent of REST auth — but it reads its
bearer token from `localStorage`. So even the WS chat dies the next
time the bridge has to reconnect, because its credential just got
wiped.

### Why the fix is architectural, not a one-line guard

We could narrow the 401 reaper to specific paths. That works as a
hotfix and is in fact step 1 below. But it does not address the
underlying split: **two transports, two auth surfaces, one combined
kill switch**. Every new REST endpoint added against the auxiliary
panels (sessions/files/tasks/status/content) re-opens the same
failure mode. The structural fix is to finish the migration the M9
plan started: move auxiliary REST onto the same WS UI Protocol v1
that already carries chat. REST then survives only for **auth**
(where it actually belongs) and **blob I/O** (where HTTP fits the
shape).

## Decision

The **data plane** for the octos web client is WebSocket UI Protocol
v1 (`/api/ui-protocol/ws`). REST survives only for two carve-outs:

1. **AUTH** — `/api/auth/*` and the bootstrap helper `GET /api/my/profile`
   used to learn `selected_profile`. REST is correct here because:
   - OTP login establishes the bearer token used by every other
     transport.
   - Pre-session cookie/profile resolution must happen before any WS
     handshake can succeed.
   - These calls are scoped to a tiny, well-known prefix and can be
     guarded by an explicit auth-only 401 interceptor.

2. **BLOB** — `POST /api/upload`, `POST /api/site-files/upload`,
   `GET /api/files/{path}`, `GET /api/my/content/{id}/thumbnail`,
   `GET /api/files/list` (small JSON, but driven by the same
   blob-shaped use case). Multi-megabyte bodies belong on HTTP, not
   on the WS text-frame budget (`MAX_TEXT_FRAME_BYTES = 1 MiB` per
   `octos-core/src/ui_protocol.rs:32`).

Everything else — session list, snapshot, messages, files panel,
tasks panel, status, content panel, content delete — becomes a
JSON-RPC method on the existing WS connection. The same connection
already carries `session/open`, `session/hydrate`, `turn/start`,
`turn/interrupt`, `task/cancel`, `task/output/read`, etc. Adding
auxiliary methods is additive and does not introduce a second
transport.

After the client migration completes, the 401 reaper at
`octos-web/src/api/client.ts:128-136` collapses to:

```ts
if (resp.status === 401 && path.startsWith("/api/auth/")) {
  clearToken();
  // redirect ...
}
```

A 401 on auxiliary REST during the deprecation window becomes a
typed error surfaced by the panel that called it — not a session
detonation.

## Endpoint inventory

Sourced from `crates/octos-cli/src/api/router.rs` (server) and
`octos-web/src/api/*.ts` (client). One row per URL the web client
actually calls today.

| URL | Method | Current client caller | Category | Proposed WS frame |
| --- | --- | --- | --- | --- |
| `/api/auth/send-code` | POST | `api/auth.ts:9` | AUTH | — (stays REST) |
| `/api/auth/verify` | POST | `api/auth.ts:19` | AUTH | — (stays REST) |
| `/api/auth/me` | GET | `api/auth.ts:26` | AUTH | — (stays REST) |
| `/api/auth/status` | GET | `api/auth.ts:30` | AUTH | — (stays REST) |
| `/api/auth/logout` | POST | `api/auth.ts:34` | AUTH | — (stays REST) |
| `/api/my/profile` | GET | `api/client.ts:65` (bootstrap) | AUTH | — (stays REST; pre-WS bootstrap) |
| `/api/status` | GET | `api/sessions.ts:121` | MIGRATE | `system/status.get` |
| `/api/sessions` | GET | `api/sessions.ts:38` | MIGRATE | `session/list` |
| `/api/sessions/{id}` | DELETE | `api/sessions.ts:69` | MIGRATE | `session/delete` |
| `/api/sessions/{id}/messages` | GET | `api/sessions.ts:42` | MIGRATE | `session/messages_page` |
| `/api/sessions/{id}/status` | GET | `api/sessions.ts:75` | MIGRATE | `session/status.get` |
| `/api/sessions/{id}/files` | GET | `api/sessions.ts:93` | MIGRATE | `session/files.list` |
| `/api/sessions/{id}/tasks` | GET | `api/sessions.ts:107` | MIGRATE | `session/tasks.list` |
| `/api/sessions/{id}/workspace-contract` | GET | `api/sessions.ts:99` | MIGRATE | `session/workspace.get` |
| `/api/sessions/{id}/title` | PATCH | (server-side title flow today) | MIGRATE | `session/title.set` |
| `/api/tasks/{id}/cancel` | POST | (already covered by WS `task/cancel`) | DEPRECATE | retire REST after client cutover |
| `/api/tasks/{id}/restart-from-node` | POST | (already covered by WS `task/restart_from_node`) | DEPRECATE | retire REST after client cutover |
| `/api/my/content` | GET | `api/content.ts:51` | MIGRATE | `content/list` |
| `/api/my/content/{id}` | DELETE | `api/content.ts:86` | MIGRATE | `content/delete` |
| `/api/my/content/bulk-delete` | POST | `api/content.ts:90` | MIGRATE | `content/bulk_delete` |
| `/api/my/content/{id}/thumbnail` | GET | `api/content.ts:97` (URL only) | BLOB | — (stays REST; image source) |
| `/api/my/content/{id}/body` | GET | (download path) | BLOB | — (stays REST) |
| `/api/chat` | POST | `slides/components/slides-chat.tsx:45`, `sites/components/sites-chat.tsx:233` | DEPRECATE | already retired for main chat; sub-apps must move to `turn/start` (tracked by PR #909) |
| `/api/upload` | POST | `api/chat.ts:38` | BLOB | — (stays REST; multipart) |
| `/api/site-files/upload` | POST | `sites/api.ts:128` | BLOB | — (stays REST; multipart) |
| `/api/files/list` | GET | `store/file-store.ts:231`, `slides/api.ts:78`, `sites/api.ts:96` | BLOB | — (stays REST; serves directory listings for blob URLs) |
| `/api/files/{path}` | GET | `api/files.ts:5` (URL builder), `store/file-store.ts:156`, `slides/api.ts:132`, `sites/api.ts:148`, `components/file-delivery.tsx:45`, `components/media-panel.tsx:53`, `components/chat-thread.tsx:94`, `components/viewers/markdown-viewer.tsx:24`, `slides/components/authenticated-file-image.tsx:32` | BLOB | — (stays REST; binary download) |
| `/api/site-preview/...` | GET | served as raw HTML preview | BLOB | — (stays REST; HTML/asset preview) |
| `/api/preview/{profile}/{session}/{slug}/...` | GET | public site preview | BLOB | — (stays REST; public asset) |
| `/api/admin/*` | various | — (admin SPA only; not used by user web app) | OUT-OF-SCOPE | tracked separately |

**Twelve MIGRATE rows. Eight BLOB rows. Six AUTH rows. Three
DEPRECATE rows.** That sets the scope of Phase D precisely.

## Proposed WS frame surface

Frames follow the JSON-RPC 2.0 envelope already in use (see
`crates/octos-cli/src/api/ui_protocol.rs`). Method names below sit
alongside the existing `session/open`, `session/hydrate`,
`turn/start`, `task/output/read`, etc. They are all one-shot
request/response unless explicitly marked streaming. Error envelopes
reuse `octos-core/src/ui_protocol.rs` error codes — primarily
`UNKNOWN_SESSION (-32100)`, `INVALID_PARAMS (-32602)`, and
`INTERNAL_ERROR (-32603)`.

For every method below, the server MUST gate behind a capability
feature string negotiated at `session/open` time (see
`UI_PROTOCOL_KNOWN_FEATURES`). The proposed feature is
`auxiliary.rest_to_ws.v1`.

### `session/list`

- Request: `{}` (no params)
- Response: `{ sessions: SessionInfo[] }` — same shape as
  `GET /api/sessions` returns today (matches
  `octos-web/src/api/types.ts` `SessionInfo`).
- One-shot. Server may additively emit a `session/list/updated`
  notification when the list changes (post-Phase-D enhancement; not
  required for cutover).

### `session/snapshot`

Combined fetch for the sidebar-detail / right-rail bootstrap. One
round trip replaces today's three parallel REST calls.

- Request: `{ session_id: string, topic?: string }`
- Response:
  ```json
  {
    "status": { "active": bool, "has_deferred_files": bool, "has_bg_tasks": bool },
    "files": SessionFileInfo[],
    "tasks": BackgroundTaskInfo[]
  }
  ```
- One-shot. Reuses the same DTOs as the existing REST endpoints
  (`octos-web/src/api/sessions.ts:75-119`).

### `session/messages_page`

- Request:
  ```json
  {
    "session_id": string,
    "limit": number,    // default 500, max 1000
    "offset": number,   // default 0
    "since_seq"?: number,
    "topic"?: string
  }
  ```
- Response: `{ messages: MessageInfo[], has_more: bool, next_offset: number }`
- One-shot. NOTE: this complements `session/hydrate` — `hydrate` is
  the projection-envelope full snapshot; `messages_page` is the
  paginated history-scroll fetch the sidebar uses today.
- Error: `UNKNOWN_SESSION` returns `{ messages: [], has_more: false }`
  silently to match REST's current "404 → empty" client semantic
  (avoids the harmless-404 noise that triggered the incident).

### `session/status.get`

- Request: `{ session_id: string, topic?: string }`
- Response: `{ active: bool, has_deferred_files: bool, has_bg_tasks: bool }`
- One-shot. Folded into `session/snapshot` for bootstrap; this
  method exists for the periodic poller in the status pill.

### `session/files.list`

- Request: `{ session_id: string }`
- Response: `{ files: SessionFileInfo[] }`
- One-shot. Folded into `session/snapshot` for bootstrap. Server
  MAY additively push `session/files/updated` notifications later;
  not required for Phase D cutover.

### `session/tasks.list`

- Request: `{ session_id: string, topic?: string }`
- Response: `{ tasks: BackgroundTaskInfo[] }`
- One-shot. Folded into `session/snapshot` for bootstrap.

### `session/workspace.get`

- Request: `{ session_id: string }`
- Response: `{ contracts: SessionWorkspaceContractInfo[] }`
- One-shot. Same DTO as REST today
  (`octos-web/src/api/sessions.ts:24-36`).

### `session/title.set`

- Request: `{ session_id: string, title: string }`
- Response: `{ session_id: string, title: string }`
- One-shot. Server already emits a `session/title-updated`
  notification (UPCR-2026-016) on success — that notification
  remains the broadcast channel; this method is the imperative
  setter.

### `session/delete`

- Request: `{ session_id: string }`
- Response: `{}` (204-equivalent)
- One-shot. Server emits an existing list-changed notification or a
  new `session/deleted` notification (TBD in implementation issue).

### `system/status.get`

- Request: `{}` 
- Response: `{ version, model, provider, uptime_secs, agent_configured }`
  — same shape as `GET /api/status`.
- One-shot. Note: this is the **agent** status (different from
  `auth/status` which stays REST). Already partially redundant with
  the `session/open` capabilities envelope; keeping it as a method
  matches the existing client polling pattern without a refactor.

### `content/list`

- Request: `{ filters: ContentFilters }` — same fields as
  `octos-web/src/api/content.ts:25-34`.
- Response: `{ entries: ContentEntry[], total: number }`
- One-shot. Server MAY emit a `content/updated` notification later;
  not required for cutover.

### `content/delete` / `content/bulk_delete`

- `content/delete`: `{ id: string }` → `{}`
- `content/bulk_delete`: `{ ids: string[] }` → `{ deleted: number }`
- One-shot.

### Error envelope (all methods)

JSON-RPC error object per `octos-core/src/ui_protocol.rs`:

```json
{
  "code": -32100,
  "message": "unknown session: <id>",
  "data": { "session_id": "<id>" }
}
```

Specific code mapping:

- `UNKNOWN_SESSION (-32100)` — session id not found (replaces 404)
- `INVALID_PARAMS (-32602)` — schema validation failure (replaces 400)
- `METHOD_NOT_SUPPORTED (-32004)` — capability not advertised
- `INTERNAL_ERROR (-32603)` — server-side failure (replaces 500)

Critically: **there is no -32401 "auth failure"**. The WS
connection itself is authenticated at handshake. A surviving WS
connection is, by construction, authenticated; if the server
revokes the session mid-stream it closes the socket with a defined
close code, which the bridge surfaces as a `transport/closed`
event. The auxiliary frames therefore never have to deal with the
401 reaper case — that's the structural win.

## Migration plan

Five phases. Each ships as an independent PR. The server-side
phases are additive and reversible; the client cutover is gated by
a feature flag that defaults to OFF until soak passes.

### D-1 — Server: add WS frames (additive)

Server PR. Implement the 12 MIGRATE methods listed above as new
`UiCommand` variants in `crates/octos-cli/src/api/ui_protocol.rs`.
Reuse existing handlers in `crates/octos-cli/src/api/handlers.rs`
(extract their bodies into shared functions; REST handlers become
thin adapters that call the shared core).

- Add `UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1 = "auxiliary.rest_to_ws.v1"`
  to `octos-core/src/ui_protocol.rs:UI_PROTOCOL_KNOWN_FEATURES`.
- Capability is advertised by `session/open` only when the server
  has the new handlers wired.
- REST endpoints remain unchanged — no behavior break for older
  clients.
- Add golden tests for each new method against
  `crates/octos-cli/src/api/ui_protocol_ledger.rs`.

Gate: full `cargo test --workspace` green; new methods covered by
unit + integration tests; capability negotiation tests assert the
feature appears only when the server supports it.

### D-2 — Client: WS bridge methods (additive, behind flag)

Client PR. Add typed wrappers in
`octos-web/src/runtime/ui-protocol-bridge.ts` matching the existing
`request<T>()` private method pattern (`bridge.ts:1325`):

```ts
sessionList(): Promise<SessionInfo[]>
sessionSnapshot(args): Promise<SessionSnapshot>
sessionMessagesPage(args): Promise<MessagesPage>
...
```

Introduce a feature flag `aux_rest_to_ws_v1` (same pattern as
`chat_app_ui_v1`). When OFF, panels use the existing REST helpers
(unchanged). When ON, panels route through the bridge.

Gate: flag defaults OFF on this PR. Unit tests for each wrapper
mock the bridge.

### D-3 — Client: panel-by-panel cutover

Sequence (each is its own PR for blast-radius control):

1. Status pill (`getStatus` + `getSessionStatus`)
2. Sidebar list (`listSessions` + `deleteSession`)
3. Right-rail snapshot (combined `getSessionFiles` + `getSessionTasks` + `getSessionStatus`) → single `session/snapshot` call
4. Messages history-scroll (`getMessages`)
5. Workspace contract panel (`getSessionWorkspaceContract`)
6. Content panel (`fetchContent` + `deleteContent` + `bulkDeleteContent`)
7. Title editor (existing `PATCH /api/sessions/{id}/title` callers)

For each: flip the flag locally, run the relevant soak scenario
(`marathon-thirty-messages`, `thread-interleave`, `content-grid`),
ship. Flag stays OFF in default config until step 7 lands.

Gate per PR: the panel works identically with flag ON vs flag OFF
under the full soak. No new behavior visible to the user.

### D-4 — Default flag ON; tighten 401 reaper

After D-3.7 lands and the fleet has soaked clean for 48 hours:

- Flip `aux_rest_to_ws_v1` default to ON in
  `octos-web/src/lib/feature-flags.ts`.
- Rewrite `octos-web/src/api/client.ts:128-136` to:
  ```ts
  if (resp.status === 401 && path.startsWith("/api/auth/")) {
    clearToken();
    // redirect ...
  }
  ```
- Remove the duplicate reaper in `octos-web/src/api/chat.ts:45-52`
  (it only protects the upload path, which is BLOB and not subject
  to the data-plane 401 problem — but it should at least scope the
  check the same way).

Gate: re-run the original mini5 incident reproduction. Bootstrap
with a misconfigured global agent must NOT wipe the OTP token.

### D-5 — Retire REST endpoints (cleanup)

Server PR matching the `#908` / `#909` pattern. Once `git grep` in
both repos shows zero non-test callers of an endpoint, delete it.
Stagger across multiple PRs (one per group) so a regression bisects
cleanly:

- `/api/sessions` family (list/get/delete/title/messages/status/files/tasks/workspace-contract)
- `/api/my/content` family (list/delete/bulk-delete)
- `/api/status`
- `/api/tasks/{id}/cancel` + `/restart-from-node` (already covered by `task/cancel` + `task/restart_from_node` in WS)

BLOB endpoints (`/api/upload`, `/api/site-files/upload`,
`/api/files/*`, `/api/my/content/{id}/thumbnail|body`,
`/api/site-preview/*`, `/api/preview/*`) and AUTH endpoints
(`/api/auth/*`, `GET /api/my/profile`) are **not** retired. They
stay REST forever.

### Rollback

- D-1 → D-2: server-only PR; revert is a server-only PR.
- D-3.x: each panel cutover ships behind the flag with default OFF;
  rollback is flipping the flag back via a config push (no rebuild
  required at the page level).
- D-4: default-on flip is a one-line revert.
- D-5: retired REST endpoints can be restored by reverting that
  group's PR.

## Acceptance criteria

The migration is complete when:

1. `git grep -E "/api/sessions|/api/status|/api/my/content" octos-web/src`
   returns ZERO matches in non-test, non-BLOB files. (Acceptable
   matches: `/api/files`, `/api/upload`, `/api/site-files/upload`,
   `/api/auth/*`, `/api/my/profile`, `/api/my/content/{id}/thumbnail`,
   `/api/my/content/{id}/body`, `/api/preview/*`,
   `/api/site-preview/*`.)
2. The 401 reaper in `octos-web/src/api/client.ts` triggers only on
   paths starting with `/api/auth/`. A unit test in
   `octos-web/src/api/client.test.ts` asserts this.
3. The mini5 incident reproduction passes: with a misconfigured
   global agent that 401s the data plane during bootstrap, the user
   stays logged in and the WS chat continues working. (Add as
   `e2e/m12-phase-d-401-data-plane-no-logout.spec.ts`.)
4. Server emits `auxiliary.rest_to_ws.v1` in `session/open`
   capabilities; clients gate calls on this feature.
5. Soak gate: marathon-thirty-messages + thread-interleave +
   overflow-stress + content-grid pass 9/9 on mini1, mini2, mini3,
   mini5 with the flag default ON.
6. The 12 retired REST routes referenced in D-5 return `404`
   uniformly. Existing healthchecks and curl probes that don't use
   them keep passing.

## Out of scope

- **Auth flow.** `/api/auth/*` stays REST. PKCE/OTP/session token
  exchange is the right shape for HTTP, and decoupling auth from
  the data plane is half the point of this ADR.
- **File blob endpoints.** `/api/files/*`, `/api/upload`,
  `/api/site-files/upload`, `/api/my/content/{id}/thumbnail`,
  `/api/my/content/{id}/body`, `/api/site-preview/*`,
  `/api/preview/*` stay REST. Multi-MiB bodies and direct `<img>`
  / `<video>` sourcing belong on HTTP.
- **Admin SPA.** `/api/admin/*` is consumed by a separate SPA, not
  the user web app, and is not driven by the 401-reaper code path
  this ADR fixes. Migrating the admin surface is a separate
  decision tracked elsewhere.
- **Server-side session TTL, refresh, and rotation logic.** The
  existing auth manager keeps owning that. This ADR only moves
  what the data plane sends over.
- **`octos-tui`.** The TUI already speaks UI Protocol v1
  natively — no auxiliary REST surface to migrate. It will
  consume the new methods opportunistically but is not a release
  gate.
- **Slides / sites sub-apps.** `slides/api.ts`, `sites/api.ts` still
  call `/api/chat` (legacy) and `/api/files/list` (blob). PR #909
  already tracks the `/api/chat` cleanup; `/api/files/list` stays
  BLOB.

## Open questions

1. **Combined snapshot vs separate methods.** The proposed
   `session/snapshot` collapses three REST calls into one WS
   request. Useful for bootstrap; but the polling status pill
   only wants `status`. Decision: ship both. `snapshot` is the
   bootstrap helper; the individual methods exist for fine-grained
   pollers and for cache invalidation when one piece changes.
2. **Push vs pull for content list.** `content/list` is one-shot
   today. Pushing `content/updated` notifications on add/delete
   would let the gallery refresh without a client poll. Out of
   scope for Phase D cutover; reconsider in a follow-up.
3. **Frame size budget.** `MAX_TEXT_FRAME_BYTES = 1 MiB`. The
   largest auxiliary payload today is `session/messages_page` with
   `limit=500` — well under 1 MiB for typical messages, but a long
   conversation with embedded large `tool_call_result` payloads
   could exceed it. Mitigation: cap `limit` server-side at the
   value that yields ≤ 512 KiB; if exceeded, return a truncated
   page + `has_more=true` and let the client request the next
   slice. Tracked in the server implementation issue.
4. **Backpressure on bulk-delete responses.** `content/bulk_delete`
   with thousands of IDs returns only a count — the WS frame is
   tiny. No backpressure concern. Listed only for completeness.
5. **Two-transport coexistence during D-3.** During the panel-by-
   panel cutover, some panels use WS and some use REST against the
   same browser session. Both already share the same bearer token
   via localStorage. No new auth surface introduced; the 401
   reaper risk reduces monotonically as panels migrate.
6. **Test fleet.** `mini5` is reserved for coding-green soaking
   (per project memory). Phase D soak should run on mini1, mini2,
   mini3, and mini5 must be coordinated with whoever owns the
   coding-green slot at the time.

## References

- M9-α Sole Transport ADR: `docs/M9-ALPHA-SOLE-TRANSPORT-ADR.md`
- M9-γ Server Projection ADR: `docs/M9-GAMMA-SERVER-PROJECTION-ADR.md`
- M11 Profile/Session Runtime ADR: `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md`
- Phase C cleanup PRs: `octos#855`, `octos#908`, `octos#909`
- 401 reaper site: `octos-web/src/api/client.ts:128-136`
- REST router: `crates/octos-cli/src/api/router.rs:88-263`
- WS method registry: `crates/octos-core/src/ui_protocol.rs:642-707`
