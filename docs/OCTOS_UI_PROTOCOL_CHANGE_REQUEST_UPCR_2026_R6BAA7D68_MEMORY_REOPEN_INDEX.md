# UPCR-2026-R6BAA7D68 — memory reopen search-index regression coverage

Status: proposed (test-only, no contract change)
PR: TBD (T-0423)

## Decision record

`UPCR-2026-R6BAA7D68`.

Test-only addition (`ui_protocol_memory_reopen_tests.rs`) pinning that
`memory/search` keeps returning ingested records after a session reopen,
matching `memory/load` by id: index rebuild from persisted records,
retire/bootstrap_replacing consistency, embedder-id-change BM25 fallback,
and hybrid search returning vector-less records. No wire shape, method, or
permission change.
