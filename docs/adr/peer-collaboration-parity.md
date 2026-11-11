# Peer collaboration parity: machine-wide sessions and task-driven staged peers

- Date: 2026-11-10
- Status: Proposed; nothing implemented
- Scope: Octos server, OUP, and Octoscode TUI
- Builds on: [Automatic workspace peer teams](automatic-workspace-peer-teams.md)
  and [`FLEET-RUNTIME-ADR.md`](../FLEET-RUNTIME-ADR.md)

## Context

Octos has two ways for coding sessions to work together.

**Workspace teams** are automatic. Sessions opened in the same profile and
canonical folder share one server and join a team. Members list each other,
send direct and broadcast messages, and read each other's latest result. A
coordinator the user selects hands out work with `peer_assign`.

**Staged peers** are created by an agent. `peer_handoff` stages a peer with a
written brief and an optional fenced worktree, and the user's client opens it.
The originating session sends follow-up input, answers a blocked peer with
`peer_respond`, and reads results with `peer_gather`. A fleet that finishes
triggers one synthesis turn on the originator. Peers cannot create peers, and a
turn may stage at most four.

Claude Code ships two features that occupy the same ground, and comparing them
shows where each Octos mode stops short.

| Claude Code | Closest Octos mode | Where Octos stops short |
| --- | --- | --- |
| Cross-session messaging: on by default, any session lists and messages the user's other local sessions | Workspace teams | Covers one folder, not the machine. The receiving window shows a reply with no visible cause. No way to hold or refuse incoming messages. |
| Agent teams: opt-in, a lead spawns teammates that share a task list | Staged peers | No task list that helpers claim. Peers cannot message each other. No plan approval. No hooks for peer or task events. |

Octos is ahead in other respects and this record does not change them:
sessions live on the server and survive a closed window, the coordinator can be
changed, workspace teams have broadcast, staged peers can have their own
worktree, and an originator can unblock a parked peer.

Three facts constrain the design.

- Each folder has its own runtime directory, server and session store. Only
  the profile registry and model catalog are shared. No single record lists
  every session on the machine.
- Whether a session is running, idle or attached is held in its server's
  memory. A disk record cannot answer it.
- Every running shared server publishes an owner-readable discovery record in
  its runtime directory, and those directories share one parent. The list of
  live servers is therefore already discoverable.

## Decisions taken

Decided by the maintainer on 2026-11-10.

1. Keep one server per folder. Reach other folders through cross-server
   calls, not by moving every project onto one server.
2. Staged peers get a task list, although the fleet kernel also plans and
   dispatches work for goals.
3. A held incoming message is delivered automatically when the session next
   goes idle. The user does not release it by hand.
4. A member of another workspace is addressed by its agent id qualified by
   the workspace name.
5. A held message expires, and its sender is told when it lands.
6. The task list has size caps.

## Decision

Eight changes in two independent stages. Stage 1 extends workspace teams.
Stage 2 extends staged peers. Method and field names below are provisional
until each change has its own protocol change request.

### Stage 1: sessions across the machine

**1. The incoming message is visible.** When a member receives a peer
message, persist it in the recipient's history as an attributed record whose
source marks it as a workspace peer message and whose participant is the
sending agent. Clients render it as a one-line card naming the sender. It is
never rendered as something the user typed.

**2. Inbound control.** Each session has an inbound setting with a
profile-level default: `accept`, `hold` or `refuse`.

- `accept` is today's behavior. An idle recipient is woken and a busy one
  receives the message after its current turn.
- `hold` never starts a turn. The message stays queued and is delivered when
  the session next returns to idle after a turn its own user started. The
  sender's receipt says `held`.
- A held message expires after seven days, the lifetime Octos already gives
  loops. An expired message is dropped and is never delivered late.
- The sender is told when a held message lands and when it expires. The
  notice is an attributed record in the sender's history and a client
  notification. It never starts a turn, so it cannot begin an
  acknowledgement loop.
