# Hook System — Stable API Contract

Hooks are the primary extension point for external applications to enforce LLM policies, record metrics, and audit agent behavior — per profile, without modifying octos code.

## Overview

Hooks are shell commands that run at agent lifecycle events. Each hook receives a JSON payload on stdin and communicates its decision via exit code:

| Exit Code | Meaning | Before-events | After-events |
|-----------|---------|---------------|--------------|
| 0 | Allow | Operation proceeds | Success logged |
| 1 | Deny | Operation blocked (reason on stdout) | Treated as error |
| 2+ | Error | Logged, operation proceeds | Logged |

## Events

Four lifecycle events, each with a specific payload:

### `before_tool_call`

Fires before each tool execution. **Can deny** (exit 1).

```json
{
  "event": "before_tool_call",
  "tool_name": "shell",
  "arguments": {"command": "ls -la"},
  "tool_id": "call_abc123",
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

### `after_tool_call`

Fires after each tool execution. Observe-only.

```json
{
  "event": "after_tool_call",
  "tool_name": "shell",
  "tool_id": "call_abc123",
  "result": "file1.txt\nfile2.txt\n...",
  "success": true,
  "duration_ms": 142,
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

Note: `result` is truncated to 500 characters.

### `before_llm_call`

Fires before each LLM API call. **Can deny** (exit 1).

```json
{
  "event": "before_llm_call",
  "model": "kimi-2.5",
  "message_count": 12,
  "iteration": 3,
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

### `after_llm_call`

Fires after each successful LLM response. Observe-only.

```json
{
  "event": "after_llm_call",
  "model": "kimi-2.5",
  "iteration": 3,
  "stop_reason": "EndTurn",
  "has_tool_calls": false,
  "input_tokens": 1200,
  "output_tokens": 350,
  "provider_name": "moonshot",
  "latency_ms": 2340,
  "cumulative_input_tokens": 5600,
  "cumulative_output_tokens": 1800,
  "session_cost": 0.0042,
  "response_cost": 0.0012,
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

## Configuration

In `config.json` or per-profile JSON:

```json
{
  "hooks": [
    {
      "event": "before_tool_call",
      "command": ["python3", "~/.octos/hooks/guard.py"],
      "timeout_ms": 3000,
      "tool_filter": ["shell", "write_file"]
    },
    {
      "event": "after_llm_call",
      "command": ["python3", "~/.octos/hooks/cost-tracker.py"],
      "timeout_ms": 5000
    }
  ]
}
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `event` | yes | — | One of the 4 event types |
| `command` | yes | — | Argv array (no shell interpretation) |
| `timeout_ms` | no | 5000 | Kill hook process after this timeout |
| `tool_filter` | no | [] (all) | Only trigger for these tool names (tool events only) |

Multiple hooks can be registered for the same event. They run sequentially; the first deny wins.

## Circuit Breaker

Hooks are auto-disabled after 3 consecutive failures (timeout, crash, or exit code 2+). A successful execution (exit 0 or deny exit 1) resets the counter.

## Security

- Commands use argv arrays — no shell interpretation
- 18 dangerous environment variables are removed (`LD_PRELOAD`, `DYLD_*`, `NODE_OPTIONS`, etc.)
- Tilde expansion is supported (`~/` and `~username/`)

## Backward Compatibility

- New fields may be added to payloads (always with `skip_serializing_if`).
- Existing fields will never be removed or renamed.
- Hook scripts should ignore unknown fields (standard JSON practice).

## Per-Profile Hooks

Each profile can define its own hooks via the `hooks` field in `ProfileConfig`. This allows different policy enforcement per channel/bot. Hook changes require a gateway restart.

## Example: Cost Budget Enforcer

```python
#!/usr/bin/env python3
"""Deny LLM calls when session cost exceeds $1.00."""
import json, sys

payload = json.load(sys.stdin)
if payload.get("event") == "before_llm_call":
    # Read cumulative cost from previous after_llm_call
    try:
        with open("/tmp/crew-cost.json") as f:
            state = json.load(f)
    except FileNotFoundError:
        state = {}
    sid = payload.get("session_id", "default")
    if state.get(sid, 0) > 1.0:
        print(f"Session cost exceeded $1.00 (${state[sid]:.4f})")
        sys.exit(1)

elif payload.get("event") == "after_llm_call":
    cost = payload.get("session_cost")
    if cost is not None:
        sid = payload.get("session_id", "default")
        try:
            with open("/tmp/crew-cost.json") as f:
                state = json.load(f)
        except FileNotFoundError:
            state = {}
        state[sid] = cost
        with open("/tmp/crew-cost.json", "w") as f:
            json.dump(state, f)

sys.exit(0)
```

## Example: Audit Logger

```python
#!/usr/bin/env python3
"""Log all tool and LLM calls to a JSONL file."""
import json, sys, datetime

payload = json.load(sys.stdin)
payload["timestamp"] = datetime.datetime.utcnow().isoformat()

with open("/var/log/crew-audit.jsonl", "a") as f:
    f.write(json.dumps(payload) + "\n")

sys.exit(0)
```
