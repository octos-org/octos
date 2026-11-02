# Work Secret Session Ingress

Status: implemented

Issue: #296

## Purpose

Work secrets are short-lived, session-scoped credentials for external CLI
agents. They avoid giving guest tools a dashboard bearer token while still
allowing them to attach to one existing AppUI session.

The encoded secret is base64url JSON:

```json
{
  "version": 1,
  "session_ingress_token": "<opaque bearer>",
  "api_base_url": "http://127.0.0.1:50080"
}
```

## Issue A Secret

```bash
octos auth issue-work-secret \
  --session dspfac:local:tui#coding \
  --profile dspfac \
  --ttl 1h \
  --api-base-url http://127.0.0.1:50080
```

The command writes a hashed grant to `$OCTOS_HOME/work_secrets.json` (or
`~/.octos/work_secrets.json`) and prints the encoded secret to stdout.
Operator notes (expiry, the replacement semantics, how to revoke) go to
stderr, so stdout stays scriptable. Re-issuing for the same session replaces
it: the session's earlier grant is removed from the file — it will no longer
appear in the listing rather than showing up as revoked.

## List Grants

```bash
octos auth list-work-secrets
```

Lists every recorded grant, in recorded order, with its session, profile,
token hash prefix, created/expires timestamps, ttl, and status (active /
expired / revoked, with the revocation time). The bearer token itself is
never stored, so only its SHA-256 hash can be shown.

## Connect

Decode the secret, then connect to:

```text
ws://127.0.0.1:50080/v1/session_ingress/ws/dspfac:local:tui%23coding
```

Pass `session_ingress_token` as `Authorization: Bearer <token>`. WebSocket
clients that cannot set headers may fall back to `?token=<token>`; this query
form is deprecated (a bearer credential in the request line leaks into
intermediary access logs) and its use is logged server-side. The former
`_token` / `session_ingress_token` query aliases were removed.

The socket speaks the normal UI Protocol v1 JSON-RPC frames. The server
revalidates the grant before handling each client text frame (not WebSocket
ping/pong control frames) and rejects any method whose
`session_id` is not the granted session. Non-session global methods such as
`system/status.get` and `content/list` are not available through a work secret.

Tiny Python client:

```python
import asyncio
import base64
import json
from urllib.parse import quote

import websockets

SESSION_ID = "dspfac:local:tui#coding"
ENCODED_SECRET = "<paste octos auth issue-work-secret output>"


def decode_secret(encoded):
    padded = encoded + "=" * (-len(encoded) % 4)
    return json.loads(base64.urlsafe_b64decode(padded))


async def main():
    secret = decode_secret(ENCODED_SECRET)
    api = secret["api_base_url"].rstrip("/")
    ws_base = api.replace("https://", "wss://", 1).replace("http://", "ws://", 1)
    url = (
        f"{ws_base}/v1/session_ingress/ws/{quote(SESSION_ID, safe=':@')}"
        "?ui_feature=auxiliary.rest_to_ws.v1"
    )
    headers = {"Authorization": f"Bearer {secret['session_ingress_token']}"}
    # `additional_headers` needs websockets >= 14; older releases take
    # the same dict as `extra_headers=`.
    async with websockets.connect(url, additional_headers=headers) as ws:
        await ws.send(json.dumps({
            "jsonrpc": "2.0",
            "id": "status-1",
            "method": "session/status.get",
            "params": {"session_id": SESSION_ID},
        }))
        print(await ws.recv())


asyncio.run(main())
```

## Revoke

```bash
octos auth revoke-work-secret '<encoded-secret>'
```

Revocation is read from disk by `octos serve` before handling each client text
frame. An already-open ingress socket is closed with policy-violation code
`1008` when its grant is found revoked, expired, or replaced. Ping/pong frames
do not revalidate the grant, and there is no background revocation watcher.

Ingress turns are connection-bound: closing the socket interrupts its turns,
prevents subsequent tool calls, cancels pending approvals and questions, and
clears remembered authorization scopes. This also applies to an ordinary
ingress disconnect while the grant is still valid; disconnecting does not
revoke the grant. Ordinary dashboard WebSocket disconnects continue to leave
server-owned turns running.

When revalidation detects a revoked, expired, or replaced grant, teardown also
cancels all surviving spawn-only tasks for that profile/session and clears its
pending terminal and failure continuations across workspaces, even if the parent
turn has already ended. Already-admitted automatic continuations are interrupted.
This intentionally fails closed for a shared session: background work started by
a browser in the same profile/session is cancelled too. Work in other sessions
or profiles is unaffected. A valid-grant disconnect preserves background work.

Native tools that have already started may continue to finish or reach their own
configured timeout, including producing side effects after the turn has ended.
This change stops subsequent background work; it does not add cancellation of
already-running native tool tasks or processes. The connection-close terminal
notification remains `errored` with code `connection_closed`, even though the
internal turn state is interrupted.

After a `1008` policy close or a `401` handshake rejection caused by an invalid
grant, obtain a newly issued work secret from the session owner. Reconnecting
with the same revoked/expired/replaced secret cannot restore access. With a
still-valid grant, reconnect, review the retained prompt, and explicitly resend
if a new foreground turn is needed.
