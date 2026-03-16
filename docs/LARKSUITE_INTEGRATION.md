# Larksuite / Feishu Integration Guide

octos supports both **Feishu** (China, `open.feishu.cn`) and **Larksuite** (international, `open.larksuite.com`) as messaging channels.

## Architecture

- **Feishu (CN)**: WebSocket long connection mode (`/callback/ws/endpoint`)
- **Larksuite (Global)**: Webhook HTTP mode (Lark pushes events via POST)

The webhook mode runs an HTTP server inside the gateway that receives event callbacks from Lark. An ngrok tunnel (or any reverse proxy) exposes the local webhook to the internet.

```
Lark Cloud ──POST──> ngrok ──> localhost:9321/webhook/event ──> Gateway ──> LLM
                                                                  │
Lark User <──────────── REST API (interactive card) <─────────────┘
```

## Lark Developer Console Setup

1. Go to [open.larksuite.com/app](https://open.larksuite.com/app)
2. Create an app (or use an existing one)
3. **Add Bot capability**: Features → Add "Bot"
4. **Configure event subscription**:
   - Events & Callbacks → Event Configuration → Edit subscription method
   - Select "Send events to developer server"
   - Set request URL: `https://YOUR_NGROK_URL/webhook/event`
   - The gateway auto-responds to the `url_verification` challenge
5. **Add event**: `im.message.receive_v1` (Receive Message)
6. **Enable permissions**: `im:message`, `im:message:send_as_bot`, `im:resource`
7. **Publish the app**: App Release → Version Management → Create Version → Apply for Online Release

## Config (`~/.octos/config.json`)

```json
{
  "gateway": {
    "channels": [
      {
        "type": "lark",
        "allowed_senders": [],
        "settings": {
          "app_id_env": "LARK_APP_ID",
          "app_secret_env": "LARK_APP_SECRET",
          "region": "global",
          "mode": "webhook",
          "webhook_port": 9321
        }
      }
    ]
  }
}
```

### Settings Reference

| Setting | Description | Default |
|---------|-------------|---------|
| `app_id_env` | Env var name for App ID | `FEISHU_APP_ID` |
| `app_secret_env` | Env var name for App Secret | `FEISHU_APP_SECRET` |
| `region` | `"cn"` (Feishu) or `"global"`/`"lark"` (Larksuite) | `"cn"` |
| `mode` | `"ws"` (WebSocket) or `"webhook"` (HTTP) | `"ws"` |
| `webhook_port` | Port for webhook HTTP server | `9321` |
| `encrypt_key` | Encrypt Key from Lark console (optional, for AES-256-CBC) | none |
| `verification_token` | Verification Token from Lark console (optional) | none |

For Feishu China, use `"type": "feishu"` with `"mode": "ws"` (default).

## Build & Run

```bash
# Build with feishu feature
cargo build --release -p octos-cli --features telegram,whatsapp,feishu

# Start ngrok tunnel
ngrok http 9321

# Start gateway
LARK_APP_ID="cli_xxxxx" \
LARK_APP_SECRET="xxxxx" \
crew gateway --cwd /path/to/workdir
```

## Features

### Inbound (receiving messages)
- Text messages
- Images (downloaded via Lark API, passed as media paths)
- Files (PDF, docs, etc.)
- Audio messages
- Video messages
- Stickers

### Outbound (sending replies)
- **Markdown rendering** via interactive cards (`msg_type: "interactive"`)
- Image upload and send
- File upload and send

### Security
- **Signature verification**: Validates `X-Lark-Signature` header using SHA-256 (requires `encrypt_key`)
- **AES-256-CBC decryption**: Decrypts encrypted event payloads (requires `encrypt_key`)
- **Verification Token**: Validates events belong to the correct app (optional)
- **Message dedup**: Tracks message IDs to prevent duplicate processing

## Encryption Setup (Optional)

If you configure an Encrypt Key in the Lark console (Events & Callbacks → Encryption Strategy):

```json
{
  "type": "lark",
  "settings": {
    "app_id_env": "LARK_APP_ID",
    "app_secret_env": "LARK_APP_SECRET",
    "region": "global",
    "mode": "webhook",
    "webhook_port": 9321,
    "encrypt_key": "your-encrypt-key-here",
    "verification_token": "your-verification-token"
  }
}
```

With encryption enabled:
- Lark sends encrypted POST bodies (`{"encrypt": "base64..."}`)
- The gateway decrypts using AES-256-CBC with SHA-256 key derivation
- Signature verification uses `X-Lark-Request-Timestamp` + `X-Lark-Request-Nonce` + `encrypt_key` + body

## Troubleshooting

| Issue | Solution |
|-------|----------|
| 404 on WS endpoint | Larksuite international doesn't support WebSocket mode. Use `"mode": "webhook"` |
| Challenge verification fails | Ensure ngrok is running and URL matches what's in Lark console |
| No events received | Publish the app version after adding events. Check Event Log Retrieval in console |
| Bot doesn't reply | Check `im:message:send_as_bot` permission is granted |
| Markdown not rendering | Messages are sent as interactive cards; Lark supports a subset of markdown |
| Ngrok URL changed | Free ngrok URLs change on restart. Update the request URL in Lark console |
