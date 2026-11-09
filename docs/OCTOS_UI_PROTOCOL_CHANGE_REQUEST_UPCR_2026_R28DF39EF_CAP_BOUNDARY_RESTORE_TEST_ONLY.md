# UPCR-2026-R28DF39EF — spawn-report exact-cap boundary restore (test-only)

Status: proposed
PR: #3166

## Decision record

This PR makes NO protocol-visible change. It edits only `ui_protocol_tests.rs`
(a test-only file matched by the UPCR gate's glob), restoring the
`spawn_report_announcement_cap_boundary_is_exact_and_over_cap_recovers` test
(table-driven exact-cap/off-by-one + over-cap truncation with the recovery
pointer) that #3144's keep-list deletion removed. The spawn-report
announcement behaviour is unchanged; this only re-pins its exact cap boundary.
Cite this record elsewhere as `UPCR-2026-R28DF39EF`.
