/// Resolve the canonical `SessionManager` handle for read operations
/// (hydrate, state, etc.). Closes #919.1: turn persistence writes to
/// the profile's `SessionRuntime.sessions`, so reads under profile
/// auth MUST hit the same handle — otherwise `state.sessions` (the
/// top-level data-dir store) reports `unknown_session` on reconnect.
///
/// Returns the per-profile session manager if a `ProfileRuntime` is
/// registered for the resolved profile and the cache can bootstrap
/// a `SessionRuntime` for `session_id`. Falls back to
/// `state.sessions` so the legacy no-profile flow continues to work.
///
/// #924 BLOCK 4: the active-profile precedence MUST mirror
/// `handle_turn_start` — `session_id.profile_id()` first (the key
/// itself encodes the owner profile), then `connection_profile_id`
/// (token-auth scope), then `routed_profile_id` (host/header
/// routing). Without the `session_id` precedence + the routed
/// fallback, a host-routed admin session on a hosted subdomain
/// hydrated from `_main`/`state.sessions` instead of the right
/// profile runtime — and turns whose `SessionKey` already carried
/// `<profile>:api:...` resolved to a different store than the one
/// that persisted them.
pub(crate) async fn resolve_sessions_for_lookup(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    session_id: &SessionKey,
) -> Option<Arc<tokio::sync::Mutex<octos_bus::SessionManager>>> {
    resolve_sessions_for_mutation(
        state,
        connection_profile_id,
        routed_profile_id,
        session_id,
        false,
    )
    .await
    .ok()
    .flatten()
}

/// Delete needs only the authoritative transcript manager, never an executable
/// runtime. A corrupt permission record must not make the session undeletable.
pub(crate) async fn resolve_sessions_for_delete(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    session_id: &SessionKey,
) -> Option<Arc<tokio::sync::Mutex<octos_bus::SessionManager>>> {
    let open_binding_profile = session_open_profiles().lookup(session_id);
    let active_profile_id = session_id
        .profile_id()
        .or(connection_profile_id)
        .or(routed_profile_id)
        .or(open_binding_profile.as_deref());
    if let Some(profile_runtime) = resolve_session_profile_runtime(state, active_profile_id) {
        let workspace_profile_id = workspace_profile_scope(active_profile_id, session_id);
        let hint = session_workspaces().runtime_hint(&workspace_profile_id, session_id);
        if let Some(runtime) = state
            .session_cache
            .get_cached_with_hint(&profile_runtime, session_id.clone(), hint.clone())
            .await
        {
            return Some(runtime.sessions.clone());
        }
        let root = state
            .session_cache
            .sessions_root_for_hint(&profile_runtime, hint.as_deref());
        let manager = octos_bus::SessionManager::open(&root).ok()?;
        return Some(Arc::new(tokio::sync::Mutex::new(manager)));
    }
    state.sessions.clone()
}

/// Exact canonical mutations must not acknowledge a fallback-store write when
/// the authoritative profile's SessionRuntime failed to initialize.
pub(crate) async fn resolve_sessions_for_mutation(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    session_id: &SessionKey,
    strict: bool,
) -> Result<Option<Arc<tokio::sync::Mutex<octos_bus::SessionManager>>>, RpcError> {
    // The mutation caller has already authorized this explicit legacy key.
    // Its top-level store remains authoritative even with a profile store and
    // without a `_main` runtime; never substitute a routed tenant's runtime.
    if strict && session_id.profile_id().is_none() && session_id.channel() == "api" {
        return Ok(state.sessions.clone());
    }
    // #2162: last resort for a raw id on an unscoped, un-routed connection —
    // the profile a successful `session/open` resolved it under. Never
    // overrides the explicit scopes below.
    let open_binding_profile = session_open_profiles().lookup(session_id);
    let active_profile_id = session_id
        .profile_id()
        .or(connection_profile_id)
        .or(routed_profile_id)
        .or(open_binding_profile.as_deref());
    if let Some(profile_runtime) = resolve_session_profile_runtime(state, active_profile_id) {
        let workspace_profile_id = workspace_profile_scope(active_profile_id, session_id);
        let hint = session_workspaces().runtime_hint(&workspace_profile_id, session_id);
        let permissions_epoch = state.session_cache.session_generation(session_id);
        let permissions =
            effective_permissions_for_session(state, session_id, active_profile_id).await?;
        if let Ok(runtime) = state
            .session_cache
            .get_or_init_with_permissions(
                &profile_runtime,
                session_id.clone(),
                hint,
                permissions,
                permissions_epoch,
            )
            .await
        {
            return Ok(Some(runtime.sessions.clone()));
        }
        if strict {
            return Ok(None);
        }
    }
    if strict && state.profile_store.is_some() {
        return Ok(None);
    }
    Ok(state.sessions.clone())
}
