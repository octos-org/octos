# iMessage Channel Evaluation

Evaluated 2026-03-13. Conclusion: **not worth pursuing**.

## Options Considered

### 1. AppleScript + chat.db (no signup needed)
- Send via `osascript` → Messages.app
- Receive by polling `~/Library/Messages/chat.db` (SQLite)
- Requires a Mac with a signed-in Apple account
- Works for personal use; Apple TOS prohibits automated bulk messaging
- Apple will ban accounts showing bot-like behavior (high volume, identical messages)

### 2. Apple Messages for Business (official API)
- Must register as an MSP at register.apple.com
- Apple manually reviews applications — expects enterprise customer service platforms
- Requires: live agent support, intent routing, automation, public HTTPS (no ngrok)
- Overkill for crew's use case

### 3. Third-party MSP (Sinch, CM.com, Infobip)
- Use an approved provider's API as a middleman
- Fastest official path but adds cost and dependency

## Why iMessage Is a Poor Fit for Bots

| Feature | Telegram | iMessage |
|---------|----------|----------|
| Bot API | Official, full-featured | None |
| Commands (/start, /help) | Built-in | No |
| Inline keyboards/buttons | Yes | No |
| Group bots | Yes | No |
| File sharing | Easy, up to 2GB | Harder to automate |
| Webhooks | Native | Poll chat.db yourself |
| Rich formatting (markdown) | Yes | Plain text only |
| Bot discovery | Search by username | Must know Apple ID |
| Rate limits | Generous | Apple will ban you |

## Decision

Skip iMessage. Telegram, WhatsApp, and Feishu already cover all use cases with proper bot APIs. iMessage was designed for human-to-human messaging — building a bot on it means fighting the platform with no rich UX payoff.
