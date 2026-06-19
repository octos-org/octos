# Robrix Integration Workstream

> Branch: `feat/robrix-integration` | Source: salvage of closed PR #345 | Created: 2026-06-12

## Background

PR #345 (`feat(matrix): bidirectional media support, bot routing & web chat fix`,
fork `ZhangHanDong/octos`, head `feat/matrix-media-and-fixes`) was closed unmerged.
Its diff (370 files, +110k) was polluted by repeated merges from main; the actual
work is ~10 commits across 6 features. This workstream re-implements the still-missing
features on top of current main, one small PR per phase. **No cherry-picks** — the PR
commits serve as reference specs only; code is rewritten against today's structure.

Reference commits (fetchable via `git fetch origin pull/345/head:pr-345-head`):

| Commit | Feature |
|--------|---------|
| `feaf9698` | Matrix bidirectional media |
| `83e45871` | app-card reply tools for Robrix-capable clients |
| `319d30f6` | mission_room app responses |
| `da673310` + `0f84b8cb` | `/allbots` broadcast + natural-language scheduling |
| `73313637` | config-driven approval flow |

## Gap analysis vs current main

Already on main (do NOT redo):
- Channel-side app metadata projection: `CONTENT_APP` / `CONTENT_ACTIONS` /
  `CONTENT_ACTION_RESPONSE` constants, outbound projection into event content,
  inbound `action_response` extraction into `InboundMessage.metadata`
  (`crates/octos-bus/src/matrix_channel.rs`).
- Mention routing fixes (boundary-checked `contains_exact_matrix_user_id_mention`),
  `profile_factory.rs`, MSC4357 streaming, BotFather commands
  (`/createbot` `/deletebot` `/listbots` `/bothelp`).
- `octos-bus/src/media.rs` download helper (reusable for Phase 2).
- Web-chat static fix: obsolete, dashboard rewritten in M9–M11.

Missing from main:
- Agent-side app-card tools (`send_app_card`, weather demo card) + wiring.
- Matrix media upload/download (outbound `media` ignored; inbound non-text dropped).
- `/allbots` broadcast routing.
- `/schedule` `/schedules` `/unschedule` NL scheduling (cron_tool NL parsers, tz handling).
- Config-driven approval flow (`octos-agent/src/approval.rs` + loop integration).

## Phases

### Phase 1 (P0) — app-card tool chain  ✅ DONE (54dcdf73)
The Robrix-facing core. Producer contract v1 (see `specs/task-agent-to-app-system.spec.md`
in the robrix2 repo): event content carries `msgtype:"m.text"`, fallback `body`,
`org.octos.app` `{type, version, initial_state, scope?, app_id?}`, optional
`org.octos.actions`, `org.octos.action_response`. Unknown `type` ⇒ client falls
back to plain body.

1. Port `SendAppCardTool` (incl. `mission_room`: requires `scope:"room"` + stable
   `app_id`; actions array `{id,label,style}`; action_response replies) — TDD.
2. Wire: `tools/mod.rs`, `ToolRegistry`, gateway per-session tool injection,
   `gateway_default.txt` prompt fragment.
3. E2E: assert event-content projection via existing `spawn_mock_homeserver` harness.
4. `show_weather_card` (846-line demo): deferred — re-evaluate as an app-skill.

### Phase 2 (P1) — bidirectional media  ✅ DONE (a063c54b)
- Inbound: accept `m.image|m.file|m.audio|m.video`, download `mxc://` via
  `/_matrix/media/v3/download` (reuse `media.rs`) into `with_media_dir`, attach to
  `InboundMessage.media`; degrade gracefully on failure.
- Outbound: `upload_media` via `/_matrix/media/v3/upload`, msgtype by MIME prefix,
  captions, multi-attachment.
- Check whether audio-attachment persistence fixups (PR `284c5193`/`0043a206`) still apply.

### Phase 3 (P1) — /allbots + NL scheduling  ✅ DONE
- `cron_tool.rs`: NL parser family (relative one-shot, interval, daily, weekly;
  CJK + English; local-wall-time→UTC; CJK-aware job naming). Pure functions, test-first.
- `matrix_channel.rs`: `/schedule|/schedules|/unschedule|/allbots` dispatch,
  `BotManager` trait + `schedule_bot_task`/`list_schedules`/`unschedule_bot_task`,
  `org.octos.broadcast_targets`, stale-binding skip, `MAX_ALLBOTS_TARGETS = 8`.
- Wire `matrix_integration.rs`; document in `book/` (channels, cli-reference, advanced).

### Phase 4 (P2) — approval flow  ✅ DONE
Decision recorded in
[docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md](../docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md)
(suspend-and-resume transport, semantics unified onto main's approval
vocabulary). Implemented per the ADR's 6-step sketch:
`octos-agent/src/approval.rs` (`HumanApprovalRules` model), conversation-loop
interception (`ConversationResponse.pending_approval`; background loops deny
instead of suspending), `approval_policy` config schema + per-profile
passthrough, session-actor bridge (card emit, pending store, expiry timer,
validate/consume/revalidate/execute, audit), Matrix projection of
`org.octos.approval_request`/`approval_response`. `approvals_audit` relocated
out of the api feature gate so gateway builds share the JSONL audit trail.
Hook exit-code-3 deferred as decided.

## Verification

```bash
cargo test -p octos-bus --features matrix
cargo test -p octos-agent send_app_card
cargo test -p octos-cli cron_tool        # Phase 3
cargo clippy --workspace
```

Manual: real Matrix homeserver + Robrix client rendering app cards; non-capable
client (Element) shows fallback body.
