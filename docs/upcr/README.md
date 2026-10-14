# UPCR Registry

Every `docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_NNN[_SLUG].md` file is
a UI Protocol change request cited elsewhere as `UPCR-2026-NNN` — in the
spec, the wire inventory, and Rust doc comments. This table is the answer to
"which file is the canonical UPCR-2026-NNN". Before #2423 no such index
existed and three numbers (022, 026, 027) were each claimed by two unrelated
records.

## Rules

- A new UPCR claims the **next free number** (check the table and the
  in-flight note below) and **adds its row here in the same PR**.
- Numbers are held by merge order, not by who drafted first: if two branches
  claim the same number, the one that lands on main first keeps it and the
  other renumbers to the next free number (#2730 renumbered 041 → 042 under
  this rule).
- Filenames are `OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_NNN_SHORT_SLUG.md`.
  `scripts/lint-upcr-registry.py` fails the `check` job on a duplicate number,
  a non-conforming filename, or the table disagreeing with the files (a file
  with no row, a row naming no file or a different file) — #2776 is the
  instance that turned the rows from a review convention into a lint rule.
- Rows carry each record's self-declared status verbatim. `Landed` is the
  landing PR, or the landing commit for records that predate the `(#N)`
  commit-subject convention. The table's File cell uses the `..._NNN_SLUG.md`
  shorthand for the full filename. All files live in `docs/` or below it.

