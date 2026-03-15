---
name: send-email
description: Send emails via SMTP or Feishu/Lark Mail. Triggers: send email, 发邮件, email to, 发送邮件, mail, send mail.
version: 1.0.0
author: hagency
always: false
---

# Send Email Skill

## Overview

This skill provides the `send_email` tool for sending emails via SMTP or Feishu/Lark Mail. It is a standalone binary with no LLM dependencies -- pure networking only.

## Usage

Call `send_email` with the following parameters:

- **to** (required, string): Recipient email address.
- **subject** (required, string): Email subject line.
- **body** (required, string): Plain text email body.
- **provider** (optional, string): Either `"smtp"` or `"feishu"`. If omitted, the tool auto-detects based on which environment variables are present.
- **html** (optional, bool): When `true`, the `body` parameter is treated as HTML content. For SMTP this sends a multipart/alternative message with both plain text and HTML parts. For Feishu the body is sent as HTML directly.

## Providers

### SMTP

Sends email via any SMTP server (Gmail, Outlook, custom, etc.) using TLS.

**Required environment variables:**

| Variable | Description | Example |
|---|---|---|
| `SMTP_HOST` | SMTP server hostname | `smtp.gmail.com` |
| `SMTP_PORT` | SMTP server port (465 for implicit TLS, 587 for STARTTLS) | `587` |
| `SMTP_USERNAME` | SMTP login username | `user@gmail.com` |
| `SMTP_PASSWORD` | SMTP login password or app-specific password | `abcdefghijklmnop` |
| `SMTP_FROM` | Sender email address | `user@gmail.com` |

### Feishu / Lark

Sends email via the Feishu (or Lark) Open API using a tenant access token.

**Required environment variables:**

| Variable | Description | Example |
|---|---|---|
| `LARK_APP_ID` | Feishu/Lark application ID | `cli_a9150c716078c07e` |
| `LARK_APP_SECRET` | Feishu/Lark application secret | `IrPGLtOq...` |
| `LARK_FROM_ADDRESS` | Sender email address in Feishu Mail | `sender@company.com` |

**Optional environment variable:**

| Variable | Description | Default |
|---|---|---|
| `LARK_REGION` | Set to `"global"` or `"lark"` to use `open.larksuite.com`; otherwise uses `open.feishu.cn` | `feishu` |

## Provider Auto-Detection

If `provider` is not specified in the tool call:

1. If `SMTP_HOST` is set, uses SMTP.
2. If `LARK_APP_ID` is set, uses Feishu.
3. If neither is set, the tool returns an error.

## Examples

### Send a plain text email via SMTP

```json
{
  "to": "recipient@example.com",
  "subject": "Meeting Tomorrow",
  "body": "Hi, just a reminder about our meeting tomorrow at 10am."
}
```

### Send an HTML email via Feishu

```json
{
  "to": "recipient@example.com",
  "subject": "Weekly Report",
  "body": "<h1>Weekly Report</h1><p>All tasks completed.</p>",
  "provider": "feishu",
  "html": true
}
```

### Explicitly select SMTP provider

```json
{
  "to": "recipient@example.com",
  "subject": "Hello",
  "body": "Plain text content here.",
  "provider": "smtp"
}
```
