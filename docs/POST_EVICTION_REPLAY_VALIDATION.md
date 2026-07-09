# Post-Eviction Replay Validation

Issue #1280 tracks a safe way to prove AppUI replay after the target
session has been evicted from the in-memory ledger cache. The default
live soak did not prove that path because the production active-session
cap is high enough that its filler sessions did not evict the target.

Use the non-production validator:

```sh
npm --prefix e2e run soak:m11:post-eviction-replay
```

The validator runs one focused Rust test with a temporary durable ledger
directory and a one-session active cache cap. It proves:

- the target session writes a JSONL ledger under `ui-protocol/<session>/`;
- a filler session evicts the target from memory before reconnect;
- `snapshot_with_cursor`, the production hydrate path, reloads the target
  from persisted JSONL and returns only events after the supplied cursor.

The script does not start, restart, or reconfigure production daemons, and
it does not change `idle_ttl`. It writes build output only under
`CARGO_TARGET_DIR`, defaulting to `/private/tmp/octos-post-eviction-replay-target`.
