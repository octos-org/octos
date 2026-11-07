# UPCR-2026-RE6926211 — peer/gather rows carry the staging originator session id

Status: proposed (additive optional field, backward compatible)
PR: TBD (T-0454)

## Decision record

`UPCR-2026-RE6926211`.

`peer/gather` rows gain an `originator` field: the trimmed staging session
id from `peers/<slug>/originator` (read via the existing
`peer_io::read_peer_identity_file`, same no-follow/capped reader as the
adopt cross-check), `null` when the record is missing or invalid. It is
best-effort attribution for UI regrouping after a page refresh (restore a
peer under its originating master session); it does NOT feed the trusted
`master_session_id` lifetime projection. CLI `peer list` and the
`peer_gather` tool output are unchanged. Additive and optional — older
clients ignore it.
