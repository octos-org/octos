# Working in Octos

Start with [README.md](README.md), [runtime architecture](docs/ARCHITECTURE.md),
and, for app hosts, the
[OctoSense integration walkthrough](docs/octosense-integration-walkthrough.md).

- Octos owns agent execution and OUP runtime contracts. A product host owns UI,
  app records/adapters and its app authorization policy. Keep that boundary
  explicit; tool declarations or model text do not themselves authorize access.
- Pin related Octos crates together in consumers. Distinguish this checkout
  from a consumer's selected revision in integration documentation.
- For host-owned app peers read `peers/app_binding.rs`, `peers/host_tools.rs`,
  `peers/turn_origin.rs`, `peers/shared_history.rs` and their dispatcher call
  sites. Preserve workspace/memory binding, host-token and connection ownership,
  context lifetime, tool confinement, reply correlation and origin checks.
- A request context is a session, not a new peer. Shared history is a bounded
  projection between separate transcripts. Preserve tool-call pairing and do
  not merge transcripts or leak sibling context history as an incidental fix.
- Do not describe a peer/session as exactly one Tokio task. Trace the actual
  frontend, transport, admitted turn, agent task and tool execution lifetimes.
  OUP stdio's writer is a dedicated OS thread; the gateway `SessionActor` is a
  separate runtime path. Honor the embedded API's 8 MiB worker-stack requirement.
- Protocol changes must align the OUP specification, core types/codecs,
  dispatcher, capability advertisement, permissions and affected clients.
  Read [the protocol specification](api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md).
- Run focused existing tests for changed behavior, including negative
  authorization/cancellation cases where relevant. Repository-wide checks are
  listed in README. Do not run model/provider/device workflows merely to claim
  documentation validation; clearly distinguish source inspection from executed
  integration tests and keep credentials out of logs/docs.
- Keep English/Chinese README entry points aligned. Preserve existing unrelated
  work and do not change consumer pins, deploy services or publish as part of
  a documentation correction unless requested.
