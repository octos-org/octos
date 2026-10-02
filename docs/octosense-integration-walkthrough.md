# OctoSense integration: agents, sessions, tools and Tokio

Follow the implementation of host-owned app peers alongside the
[runtime architecture](ARCHITECTURE.md) and
[OUP specification](../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md).
OctoSense pins Octos in its root
[Cargo.toml](https://github.com/OctoSense-org/OctoSense/blob/main/Cargo.toml);
read the Octos source at that revision when debugging an integration.

## Runtime concepts

| Term | Meaning here | Rust counterpart |
| --- | --- | --- |
| Agent | Model-driven loop that reads context, calls tools and produces an answer | `octos_agent::Agent` and its async processing methods |
| System agent | Product role that coordinates the host and its apps | A host-selected session, profile and tool policy |
| App peer | Host-owned agent identity bound to one app and account scope | Durable peer binding, sessions and registered tools |
| Session | Addressed conversation, history and execution scope | `SessionKey`, transcript and context stores and runtime registries |
| Turn | One admitted request and its model and tool processing | Async orchestration and agent futures, interrupt state and IDs |
| Tool | Typed operation a model can request | Registry entry, arguments, permissions, execution and result |
| Tokio task | Independently scheduled Rust future | `tokio::spawn` and `JoinHandle` |
| OS thread | Runtime worker or dedicated blocking worker | Executes polls or blocking work |

The model proposes a tool call; Rust code checks and executes the operation.
An agent can be idle without a running turn, and one active turn can create several tasks. Persistent identity,
session lifetime and task lifetime are different.

## Find the layers

Read these files in order:

1. [OUP types](../crates/octos-core/src/ui_protocol.rs): commands, events,
   `SessionKey` references, turn IDs, `TurnOrigin` and peer notifications.
   OUP uses JSON-RPC requests, responses and notifications. `turn/start`
   acknowledges admission; subsequent message events and terminal state carry
   the answer and execution outcome.
2. [Local runtime bootstrap](../crates/octos-cli/src/runtime/local_oup.rs):
   builds runtime state from a configured profile and model provider.
3. [OUP transport and dispatcher](../crates/octos-cli/src/api/ui_protocol_transport.rs):
   WebSocket and stdio connection loops, session opening, admission, subscriptions,
   `run_standalone_turn`, event forwarding and shutdown.
4. [Peer app binding](../crates/octos-cli/src/peers/app_binding.rs): canonical
   workspace, memory namespace, peer token and request-context bindings.
5. [Host tools](../crates/octos-cli/src/peers/host_tools.rs) and
   [tool wrapper](../crates/octos-agent/src/tools/peer_host_tool.rs): registration,
   tool confinement, risk gating, outbound host calls and correlated replies.
6. [Agent loop](../crates/octos-agent/src/agent/loop_runner.rs): model requests,
   tool batches, history and context updates, convergence, budgets and output.
7. [Turn origins](../crates/octos-cli/src/peers/turn_origin.rs) and
   [shared history](../crates/octos-cli/src/peers/shared_history.rs): who is
   speaking and how human and system lanes see bounded context from each other.

## App peer creation and app data access

A host uses `peer/prepare` with a host binding. The durable binding fixes the
app's canonical `cwd` and `memory_namespace`; Octos creates a peer capability
token and stores its SHA-256 digest. The wire field is named `host_token`.
Control calls require that token; a session ID alone is not accepted. This peer
token is distinct from the server bearer credentials described below.
Opening the app peer under a different workspace is refused.

The host then uses `peer/tools/register` to register app operations and an
optional `generic_tools` list. Registration is versioned and replaced as a
whole and persisted in `host_tools.json`. `generic_tools` narrows the Octos
tools already available to the peer. The code checks the owning host connection
before allowing a host-driven turn to use the registered tools.

For example, OctoSense Calendar declares
[`calendar.events`](https://github.com/OctoSense-org/OctoSense/blob/main/apps/calendar/bundle/tools.json)
with a JSON input schema. The model supplies arguments. `HostRoutedTool` applies its risk and approval rules;
`TurnHostToolRouter` sends `peer/tool/call` to the registered host. The host's
service or app adapter accesses its database or account API and answers
`peer/tool/result`. The result is fed into the agent loop, which can explain
it to the requester. The installed app's declarations define its available
operations and schemas.

```mermaid
sequenceDiagram
    participant M as App peer turn
    participant K as Octos host tool router
    participant H as OctoSense host adapter
    participant D as App store or API
    M->>K: Typed tool request
    K->>K: Check registry, scope and risk
    K->>H: peer/tool/call with call ID
    H->>D: Execute allowed operation
    D-->>H: Data or error
    H-->>K: peer/tool/result with call ID
    K-->>M: Tool result
    M-->>H: Answer events and turn completion
```

The reply wait uses a Tokio `oneshot` channel, with timeout and interrupt
handling. The router can emit `peer/tool/cancel`; an interrupted write can have
an unknown outcome even after cancellation. Pending
calls are bounded and tool calls are audited in `tool_audit.jsonl`.

App records belong to host services; session transcripts preserve conversations;
the peer memory namespace scopes agent memory capture and retrieval. Database
access goes through a host adapter. Generic file tools depend on the effective
tool policy and workspace scope.

A host can register an allowed tool owned by another app using the declaration's
`app` owner field. Cross-app calls require that host authorization in addition
to `shareable` metadata. A peer reaches the system agent only through a
coordination tool the host registers. Follow OctoSense's
[`crates/app-peers`](https://github.com/OctoSense-org/OctoSense/tree/main/crates/app-peers)
and shell tool policies for the supported product routes.

## System agent and human conversations

For a host-owned peer, the system agent's `peer_send_input` is delivered as
`peer/input` to the host. The host validates and routes the input, then starts
the bound app turn. `host_tools::deliver_peer_input`, `start_peer_input_turn` and
`await_peer_input_answer` are the useful trace points. The answer must remain
correlated to that input and turn, separately from human conversation replies.

The human can also address the app peer through the app's UI or cards. The
host submits a turn with `origin.kind = person`; `app` identifies app-initiated
work. Only the owning connection may set origin on eligible sessions.
Octos reserves `system_agent` for its own
peer-input delivery. `turn_origin::label_prompt` adds speaker markers to the
prompt. `record_turn_origin` stores the origin by session and turn;
`is_person_turn` reads it when the dispatcher decides whether a pending question
should wake the system agent. A question from a human turn stays with the human.

There are two relevant conversation arrangements:

- A human and system agent can use the peer conversation with distinct turn
  origins, subject to that session's active-turn admission.
- For concurrent interaction the host opens a request context using
  `peer/context/open` with `share_history`. The peer's own session is the
  system lane; the context session is the human lane. Each has its own
  transcript, so simultaneous writers cannot corrupt tool-call pairing or
  compaction. Shared history is a bounded, labelled, read-only projection of
  recent user and assistant text and in-progress status; it omits tool rows. It is
  not copied into the other lane's durable transcript. A context does not see
  its sibling contexts through this mechanism.

A request context is a session, with a derived key such as
`<originator base>#peerctx-<slug>.<context_id>`, its own workspace beneath the
peer workspace and child memory namespace. It is not a new app peer. A closed
or unknown context is rejected at bootstrap and turn start. Mini-apps can also
use such contexts; whether a context shares history is an explicit host choice.

Answers reach the appropriate frontend as OUP events. System-input results
also feed the peer blackboard; origin and correlation identify the requester.

## Peer lifecycle

The owning host also controls the lifecycle through the dispatcher:

| Method | Effect |
| --- | --- |
| `peer/model/set` | Selects a configured model lane, or clears the override. The change applies at the next turn; profile defaults and credentials stay intact. Unknown lanes are refused. |
| `peer/context/close` | Permanently closes a request context and interrupts its active turn. The transcript and workspace remain on disk under host retention policy. Repeating close is idempotent. |
| `peer/purge` | Erases transcripts, memory, control files and Octos-owned workspace data; invalidates stale sessions. Tombstones and an audit record survive so a retry with the same token can return `already_purged`. This is separate from deleting the app's business data. |

Read `raw_peer_model_set` and `raw_peer_context_close` in the dispatcher,
[the purge handler](../crates/octos-cli/src/api/ui_protocol_peer_purge.rs)
and [purge records](../crates/octos-cli/src/peers/purge.rs). A busy purge returns
`peer_purge_busy` with the peer closed; the host retries to finish cleanup.
OctoSense selects the model lane during peer preparation, closes released
contexts and requests purge on account removal. Its broker does not currently
expose `peer/model/set` as an app operation.

## How this maps onto Tokio

A turn spans several tasks; the table lists their ownership by runtime path.
A session and its durable peer binding can survive after those tasks finish.

| Path | Owners, tasks and channels |
| --- | --- |
| OUP WebSocket | `ui_protocol_connection` splits the socket; a bounded Tokio `mpsc` queue feeds a spawned `WsConnection::writer_loop`. The async connection loop dispatches input and owns per-connection turn and forwarder handles. |
| OUP stdio, including the generic embedded adapter | `stdio_connection_with_io_policy` reads NDJSON asynchronously, but output uses a bounded standard-library `sync_channel` and dedicated OS thread, `octos-appui-stdio-writer`. That thread uses a current-thread Tokio runtime to drive the async writer; a `oneshot` reports completion. |
| Admitted OUP turn | `handle_turn_start_with_accept` creates an interrupt `mpsc` channel with capacity 1 and a `oneshot` start barrier, spawns orchestration, records ownership and admission, then allows work to start. The active-turn registry serializes conflicting work per session. |
| Agent processing within an OUP turn | `run_standalone_turn` spawns the `process_message_tracked_with_attachments` future and coordinates output, interrupts, approvals and terminal persistence. Progress forwarding, failover and heartbeat work can add tasks. Tool calls may add concurrency of their own. |
| Host app tool request | A pending call stores a reply `oneshot`; `tokio::select!` handles result, cancellation and timeout. Waiting is asynchronous; the app's actual operation runs wherever its host adapter schedules it. |
| CLI `chat --peers` | [`OupPeerHost`](../crates/octos-cli/src/commands/oup_peers.rs) stores `(CancellationToken, JoinHandle)` per presented peer. `serve_peer` opens and listens to a child OUP session and closes it on cancellation. This frontend task is additional to backend turn tasks. |
| Gateway session actor path | `ActorFactory` in [session_actor.rs](../crates/octos-cli/src/session_actor.rs) creates a bounded `ActorMessage` inbox and outbound proxy queue, spawns `actor.run()` and an outbound forwarder. Agent turns and stream forwarding can spawn further tasks. This is the gateway actor path, separate from the OUP dispatcher above. |

The shared-history registry uses a short mutex scope, released before awaiting
other work. Blocking SQLite cost-ledger operations use `spawn_blocking` in
[cost_ledger.rs](../crates/octos-agent/src/cost_ledger.rs); stdio output uses the
dedicated thread above. Native hosts keep waits for turns and tool results off
the UI thread.

## Hosting and running

To build the server, run from the Octos root with Rust, Cargo and native build
prerequisites installed:

```sh
cargo build --release -p octos-cli --no-default-features --features api --bin octos
target/release/octos --help
target/release/octos serve --help
```

The `api` feature enables OUP serving. Configure a model profile before starting
a turn. A standalone protocol client can launch
`target/release/octos serve --stdio` and exchange JSON frames on stdin and
stdout. The [OctoSense launcher](https://github.com/OctoSense-org/OctoSense/blob/main/crates/kernel/src/launch.rs)
selects these modes:

| Platform or setting | Launch and transport |
| --- | --- |
| Desktop, Talk to Octos off | Explicit program or `OCTOS_APP_CORE_BIN`; `serve --stdio` with the shell's data directory and configuration. |
| Android, Talk to Octos off | Packaged `liboctos.so` executable; `serve --stdio` with the kernel home. |
| Desktop or Android, Talk to Octos on | The launcher replaces `--stdio` with `--host 127.0.0.1 --host-managed`. Native and permitted external clients share that child's WebSocket server. |
| OpenHarmony | In-process `octos_cli::embedded::serve_io` over a duplex pipe. |
| iOS | Kernel unavailable in this implementation. |

Talk to Octos is an explicit, persisted host setting. Its host-managed launch
passes two lines on stdin: the server host bearer token, then the external
client bearer token. Stdin stays open as a process lifeline. The host connects
to `/api/ui-protocol/ws` with its bearer token; external clients have a restricted
method and session scope. In particular, they cannot open host-owned app-peer
sessions. See [host-managed serve](HOST_MANAGED_SERVE.md) and
[its access checks](../crates/octos-cli/src/api/host_managed.rs).

Keep the credentials distinct:

| Credential | Scope and source |
| --- | --- |
| Server host bearer token | First stdin line in host-managed mode; authenticates the shell's server connection. |
| External client bearer token | Second stdin line in host-managed mode; authenticates permitted external clients with restricted access. |
| Peer capability token (`host_token` field) | Minted by `peer/prepare` for one host-owned peer; its digest is stored in the peer binding and checked on peer control calls. |

For generic in-process embedding, [embedded.rs](../crates/octos-cli/src/embedded.rs)
exports `serve_io(home, reader, writer)` over caller-supplied I/O. It requires a
stored `_main` model profile and uses a private home
(`.octos` and `.config/octos` below it). The caller owns the Tokio runtime.
Its worker-stack requirement is 8 MiB:

```rust,ignore
let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .thread_stack_size(8 * 1024 * 1024)
    .build()?;
// Move the owned home path and I/O halves into a task on this runtime.
let serving = runtime.spawn(async move {
    octos_cli::embedded::serve_io(&home, reader, writer).await
});
```

Spawn the serving future onto that runtime: `block_on` would poll it on the
calling thread, whose stack may be smaller. Connection cleanup settles owned
work and releases the writer; peer persistence has a separate lifetime.
Follow the [OctoSense walkthrough](https://github.com/OctoSense-org/OctoSense/blob/docs/junior-architecture-walkthrough/docs/architecture-walkthrough.md)
for desktop, Home and ROM commands and the app-to-kernel bridge.

## Tests

[Host-tool protocol tests](../crates/octos-cli/src/api/ui_protocol_peer_host_tools_tests.rs)
exercise registration, connection ownership, dispatch and failure boundaries.
The [embedded module tests](../crates/octos-cli/src/embedded.rs) cover profile
requirements, session opening and pipe-close shutdown with 8 MiB stacks.
[Session actor tests](../crates/octos-cli/src/session_actor_tests.rs) cover the
separate gateway actor implementation. Select focused tests by name and feature
gate using the commands in [CLAUDE.md](../CLAUDE.md).
