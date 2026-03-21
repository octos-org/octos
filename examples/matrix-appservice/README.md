# Matrix Appservice Configuration

This directory contains example configuration files for running octos as a
Matrix appservice with the BotFather architecture.

## Architecture

```
Robrix (client)  <-->  Palpo (homeserver)  <-->  octos gateway (appservice :8009)
```

One appservice registration, one namespace, multiple virtual users managed by
BotFather through `/createbot` / `/deletebot` / `/listbots`.

## Files

| File | Purpose |
|------|---------|
| `registration.yaml` | Homeserver-side appservice registration |
| `botfather.json` | octos profile for the BotFather gateway |

## Configuration Reference

### registration.yaml (homeserver side)

Place this file in the homeserver's appservice registration directory.

| Field | Description |
|-------|-------------|
| `id` | Unique appservice identifier (arbitrary string, must be unique per homeserver) |
| `url` | URL where homeserver pushes events to octos (must match gateway's appservice port) |
| `as_token` | Appservice token — octos uses this when calling homeserver API |
| `hs_token` | Homeserver token — homeserver uses this when pushing events to octos |
| `sender_localpart` | Localpart of the main bot user (e.g. `bot` → `@bot:server`) |
| `rate_limited` | Whether homeserver rate-limits this appservice (recommended: `false`) |
| `namespaces.users` | Regex defining which user IDs this appservice exclusively controls |

### botfather.json (octos side)

Place this file at `~/.octos/profiles/botfather.json` or pass via `--profile`.

#### Profile fields

| Field | Description |
|-------|-------------|
| `id` | Profile identifier (used in session keys and data directories) |
| `name` | Display name |
| `enabled` | Whether this profile is active |
| `config.provider` | LLM provider: `deepseek`, `openai`, `anthropic`, `gemini`, etc. |
| `config.model` | Model identifier for the provider |
| `config.api_key_env` | Env var name holding the API key (read at startup) |
| `config.admin_mode` | Enable BotFather management commands (`/createbot`, `/deletebot`, `/listbots`). Only BotFather needs this. |

#### Matrix channel settings (`config.channels[]`)

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `type` | yes | — | Must be `"matrix"` |
| `homeserver` | no | `http://localhost:6167` | Homeserver Client-Server API URL (Palpo/Synapse, NOT the appservice) |
| `as_token` | **yes** | — | Appservice token. Must match `registration.yaml` |
| `hs_token` | **yes** | — | Homeserver token. Must match `registration.yaml` |
| `server_name` | no | `localhost` | Domain part of Matrix user IDs (e.g. `127.0.0.1:6006`) |
| `sender_localpart` | no | `bot` | Main bot's localpart → `@bot:<server_name>` |
| `user_prefix` | no | `bot_` | Prefix for virtual users → `@bot_weather:<server_name>` |
| `port` | no | `8009` | Appservice HTTP listener port (receives events from homeserver) |

#### Gateway settings (`config.gateway`)

| Field | Default | Description |
|-------|---------|-------------|
| `max_history` | `50` | Max conversation history messages per session |
| `queue_mode` | — | Message queuing: `"followup"` processes sequentially |

## Consistency Checklist

These fields **must match** between the two files:

| Field | registration.yaml | botfather.json |
|-------|-------------------|----------------|
| AS token | `as_token` | `config.channels[].as_token` |
| HS token | `hs_token` | `config.channels[].hs_token` |
| Sender localpart | `sender_localpart` | `config.channels[].sender_localpart` |
| Server name | namespace regex domain | `config.channels[].server_name` |
| User prefix | namespace regex prefix | `config.channels[].user_prefix` |
| Appservice port | `url` port | `config.channels[].port` |

## Quick Start

```bash
# 1. Generate tokens
AS_TOKEN=$(openssl rand -hex 32)
HS_TOKEN=$(openssl rand -hex 32)

# 2. Copy and edit configs (replace CHANGE_ME with generated tokens)
cp examples/matrix-appservice/registration.yaml /path/to/palpo/appservices/
cp examples/matrix-appservice/botfather.json ~/.octos/profiles/

# 3. Start homeserver (Palpo example)
cd /path/to/palpo && cargo run --release

# 4. Start octos gateway
unset OCTOS_SERVE_URL
DEEPSEEK_API_KEY="your-key" \
  octos gateway --profile ~/.octos/profiles/botfather.json --data-dir ~/.octos
```
