//! Read-only catalog across authorized profiles and known workspace stores.
//! Workspace hints are locations to query, never authority or cached sessions.
use super::*;

#[derive(Default, Deserialize)]
struct Params {
    #[serde(default)]
    workspaces: Vec<PathBuf>,
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    limit: Option<usize>,
}

pub(super) async fn list(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: Params = parse_raw_params(request)?;
    if params.workspaces.len() > 128 {
        return Err(RpcError::invalid_params(
            "At most 128 workspace hints are accepted",
        ));
    }
    let store = profile_store(state)?;
    let selected = match params.profile_id {
        Some(id) => Some(raw_profile_skill_profile_id(
            Some(id),
            connection_profile_id,
        )?),
        None => connection_profile_id.map(str::to_owned),
    };
    let profiles = store
        .list()
        .map_err(|e| RpcError::internal_error(e.to_string()))?
        .into_iter()
        .filter(|p| selected.as_ref().is_none_or(|id| id == &p.id))
        .collect::<Vec<_>>();
    let hints = params.workspaces;
    let mut stores = Vec::new();
    let mut unavailable = Vec::new();
    let mut scanned = Vec::new();
    for profile in profiles {
        let data_dir = store.resolve_data_dir(&profile);
        stores.push((
            profile.id.clone(),
            data_dir.clone(),
            None,
            data_dir.join("peers"),
        ));
        if !state.session_cache.sessions_in_cwd() {
            continue;
        }
        let mut roots = hints.clone();
        roots.extend(
            crate::runtime::workspace_history::load(&data_dir).map_err(|e| {
                RpcError::internal_error(format!("Cannot read workspace history: {e}"))
            })?,
        );
        {
            let workspaces = session_workspaces();
            let entries = workspaces.entries.lock().unwrap_or_else(|e| e.into_inner());
            roots.extend(
                entries
                    .iter()
                    .filter(|((id, session), _)| {
                        id == &profile.id
                            && !crate::peers::app_binding::resolve_session_app_binding(
                                &data_dir.join("peers"),
                                session,
                            )
                            .is_bound_or_refused()
                    })
                    .filter_map(|(_, binding)| binding.runtime_hint.clone()),
            );
        }
        roots.sort();
        roots.dedup();
        for root in &roots {
            let canonical = match dunce::canonicalize(root) {
                Ok(p)
                    if validate_session_workspace_allowed(state, Some(&profile.id), &p).is_ok() =>
                {
                    p
                }
                _ => {
                    unavailable.push(root.to_string_lossy().into_owned());
                    continue;
                }
            };
            let project = crate::runtime::session::project_sessions_root(&canonical, &profile.id);
            scanned.push(canonical.to_string_lossy().into_owned());
            if project.join("sessions").is_dir() {
                // A validated existing store becomes discoverable after a restart.
                // Never materialize a project or remember a missing store on a read.
                if let Err(error) =
                    crate::runtime::workspace_history::remember(&data_dir, &canonical)
                {
                    tracing::warn!(%error, "could not remember session-history workspace");
                }
                stores.push((
                    profile.id.clone(),
                    project,
                    Some(canonical),
                    data_dir.join("peers"),
                ));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    stores.retain(|(profile, root, _, _)| seen.insert((profile.clone(), root.clone())));
    let busy = active_turn_sessions(&active_turns_registry()).await;
    let offset = params.offset;
    let limit = params.limit.unwrap_or(100).clamp(1, 200);
    let rows = tokio::task::spawn_blocking(move || {
        let mut rows = Vec::new();
        for (profile, root, workspace, peers) in stores {
            if !root.join("sessions").is_dir() {
                continue;
            }
            for row in crate::api::handlers::list_profile_sessions(&root, &busy, &profile) {
                let id = history_session_id(&profile, &row.id);
                // Host-owned peer/context history is not a general account catalog.
                if crate::peers::app_binding::resolve_session_app_binding(
                    &peers,
                    &SessionKey(id.clone()),
                )
                .is_bound_or_refused()
                {
                    continue;
                }
                if let Ok(mut value) = serde_json::to_value(row) {
                    value["id"] = json!(id);
                    value["profile_id"] = json!(profile);
                    value["workspace_root"] = json!(workspace);
                    rows.push(value);
                }
            }
        }
        rows.sort_by(|a, b| {
            b["updated_at"]
                .as_str()
                .cmp(&a["updated_at"].as_str())
                .then_with(|| a["profile_id"].as_str().cmp(&b["profile_id"].as_str()))
                .then_with(|| {
                    a["workspace_root"]
                        .as_str()
                        .cmp(&b["workspace_root"].as_str())
                })
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        });
        rows.dedup_by(|a, b| {
            a["id"] == b["id"]
                && a["profile_id"] == b["profile_id"]
                && a["workspace_root"] == b["workspace_root"]
        });
        let total = rows.len();
        let page: Vec<_> = rows.into_iter().skip(offset).take(limit).collect();
        (total, page)
    })
    .await
    .map_err(|e| RpcError::internal_error(e.to_string()))?;
    scanned.sort();
    scanned.dedup();
    unavailable.sort();
    unavailable.dedup();
    let next = offset.saturating_add(rows.1.len());
    Ok(
        json!({"sessions":rows.1,"total":rows.0,"next_offset":(next < rows.0).then_some(next),
        "workspaces":scanned,"unavailable_workspaces":unavailable,"coverage":"profile_and_known_workspaces"}),
    )
}

fn history_session_id(profile: &str, id: &str) -> String {
    if id.starts_with(&format!("{profile}:")) {
        id.to_owned()
    } else if id == "main" {
        format!("{profile}:main")
    } else {
        format!("{profile}:api:{id}")
    }
}
