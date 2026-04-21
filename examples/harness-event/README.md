# Octos Harness Event Emitters

These copyable helpers emit `octos.harness.event.v1` JSON records for non-Rust
tools. They write to `OCTOS_EVENT_SINK` when it is set and do nothing when the
sink is missing.

The contract is:

- `stderr` is for diagnostics only
- the helper does not validate record size or schema shape
- the runtime is responsible for consuming and rejecting bad records

## Python

```bash
export OCTOS_EVENT_SINK="file:///tmp/octos-events.jsonl"
python3 examples/harness-event/python/emit_progress.py \
  --session-id sess-123 \
  --task-id task-456 \
  --workflow deep_research \
  --phase fetching_sources \
  --message "Fetching source 3/12" \
  --progress 0.42
```

## JavaScript / Node

```bash
export OCTOS_EVENT_SINK="file:///tmp/octos-events.jsonl"
node examples/harness-event/node/emit_progress.mjs \
  --session-id sess-123 \
  --task-id task-456 \
  --workflow deep_research \
  --phase fetching_sources \
  --message "Fetching source 3/12" \
  --progress 0.42
```

## Validation

Run the fixture test from the repo root:

```bash
bash scripts/test-harness-event-emitters.sh
```