- `refuse` rejects at admission. The sender's receipt says `refused` and
  nothing is queued.

**3. Machine roster.** A server can list the members of sibling servers.

- It enumerates discovery records in the sibling runtime directories and
  applies the validation a client applies today before any credential is
  sent: loopback endpoint, matching runtime directory, known record version
  and protocol, then identity verification over the connection.
- Each server answers a read-only roster request that needs no opened
  session. A row carries the instance identity, workspace, agent id, role,
  running or idle, and attached or detached.
- Each row has a qualified name: the agent id, then the workspace name, as
  in `workspace-2@octoscode`. The workspace name is the last component of the
  canonical workspace path. When two workspaces share that name, the roster
  appends a short suffix taken from the instance identity so that every name
  is unique. Agents and `/agents` address a member by the name the roster
  printed.
- The roster is limited to the caller's profile.
- The discovery record gains a second credential that is valid only for the
  roster, delivery and delivery-notice requests. Cross-server calls use it
  and never the full client credential.
- `peer_list` gains a machine scope and `/agents` gains an "other workspaces"
  section. Launches with an explicit data directory have no siblings and keep
  folder scope.

**4. Cross-server messages.** A session can send a plain message to a member
shown in the machine roster.

- The sender's server calls a delivery request on the recipient's server with
  the narrow credential. The recipient's server enqueues it on the existing
  durable continuation path with an envelope that names the sending workspace
  and agent.
- The recipient is addressed by its qualified roster name. A name that is
  unknown or no longer unique is refused, and the sender is told to list
  again.
- Occurrence ids, persistence before acknowledgement, per-recipient receipts
  and the inbound setting apply unchanged.
- Landing and expiry notices for held messages travel back to the sender's
  server with the same narrow credential.
- A recipient whose workspace is not established in the running server is
  refused with the existing "reopen the session" reason.
- Only plain messages cross servers. Coordinator selection, `peer_assign` and
  result gathering stay inside one workspace team.

### Stage 2: staged peers as a team

**5. Plan approval.** `peer_handoff` accepts a plan-first option. The peer
starts under the read-only permission profile and submits its plan as a parked
question with two fixed answers, approve or revise. The originator answers
with `peer_respond`, or the user answers from the client as with any parked
prompt. On approval the peer returns to the profile it would otherwise have
had. An originator can never raise a peer above its own profile.

**6. Sibling messages.** A peer may send a message to another peer staged by
the same originator. Authorization widens from "the originator" to "the same
originator". The message is delivered as attributed agent input using the
workspace peer envelope, not as verbatim operator input. A per-turn cap and
the no-acknowledgement rule apply. Host-owned app peers keep receiving input
through their host connection.

**7. Fleet task list.** An originating session owns one task list stored
beside its peers' briefs and results.

- A task has an id, title, detail, dependencies, acceptance notes, a status
  of pending, in progress or completed, and an owner. Field names follow the
  fleet kernel's plan task so the list can move onto the kernel later.
- Every write is a compare-and-set on the list revision through the existing
  atomic peer-file writer.
- The originator creates, edits and assigns tasks. A peer lists tasks, claims
  one that is unowned and whose dependencies are completed, and marks its own
  task completed.
- A list holds at most 200 tasks, the row cap `peer_list` already uses. A
  title is at most 200 bytes. A task's detail and acceptance notes together
  are at most 16 KiB, the cap on a team member's stored result. A task has at
  most 32 dependencies. A write over a limit is rejected with the limit
  named. Completed tasks count toward the total until the originator deletes
  them.
- The list records work. It does not dispatch it. Staging peers remains an
  explicit `peer_handoff`.
- Clients receive the list in a notification and Octoscode shows it in the
  peer dock.

**8. Hooks.** Three lifecycle events: peer idle, task created and task
completed. They follow the existing hook contract, in which exit 1 denies and
standard output adds context.

