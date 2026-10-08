# Automatic workspace peer teams

- Date: 2026-10-08
- Status: Accepted; implemented in backend and TUI source, not released
- Scope: Octos server, OUP, and Octoscode TUI; shared endpoint available to other clients

## Context

Independent Octoscode launches currently start private stdio servers. Launches
in the same canonical directory resolve to the same runtime directory, so the
second server cannot acquire the database ownership lock. Existing peers have
durable sessions, a result blackboard, supervised tasks, and continuation-based
delivery, but creation provenance also determines who may send them work.

Users should get these collaboration features simply by opening another
Octoscode session in the same folder. They should not need to know how to spawn
a peer. The user must be able to inspect the team and choose its coordinator.

## Decision

### One server owns the runtime

Implicit local Octoscode launches discover or start a shared loopback OUP
WebSocket server for the existing resolved runtime directory. Keep the serve
lock and the database single-writer rule. A separate startup lock serializes
discovery/start, and an owner-readable endpoint record identifies the server.
Clients authenticate and verify the instance identity before attaching. A
stale record is not evidence that its port still belongs to Octos. Never kill
an unknown process or start a second database owner on an attachment failure.

Explicit remote endpoints and custom stdio commands retain their meanings.
The resolved launch `--cwd`, not an unrelated process directory, determines
local discovery. Closing a client detaches it; the server owns running tasks.
Reconnect restores session state and never resends an ambiguous user turn.
Native and web clients use the same OUP endpoint and server-side folder;
remote access does not synchronize files between machines.

### Automatic membership extends peers

The team key is the authenticated profile plus canonical server-side workspace
inside one runtime. Successfully opened coding sessions join idempotently.
Multiple connections to one session represent one member. Historical sessions
are not started just because their files exist. App-hosted peers retain their
host tool and approval boundary. Explicit worktree peers retain their isolation.

Persist member identities and leadership beside the existing peer state. Do
not rename independent sessions into `#peer-*`, replay their initial prompts,
or rewrite an existing peer's originator. Creation provenance and current team
leadership are separate facts. Membership grants no additional filesystem,
tool, profile, or approval privileges.

### Leadership belongs to a session, selected by the user

The first member is the initial coordinator; no extra model session is created.
`/agents` lists the current folder's members and their leader/member role and
status. `/agents leader <id>` transfers leadership. Changes use an expected
revision and one atomic server-side update; stale selections fail with a refresh instruction.
The selected leader survives reconnects and server restarts. Disconnecting a
UI does not elect a different leader while its session may still be working.
The user can transfer leadership when the selected session is unavailable.

The coordinator divides work, steers members, tracks results, and synthesizes
their replies using the existing peer tools and continuation scheduler. Each
session keeps its own conversation and user task. Leadership is coordination
authority, not permission to overwrite another user's instructions, grant tool
approvals, or delete that session. Old coordinator decisions must be checked
against current leadership when admitted.

### Messages use the existing execution machinery

Every member can discover and message other members. Extend `peer_list`,
`peer_send_input`, and `peer_gather` for workspace members while retaining the
existing staged-peer paths. Use the existing durable continuation scheduler
and per-session turn admission, rather than another agent runtime or broker.
Attribute messages to the sending session; agent-generated messages must not
be presented as direct human instructions. Idle recipients can be woken;
busy recipients receive a queued follow-up without a concurrent second turn.

Messages have stable occurrence IDs. A retry of the same occurrence is
deduplicated; separate identical messages remain distinct. Persist before
acknowledging. Delivery is not a promise that the recipient completed the work.
Broadcast excludes the sender and returns per-recipient receipts. Avoid
automatic acknowledgment loops. Results and blockers can return directly to
the coordinator through the same messaging path.

### User controls and protocol

Extend the existing `/agents` surface:

| Command | Behavior |
| --- | --- |
| `/agents` | List the current workspace team, including the current session |
| `/agents leader <id>` | Select the coordinator using the observed revision |
| `/agents message <id> <text>` | Send a direct message |
| `/agents broadcast <text>` | Send to all other members |

Existing subagent status/output/artifact commands remain available. OUP owns
membership, authorization, leadership and delivery. Clients consume the same
capability-advertised methods; they do not reconstruct teams from titles or
assume a new socket is a new agent. Older servers retain the existing `/agents`
behavior and do not receive unsupported team methods.

## Alternatives rejected

- Removing the database lock: permits competing owners of single-process stores.
- A database per window: fragments history and does not provide coordination.
- Requiring explicit peer creation or team join: misses the default-on workflow.
- A second agent framework or message broker: duplicates existing peers and queues.
- Moving every new session into a worktree: changes the requested same-folder behavior.
- Automatically replacing a disconnected coordinator: mistakes UI presence for liveness.

## Acceptance

1. Simultaneous local launches in one folder converge on one server and create
   distinct sessions; launch directory aliases canonicalize to the same team.
2. `/agents` sees all joined members. Reopening a session creates no duplicate.
3. Leader transfer is atomic, survives restart, and rejects stale revisions.
4. Members send direct and broadcast messages; busy recipients are serialized,
   and retries cannot enqueue the same occurrence twice.
5. Another profile or workspace cannot address a team's members.
6. Disconnecting one client does not stop another session or its server.
7. Existing staged peers, subagents, custom stdio and remote endpoints continue
   to work. App-hosted peer boundaries remain enforced.
8. Changes include protocol contracts and meaningful regression tests; record
   actual validation and remaining limits here before marking implemented.

## Implementation evidence

