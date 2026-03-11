# Reply Threading: Context Anchoring via reply_to_message_id

## Problem

In long conversations, the LLM loses context of what the user is referring to. After compaction, early messages are summarized and detail is lost. Users have no way to reference specific bot replies.

In speculative queue mode, overflow agents are fire-and-forget — when the LLM asks an interactive question (e.g. depth selection) and the user replies, the reply becomes a new overflow with no context of the question.

## Design

Use Telegram's native reply-to-message feature (swipe to reply) to anchor user messages to specific bot replies.

### Data Flow

```
Bot sends msg #100: "VanEck EMLC 2025年收益率16.04%..."
  ↓ Telegram returns msg_id → store in session history

... 30 messages later, user swipes on #100 ...

User msg #789 { text: "展开说说", reply_to_message: { message_id: 100 } }
  ↓ Telegram channel captures reply_to_message_id

Session actor receives InboundMessage {
    content: "展开说说",
    message_id: Some("789"),
    reply_to_message_id: Some("100"),  // ← NEW
}
  ↓ Look up msg #100 from session history

Inject into LLM prompt:
  "[User is replying to your earlier message: 'VanEck EMLC 2025年收益率16.04%...']
   展开说说"
```

### Use Cases

1. **Context anchoring** — user references a specific bot reply in a long conversation, even after compaction
2. **Speculative routing** — route reply to the correct overflow agent instead of spawning a new one
3. **Multi-thread conversations** — parallel discussion threads in the same chat via reply-to
4. **Disambiguation** — "这个不对" / "展开说说" / "翻译成英文" — reply-to tells us exactly what "this" refers to
5. **Group chats** — multiple users replying to different bot messages simultaneously

### Implementation Steps

#### Phase 1: Plumbing

1. Add `reply_to_message_id: Option<String>` to `InboundMessage` (crew-core/src/gateway.rs)
2. Capture `msg.reply_to_message().map(|r| r.id.0.to_string())` in Telegram channel inbound path
3. Store outbound platform message IDs in session history `Message` struct (new field `platform_msg_id: Option<String>`)
4. After `send_message` / `send_html_with_fallback`, capture the returned Telegram msg ID and save it back to session

#### Phase 2: Context Retrieval

5. In session actor, before sending to LLM: if `reply_to_message_id` is set, search session history for the message with matching `platform_msg_id`
6. Prepend the referenced message content as context annotation in the user message
7. Works even after compaction — session JSONL on disk has full history; search the file if not in memory window

#### Phase 3: Speculative Routing

8. Track which agent task (primary vs overflow) sent which outbound message IDs
9. When an inbound has `reply_to_message_id`, route it to the agent that owns that outbound message
10. If the owning agent has exited (fire-and-forget overflow), inject the referenced context and process as a new primary with full context

#### Phase 4: Multi-Channel

11. Add reply_to support to other channels: Feishu, WhatsApp, Discord, Slack (all support threading)
12. Normalize threading semantics across channels

### Notes

- Session JSONL already stores full history — this is the source of truth for lookup even after in-memory compaction
- Telegram `sendMessage` returns the sent message object with `message_id` — we need to capture this return value
- For channels without reply-to (SMS/Twilio, email subject threads), fall back to chronological ordering
- The referenced message content should be truncated if too long (e.g. first 500 chars) to avoid bloating the prompt
