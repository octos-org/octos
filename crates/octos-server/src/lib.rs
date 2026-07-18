//! `octos-server` — the HTTP/WebSocket API server and session runtime for octos.
//!
//! Extracted from `octos-cli` (see `docs/OCTOS_SERVER_EXTRACTION_PLAN.md`). The
//! server-core (session actor, gateway runtime) is **ungated**; the HTTP/WS layer
//! lives behind the **`api`** feature so non-API `octos gateway` still builds
//! without pulling in axum/tower/rustls/etc.
//!
//! ## Stage 1 (this crate, now): scaffold only
//! Intentionally empty. This establishes the crate, the ungated-core + `api`
//! feature split, the channel-feature forwarding, and the dependency graph so the
//! structure is reviewable BEFORE the ~146k-LOC server slice moves.
//!
//! ## Stage 2 (next): the server slice moves here
//! ```ignore
//! pub mod session_actor;                 // ungated server-core
//! #[cfg(feature = "api")] pub mod api;   // HTTP/WS transport + handlers
//! // + context_manager, agent_orchestrator, serve/gateway bootstrap,
//! //   and the provider/prompt/skills helper impls pulled from commands.
//! ```
//! Process-global state (`metrics::METRICS_HANDLE`, the orchestrator, ui-protocol
//! `OnceLock` stores) and `static/` assets move atomically with it.
