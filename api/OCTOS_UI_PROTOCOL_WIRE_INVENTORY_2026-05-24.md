# Octos UI Protocol Wire Inventory

Status: current inventory for Octos issue #716
Date: 2026-05-24
Protocol: `octos-ui/v1alpha1`

This inventory reconciles the shipped AppUI/UI Protocol wire surface with the
spec and UPCR documents. The authoritative source remains code:

- commands: `crates/octos-core/src/ui_protocol.rs::UI_PROTOCOL_COMMAND_METHODS`,
  `UI_PROTOCOL_FIRST_SERVER_METHODS`, plus
  `crates/octos-cli/src/api/ui_protocol_transport.rs::APPUI_EXTRA_METHODS`
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
| `session/btw` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `user_question/respond` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `session/rollback` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `session/fork` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `monitor/create` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `monitor/list` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `monitor/pause` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `monitor/resume` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `monitor/delete` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `memory/overview` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `memory/entity` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `cron/list` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `cron/toggle` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `launch/resolve` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `smart_home/status.get` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `smart_home/device.list` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `smart_home/device.command` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `smart_home/camera.stream_start` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `smart_home/camera.stream_stop` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `profile/sub_providers/list` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `profile/sub_providers/upsert` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `profile/sub_providers/remove` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `snapshot/list` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `snapshot/restore` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `peer/prepare` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `peer/gather` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `turn/steer` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `session/compact` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `session/compact/mode/set` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |

## Notifications

| Method | Status |
|---|---|
| `session/open` | shipped open/resume notification |
| `turn/started` | shipped base notification |
| `turn/completed` | shipped base notification |
| `turn/error` | shipped base notification |
| `message/delta` | shipped base notification |
| `message/reasoning_delta` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `tool/started` | shipped base notification |
| `tool/progress` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `tool/completed` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `approval/requested` | shipped base notification, UPCR-2026-001 |
| `approval/auto_resolved` | shipped durable approval notification |
| `approval/decided` | shipped durable approval notification |
| `approval/cancelled` | shipped durable approval notification |
| `user_question/requested` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `task/updated` | shipped, UPCR-2026-004 |
| `plan/updated` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `task/output/delta` | shipped task output notification |
| `progress/updated` | shipped typed progress notification |
| `warning` | shipped base notification |
| `protocol/replay_lossy` | shipped backpressure/replay notification |
| `turn/spawn_complete` | shipped background completion notification |
| `file/attached` | shipped, UPCR-2026-014 |
| `visual/generating` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `visual/succeeded` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `visual/failed` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `voice/exit` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `skill/action/job/updated` | shipped AppUI extra notification, UPCR-2026-027 |
| `voice/audio_chunk` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `projection/envelope` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `session/event` | shipped, UPCR-2026-014 |
| `router/status` | shipped adaptive-router notification |
| `router/failover` | shipped adaptive-router notification |
| `queue/state` | known client-emitted queue notification |
| `agent/updated` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `agent/output/delta` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `agent/artifact/updated` | shipped, UPCR-2026-019 / UPCR-2026-021 |
| `session/goal/updated` | shipped, UPCR-2026-021 |
| `session/goal/cleared` | shipped, UPCR-2026-021 |
| `loop/updated` | shipped, UPCR-2026-021 |
| `loop/fired` | shipped, UPCR-2026-021 |
| `loop/completed` | shipped, UPCR-2026-021 |
| `monitor/fired` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `monitor/updated` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `monitor/expired` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `context/compaction_completed` | shipped M16 context lifecycle notification |
| `context/compaction_started` | shipped M16 context lifecycle notification |
| `context/normalization_reported` | shipped M16 context lifecycle notification |
| `peer/staged` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `peer/closed` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |
| `background/activity` | shipped; backfilled from code constants (spec-vs-impl audit 2026-08-21) |

> **Audit note (2026-08-21):** commands were backfilled from the code constants of
> truth (`UI_PROTOCOL_COMMAND_METHODS` in `crates/octos-core/src/ui_protocol.rs` plus
> `APPUI_EXTRA_METHODS`). Notifications above are now the full
> `UI_PROTOCOL_NOTIFICATION_METHODS` list. `message/persisted` (UPCR-2026-012) was
> **retired**: the ledger explicitly skips it (`ui_protocol_ledger.rs:1001`) and tests
> assert no new frame carries it; its successor is `projection/envelope`.

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
