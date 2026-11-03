# Fixed tool dispatcher

Run `octos chat`. The model sees six stable tool
specifications: `read_file`, `write_file`, `edit_file`, `exec_command`,
`discover_tools`, and `run_tool`. Other authorized implementations remain in
the registry, including search, patches, web, browser, persistent command input,
background task inspection, agent lifecycles, plugin and MCP tools.

This is the standard interface for built-in runtime registries. No presentation
mode switch is required. The old `coding-full`, `coding-compact` and
`coding-dispatch` built-in variants, the `OCTOS_TOOL_SURFACE` switch and the
profile presentation flags have been removed. Custom profiles still configure
authorization, model preferences and agent setup; explicit host restrictions
remain in force. A low-level `ToolRegistry::new()` remains empty for hosts that
construct their own authorized registry.

## Model workflow

1. Use the four directly advertised tools for ordinary coding.
2. Discover additional capabilities: `discover_tools({"query":"web search"})`.
   Search returns names and short descriptions, with pagination. An empty query
   browses the authorized catalog.
3. Get the exact parameter schema: `discover_tools({"name":"web_search"})`.
4. Execute using those parameters:
   `run_tool({"name":"web_search","arguments":{"query":"official reference"}})`.
5. Reuse the schema in the conversation. Discovery does not activate tools or
   inject new declarations into the next request's tool array.

Discovery defaults to five matches, at most ten per page. This bounds catalog
output, not concurrent agents. A full schema over 24,000 bytes is explicitly
omitted, with keys for inspection through `schema_pointer` (JSON Pointer within
`input_schema`). No truncated JSON is presented as a valid schema. Extremely
large atomic schema values may remain too large to return as a single fragment.

`run_tool` invokes one tool. It does not evaluate JavaScript, chain dependent
operations, or create another model conversation. Native batching remains
available (`read_file.files`, `web_search.requests`, `web_fetch.requests`,
`edit_file.edits`). Independent calls may use normal batch execution; submit
writes and subsequent verification in separate dependent steps.

## Execution and persistence

The agent resolves the gateway to the authorized native call **before** applying
native approval rules, concurrency admission, snapshots, timeout selection,
lifecycle hooks and progress detection. Both gateway and target session limits
apply. Target hooks receive the target's real name and arguments. Target human
approval rules apply; a gateway rule is used when no target rule matches.
Approval drafts carry the target name and arguments and the original call ID.

The assistant transcript retains the original `run_tool` call. Tool results keep
its ID, ordinary token usage and metadata, and native attachments. Images use
the existing immutable main-history media path. History analysis and compaction
unwrap calls for file/command summaries and plan retention without rewriting
the stored provider transcript. Background task IDs, permission context and
result polling remain native; `read_task_output` is reachable via `run_tool`.

Resolution checks the current registry, host list, provider policy, context,
availability, internal-hidden flag and profile permissions. Discovery applies
the same filters. Neither gateway authorizes a tool the host removed or denied.
If discovery or invocation is excluded, optional tools fall back to direct
advertisement to honor that exact host policy without making allowed tools
inaccessible. This authorization fallback is not a selectable presentation mode.
Snapshots inherit the registered gateways and the host restrictions.

Invalid or unavailable invocations fail as ordinary tool errors; they cannot
be repaired by a wrapper hook after native admission has already happened.
Bare registry execution shares authorization, timeouts and cancellation; agent
hooks and approval orchestration belong to the Agent executor, as for direct
native calls.

## Cache tradeoff and validation

The fixed tool array reduces upfront schema bytes and avoids discovery-driven
changes to that array. Descriptions/schema results append to conversation
history. Discovery adds requests for previously unused capabilities; known
schemas can be reused in later calls. System prompts, provider serialization,
compaction and changing authorization can still affect cache hits. No live cache-rate or throughput
improvement is implied by schema size alone.

With optional Git/AST features disabled, the default six specifications use
4,607 serialized JSON bytes. The prior full-surface build used 23,631 bytes
for 34 entries (an 80.5% reduction in schema bytes). This is a historical schema-byte comparison,
not a provider token or live-cache benchmark. Run `cargo test -p octos-agent
--no-default-features --test fixed_dispatcher -- --nocapture` to check the
current default footprint and behavior.

Tests cover stable schemas across late registration and actual agent requests,
permission denial, image delivery, native approval/resume, hooks, native and
gateway limits, bounded schema inspection, snapshots and cancellation. This is
a typed single-invocation dispatcher, not Pi's JavaScript codemode runtime.
