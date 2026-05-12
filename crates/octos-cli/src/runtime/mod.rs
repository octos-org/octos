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
//! # Consolidation history
//!
//! M11-F finished the single-agent → profile-aware consolidation.
//! The pre-M11 entry points each ran their own ad-hoc per-profile
//! assembly; both `octos serve` and `octos gateway` now call
//! [`ProfileRuntime::bootstrap`] as the single per-profile
//! assembler. Per-session state — workspace_root, the workspace-
//! bound tool registry, the per-session Agent, the per-session
//! SessionManager — lives on [`SessionRuntime`] and is materialized
//! on demand via [`SessionRuntimeCache::get_or_init`].
//!
//! Every `/api/chat` and UI Protocol turn dispatcher resolves
//! through `state.profiles` + `state.session_cache` and fails
//! closed (503) on an unregistered profile — there is no server-
//! wide agent fallback. Gateway layers its gateway-specific
//! composition (`SwappableProvider`, `provider_router`,
//! `SwitchModelTool`, admin tools, auto-defer, `pipeline_factory`)
//! on top of the profile runtime; nothing duplicates the LLM/
//! credentials/skills/plugin/registry assembly the runtime owns.

pub mod cache;
pub mod profile;
pub mod session;

pub use cache::SessionRuntimeCache;
pub use profile::{BootstrapRole, ProfileRuntime};
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
