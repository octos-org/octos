//! Goal / autonomy state engine.
//!
//! This cluster used to live under the `api`-gated `crate::api` tree even
//! though none of it touches axum, `AppState`, or the WebSocket transport —
//! it only ever shared a directory with them. Keeping it there meant the goal
//! tooling (`crate::goal_tool`) and the SessionActor's goal glue could not
//! compile unless the `api` feature was on, which blocked `octos chat` from
//! driving the same engine `octos serve` does.
//!
//! Nothing here is web/transport-specific, so the whole subgraph is
//! unconditionally compiled. `crate::api` consumes these modules through
//! their `crate::autonomy::…` paths.

pub(crate) mod agent_orchestrator;
pub(crate) mod fleet_wake;
pub(crate) mod goal_loop_runtime;
pub(crate) mod human_events;
pub(crate) mod master_continuation_scheduler;
pub(crate) mod monitor_runtime;
pub(crate) mod specialist_runner;
pub(crate) mod supervisor_store;

/// Stable, collision-resistant filename-safe hash of a session id.
/// Renders as 16 hex chars (64 bits) from `DefaultHasher` (SipHash-1-3).
///
/// Lives here (rather than in `api::ui_protocol`, where it was defined)
/// because `monitor_runtime`'s durable note channel derives its filenames
/// from the SAME hash as the goal-notes channel, and `monitor_runtime` is no
/// longer `api`-gated. `api::ui_protocol` re-exports it so its own call sites
/// keep resolving unchanged.
///
/// # Caveats
///
/// - NOT cryptographic. SipHash's collision resistance is adequate for a
///   profile-private inbox dir under a single-process trust boundary, but
///   an adversary who can choose session ids AND access the inbox dir
///   could force collisions. If that threat model changes, swap to a
///   cryptographic hash (sha2::Sha256) or a collision-free encoding
///   (percent-encoding the raw session id).
/// - `DefaultHasher`'s algorithm is NOT guaranteed stable across Rust
///   releases. A pending `.notes` file written under release N may become
///   orphaned (unfindable by release N+1). The wake is best-effort (the
///   goal ledger is the source of truth), so this is acceptable — but do
///   NOT rely on this filename for anything durable.
pub(crate) fn hash_session_for_inbox(session_id: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    session_id.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
