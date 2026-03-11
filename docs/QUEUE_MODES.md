# Queue Modes

Queue modes control how the session actor handles incoming user messages while the agent is busy processing a previous request.

Set via `/queue <mode>` in chat, or `queue_mode` in profile config.

## Modes

### Followup (default)

Sequential processing. Each message waits its turn.

- Agent processes A → finishes → processes B → finishes → processes C
- Simple and predictable
- User is blocked until current request completes

### Collect

Batch queued messages into a single combined prompt.

- Agent processes A. User sends B, then C.
- When A finishes, B and C are merged: `B\n---\nQueued #1: C`
- One LLM call for the batch
- Good for users who send thoughts in multiple short messages (common in chat apps)

### Steer

Keep only the newest queued message, discard older ones.

- Agent processes A. User sends B, then C.
- When A finishes, B is discarded, only C is processed
- Good for when the user corrects/refines their question mid-flight
- Example: "search for X" → "actually search for Y" → only Y is processed

### Interrupt

Same as Steer, but cancels the running agent.

- Agent processes A. User sends B, then C.
- A is **cancelled**, B is discarded, C is processed immediately
- Fastest response to course-correction
- Use when responsiveness matters more than completing the current task

### Speculative

Spawn concurrent overflow agents for each new message while the primary runs.

- Agent processes A. User sends B, then C.
- B and C each get their own concurrent agent task (overflow)
- All three run in parallel — no blocking
- Best for slow LLM providers where users don't want to wait
- Overflow agents use a snapshot of conversation history from before the primary started

## Speculative Mode Details

### How overflow works

1. Primary agent spawned for first message
2. While primary runs, new messages arrive in inbox
3. Each new message triggers `serve_overflow()` → spawns a full agent task with its own streaming bubble
4. Overflow agents use `pre_primary_history` (history snapshot before primary) to avoid re-answering the primary question
5. All agents run concurrently, save results to session history

### Known limitations

**Interactive prompts break in overflow**: If the LLM asks a follow-up question (e.g. "choose search depth: 1/2/3") and returns EndTurn, the overflow agent exits. The user's reply (e.g. "2") spawns a *new* overflow with no context of the question — it gets misrouted.

**Slash commands during overflow**: Fixed — `/queue`, `/adaptive` and other slash commands are now handled inline instead of spawning overflow agents. Commands are buffered during the select loop and executed after the primary finishes.

**Short replies misrouted**: A "yes", "2", or other short continuation intended for the current conversation may be treated as an independent new query. Future fix: reply-threading via Telegram's reply-to-message feature (see `REPLY_THREADING_PLAN.md`).

## Auto-escalation

The session actor can auto-escalate from Followup to Speculative when sustained latency degradation is detected:

- `ResponsivenessObserver` tracks LLM response times
- If consecutive slow responses exceed 2x baseline, auto-activates Speculative + Hedge racing
- When provider recovers, reverts to normal mode

## Usage

```
/queue                  — show current mode
/queue followup         — sequential processing
/queue collect          — batch queued messages
/queue steer            — keep newest only
/queue interrupt        — cancel current + keep newest
/queue speculative      — concurrent overflow agents
```
