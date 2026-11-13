# UPCR-2026-R232AAE8F — Transport DTO Extraction

Status: proposed (implemented for review)
PR: #3232

## Decision record

UPCR-2026-R232AAE8F records a pure module-boundary move with no UI Protocol
wire, capability, ledger, cursor, terminal-delivery, backpressure, or runtime
contract change. Thirty-seven private `Raw*Params` and related wire-data DTOs
move from `ui_protocol_transport.rs` into `ui_protocol_transport/dto.rs` and
are re-exported through the existing module path.

Serde field names, defaults, validation methods, dispatch, WS/stdio lifecycle,
terminal uniqueness, cursor ordering, and the embedded worker-stack behavior
remain in their original implementations and are byte-identical. The only
source changes beyond the move are `pub(crate)` visibility required by the new
module boundary and rustfmt line wrapping.
