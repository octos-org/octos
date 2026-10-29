# UPCR-2026-059 — Serve Cron Delivery Through Session Continuations

Status: proposed (implemented for review)
Task: T-0014 PR1

## Scope

Fix the serve cron delivery bug without adding a new client-facing method. The internal continuation-dispatch path is covered because `cron_fire` becomes a real session turn, while the gateway's inbound-message behavior is unchanged. UPCR-2026-058 remains reserved for the separate `cron/create` / `cron/delete` PR.

## Delivery contract

- An occurrence is committed only after the exact session/profile's durable continuation queue accepts it. One-shot deletion and `ok` happen at that point; a later turn failure does not re-fire the occurrence.
- Delivery refusal does not record success or delete the job. It retains `last_error` and retries at most 5 times at 15-minute intervals. After the fifth refusal, the job remains stored and disabled.
- The original scheduled fire time is retained separately from retry backoff and participates in occurrence deduplication.
- A job already due at startup is delivered once and marked late. Recurring jobs do not replay every missed interval. Ordinary timer delivery has a 60-second grace window; only startup catch-up or a delayed retry beyond that window is marked late.
- Per-session cron-tool origins carry the exact session/profile; an unopened workspace or mismatched profile cannot receive the occurrence.

## Implementation references

- `crates/octos-bus/src/cron_service.rs`
- `crates/octos-cli/src/runtime/profile.rs`
- `crates/octos-cli/src/autonomy/agent_orchestrator.rs`
- `docs/SERVE_CRON_DELIVERY.md`