| # | Title | Status | File | Landed |
|---|-------|--------|------|--------|
| 001 | Add typed approval request/response fields | accepted | `..._001_TYPED_APPROVAL.md` | #1228 |
| 002 | Add `session/open` pane snapshots | accepted | `..._002_PANE_SNAPSHOTS.md` | #1228 |
| 003 | Add per-session workspace `cwd` binding to `session/open` | accepted | `..._003_SESSION_WORKSPACE_CWD.md` | #1228 |
| 004 | Add `cancelled` variant to `TaskRuntimeState` | accepted | `..._004_TASK_RUNTIME_CANCELLED.md` | 73d548d3 |
| 005 | Add `task/list`, `task/cancel`, `task/restart_from_node` RPC commands | accepted | `..._005_TASK_CONTROL_RPCS.md` | 67f668b4 |
| 006 | Add `is_snapshot_projection: bool` to `task/output/read` result | accepted | `..._006_TASK_OUTPUT_SNAPSHOT_PROJECTION.md` | 5c37e9d9 |
| 007 | Emit `UiProtocolCapabilities` on `SessionOpened` so clients can discover server features in-band | accepted | `..._007_SESSION_OPEN_CAPABILITIES.md` | 899d9b56 |
| 008 | Add typed `reason`, `terminal_state`, and `ack_timeout` optional fields to `TurnInterruptResult` | accepted | `..._008_TURN_INTERRUPT_TYPED_FIELDS.md` | 7f879eba |
| 009 | Add `session/hydrate` RPC command for authoritative chat-state reload | accepted | `..._009_SESSION_HYDRATE.md` | #743 |
| 010 | Add `thread/graph/get` RPC command exposing the inferred thread graph | accepted | `..._010_THREAD_GRAPH_GET.md` | #743 |
| 011 | Add `turn/state/get` RPC command for deterministic turn lifecycle introspection | accepted | `..._011_TURN_STATE_GET.md` | #743 |
| 012 | Add `message/persisted` notification for durable message-row commit confirmation | accepted | `..._012_MESSAGE_PERSISTED.md` | #743 |
| 013 | — | never assigned | — | — |
| 014 | Add canonical M9-γ `Envelope` shape (`thread_id`, `seq`, `client_message_id?`, `payload`) for deterministic web client projection | accepted | `..._014_PROJECTION_ENVELOPE.md` | #856 |
| 015 | Extend `TurnStartParams` with optional `media: Vec<FileRef>`, `topic: Option<String>`, and `rewrite_for: Option<String>` to restore three chat-flow features stubbed out by the M9-α-5/α-6 atomic SSE delete | accepted | `..._015_TURN_START_MEDIA_TOPIC_REWRITE.md` | #860 |
| 016 | AppUI Stdio Transport | accepted | `..._016_STDIO_TRANSPORT.md` | #962 |
| 017 | AppUI Runtime, Auth, And LLM Profile Inspection | accepted | `..._017_RUNTIME_PROFILE_INSPECTION.md` | #962 |
| 018 | Local Solo Onboarding And Policy Inspection | accepted | `..._018_LOCAL_SOLO_ONBOARDING_AND_POLICY.md` | #962 |
| 019 | AppUI Backend-Owned Supervised Task Inspection | accepted (implemented) | `..._019_AGENT_SUPERVISION.md` | #1060 |
| 020 | AppUI Coding Tool Contract Inspection | proposed (implemented; pending formal acceptance) | `..._020_CODEX_TOOL_PARITY.md` | #1060 |
| 021 | Agent, Goal, And Loop Autonomy Inspection | accepted (implemented) | `..._021_AGENT_GOAL_LOOP_AUTONOMY.md` | #1060 |
| 022 | Wire Contracts Backfill For 8 Emitted Notifications | Accepted | `..._022_WIRE_CONTRACTS_BACKFILL.md` | #1334 |
| 023 | AppUI Structured User Question (AskUserQuestion) | proposed | `..._023_ASK_USER_QUESTION.md` | aa7a17d7 |
| 024 | Voice Rich-Output Visual Lifecycle | accepted | `..._024_VOICE_VISUAL_LIFECYCLE.md` | #1477 |
| 025 | Voice Exit Intent | accepted | `..._025_VOICE_EXIT.md` | eb1c3172 |
| 026 | Add `context/compaction_started` notification for in-progress compaction UX | accepted | `..._026_COMPACTION_STARTED.md` | #1558 |
| 027 | Resync the §6 "Envelope Model" method catalog and guard it against drift | accepted | `..._027_SECTION6_CATALOG_GUARD.md` | #1625 |
| 028 | Extend the §6 catalog guard to the `UI_PROTOCOL_FIRST_SERVER_METHODS` registry | accepted | `..._028_CATALOG_GUARD_FIRST_SERVER.md` | #1626 |
| 029 | Negotiate display-only OUP semantic context and cache diagnostics | accepted | `..._029_SEMANTIC_CONTEXT_CACHE_DIAGNOSTICS.md` | #2265 |
| 030 | Incomplete Turn Results | implemented; local process and bindings validation passed | `..._030_INCOMPLETE_TURN_RESULTS.md` | #2345 |
| 031 | Turn State "Not Running" Certainty | implemented | `..._031_TURN_STATE_NOT_RUNNING.md` | e8f38568 |
| 032 | Server Shutdown | implemented | `..._032_SERVER_SHUTDOWN.md` | 6289b774 |
| 033 | Advertise `turn/steer_dropped` and `session/orchestration` | implemented | `..._033_STEER_DROPPED_AND_ORCHESTRATION_NOTIFICATIONS.md` | #2542 |
| 034 | Host-Owned App Peers | implemented | `..._034_HOST_APP_PEERS.md` | cf2b4000 |
| 035 | Host-Registered Tools per App Peer | implemented | `..._035_PEER_HOST_TOOLS.md` | 544c061f |
| 036 | Host-Managed Serve | implemented | `..._036_HOST_MANAGED_SERVE.md` | c68d727d |
| 037 | Client Commands On `session/open` | implemented | `..._037_CLIENT_COMMANDS.md` | #2671 |
| 038 | Accepted Client Commands | implemented | `..._038_ACCEPTED_CLIENT_COMMANDS.md` | #2672 |
| 039 | Hydrated Tool Call Identity | implemented | `..._039_HYDRATED_TOOL_CALL_IDENTITY.md` | #2675 |
| 040 | Persistent Project Session History | implemented | `..._040_SESSION_HISTORY.md` | #2710 |
| 041 | peer route ownership on tool registration, unregister, and session tool-list set | implemented (merged as #2690) | `..._041_PEER_ROUTE_OWNERSHIP.md` | #2690 |
| 042 | Add record-only `session/append_message` history writes without an agent turn | implemented | `..._042_SESSION_APPEND_MESSAGE.md` | #2730 |
| 043 | Session Sandbox Narrowing | proposed | `..._043_SESSION_SANDBOX_NARROWING.md` | #1281 |
| 044 | Manifest-Declared Skill Actions | accepted | `..._044_SKILL_ACTIONS.md` | #1637 |
| 045 | Skill Action Background Jobs | accepted | `..._045_SKILL_ACTION_JOBS.md` | #1637 |
| 046 | Server-Owned Session Identity Binding for Raw Wire Ids | proposed | `..._046_SESSION_IDENTITY_BINDING.md` | #2162 |
| 048 | Cooperative Turn-Drain Delivery With A Budgeted Terminal Shed | implemented | `..._048_STALLED_CONSUMER_TERMINATION.md` | #2810 |

## Renumbering history

- **043** (Session Sandbox Narrowing, landed in #1281) was originally filed
  as **022** alongside the accepted Wire Contracts Backfill; renumbered
  2026-10 in #2423. The surviving 022 record's bare filename
  (`..._2026_022.md`) gained its `WIRE_CONTRACTS_BACKFILL` slug in the same
  change.
- **044** (Manifest-Declared Skill Actions) and **045** (Skill Action
  Background Jobs), both landed in #1637, were originally filed as **026**
  and **027**; renumbered 2026-10 in #2423 because #1558 (026) and #1625
  (027) had landed first.
- **013** was never assigned: no record for it exists anywhere in the
  repository's history.

In flight: **042** is currently claimed by two open PRs — #2730 (record-only
session history append) and #2761 (read-only workspace changes snapshots) —
**047** by #2801 (budgeted `monitor/expired` ledger admission), and **048**
by #2810 (cooperative turn-drain delivery). Whichever lands first keeps its
number and adds its row; the other renumbers per the rules above — the
`check` job's lint flags the collision once the loser's branch updates onto
main.
