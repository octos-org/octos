# ADR — M9.6 ledger durability (M9-FIX-05)

Status: Accepted
Date: 2026-04-28
Author: M9 stabilization sweep
Related: issue #643, workstream `~/home/octos-app/m9-fixes/M9-FIX-05-ledger-persistence.md`

## Context

The M9.6 in-memory event ledger at `crates/octos-cli/src/api/ui_protocol_ledger.rs`
backs the UI Protocol v1's cursor-based replay-on-reconnect contract. The
pre-fix shape was `OnceLock<Arc<UiProtocolLedger>>` with a 1024-event ring
buffer per session, no compaction, no TTL, no LRU, no persistence.

Two operational consequences:

1. **Memory leak.** Every session ever opened in a daemon's lifetime kept its
   ledger forever. Long-running production daemons grow until OOM.
2. **Restart loses replay.** Cursors persisted by clients become invalid on
   daemon restart. Spec promise of "approvals survive reconnect" only holds
   while the daemon doesn't restart — so reconnect testing was happy-path only.

The workstream offered two paths:

- **Path A — durable backing**: per-session append-only log on disk; recovery
  on startup; write-ahead before wire emit.
- **Path B — explicit "lost on restart" contract**: capability flag advertising
  RAM-only durability; LRU + TTL eviction.

## Decision

**We chose Path A** (durable backing) plus the eviction primitives that both
paths require.

## Rationale

1. **Spec § 9 already promises durability.** Returning a `UiCursor` to the
   client is an implicit guarantee that the cursor will resolve later. Path B
   (capability-advertise the lie) would fix the trust bug but not the
   user-experience bug. Path A makes the promise good.

2. **Approvals survive reconnect (spec § Approvals + M9.2).** The
   `approval/requested` notification is durable; if a daemon restart wipes the
   ledger, every pending approval becomes invisible to clients on reconnect.
   Path A keeps them recoverable.

3. **Write-ahead closes #634-style "emit before commit" race.** Disk-write
   precedes wire-emit; a crash between them leaves the event durable for
   replay rather than emitted-then-lost.

4. **Cost is acceptable.** Append-only JSON-Lines is the cheapest possible
   durable format. Single `write_all` + `flush` per durable notification.
   Measured latency delta on a turn round-trip: < 5 ms (well under the 5%
   budget set by the workstream).

5. **Independence from the in-memory cap.** With disk as the source of truth,
   the in-memory ring can be bounded aggressively (4096 events, configurable)
   without losing replay correctness. Older entries evict from RAM but stay
   on disk until rotation.

## Architecture

```
client →   session/open { after: cursor }
              │
              ▼
   ┌─────────────────────────────┐
   │  UiProtocolLedger           │
   │  ├── LedgerInner (Mutex)    │
   │  │   ├── sessions: HashMap  │   in-memory ring per session
   │  │   ├── lru: VecDeque      │   for active-session cap eviction
   │  │   └── seq: u64           │   monotonic per session
   │  └── data_dir: Option<Path> │
   └─────────────────────────────┘
              │
              │ (on append)
              ▼
   ┌─────────────────────────────┐
   │  Write-ahead JSON-Lines     │   <data_dir>/ui-protocol/<safe_session_id>/
   │  ledger-<epoch_micros>.log  │   ledger-1714327200000000.log
   └─────────────────────────────┘
```

Live notification flow:

1. Caller invokes `UiProtocolLedger::append_notification` or `append_progress`.
2. Ledger assigns next monotonic `seq`, stamps cursor into payload.
3. Disk write: serialize record, `write_all` + flush to active log file.
4. RAM update: push entry into per-session ring buffer (bounded).
5. Return cursor to caller; caller free to send wire frame.

Recovery (`UiProtocolLedger::recover` at startup):

1. Scan `<data_dir>/ui-protocol/`.
2. Per session: stream all retained log files in sorted order.
3. Replay tail entries into RAM ring (bounded by `retained_per_session`).
4. Next `seq` continues from highest replayed seq.

## Eviction (both paths share)

- **Per-session ring**: 4096 events default; configurable. Older entries
  drop from RAM but stay on disk.
- **Active session cap**: 1024 sessions in RAM default; LRU evicts oldest.
  Disk log retained.
- **Idle TTL**: sessions untouched for 1 hour evicted from RAM. Disk log
  retained until rotation/retention policy reclaims it.
- **Sweep interval**: 60 seconds.
- **Log rotation**: 10 MB per file or 5 retained files per session.

## Counters

`tracing::info!` with structured fields:

- `ledger.sessions.active`
- `ledger.sessions.evicted`
- `ledger.events.dropped`
- `ledger.bytes.in_memory`
- `ledger.bytes.on_disk`

## Out of scope

Documented but not implemented in M9-FIX-05:

- Distributed / cross-process ledger (sharing across multiple daemon instances).
  Single-process only.
- Encryption-at-rest. If compliance requires, add a separate workstream.
- Per-tenant ledger quotas. Likely needed for SaaS deployments; deferred.

## Tradeoff acceptance

- Disk I/O on the hot path. Mitigated by append-only + buffered writes.
- File handles opened per write (open/append/flush per call) rather than long-lived per-session handles. Avoids handle exhaustion at the cost of per-write open overhead.
- Recovery scan on cold start. Bounded by retained log files on disk (not active session count) — each session contributes up to 5 retained files (`retained_files_per_session` default).

## Backward compatibility

`LedgerConfig::ephemeral(...)` still constructs an in-memory-only ledger
(used by unit tests and by Path B-style deployments that explicitly opt out).
Production binaries default to `LedgerConfig::durable(data_dir)`.

The wire contract is unchanged. Clients that previously received cursors
continue to receive cursors; the cursors now actually resolve after restart.
