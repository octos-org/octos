# Troubleshooting

This chapter covers common issues organized by category, along with environment variable reference.

---

## API & Provider Issues

### API Key Not Set

```
Error: ANTHROPIC_API_KEY environment variable not set
```

**Fix**: Export the key in your shell or verify with `octos status`:

```bash
export ANTHROPIC_API_KEY="your-key"
```

If running as a service, ensure the environment variable is set in the service environment (launchd plist or systemd unit), not just your interactive shell.

### Rate Limited (429)

The retry mechanism handles this automatically (3 attempts with exponential backoff). If the error persists:
- Try switching to a different provider via `/queue` or in-chat model switching.
- Wait for the rate limit window to reset.

### Debug Logging

Enable detailed logs to diagnose issues:

```bash
RUST_LOG=debug octos chat
RUST_LOG=octos_agent=trace octos chat --message "task"
```

---

## Build Issues

| Problem | Solution |
|---------|----------|
| Build fails on Linux | Install build dependencies: `sudo apt install build-essential pkg-config libssl-dev` |
| macOS codesign warning | Sign the binary: `codesign -s - ~/.cargo/bin/octos` |
| `octos: command not found` | Add cargo bin to PATH: `export PATH="$HOME/.cargo/bin:$PATH"` |

---

## Channel-Specific Issues

### Lark / Feishu

| Issue | Solution |
|-------|----------|
| 404 on WebSocket endpoint | Larksuite international does not support WebSocket mode. Use `"mode": "webhook"` in your config |
| Challenge verification fails | Ensure your tunnel (e.g., ngrok) is running and the URL matches the one configured in the Lark console |
| No events received | Publish the app version after adding events. Check Event Log Retrieval in the console |
| Bot does not reply | Check that the `im:message:send_as_bot` permission is granted |
| Markdown not rendering | Messages are sent as interactive cards; Lark supports a subset of markdown |
| Tunnel URL changed | Free tunnel URLs change on restart. Update the request URL in the Lark console |

### WeCom / WeChat

**"Environment variable WECOM_BOT_SECRET not set"**

Set the secret before starting the gateway:

```bash
export WECOM_BOT_SECRET="your_secret"
```

**Connection drops or fails to subscribe**

- Verify `bot_id` and secret are correct.
- Check network connectivity to `wss://openws.work.weixin.qq.com`.
- The channel auto-reconnects up to 100 times with exponential backoff. Check logs for error details.

**Messages not arriving**

- Confirm the upstream relay service is running and linked to your account.
- Check that the WeCom group robot is the same one configured in octos.
- If using `allowed_senders`, verify the sender's WeCom user ID is in the list.
- Check for duplicate message filtering -- the channel deduplicates the last 1000 message IDs.

**Long messages are truncated**

Messages over 4096 characters are automatically split into multiple chunks by octos. If further truncation occurs, check the relay service's own message length settings.

---

## Platform-Specific Issues

| Problem | Solution |
|---------|----------|
| Dashboard not accessible | Check port: `octos serve --port 8080`, open `http://localhost:8080/admin/` |
| WSL2 port not forwarded | Restart WSL: `wsl --shutdown` then reopen terminal |
| Service will not start | Check logs: `tail -f ~/.octos/serve.log` (macOS) or `journalctl --user -u octos-serve` (Linux) |
| Windows: `octos` not found | Ensure `%USERPROFILE%\.cargo\bin` is in your PATH |
| Windows: shell commands fail | Commands run via `cmd /C`; use Windows-compatible syntax |

---

## Environment Variables Reference

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `OPENAI_API_KEY` | OpenAI API key |
| `GEMINI_API_KEY` | Gemini API key |
| `OPENROUTER_API_KEY` | OpenRouter API key |
| `DEEPSEEK_API_KEY` | DeepSeek API key |
| `GROQ_API_KEY` | Groq API key |
| `MOONSHOT_API_KEY` | Moonshot API key |
| `DASHSCOPE_API_KEY` | DashScope API key |
| `MINIMAX_API_KEY` | MiniMax API key |
| `ZHIPU_API_KEY` | Zhipu API key |
| `ZAI_API_KEY` | Z.AI API key |
| `NVIDIA_API_KEY` | Nvidia NIM API key |
| `OMINIX_API_URL` | Local ASR/TTS API URL |
| `RUST_LOG` | Log level (`error` / `warn` / `info` / `debug` / `trace`) |
| `TELEGRAM_BOT_TOKEN` | Telegram bot token |
| `DISCORD_BOT_TOKEN` | Discord bot token |
| `SLACK_BOT_TOKEN` | Slack bot token |
| `SLACK_APP_TOKEN` | Slack app-level token |
| `FEISHU_APP_ID` | Feishu app ID |
| `FEISHU_APP_SECRET` | Feishu app secret |
| `EMAIL_USERNAME` | Email account username |
| `EMAIL_PASSWORD` | Email account password |
| `WECOM_CORP_ID` | WeCom corp ID |
| `WECOM_AGENT_SECRET` | WeCom agent secret |