Implemented on `feat/workspace-peer-teams` in Octos and Octoscode. The OUP
contract is recorded in
[`OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md`](../../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md).

- `serve --shared --solo` publishes a private, authenticated loopback endpoint.
  Octoscode uses a separate startup lock to converge on the server while the
  existing serve lock continues to protect the databases. Normal stdio is kept.
- `WorkspaceTeams` persists idempotent membership, leadership epochs and bounded
  latest results using the peer blackboard's atomic writer. User selection uses
  compare-and-set revisions. A previously elected coordinator's stale turn is
  fenced even if that session is elected again later.
- The existing `peer_list`, `peer_send_input`, `peer_gather`, continuation
  scheduler and per-session turn admission handle workspace peers. Coordinator
  turns get `peer_assign`; its callback checks current leadership at admission.
  Independently launched sessions remain roots, preserving their histories and
  permissions; they are not re-parented into a spawned subagent's lifecycle.
- Message occurrences are remembered from durable continuation records,
  including completed ones. Persistence failure rolls back admission. Messages
  remain attributed agent input. Members can send results/blockers back, and
  coordinators can gather the latest result; this is not a new fleet DAG runtime.
- `/agents` provides a live member/coordinator picker in English and Chinese.
  Clients that list a team receive coalesced full snapshots on changes.
  Separate local launches get fresh topics; reconnect/bootstrap retries retain
  the same session. Explicit resume remains explicit.

Validation on macOS (2026-10-08):

| Check | Result |
| --- | --- |
| Backend `cargo test -p octos-cli --no-default-features --features api --offline workspace_ --lib` | 93 passed, one existing ignored test |
| Backend shared/auth/publication regression group | 32 passed |
| Existing `peer_send_input` regression group | 7 passed |
| OUP advertised-method catalog guard | Passed |
| `serve_workspace_team_shared_identity_two_clients_message_and_disconnect` | Passed against a real server and local model stub: concurrent same-folder turns with the same TUI base identity, leader transfer, duplicate delivery receipt, recipient completion after sender disconnect |
| TUI `shared_discovery_two_cold_launches_start_one_real_server` | Explicitly run and passed with `OCTOS_WORKSPACE_TEST_BINARY` pointing at the matching backend; one real server owns both launches |
| TUI unit suite (including native-validation follow-up) | 2,112 passed; two explicitly ignored tests |
| TUI `cargo test --all-targets --offline --no-fail-fast` | Ran; six existing shell integration targets fail because this Mac lacks GNU `flock`, `stat -c`, and `realpath -m`; other targets completed |
| Clippy (`-D warnings`) | Backend/agent libraries and all TUI targets passed |
| Formatting and whitespace checks | Passed |

The six platform-dependent TUI targets are `olp_evo_harvest`, `olp_evo_replay`,
`olp_evo_retro`, `olp_evo_skeleton`, `olp_watch_board`, and `verify_environment`.
Linux CI remains necessary before a release. The real-server tests use only
fixture credentials and a local deterministic model; no paid model is needed.

## Native app and terminal validation follow-up

A real standalone OctosCode app and Octoscode TUI were launched against the
same shared server and canonical workspace on macOS. Makepad's authenticated
remote instrumentation drove the hidden native window, including composer
clicks, text entry and submission. A PTY drove the actual TUI and its `/agents`
picker. The backend used a local deterministic streaming model fixture.

Both conversations ran concurrently. The TUI discovered the native session,
transferred coordination to it, and delivered a peer message that appeared in
the native transcript. A native composer request invoked `peer_send_input` and
the recipient's result appeared in the TUI. Closing the app preserved the
coordinator and server; a message to its detached session completed, and
reopening the app restored both delivered results without adding a team member.
The app's sidebar listed both independent conversation histories. Twelve
assertions over the saved UI snapshots, terminal captures and model-request
timing passed. The restored transcript was scrolled through Makepad input to
verify the new detached-session result below the initially visible rows.

This exercise also exposed a missed TUI route: two simultaneous first-use
activation confirmations used the legacy `#coding` identity. Activation and
cross-profile menus now share the per-client launch topic already used by the
normal launch path; switching profiles retains that client's topic. Regression
coverage exercises the actual menu acceptance path, not just the session helper.
The rebuilt TUI passed the two-terminal activation reproduction with distinct
session IDs, and the native/TUI collaboration checks were repeated successfully.
The fix is Octoscode commit `099a2bc`; all-target Clippy and formatting passed.
The full all-target test run completed with the same six macOS GNU-tool failures
listed above and no failed unit tests.

The installed native app has a separate path-alias defect: it rejects a fresh
`session/open` when the requested `/tmp/...` is returned canonically as
`/private/tmp/...`. The collaboration checks use the canonical path. This
native defect remains open; backend directory canonicalization is not changed.

## Delivery limits

This change does not publish or deploy a release. Workspace teams require a
matching backend that supports `--shared`. Implicit TUI launches probe that
capability and retain private stdio with a notice on older backends, including
the currently pinned auto-install release. Explicit transport choices remain
unchanged. Packaging must pin a matching backend release to enable teams for
new installations.
Native and web clients can use the OUP team API through the same server; their
own team-picker UI is not part of this implementation. Windows startup has not
been exercised here.

Team membership survives disconnect and does not start historical sessions on
its own. New messages after a restart require the recipient to reopen its
workspace first. Teams currently retain up to 1,024 joined session identities;
latest gathered results are limited to 16 KiB per member. Agents coordinate
file ownership through messages; concurrent conflicting file edits are not
automatically merged.

