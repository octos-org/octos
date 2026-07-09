# Octos UI Protocol Wire Inventory

Status: current inventory for Octos issue #716
Date: 2026-05-24
Protocol: `octos-ui/v1alpha1`

This inventory reconciles the shipped AppUI/UI Protocol wire surface with the
spec and UPCR documents. The authoritative source remains code:

- commands: `crates/octos-core/src/ui_protocol.rs::UI_PROTOCOL_COMMAND_METHODS`,
  `UI_PROTOCOL_FIRST_SERVER_METHODS`, plus
  `crates/octos-cli/src/api/ui_protocol.rs::APPUI_EXTRA_METHODS`
- notifications:
  `crates/octos-core/src/ui_protocol.rs::UI_PROTOCOL_NOTIFICATION_METHODS`
- executable route fixture:
  `e2e/fixtures/appui-conformance/m18-route-inventory.json`
- parity runner:
  `e2e/scripts/m18-appui-transport-parity-soak.mjs`

## Commands

| Method | Status |
|---|---|
| `client_hello` | shipped AppUI extra, stdio/websocket negotiation |
| `config/capabilities/list` | shipped AppUI extra, UPCR-2026-017 |
| `profile/local/create` | shipped, UPCR-2026-018 |
| `session/open` | shipped base method |
| `session/list` | shipped REST-to-WS method |
| `session/snapshot` | shipped REST-to-WS method |
| `session/messages_page` | shipped REST-to-WS method |
| `session/status.get` | shipped REST-to-WS method |
| `session/files.list` | shipped REST-to-WS method |
| `session/tasks.list` | shipped REST-to-WS method |
| `session/workspace.get` | shipped REST-to-WS method |
| `session/title.set` | shipped REST-to-WS method |
| `session/delete` | shipped REST-to-WS method |
| `session/status/read` | shipped AppUI extra, UPCR-2026-017 |
| `session/hydrate` | shipped, UPCR-2026-009 |
| `thread/graph/get` | shipped, UPCR-2026-010 |
| `turn/state/get` | shipped, UPCR-2026-011 |
| `turn/start` | shipped base method |
| `turn/interrupt` | shipped base method, UPCR-2026-008 typed fields |
| `approval/respond` | shipped base method, UPCR-2026-001 optional fields |
| `approval/scopes/list` | shipped, UPCR-2026-001 |
| `permission/profile/list` | shipped, UPCR-2026-018 |
| `permission/profile/set` | shipped, UPCR-2026-018 |
| `diff/preview/get` | shipped base method |
| `task/output/read` | shipped, UPCR-2026-006 |
| `task/list` | shipped, UPCR-2026-005 |
| `task/cancel` | shipped, UPCR-2026-005 |
| `task/restart_from_node` | shipped, UPCR-2026-005 |
| `task/artifact/list` | shipped, UPCR-2026-019 |
| `task/artifact/read` | shipped, UPCR-2026-019 |
| `agent/list` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `agent/status/read` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `agent/output/read` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `agent/artifact/list` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `agent/artifact/read` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `agent/interrupt` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `agent/close` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `session/goal/get` | shipped, UPCR-2026-021 |
| `session/goal/set` | shipped, UPCR-2026-021 |
| `session/goal/clear` | shipped, UPCR-2026-021 |
| `loop/create` | shipped, UPCR-2026-021 |
| `loop/list` | shipped, UPCR-2026-021 |
| `loop/delete` | shipped, UPCR-2026-021 |
| `loop/pause` | shipped, UPCR-2026-021 |
| `loop/resume` | shipped, UPCR-2026-021 |
| `loop/fire_now` | shipped, UPCR-2026-021 |
| `review/start` | shipped, UPCR-2026-019 |
| `system/status.get` | shipped REST-to-WS method |
| `content/list` | shipped REST-to-WS method; auth-bound unavailable over unauthenticated stdio |
| `content/delete` | shipped REST-to-WS method; auth-bound unavailable over unauthenticated stdio |
| `content/bulk_delete` | shipped REST-to-WS method; auth-bound unavailable over unauthenticated stdio |
| `router/set_mode` | shipped adaptive-router method |
| `router/get_metrics` | shipped adaptive-router method |
| `auth/status` | shipped AppUI extra, UPCR-2026-017 |
| `auth/send_code` | shipped AppUI extra, UPCR-2026-017 |
| `auth/verify` | shipped AppUI extra, UPCR-2026-017 |
| `auth/me` | shipped AppUI extra; auth-bound unavailable over unauthenticated stdio |
| `auth/logout` | shipped AppUI extra; auth-bound unavailable over unauthenticated stdio |
| `profile/llm/catalog` | shipped AppUI extra, UPCR-2026-017 |
| `profile/llm/list` | shipped AppUI extra, UPCR-2026-017 |
| `profile/llm/upsert` | shipped AppUI extra, UPCR-2026-017 |
| `profile/llm/select` | shipped AppUI extra, UPCR-2026-017 |
| `profile/llm/delete` | shipped AppUI extra, UPCR-2026-017 |
| `profile/llm/test` | shipped AppUI extra, UPCR-2026-017 |
| `profile/llm/fetch_models` | shipped AppUI extra, UPCR-2026-017 |
| `mcp/status/list` | shipped AppUI extra, UPCR-2026-017 |
| `tool/status/list` | shipped AppUI extra, UPCR-2026-017 / UPCR-2026-020 |
| `profile/skills/list` | shipped AppUI extra when profile store is configured |
| `profile/skills/registry/search` | shipped AppUI extra when profile store is configured |
| `profile/skills/install` | shipped AppUI extra when profile store is configured |
| `profile/skills/remove` | shipped AppUI extra when profile store is configured |
| `skill/action/list` | shipped AppUI extra, UPCR-2026-026 |
| `skill/action/invoke` | shipped AppUI extra, UPCR-2026-026 |
| `skill/action/job/list` | shipped AppUI extra, UPCR-2026-027 |
| `skill/action/job/read` | shipped AppUI extra, UPCR-2026-027 |
| `onboarding/workspace_probe` | shipped local-solo AppUI extra |

