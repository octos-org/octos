# Octos UI Protocol Change Request: Hydrated Tool Call Identity

## Header

- Request id: `UPCR-2026-039`
- Date: 2026-10-02
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: three additive optional fields on `HydratedMessage`, the
  `messages` rows of `session/hydrate` and of `session/rollback`'s `thread`

## Problem

`session/hydrate` (`UPCR-2026-009`) returns a turn's tool-result rows
(`role: "tool"`) with their content and `thread_id`, but not the tool call
each one answers or the tool's name. It returns the assistant row that made
the calls without the calls. The session store has both: a result row keeps
its `tool_call_id` and the assistant row keeps its `tool_calls`. The hydrate
projection dropped them.

The v2 tool envelopes (`replayed_tool_envelopes`) carry the names, but only
for a connection that negotiated `projection.envelope.v2`, and only for the
retained replay window. A client without them cannot label a reloaded tool
row. OctoSense's shell is such a client: a stdio host on the stdio default
features, which leave out `projection.envelope.v2`. After a reload it shows
each tool row as "tool" instead of, say, `peer_send_input`.

## Contract

`HydratedMessage` gains three optional fields:

- `tool_call_id` (string): on a tool-result row, the id of the assistant tool
  call it answers, as stored with the row. Absent on other rows.
- `tool_name` (string): on a tool-result row, the name of the tool that call
  ran, the value `tool/started` carries as `tool_name`. The stored row has
  only the call id. The server takes the name from the nearest earlier row
  whose tool calls hold that id, because a provider can reuse an id in a later
  turn. The lookup covers the whole transcript the server holds, so an `after`
  cursor that skips the call's row still names the result. Absent when that
  transcript lacks the call.
- `tool_calls` (array of `{tool_call_id, tool_name}`): on an assistant row
  that called tools, each call's id and tool name, in call order. The
  arguments are not included. Omitted when the row made no call.

The names follow the v1 tool notifications (`tool/started`,
`tool/completed`). The v2 `tool_start` payload calls the tool name `name`.

The fields are ungated, like `media`. Every connection that receives
`messages` gets them, whether or not it negotiated `projection.envelope.v2`.
Older clients ignore them. Rows that neither call tools nor answer a call
keep the same shape, and payloads without the fields decode as absent.

`session/rollback` returns its trimmed thread in the hydrate shape, so its
rows carry the same fields.

Clients that draw tool cards from `replayed_tool_envelopes` are unaffected.
They can match a row to its envelope by `tool_call_id`.

## Risk

- Tool names and call ids already reach clients for live turns
  (`tool/started`, v2 `tool_start`) and in `replayed_tool_envelopes`. The rows
  show a connection that can hydrate the session nothing it could not see
  live.
- Arguments stay off the rows, so hydrate does not grow with tool inputs and
  does not repeat them.
- The names are found in one pass over the transcript per hydrate.
- Rust callers that build `HydratedMessage` with a struct literal must add
  the three fields, as they did for `reasoning_content`.

## Tests

- `should_carry_tool_call_identity_when_a_hydrated_row_is_a_tool_call_or_result`
  (octos-core: wire names, omission on other rows, pre-UPCR rows decode)
- `session_hydrate_rows_carry_tool_call_identity` (stdio defaults, no
  features, and `projection.envelope.v2` alike)
- `should_name_a_tool_row_when_the_after_cursor_skips_its_call`
- `should_name_each_tool_row_by_its_nearest_call_when_call_ids_repeat`
  (reused ids, and a result whose call is gone)
- `session_rollback_rows_carry_tool_call_identity`