- A deny on peer idle sends the reason to the peer as its next input, at most
  three times in a row.
- A deny on task created rejects the task and returns the reason to the tool
  caller.
- A deny on task completed leaves the task in progress and returns the reason.

### Rules for every change

- A protocol change request and a capability flag come first. Clients call a
  new method only when the server advertises it, and Octoscode keeps working
  against an older backend.
- The database keeps one writer. No change starts a second owner or kills an
  unknown process.
- Team membership and task ownership grant no filesystem, tool, profile or
  approval privileges.
- Agent-generated messages are attributed to the sending session and are never
  presented as direct human instructions.
- No change may create an acknowledgement loop.

## Alternatives rejected

- One server per user for every folder: makes the roster trivial but puts all
  projects behind one process and one database. It remains available as an
  opt-in.
- Building the roster from files: cannot report running, idle or attached, and
  would list sessions whose server is not running.
- Reusing the full client credential between servers: a compromised or buggy
  server would hold full access to every sibling.
- Running staged-peer tasks on the fleet kernel now: the kernel dispatches
  stateless workers from a keeper, and its session worker is not built yet.
- Adding staged peers to the workspace team: breaks the originator boundary
  and the isolation of fenced worktrees.
- Pushing team snapshots to every member: updates are opt-in by listing after
  the polling and copy costs found in #2745.
- Copying Claude Code's hook exit codes: Octos already assigns exit 1 to deny
  and exit 2 to input modification.

## Acceptance

1. A recipient's history shows an attributed record for each delivered peer
   message, and the TUI renders it as a sender card.
2. `hold` never starts a turn and delivers after the next user-started turn.
   `refuse` queues nothing. Receipts report `held` and `refused`. A held
   message is dropped after seven days. The sender's history records the
   landing or the expiry, and neither notice starts a turn.
3. Two servers in different folders list each other's members for the same
   profile and omit other profiles. A stale, remote or mismatched record is
   skipped before any credential is sent. Every roster name is unique, also
   when two workspaces share a folder name.
4. The narrow credential is rejected by every method other than roster,
   delivery and delivery notice.
5. A cross-server message is delivered once for one occurrence id, survives a
   recipient-server restart while queued, and honors the inbound setting.
6. A plan-first peer cannot write until its plan is approved, and approval
   never exceeds the originator's permission profile.
7. A peer can message a sibling and cannot message a peer of another
   originator.
8. Two peers claiming the same task produce one owner. A task with
   incomplete dependencies cannot be claimed. A write beyond a task-list
   limit is rejected and names the limit.
9. Each hook fires once per event, and a repeated idle deny stops after three
   deliveries.
10. Existing workspace teams, staged peers, app-hosted peers, custom stdio and
    remote endpoints behave as before against old and new clients.

Tests follow the `should_<expected>_when_<condition>` convention and are
written before the implementation.

## Delivery order and limits

| # | Change | Repositories | Size | Depends on |
| --- | --- | --- | --- | --- |
| 1 | Incoming message card | octos, octoscode | Small | none |
| 2 | Inbound control | octos, octoscode | Small | none |
| 3 | Machine roster with narrow credential | octos, octoscode | Medium | none |
| 4 | Cross-server messages | octos, octoscode | Large | 3, security review |
| 5 | Plan approval | octos | Small | none |
| 6 | Sibling messages | octos | Medium | none |
| 7 | Fleet task list | octos, octoscode | Medium | none |
| 8 | Hooks | octos | Small | 7 |

The stages are independent and can proceed in parallel. Each Octos release
that carries a change needs a matching Octoscode backend pin.

Out of scope: live visibility of another agent's edits, file locking between
agents, split-pane launching, and anything that crosses machines.

## Open questions

None. The numeric limits in this record are starting values chosen to match
existing Octos limits: seven days for held messages, 200 tasks, 16 KiB of
task detail and 32 dependencies. Review may tune them.
