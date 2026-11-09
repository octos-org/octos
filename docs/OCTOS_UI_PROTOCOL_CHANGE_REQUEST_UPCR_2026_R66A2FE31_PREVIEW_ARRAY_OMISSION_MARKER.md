# UPCR-2026-R66A2FE31 — bounded array previews carry an omission marker

Status: proposed
PR: #3143

## Decision record

When the bounded frame preview must shrink an oversized records/items array
(structural fallback), the dropped middle is no longer removed silently. One
omission-marker element `__octos_preview_omitted__{count,head_kept,tail_kept}`
is inserted at the drop point, carrying the cumulative dropped count and the
kept head/tail sizes, so a client can render "N rows omitted" instead of
mistaking the bounded preview for the complete history. Multi-round shrinks
fold the prior marker's count into the new one. Additive and old-client
compatible: pre-existing clients see one extra object row where the gap was,
not silent data loss. The full hydrate still honors the existing 1 MiB frame
budget. Cite this record elsewhere as `UPCR-2026-R66A2FE31`.
