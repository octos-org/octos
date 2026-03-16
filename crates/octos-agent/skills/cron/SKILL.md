---
name: cron
description: Schedule reminders and recurring tasks using the cron tool.
version: 1.0.0
author: octos
always: true
---

# Cron Scheduling

Use the `cron` tool to schedule reminders and recurring tasks.

## IMPORTANT: User Consent Required

NEVER add, remove, enable, or disable cron jobs unless the user **explicitly** asks you to. Do NOT proactively create scheduled tasks, reminders, or recurring jobs. Only use the cron tool when the user's message clearly requests scheduling (e.g., "remind me every day at 9am", "set up a recurring check", "schedule a task"). Listing jobs is always allowed.

## Actions

### Add a recurring job
```json
{"action": "add", "name": "standup", "message": "Time for daily standup!", "every_seconds": 86400}
```

### Add a cron-expression job
```json
{"action": "add", "name": "morning", "message": "Good morning check-in", "cron_expr": "0 0 9 * * * *", "timezone": "America/Los_Angeles"}
```

### Add a one-time job
```json
{"action": "add", "name": "reminder", "message": "Meeting in 5 minutes", "at_ms": 1707552000000}
```

### List jobs
```json
{"action": "list"}
```

### Remove a job
By exact ID:
```json
{"action": "remove", "job_id": "abc12345"}
```
By name (partial match, preferred when user says "cancel X"):
```json
{"action": "remove", "name": "ua877"}
```
This removes ALL jobs whose name or message contains "ua877" (case-insensitive). Always prefer name-based removal — don't ask the user for a job ID.

### Enable/disable a job
```json
{"action": "enable", "job_id": "abc12345"}
{"action": "disable", "job_id": "abc12345"}
```

## Delivery

To deliver responses to a specific channel:
```json
{"action": "add", "name": "alert", "message": "Check metrics", "every_seconds": 3600, "channel": "telegram", "chat_id": "123456"}
```

## Timezone

**IMPORTANT:** Cron expressions are evaluated in UTC by default. You MUST use the `timezone` parameter when the user specifies a local time. Use IANA timezone names.

**Before creating any cron job with a specific time, you MUST confirm the user's timezone.** Ask: "What is your timezone? (e.g. America/Los_Angeles for Pacific Time, Asia/Shanghai for China Standard Time)". Do NOT guess or assume — always ask if you don't know.

Once confirmed, always include `timezone` in the cron tool call:
```json
{"action": "add", "name": "daily-report", "message": "Send report", "cron_expr": "0 0 9 * * * *", "timezone": "Asia/Shanghai"}
```

Common timezones: `America/Los_Angeles` (Pacific), `America/New_York` (Eastern), `Asia/Shanghai` (China), `Asia/Tokyo` (Japan), `Europe/London` (UK), `Europe/Berlin` (Central Europe).

## Cron Expression Format

Standard 7-field cron: `sec min hour day-of-month month day-of-week year`

| Expression | Meaning |
|---|---|
| `0 0 9 * * * *` | Every day at 9:00 AM (in the configured timezone) |
| `0 0 */2 * * * *` | Every 2 hours |
| `0 30 9 * * 1-5 *` | Weekdays at 9:30 AM |
| `0 0 0 1 * * *` | First of every month |
