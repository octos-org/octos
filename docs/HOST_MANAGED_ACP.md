# Host-managed ACP workers

`octos acp --host-managed` runs the existing Octos agent loop in a confined
process. The parent supplies model completions and tools over the same ACP
connection. This mode does not load Octos configuration, provider credentials,
plugins, bootstrap files, native tools, embeddings, or persistent conversation
history. Ordinary `octos acp` and embedded factories keep their existing behavior.
Host-managed turns default to 20 model-loop iterations; a positive
`--max-iterations` overrides that limit. In this mode, zero selects the bounded
default, while ordinary ACP retains its unlimited-zero convention.

The parent must launch the worker with
`octos_sandbox::host_managed_command(absolute_executable_path)`. The command
clears the environment, changes to `/`, and enforces OS confinement before the
executable starts. The worker then calls `confine_host_managed()` before creating
runtime threads or accepting input. Failure is fatal; there is no unconfined
fallback. The capability response reports this startup state, and is not a
cryptographic attestation of an arbitrary executable. Hosts must select a trusted
Octos binary and use the parent launcher, rather than trusting the response alone.

Only macOS and Linux with the required kernel confinement facilities are
supported. See `octos-sandbox` for the exact platform restrictions. One worker is
bound to one host compartment and accepts exactly one `session/new`. Session
loading and client-specified MCP servers are rejected. `cwd` does not grant file
access. Session state stays in memory, including the required episode-store
handle; all of it disappears when the worker exits.

## Execution mode boundary

