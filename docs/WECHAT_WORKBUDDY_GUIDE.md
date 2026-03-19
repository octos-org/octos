# WeChat Integration via WorkBuddy (Option C)

Connect regular WeChat users to your octos agent by using Tencent WorkBuddy as a bridge. WorkBuddy handles the WeChat transport; octos handles the AI agent logic via its existing WeCom Bot channel.

## How It Works

```
WeChat (mobile)          WorkBuddy (desktop)         octos (server)
┌──────────┐            ┌───────────────────┐       ┌──────────────┐
│ User sends│            │                   │       │              │
│ message   │──────────▶│  Relays to WeCom  │──────▶│  wecom-bot   │
│           │            │  group robot      │  WSS  │  channel     │
│           │◀──────────│  Pushes reply to  │◀──────│              │
│ Gets reply│            │  WeChat chat      │       │  Agent loop  │
└──────────┘            └───────────────────┘       └──────────────┘
```

1. User sends a message in WeChat to the WorkBuddy bot
2. WorkBuddy forwards it to a WeCom group robot via WebSocket
3. octos receives the message on its `wecom-bot` channel
4. The agent processes the request and sends back a response
5. WorkBuddy pushes the response back to the user's WeChat chat

## Prerequisites

- A **WeCom (Enterprise WeChat)** account with a group robot created
- The robot's **Bot ID** and **Secret**
- **WorkBuddy** desktop client installed and linked to your WeChat
- octos built with the `wecom-bot` feature enabled

## Step 1: Create a WeCom Group Robot

1. Log in to the [WeCom Admin Console](https://work.weixin.qq.com/)
2. Go to **Applications > Group Robot** and create a new robot
3. Note the **Bot ID** and **Secret** — you'll need these for octos

## Step 2: Configure octos

Add the `wecom-bot` channel to your gateway config (`~/.crew/config.json` or your project config):

```json
{
  "gateway": {
    "channels": [
      {
        "type": "wecom-bot",
        "allowed_senders": [],
        "settings": {
          "bot_id": "YOUR_BOT_ID",
          "secret_env": "WECOM_BOT_SECRET"
        }
      }
    ]
  }
}
```

### Config fields

| Field | Required | Description |
|-------|----------|-------------|
| `bot_id` | Yes | The WeCom group robot ID |
| `secret_env` | No | Name of the env var holding the robot secret. Defaults to `WECOM_BOT_SECRET` |
| `allowed_senders` | No | List of WeCom user IDs allowed to interact. Empty array = allow everyone |

### Set the secret

```bash
export WECOM_BOT_SECRET="your_robot_secret_here"
```

For persistent deployments, add it to your env file (e.g., `~/.crew/env`).

## Step 3: Build and Start octos

```bash
# Build with the wecom-bot feature
cargo build --release -p octos-cli --features "wecom-bot"

# Start the gateway
octos gateway
```

You should see a log line confirming the WebSocket connection to `wss://openws.work.weixin.qq.com`.

## Step 4: Set Up WorkBuddy

1. Install the WorkBuddy desktop client on your office PC
2. Open **Claw Settings** and select the WeChat integration option
3. Scan the QR code with your WeChat app to authorize the link
4. In WorkBuddy, connect to the same WeCom group robot you configured in octos
5. WorkBuddy will now relay messages between your WeChat and the WeCom group

## Step 5: Test the Connection

1. Open WeChat on your phone
2. Send a message to the WorkBuddy bot (e.g., "Hello")
3. The message flows: WeChat → WorkBuddy → WeCom → octos
4. octos processes it and the reply appears in your WeChat chat

## Connection Details

The `wecom-bot` channel uses an outbound WebSocket connection — no public URL or port forwarding is required. This makes it ideal for servers behind NAT or firewalls.

| Property | Value |
|----------|-------|
| Protocol | WebSocket (WSS) |
| Endpoint | `wss://openws.work.weixin.qq.com` |
| Heartbeat | Ping/pong every 30 seconds |
| Auto-reconnect | Yes, exponential backoff (5s–60s) |
| Max message length | 4096 characters |
| Message format | Markdown |

## Security Considerations

- **Restrict senders**: Use `allowed_senders` to limit who can interact with the agent. An empty list allows anyone in the WeCom group to send commands.
- **Secret management**: Never commit the robot secret to version control. Use environment variables or a secrets manager.
- **WorkBuddy sandbox**: WorkBuddy runs in a local sandbox and only accesses folders you explicitly authorize. It does not have access to octos internals.
- **Network**: The WebSocket connection is TLS-encrypted. No inbound ports need to be opened.

## Troubleshooting

### "Environment variable WECOM_BOT_SECRET not set"

Set the secret before starting the gateway:

```bash
export WECOM_BOT_SECRET="your_secret"
```

### Connection drops or fails to subscribe

- Verify `bot_id` and secret are correct
- Check network connectivity to `wss://openws.work.weixin.qq.com`
- The channel auto-reconnects up to 100 times with exponential backoff — check logs for error details

### Messages not arriving

- Confirm WorkBuddy is running and linked to your WeChat
- Check that the WeCom group robot is the same one configured in octos
- If using `allowed_senders`, verify the sender's WeCom user ID is in the list
- Check for duplicate message filtering — the channel deduplicates the last 1000 message IDs

### Long messages are truncated

Messages over 4096 characters are automatically split into multiple chunks by octos. If WorkBuddy truncates further, check WorkBuddy's own message length settings.

## Limitations

- **Text only**: WeChat voice and image messages are passed through as `[voice]` / `[image]` placeholders. Full media relay depends on WorkBuddy's bridging capabilities.
- **No message editing**: The WeCom Bot channel does not support editing sent messages. Responses are sent as new messages.
- **Single direction**: This setup routes WeChat → octos. For octos to proactively push messages to WeChat, you would need to configure scheduled tasks or cron jobs in octos that send to the WeCom group.

## Next Steps

- **OpenClaw Skills (future)**: For deeper integration, octos could expose an OpenClaw-compatible skill server, allowing WorkBuddy to invoke octos capabilities as native skills. This would enable richer interaction patterns beyond simple message relay.
- **Direct WeChat API**: If you have a Chinese service account (企业服务号), a direct WeChat Official Account channel could be built following the same pattern as the existing WeCom webhook channel.
