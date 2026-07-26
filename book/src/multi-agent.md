# Multi-Agent Orchestration

octos can run more than one agent at a time in three distinct shapes. Pick by **who owns the work** and **when you need the result**:

| Model | Entry | Ownership | Result | Lifetime |
|-------|-------|-----------|--------|----------|
| **Sub-agent** | `spawn` tool | Child of the current turn | Returns **in this turn** (sync) or as a new inbound message (background) | Dies with the turn |
| **Peer** | `peer_handoff` tool | Sovereign session | Returns **asynchronously**, via files | Survives; closes on disconnect |
| **Pipeline** | `run_pipeline` tool | DOT-graph workflow | Structured multi-node result | Per run |

Sub-agents are function calls; peers are independent coworkers. The rest of this page focuses on **peers** — the durable, detached agents. For `spawn` sub-agent internals, see [Architecture → Sub-Agents & Peers](./architecture.md#sub-agents--peers).

## What a peer is

A **peer** is a sovereign session with its own durable brief, workspace, and lifecycle, running independently of the turn that created it. Unlike a sub-agent — a child of the current turn that returns its result inline — a peer:

- receives a **self-contained brief** and **cannot see the originating conversation**;
- runs its **own** `session/open` + `turn/start` lifecycle;
- **survives turn completion** — there is no per-turn auto-close; it closes only when its client disconnects;
- writes a durable **`result.md`** (plus versioned `result-N.md` and a `turns.txt` index) under `peers/<slug>/` on every turn, so its output is reviewable, diffable, and respawnable.

Use a peer when the work should live on its own — a parallel investigation, a long-running task, a second agent you check in on. Use a **sub-agent** (`spawn`) when *this* turn needs the result to keep reasoning.

## The peer tools

An agent works with peers through five tools that cover the full lifecycle — **create** (`peer_handoff`), **steer** (`peer_send_input`), **read** (`peer_gather`), **list** (`peer_list`), and **close** (`peer_close`). They are only available on the gateway/serve runtimes (not in a plain one-shot `octos chat`).

### `peer_handoff` — create a peer

Promotes work out of the current conversation into a new sovereign peer.

- **Arguments:** `brief` (required, ≤ 64 KB — the peer's entire context; it must be self-contained), `title` (optional, seeds the slug), `worktree` (optional bool — fence the peer in a git worktree on branch `peer/<slug>`).
- **Behavior:** stages the peer and returns immediately with a pointer to `peers/<slug>/result.md`. **Fire-and-forget** — you do *not* get the result back in this turn. Limited to **4 handoffs per turn**.

### `peer_send_input` — steer a running peer

Injects a follow-up turn into an already-open peer.

- **Arguments:** `slug` (required), `message` (required, ≤ 64 KB).
- **Behavior:** the message is delivered as the peer's **next user turn**, rendered verbatim (as if an operator typed it) and persisted as a real user message. Only the peer's **originator** may send input (see [Rails](#rails-authorization--limits)). Repeated sends carry a unique occurrence id, so distinct messages never collapse but a genuine retry still de-dupes.

### `peer_gather` — collect results

Reads the peer "blackboard" (fan-in).

- **Arguments:** `slugs` (optional array; omit to read every peer).
- **Behavior:** returns each named peer's brief and latest result as text; peers that have not finished are reported as *still running*. Read-only. This is the only cross-peer channel — peers do not share context in-band.

### `peer_list` — see what exists

A compact status index of your peers (the companion to `peer_gather`).

- **Arguments:** none — it always lists every peer you have staged.
- **Behavior:** returns one line per peer — slug, status (**running** / **done** / **closed**), last-updated time, turn count, and whether it has its own worktree. Use `peer_list` to see *what* exists and which peers have finished; use `peer_gather` to read a peer's actual output. Read-only.

### `peer_close` — retire a peer

Gracefully closes a running peer you created.

- **Arguments:** `slug` (required).
- **Behavior:** marks the peer **closed** (a durable marker) and evicts its live connection, so it receives no further input; `peer_list` and `peer_gather` then report it closed, and `peer_send_input` refuses it. **Only the peer's originator may close it.** This is a *graceful* retire — the peer finishes any in-flight turn; it does **not** abort a running turn — and its `result.md` stays readable via `peer_gather`.

## Creating and gathering from the client

Humans drive peers through two server methods, surfaced as `/peer` and `/gather` in octos-tui:

- **`peer/prepare`** — stage 1–8 peers as a fleet (all-or-nothing). Pure resource reservation; the client then opens each session and starts its first turn.
- **`peer/gather`** — the human-facing side of the blackboard; composes the peers' results into the caller's session.

## Lifecycle

1. **Stage** — reserve `peers/<slug>/` (atomic directory claim), optionally add a git worktree, write `brief.md`, and stamp an `originator` file recording which session owns the peer. Any failure rolls back cleanly.
2. **Notify** — a durable `peer/staged` event asks the client to open the peer session in the background. Because it is durable, a reconnect replays it; the client de-dupes if the session is already open.
3. **Open & track** — when the peer's session opens, its live connection is recorded in a process-global **peer wire registry** (`{profile}:peer:{slug}` → live session, latest-open-wins, capped at 8192 entries).
4. **Run** — the peer runs its own turns, writing `result.md` each time. It persists across turn completion.
5. **Close** — two ways. **Explicitly**, an agent calls `peer_close` (originator-only): it writes a durable `closed` marker and evicts the wire, so the peer receives no further input — a graceful retire in which an in-flight turn still finishes. **Implicitly**, on WebSocket disconnect the wire mapping is evicted (only if it still points at this session, so a concurrent reopen wins), and deleting the session purges its actor and inbox.

## Sending input: the delivery path

`peer_send_input` must reach a *running* peer that may be on a different connection or even a different process, so delivery is more than a function call:

1. **Authorize** — the caller must be the peer's recorded originator; a mismatch is rejected (fail-closed).
2. **Two paths, by process:**
   - In the **gateway**, the message is pushed straight to the peer actor's inbox (fast path).
   - In **serve**, that inbox lives in a different process, so delivery falls through to a **durable continuation queue**: the message is enqueued as a peer continuation (keyed by a unique occurrence id) and **persisted**. If the persist fails, the enqueue is rolled back and the tool returns an error rather than a false "sent."
3. **Drain** — a per-connection drainer runs about every **2 seconds**, backed by a connection-independent **global drain every 5 seconds** as a safety net when no client is attached. Continuations run only when the peer is idle.
4. **Freshness gate** — the injection dispatches only if the target is still the slug's *current* registered wire; otherwise it is re-queued.
5. **Dispatch** — delivered as the peer's next user turn (verbatim, persisted as a real user message).
6. **Re-home on reopen** — if the peer closed and reopened, stranded injections are moved onto the new wire (crash-safe: the new durable record is written *before* the old one is retired).
7. **Retry with a cap** — a message that fails to dispatch is re-queued, but **advanced behind newer work so it can never starve other messages**, and **capped at 5 attempts (~10 s)** — past the cap it is dropped and logged rather than retried forever.

## Delivery guarantees & known limitations

Peer input delivery is **best-effort, single-user**. In normal operation it behaves at-least-once, with de-dup collapsing retries; under adversarial races it can occasionally lose or (with a durable store) duplicate a message. Three limitations are documented and accepted:

- **Close-reopen race** — the freshness check is not atomic with dispatch, so a peer that closes and reopens in a narrow window can have one message delivered to the closing session (lost) or, on a crash mid-window, replayed on restart (duplicated).
- **Power-loss durability** — the durable queue flushes to the OS but does not `fsync`, so a hard power cut or kernel panic (not an ordinary process crash) can lose the most recently queued message. This is a store-wide property, not peer-specific.
- **Failed-tombstone leak** — if a disk write fails while re-homing a message on reopen, the old record can linger durably (re-dropped on each restart). It does not cause a duplicate in normal operation — only a small leak.

These are acceptable under the single-user model below; closing them fully would require cross-subsystem atomic locking or per-write `fsync`, disproportionate for a best-effort local channel.

## Rails, authorization & limits

- **Depth-1** — peers cannot `peer_handoff`, `peer_send_input`, or `peer_close` (those tools are not offered on peer sessions), so a peer cannot recursively spawn, inject, or retire peers. Peers *can* `peer_gather` and `peer_list` (read-only, no recursion hazard).
- **4 handoffs per turn**; fleets bounded to **1–8** peers.
- **Originator-only injection** — only the session that created a peer may send it input; enforced fail-closed against the peer's `originator` file.
- **Size caps** — 64 KB for both a brief and an injected message.
- **Single-user-per-profile threat model** — in serve, the authenticated identity *is* the profile, so a profile is one user's trust domain. The LLM cannot inject across sessions (its caller identity is captured server-side and it cannot open sessions); cross-*user* injection is blocked by profile scoping. The residual case — a user deliberately acting across their own sessions — is treated as the user exercising their own authority, accepted by design. A non-spoofable capability model is deferred until serve gains multi-user identities.