Host-managed execution is a distinct, opt-in mode. It does not replace ordinary
ACP execution or its configured runtime. Upstream moved ordinary ACP turns into
the Octos UI Protocol (OUP) dispatcher in
[#2265](https://github.com/octos-org/octos/pull/2265). The ordinary adapter stays
on that dispatcher. Host-managed mode uses the current shared `octos_agent::Agent`
loop through a separate ACP adapter with private, memory-only session bookkeeping.

| Operation | Host-managed worker | Ordinary ACP on upstream after #2265 |
|---|---|---|
| Prompt, progress, cancellation | ACP handlers drive the confined agent loop and cancel outstanding broker waits. | The ACP adapter submits turns to the OUP runtime and translates its events. |
| Model inference, including compaction | `HostProvider` sends `_octos/host/model`; the parent authorizes and performs provider I/O. | The configured runtime resolves and invokes providers. |
| Tool discovery and execution | `_octos/host/tools/list` and `_octos/host/tools/call`; the parent rechecks every call. No native tool registry is populated. | The configured runtime resolves tools and applies its execution policy. |
| Credentials, configuration, history | No worker credential/configuration lookup; one memory-only session, with persistent session loading rejected. | Owned by the configured runtime and its stores. |

No host-managed model or tool request is forwarded to an ordinary local
dispatcher as a fallback. The ordinary OUP bootstrap resolves profiles, opens
stores, and constructs workspace tools, so it is not used inside the confined
worker. Sharing that bootstrap would require an explicit injection interface for
the host provider, broker-only tools, and memory-only session. A trusted parent
can choose its own implementation behind the broker; that does not grant the
worker direct access to it. Compartment identity and authorization remain with
the parent regardless of which turn orchestrator the worker uses.
Host-managed mode also retains the `session/notify` extension for host-triggered
turns; ordinary upstream ACP does not currently expose that extension.

This is enforced at two layers. The worker constructs only broker-backed
providers/tools and rejects persistent history and external MCP servers. The
parent launcher also enforces OS restrictions before executable entry: default-deny
Seatbelt on macOS, and bubblewrap namespaces plus a syscall allowlist on Linux,
followed by the worker's Landlock and stricter seccomp initialization. Missing or
partial confinement terminates startup. These restrictions are independent of
ordinary tool-sandbox configuration; they do not rely on tool names, prompts,
or a worker's self-reported `confined` flag. See the
[platform restrictions and escape probes](../crates/octos-sandbox/HOST_MANAGED.md).

## Negotiation

The parent advertises `initialize.clientCapabilities._meta["octos.hostManaged"]`:

```json
{
  "version": 1,
  "model": {
    "model_id": "chosen-model",
    "provider_name": "chosen-provider",
    "context_window": 128000,
    "max_output_tokens": 8192
  },
  "system_prompt": "The host's trusted assistant instructions."
}
```

No endpoint or credentials cross this boundary. The worker validates the version
and metadata, then advertises the same version with `confined: true` and a
platform mechanism name in `initialize.agentCapabilities._meta["octos.hostManaged"]`.
Missing negotiation, unsupported versions, and repeated initialization fail.

## Broker methods

Shared serde types and validation limits live in `octos_llm::host`. The ACP SDK
is not required to use those types. All three extension methods are worker-to-host
JSON-RPC requests:

| Method | Parameters | Result |
|---|---|---|
| `_octos/host/model` | `ModelRequest { messages, tools, config }` | `ChatResponse` |
| `_octos/host/tools/list` | `{}` | `ToolsListResponse { tools }` |
| `_octos/host/tools/call` | `ToolCallRequest { name, arguments }` | `ToolCallResponse { content, is_error }` |

Version 1 transports a bounded complete model response. The existing agent
streaming adapter preserves text, reasoning, tool metadata and usage; responses
are presented after the host completion arrives. Every inference path using the
agent provider, including a compaction provider call, uses the broker.

The worker refreshes tool definitions before each prompt and idle notification
turn. It keeps the existing agent if definitions are unchanged; a changed registry
preserves the session's history and memory. The host must resolve and authorize
each tool call against its current registry, even when a tool was advertised
earlier in the turn. Tools cannot return native file attachments or subprocess
instructions through this protocol.

Model payloads are bounded to 4096 messages and 256 tools; JSON-RPC frames are
bounded to 16 MiB by the host transport. The shared payload validator reserves
envelope space without making a second serialized buffer. Hosts must bound input
before allocating a complete frame, and validate request types and their own
provider-specific constraints before doing I/O.

## Host responsibilities

Compartment identity, taint, recipient consent, credentials, cancellation and
authority belong to the parent. Requests carry none of those identities or
labels. Bind them to the launched connection and recheck policy for every model
request and tool operation, including retries and tool feedback. Recheck before
delivering asynchronous results, and cancel pending broker work when a session
or its permissions are revoked. Worker cancellation interrupts outstanding
broker waits without an alternate provider or direct-network fallback.

The broker reader must continue processing while a model or tool request is
pending. Execute requests on a bounded worker queue rather than blocking the
ACP reader; otherwise replies, cancellation and notifications can deadlock.
All diagnostics containing private content belong to the parent's policy domain.

## Integration review boundaries

The host-managed adapter shares the agent loop, not the ordinary runtime's
authority or persistence. Review these boundaries when changing either path:

| Boundary to review | Current implementation | Invariant across conflict resolution |
|---|---|---|
| Process entry and confinement | `octos-cli/src/main.rs`, `commands/acp.rs`, `octos-sandbox/src/{lib,macos,linux}.rs` | Select host-managed mode before configuration, logging workers, runtime threads, or host input. Launcher or worker confinement failure must terminate, never select ordinary ACP/OUP execution. |
| Model and tool authority | `commands/acp/host_managed.rs`, `octos-llm/src/host.rs` | Retain broker-only provider/tool construction, bounded protocol messages, per-turn tool refresh, and cancellation. Compaction, retries, and notification turns must use the same parent authority. |
| Session and storage lifecycle | `commands/acp/host_managed.rs`, `octos-memory/src/store.rs` | Keep one compartment per process, RAM-only history/episodes, no session loading, and no external MCP servers. An OUP repository or episode-store default must not introduce disk access during construction. |

Paths in the table are under `crates/`, with `commands/` under `octos-cli/src/`.
The ordinary `commands/acp/oup.rs`, agent execution implementation, and provider
registry are unchanged by this feature. Host-managed providers and tools are
constructed directly; they do not depend on a registered provider, hosted profile,
or the older ACP branch's native-tool and MCP extensions.

Keep the protocol, confinement launcher, and in-memory store as independently
reviewable seams. When changing turn/session wiring, run
ordinary ACP regressions, the host-managed protocol/cancellation tests, native
Linux/macOS escape probes, and the real confined worker model/tool turn. Tests
on an earlier base do not establish that an updated integration is safe.
