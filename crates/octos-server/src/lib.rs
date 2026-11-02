//! `octos-server` — the HTTP/WebSocket API server and session runtime for octos.
//!
//! Planned extraction from `octos-cli`. The server-core (session actor, gateway
//! runtime) is intended to be **ungated**; the HTTP/WS layer is intended to live
//! behind the **`api`** feature so non-API `octos gateway` can build without
//! pulling in axum/tower/rustls/etc.
//!
//! ## Stage 1 (this crate, now): scaffold only
//! Intentionally empty: no server runtime is exported here yet, and no crate
//! consumes `octos-server`. This establishes only the crate, feature split,
//! channel-feature forwarding, and dependency declarations. The runtime still
//! lives in `octos-cli`; see `docs/ARCHITECTURE.md` for the current boundary.
//!
//! ## Stage 2 (candidate next step): the server slice moves here
//! ```ignore
//! pub mod session_actor;                 // ungated server-core
//! #[cfg(feature = "api")] pub mod api;   // HTTP/WS transport + handlers
//! // + context_manager, agent_orchestrator, serve/gateway bootstrap,
//! //   and the provider/prompt/skills helper impls pulled from commands.
//! ```
//! Process-global state (`metrics::METRICS_HANDLE`, the orchestrator, ui-protocol
//! `OnceLock` stores) and `static/` assets move atomically with it.
