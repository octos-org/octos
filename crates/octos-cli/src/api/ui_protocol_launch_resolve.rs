/// `launch/resolve` — the pre-session launch probe. Resolves the launching
/// profile (requested `--profile` → folder-sticky → global default) and reports
/// whether the client should resume the folder's conversation, activate a new
/// one, or choose among profiles. Delegates to the tested
/// [`crate::runtime::launch`] decision core.
async fn handle_launch_resolve(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    params: octos_core::ui_protocol::LaunchResolveParams,
) {
    handle_launch_resolve_with_sticky_policy(
        ws,
        state,
        connection_profile_id,
        connection_profile_id.is_none(),
        features,
        id,
        params,
    )
    .await
}

async fn handle_launch_resolve_with_sticky_policy(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    admin_owned_sticky: bool,
    features: ConnectionUiFeatures,
    id: String,
    params: octos_core::ui_protocol::LaunchResolveParams,
) {
    match resolve_launch_result_with_sticky_policy(
        state,
        connection_profile_id,
        admin_owned_sticky,
        features,
        &params,
    ) {
        Ok(result) => {
            let body = serde_json::to_value(&result).unwrap_or_else(|_| json!({}));
            send_aux_rpc_result(
                ws,
                id,
                octos_core::ui_protocol::methods::LAUNCH_RESOLVE,
                body,
            );
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

/// Resolution core for [`handle_launch_resolve`], split out so the error paths
/// are a single `?`-chain. Applies the SAME cwd safety gates as `session/open`
/// and `session/list` before scanning the project's `.octos` store.
#[cfg(test)]
fn resolve_launch_result(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    params: &octos_core::ui_protocol::LaunchResolveParams,
) -> Result<octos_core::ui_protocol::LaunchResolveResult, RpcError> {
    resolve_launch_result_with_sticky_policy(
        state,
        connection_profile_id,
        connection_profile_id.is_none(),
        features,
        params,
    )
}

fn resolve_launch_result_with_sticky_policy(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    admin_owned_sticky: bool,
    features: ConnectionUiFeatures,
    params: &octos_core::ui_protocol::LaunchResolveParams,
) -> Result<octos_core::ui_protocol::LaunchResolveResult, RpcError> {
    use crate::runtime::launch::{
        LaunchDecision, resolve_admin_launch_decision, resolve_launch_decision,
        scan_folder_sessions,
    };
    use octos_core::ui_protocol::{LaunchDecisionKind, LaunchResolveResult};

    // The method is advertised only when `session.workspace_cwd.v1` is
    // negotiated; reject a client that calls it without the feature.
    if !features.session_workspace_cwd {
        return Err(RpcError::invalid_params(
            "launch/resolve requires feature session.workspace_cwd.v1",
        )
        .with_data(json!({
            "kind": "feature_required",
            "feature": UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
        })));
    }

    let cwd = params.cwd.trim();
    if cwd.is_empty() {
        return Err(RpcError::invalid_params(
            "launch/resolve requires a non-empty cwd",
        ));
    }
    // Path-safety ONLY (canonicalize + banned-root reject) — the SAME check the
    // write path runs before materializing `<cwd>/.octos`. Crucially NOT the
    // full `validate_session_workspace_allowed`, which additionally requires a
    // materialized `SessionRuntime` for the routed profile: launch/resolve is a
    // read-only decision that runs BEFORE any session is opened (and, in solo
    // `--stdio`, before any profile runtime is lazily bootstrapped), so gating
    // it on a loaded runtime made every Resume / CrossProfile / NoProfile
    // outcome unreachable — the folder always fell through to `activate`, or a
    // bare launch got a spurious `cwd_runtime_unavailable`. Found by the
    // launch-flow soak against a real `octos serve --stdio`.
    let workspace_root = canonical_existing_dir(cwd)?;
    validate_session_workspace_path_safety(&workspace_root)?;
    if let (Some(authenticated), Some(requested)) =
        (connection_profile_id, params.profile_id.as_deref())
        && requested != authenticated
    {
        return Err(authenticated_scope_mismatch_error(
            "profile_id is outside the authenticated profile",
            authenticated,
            Some(requested),
        ));
    }

    // Known profiles = every launchable brain that EXISTS, sourced from the
    // persistent `ProfileStore` (enabled, top-level), NOT just `state.profiles`
    // — the in-memory runtime map, which in solo `--stdio` is populated lazily
    // on `session/open` and so is EMPTY at bare-launch time. A profile is
    // launchable whether or not its runtime is loaded yet, so the scanner must
    // see its folder store and the default resolver must be able to name it.
    // Unioned with `state.profiles.keys()` so eager-load deployments stay
    // covered. A since-deleted profile's leftover store dir is ignored (the
    // scanner is bounded by this set). `BTreeSet` yields a sorted, deduped
    // listing for a deterministic default + cross-profile result. Empty (no
    // profiles) → `NoProfile`.
    let mut known_set: std::collections::BTreeSet<String> =
        state.profiles.keys().cloned().collect();
    if let Some(store) = state.profile_store.as_ref() {
        if let Ok(profiles) = store.list() {
            for profile in profiles {
                if profile.enabled && profile.parent_id.is_none() {
                    known_set.insert(profile.id);
                }
            }
        }
    }
    let known_profiles: Vec<String> = known_set.into_iter().collect();
    if let Some(authenticated) = connection_profile_id
        && !known_profiles.iter().any(|known| known == authenticated)
    {
        // An authenticated but absent/disabled profile cannot be activated or
        // opened. Keep the caller in onboarding instead of naming another
        // profile or returning an activation that would immediately fail.
        return Ok(octos_core::ui_protocol::LaunchResolveResult {
            decision: LaunchDecisionKind::NoProfile,
            resolved_profile: None,
            existing_profiles: Vec::new(),
        });
    }

    // Global default. An explicit user-chosen `default-profile` pointer (set via
    // the onboarding "make default" flow) wins when it still names a known
    // profile; otherwise fall back to the connection's own profile when
    // registered, else `_main` when present, else the first known profile.
    // `None` (→ `NoProfile`) only when no profile exists at all.
    let persisted_default = profile_store(state)
        .ok()
        .and_then(|store| store.default_profile())
        .filter(|candidate| known_profiles.iter().any(|known| known == candidate));
    let default_profile: Option<String> = persisted_default
        .or_else(|| {
            connection_profile_id
                .map(str::to_string)
                .filter(|candidate| known_profiles.iter().any(|known| known == candidate))
        })
        .or_else(|| {
            known_profiles
                .iter()
                .find(|known| known.as_str() == MAIN_PROFILE_ID)
                .cloned()
        })
        .or_else(|| known_profiles.first().cloned());

    let mut folder = scan_folder_sessions(&workspace_root, &known_profiles);
    if let Some(authenticated) = connection_profile_id {
        // A profile-scoped identity may only see and select its own history.
        // Do not offer a foreign sticky/default/existing id that session/open
        // would reject immediately after this probe.
        folder
            .profiles
            .retain(|profile| profile.profile_id == authenticated);
        folder.active_profile = folder
            .active_profile
            .filter(|profile| profile == authenticated);
    }
    let requested_profile = params.profile_id.as_deref().or(connection_profile_id);
    let decision = if admin_owned_sticky {
        resolve_admin_launch_decision(requested_profile, default_profile.as_deref(), &folder)
    } else {
        resolve_launch_decision(requested_profile, default_profile.as_deref(), &folder)
    };

    Ok(match decision {
        LaunchDecision::Resume { profile_id } => LaunchResolveResult {
            decision: LaunchDecisionKind::Resume,
            resolved_profile: Some(profile_id),
            existing_profiles: Vec::new(),
        },
        LaunchDecision::NeedsActivation { profile_id } => LaunchResolveResult {
            decision: LaunchDecisionKind::Activate,
            resolved_profile: Some(profile_id),
            existing_profiles: Vec::new(),
        },
        LaunchDecision::CrossProfile {
            launching_profile,
            existing_profiles,
        } => LaunchResolveResult {
            decision: LaunchDecisionKind::CrossProfile,
            resolved_profile: Some(launching_profile),
            existing_profiles,
        },
        LaunchDecision::NoProfile => LaunchResolveResult {
            decision: LaunchDecisionKind::NoProfile,
            resolved_profile: None,
            existing_profiles: Vec::new(),
        },
    })
}

