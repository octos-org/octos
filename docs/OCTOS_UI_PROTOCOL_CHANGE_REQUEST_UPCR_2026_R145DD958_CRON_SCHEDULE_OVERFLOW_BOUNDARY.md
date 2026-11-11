# UPCR-2026-R145DD958 — cron Every/At overflow and 2099 boundary

Status: proposed
PR: T-0290

## Decision record

`cron_types.rs` computed `Some(base + every_ms)`, which panics in debug and
wraps to a negative immediately-due time in release for `every_ms = i64::MAX`.
The At/Every creation entries (WS cron_panel, CLI `--every`/`--at`, agent
`every_seconds`/`after_seconds`/`at_ms`, and the Matrix/natural-language
`/schedule` parsers) shared no boundary, so an unschedulable interval could be
persisted. This adds shared `is_supported_cron_run_at_ms` (2099-12-31 product
horizon) and `next_every_run_at_ms` (checked_add + boundary, `None` marks an
unschedulable interval), used by every creation entry and by
`compute_next_run`. Invalid input is rejected BEFORE any disk write; valid
intervals and existing Cron expressions are unchanged. Cite this record
elsewhere as `UPCR-2026-R145DD958`.
