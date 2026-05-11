// The per-field doc blocks on `ProfileRuntime` / `SessionRuntime` use
// multi-paragraph bullet items by design — they're the contract M11-B
// and M11-C implement against, and collapsing to single-line bullets
// would lose the rationale. `cargo doc` renders them correctly; the
// continuation-indent lints would otherwise force a rewrite that
// trades readability for lint silence.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]

//! Runtime types for the M11 ProfileRuntime / SessionRuntime model.
//!
//! M11 replaces `octos serve`'s embedded server-wide `Agent` with two
//! first-class scopes, each backed by a long-lived runtime struct:
//!
//! - [`ProfileRuntime`] is the *profile scope*: one per `(host process,
//!   profile_id)` pair. It owns identity-shaped state — the LLM
//!   provider, credentials, registered skills, plugin-env template, tool
//!   policy, default sandbox, the base [`octos_agent::ToolRegistry`]
//!   template, and the per-profile memory stores. Anything that is an
//!   account property of the logged-in user lives here.
//! - [`SessionRuntime`] is the *session scope*: one per
//!   `(profile_id, session_key)` pair, cached by
//!   [`SessionRuntimeCache`]. It owns conversation-shaped state — the
//!   per-session `workspace_root`, the per-session plugin work dir, an
//!   effective sandbox config (which may override the profile default),
//!   a workspace-bound and policy-filtered clone of the profile's tool
//!   registry, the per-session [`octos_agent::Agent`], and the
//!   per-session [`octos_bus::SessionManager`]. Anything that can vary
//!   between two chats opened by the same logged-in user lives here.
//!
//! Every "is this thing per-profile or per-session?" question now has
//! one canonical answer. See `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md`
//! for the architectural rationale, the worked examples (web rooms,
//! coding-agent N-isolated-sessions, multi-TUI, gateway subprocess),
//! and the end-state acceptance checklist.
//!
//! # What this replaces
//!
//! When fully landed (M11-B/M11-C/M11-D), these types subsume:
//!
//! - `crate::commands::serve::try_create_agent` — the embedded
//!   server-wide agent constructor. Becomes
//!   [`SessionRuntime::bootstrap`].
//! - `crate::commands::serve::overlay_profile_llm` (+ the companion
//!   `populate_profile_credentials`) — the transient per-request
//!   `Config` overlay that retrofits profile awareness onto a globally
//!   scoped `Agent`. Becomes [`ProfileRuntime::bootstrap`].
//! - The per-profile bootstrap block in
//!   `crate::commands::gateway::gateway_runtime::run` (today roughly
//!   the LLM/QoS/credentials/skills/plugin/registry assembly between
//!   the bus startup and the actor-factory wiring). Becomes
//!   [`ProfileRuntime::bootstrap`] — gateway calls the same helper
//!   serve calls.
//!
//! # M11-A scope (this commit)
//!
//! Type signatures only. Function bodies are `todo!("M11-B implements
//! this")` or `todo!("M11-C implements this")`. Downstream workers
//! implement against the doc comments, not the bodies. The skeleton
//! must `cargo check` clean but makes no runtime decisions.

pub mod cache;
pub mod profile;
pub mod session;

pub use cache::SessionRuntimeCache;
pub use profile::ProfileRuntime;
pub use session::SessionRuntime;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Type-checks that the public surface of the runtime module
    /// compiles. M11-B and M11-C replace the `todo!()` bodies; this
    /// test exists so a regression in the type signatures (a field
    /// removed, a generic parameter changed, an import that no longer
    /// resolves) fails CI immediately instead of waiting for the next
    /// implementation phase to hit it.
    #[allow(dead_code)]
    fn _type_check() {
        fn _names<P, S, C>()
        where
            P: Sized,
            S: Sized,
            C: Sized,
        {
        }
        _names::<ProfileRuntime, SessionRuntime, SessionRuntimeCache>();
    }

    #[test]
    fn session_runtime_cache_stores_its_constructor_args() {
        // `new` is fully implemented in M11-A (it's a trivial
        // constructor); only `get_or_init` defers to M11-C. The cache
        // key shape `(String, SessionKey)` is part of the M11 contract
        // because dispatchers (M11-D) build that tuple from the
        // authenticated session before looking up a runtime.
        let cache = SessionRuntimeCache::new(64, Duration::from_secs(900));
        assert_eq!(cache.max_size(), 64);
        assert_eq!(cache.idle_ttl(), Duration::from_secs(900));
    }
}
