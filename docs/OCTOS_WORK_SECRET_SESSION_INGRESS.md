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

## Connect

Decode the secret, then connect to:

```text
ws://127.0.0.1:50080/v1/session_ingress/ws/dspfac:local:tui%23coding
```

Pass `session_ingress_token` as `Authorization: Bearer <token>`. WebSocket
clients that cannot set headers may use `?session_ingress_token=<token>`.

The socket speaks the normal UI Protocol v1 JSON-RPC frames. The server
revalidates the grant before every client frame and rejects any method whose
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
        f"?session_ingress_token={quote(secret['session_ingress_token'], safe='')}"
        "&ui_feature=auxiliary.rest_to_ws.v1"
    )
    async with websockets.connect(url) as ws:
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

Revocation is read from disk by `octos serve` on each frame, so an already-open
session ingress socket is closed with policy-violation code `1008` after the
grant is revoked or expires.
