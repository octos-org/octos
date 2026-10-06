# Octos UI Protocol Change Request: Accepted Client Commands

## Header

- Request id: `UPCR-2026-038`
- Date: 2026-10-02
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: one additive optional field on `SessionOpened`

## Problem

The server filters the `client_commands` a client declares on `session/open`
(`UPCR-2026-037`), and the filtering was silent. A client could learn which
names survived only from the rendered system prompt, which it never sees
(#2670).

## Contract

`SessionOpened` gains one optional field:

- `accepted_client_commands` (array of strings): the names the server
  accepted from this open's `client_commands`, each as `/name`, in declaration
  order. Present whenever the request carried `client_commands` and the
  server applied it, including as `[]` when every name was dropped. Absent
  when the request omitted `client_commands`.

A client learns what was dropped by comparing the list with what it declared,
after normalizing its own names to the `/name` form. The field does not say
why a name was dropped; the rules are in `UPCR-2026-037`.

The filter and the lifecycle of `UPCR-2026-037` are unchanged. `session/open`
still never fails because of a rejected name.

The field is ungated: it needs no feature token. Older clients ignore it, and
payloads without it decode as absent.

### Notification and replay

The `session/open` notification shares the `SessionOpened` shape, so it
carries the same field. Other connections on the session therefore observe a
replaced declaration: an absent field on another connection's open means the
session now has no client commands.

The ledger replays earlier `session/open` notifications with the value they
had when written. A replayed value is history, not the current declaration.
The `session/open` result is authoritative for the connection that sent it.

## Adoption

Shipped, awaiting a first client. As of 2026-10-06 no client in the octos-org
repositories reads `accepted_client_commands`, and none sends the
`client_commands` it answers (#2661). Until a client reads the field, #2670 is
fixed on the wire only: a user of the shipped clients still cannot see which
names were dropped.

## Risk

- The echoed names are the same strings already rendered into the system
  prompt, restricted to the validated character set. The field exposes no
  server state beyond the outcome of the documented filter.
- Other connections on the session now see which names a peer connection
  declared. They could already infer this from the agent's replies.
- The legacy single-agent serve (no profile runtime) applies no declaration
  and omits the field.

## Tests

- `session_opened_echoes_accepted_client_commands`
- `accepted_client_commands_lists_the_rendered_names_in_declaration_order`
- `serve_sessions_drop_channel_slash_commands_and_take_client_commands` (now
  also asserts the returned names)
- `session_open_result_echoes_the_accepted_client_commands`
