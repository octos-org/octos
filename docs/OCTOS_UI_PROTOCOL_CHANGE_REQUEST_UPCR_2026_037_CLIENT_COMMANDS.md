# Octos UI Protocol Change Request: Client Commands On `session/open`

## Header

- Request id: `UPCR-2026-037`
- Date: 2026-10-02
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: specifies the existing optional `client_commands` param of
  `session/open`. No wire shape changes.

## Problem

`session/open` has carried `client_commands` since #2529, and #2600 changed
its wire behaviour (reconcile on every open, release on disconnect, refusal of
gateway server-state commands) without a change request or a spec line. A
client author could not discover any of it (#2593, #2661 item 5). This request
records the contract as shipped.

## Contract

`SessionOpenParams.client_commands` is an optional array of strings: the slash
commands the client handles itself. The server lists the accepted names in the
session's system prompt, so the agent can point the user to them and does not
suggest commands the client lacks. Serve sessions do not inherit the gateway's
own slash-command guidance.

The param is ungated: it needs no feature token.

### Lifecycle

- **Per-open, not sticky.** Every `session/open` replaces the session's
  previous declaration. Omitting the field, or sending `[]`, declares none.
- **One declaration per session.** The declaration lives on the session, not
  on the connection. Across concurrent connections the last `session/open`
  wins: an open without the field clears a declaration another attached
  client made.
- **Released on disconnect.** When the declaring connection closes (WebSocket
  or stdio), the server clears the declaration. A connection that was
  superseded by a later open does not clear the newer declaration.
- **Takes effect on later turns.** Turns read the declaration from the
  session's prompt snapshot when they start.
- **Requires a profile runtime.** The legacy single-agent serve applies no
  declaration.

### Filtering

Each name is trimmed and its leading `/` removed. It is kept only if all of
the following hold:

- it is non-empty and at most 32 characters;
- it contains only ASCII alphanumerics, `-` and `_`;
- it is not a gateway server-state command: `adaptive`, `router`, `queue`,
  `reset`, compared case-insensitively. No client can honour these, because
  they act on gateway per-actor state;
- it has not already been kept (exact match).

Only the first 64 kept names are used. `status` and `thinking` are accepted:
serve intercepts them too, but a client can handle them locally.

### Result

Dropped names are not reported. `SessionOpened` carries no field that echoes
the accepted set, and `session/open` never fails because of a rejected name.
Adding an echo is tracked in #2670 and would be a separate change request.

## Risk

- Declared names reach the system prompt. The character set and the length
  and count caps bound what a client can inject there.
- A client that relies on a dropped name sees no error. Until #2670 lands,
  clients should declare only names within the rules above.

## Tests

- `session_open_client_commands_reach_the_session_agent_prompt`
- `session_reopen_without_client_commands_clears_the_previous_declaration`
- `closing_the_declaring_connection_releases_its_client_commands`
- `stdio_disconnect_releases_client_commands_declared_on_it`
- `client_commands_are_released_only_by_the_declaring_connection`
- `render_client_commands_drops_invalid_and_duplicate_names`
- `render_client_commands_rejects_gateway_only_commands`
- `render_client_commands_caps_the_list_at_64_valid_names`