## Notifications

| Method | Status |
|---|---|
| `session/open` | shipped open/resume notification |
| `turn/started` | shipped base notification |
| `turn/completed` | shipped base notification |
| `turn/error` | shipped base notification |
| `message/delta` | shipped base notification |
| `tool/started` | shipped base notification |
| `tool/progress` | shipped base notification |
| `tool/completed` | shipped base notification |
| `approval/requested` | shipped base notification, UPCR-2026-001 |
| `approval/auto_resolved` | shipped durable approval notification |
| `approval/decided` | shipped durable approval notification |
| `approval/cancelled` | shipped durable approval notification |
| `task/updated` | shipped, UPCR-2026-004 |
| `task/output/delta` | shipped task output notification |
| `progress/updated` | shipped typed progress notification |
| `warning` | shipped base notification |
| `protocol/replay_lossy` | shipped backpressure/replay notification |
| `message/persisted` | shipped, UPCR-2026-012 |
| `turn/spawn_complete` | shipped background completion notification |
| `file/attached` | shipped, UPCR-2026-014 |
| `session/event` | shipped, UPCR-2026-014 |
| `router/status` | shipped adaptive-router notification |
| `router/failover` | shipped adaptive-router notification |
| `queue/state` | known client-emitted queue notification |
| `agent/updated` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `agent/output/delta` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `agent/artifact/updated` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `skill/action/job/updated` | shipped AppUI extra notification, UPCR-2026-027 |
| `session/goal/updated` | shipped, UPCR-2026-021 |
| `session/goal/cleared` | shipped, UPCR-2026-021 |
| `loop/updated` | shipped, UPCR-2026-021 |
| `loop/fired` | shipped, UPCR-2026-021 |
| `loop/completed` | shipped, UPCR-2026-021 |
| `context/compaction_completed` | shipped M16 context lifecycle notification |
| `context/normalization_reported` | shipped M16 context lifecycle notification |

## Reconciliation Decisions

- UPCR-2026-001, UPCR-2026-002, and UPCR-2026-003 are restored as accepted
  documents because the spec linked them and code already ships their surfaces.
- `approval/scopes/list`, `approval/auto_resolved`, `approval/decided`,
  `approval/cancelled`, `progress/updated`, and `protocol/replay_lossy` are
  documented as shipped wire-visible methods/notifications rather than
  internal implementation details.
- `onboarding/workspace_probe` is added to the executable route inventory
  because `APPUI_EXTRA_METHODS` advertises it for local-solo deployments.
- `auth/logout` and all `content/*` methods are recorded as auth-bound
  unavailable over unauthenticated stdio, matching
  `APPUI_STDIO_AUTH_BOUND_UNAVAILABLE_METHODS`.
