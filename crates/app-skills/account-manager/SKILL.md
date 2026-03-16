---
name: account-manager
description: Manage sub-accounts under the current profile. Triggers: create account, 创建账号, sub account, manage account, list accounts, 子账号.
version: 1.1.0
author: octos
always: false
---

# Account Manager Skill

## Overview

This skill provides the `manage_account` tool for managing sub-accounts under the current profile. Sub-accounts share the parent profile's LLM provider configuration and API keys (same billing) but have their own data directory, memory, sessions, skills, and messaging channels.

## When to Use

Use this tool when the user asks to:
- List their sub-accounts ("show my sub-accounts", "what accounts do I have")
- Create a new sub-account ("create a work bot", "set up a new assistant called X")
- Update a sub-account's config ("add telegram token to X", "set allowed users for X", "change the system prompt")
- Enable/start or disable/stop a sub-account ("start the work bot", "stop nocodingbot")
- Restart a sub-account ("restart the work bot")
- Delete a sub-account ("remove the work bot", "delete sub-account X")
- Check sub-account details ("show info about work bot", "what's the status of X")

## Usage

### List sub-accounts

```json
{ "action": "list" }
```

### Create a sub-account

```json
{
  "action": "create",
  "name": "work bot",
  "system_prompt": "You are a professional work assistant.",
  "telegram_token": "123456:ABC-DEF...",
  "enable": true
}
```

Only `name` is required. Other fields are optional.

### Update a sub-account

Incrementally update config — only provide the fields you want to change:

```json
{
  "action": "update",
  "sub_account_id": "parent-id--work-bot",
  "telegram_token": "123456:ABC-DEF...",
  "telegram_senders": "5460262597,123456789",
  "system_prompt": "You are a coding assistant."
}
```

Supported fields: `telegram_token`, `telegram_senders`, `whatsapp` (bool), `feishu_app_id`, `feishu_app_secret`, `system_prompt`, `enabled` (bool), `sandbox` (bool), `sandbox_mode` ("auto"/"macos"/"docker"/"bwrap"), `sandbox_network` (bool).

### Toggle sandbox on a sub-account

```json
{
  "action": "update",
  "sub_account_id": "parent-id--work-bot",
  "sandbox": true,
  "sandbox_mode": "auto",
  "sandbox_network": true
}
```

### Start/stop a sub-account

```json
{ "action": "start", "sub_account_id": "parent-id--work-bot" }
{ "action": "stop", "sub_account_id": "parent-id--work-bot" }
```

### Restart a sub-account

```json
{ "action": "restart", "sub_account_id": "parent-id--work-bot" }
```

### Delete a sub-account

```json
{
  "action": "delete",
  "sub_account_id": "parent-id--work-bot"
}
```

### Get sub-account info

```json
{
  "action": "info",
  "sub_account_id": "parent-id--work-bot"
}
```

## Environment Variables

This tool reads `OCTOS_HOME` and `OCTOS_PROFILE_ID` from the environment (set automatically by the gateway). No manual configuration is needed.
