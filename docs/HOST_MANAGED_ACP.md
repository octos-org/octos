# Host-managed ACP workers

`octos acp --host-managed` runs the existing Octos agent loop in a confined
process. The parent supplies model completions and tools over the same ACP
connection. This mode does not load Octos configuration, provider credentials,
plugins, bootstrap files, native tools, embeddings, or persistent conversation
history. Ordinary `octos acp` and embedded factories keep their existing behavior.

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
