use super::*;

use octos_core::ui_protocol::{CatalogBootstrapParams, SessionRefRecord};
use serde_json::{Value, json};
use tokio::sync::mpsc;

fn save_profile(
    store: &crate::profiles::ProfileStore,
    root: &Path,
    profile_id: &str,
    parent_id: Option<&str>,
) {
    store
        .save(&crate::profiles::UserProfile {
            id: profile_id.to_owned(),
            name: profile_id.to_owned(),
            public_subdomain: None,
            enabled: true,
            data_dir: Some(
                root.join("profile-data")
                    .join(profile_id)
                    .to_string_lossy()
                    .into_owned(),
            ),
            parent_id: parent_id.map(str::to_owned),
            config: Default::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .unwrap();
}

fn profile_state(root: &Path, profiles: &[(&str, Option<&str>)]) -> AppState {
    let mut state = AppState::empty_for_tests();
    state.workspace_allowed_roots = vec![root.to_owned()];
    let store = Arc::new(crate::profiles::ProfileStore::open_unified(root).unwrap());
    for (profile_id, parent_id) in profiles {
        save_profile(&store, root, profile_id, *parent_id);
    }
    state.profile_store = Some(store);
    state.sessions = Some(Arc::new(tokio::sync::Mutex::new(
        octos_bus::SessionManager::open(root).unwrap(),
    )));
    state
}

fn info(id: &str, updated_at: &str) -> SessionInfo {
    SessionInfo {
        id: id.to_owned(),
        message_count: 1,
        title: Some(id.to_owned()),
        updated_at: Some(updated_at.to_owned()),
        last_prompt: None,
        active_turn: false,
    }
}

fn params(limit: Option<u32>) -> CatalogBootstrapParams {
    CatalogBootstrapParams {
        limit,
        ..CatalogBootstrapParams::default()
    }
}

fn bootstrap(
    state: &AppState,
    profiles: &[&str],
    params: CatalogBootstrapParams,
) -> octos_core::ui_protocol::CatalogBootstrapResult {
    let profiles = profiles
        .iter()
        .map(|profile| (*profile).to_owned())
        .collect::<Vec<_>>();
    super::catalog_bootstrap(state, &profiles, HashMap::new(), &HashSet::new(), params).unwrap()
}

fn refs(records: &[SessionRefRecord]) -> Vec<&str> {
    records
        .iter()
        .map(|record| record.session_ref.as_str())
        .collect()
}

#[test]
fn catalog_bootstrap_should_return_an_empty_session_page_with_workspace_summaries() {
    let temp = tempfile::tempdir().unwrap();
    let state = profile_state(temp.path(), &[("main", None)]);
    let result = bootstrap(&state, &["main"], params(None));
    assert!(result.workspaces.is_empty());
    assert!(result.sessions.is_empty());
    assert_eq!(result.next_cursor, None);
    assert_eq!(result.connection_scope.profile_ids, ["main"]);
    assert_eq!(result.summary.workspace_count, 0);
    assert_eq!(result.summary.session_count, 0);
    assert_eq!(result.summary.returned_count, 0);
    assert!(!result.summary.has_more);

    let root = temp.path().join("empty-workspace");
    std::fs::create_dir(&root).unwrap();
    let workspace = super::super::workspace_registry::register_workspace(
        &state,
        "main",
        root.to_str().unwrap(),
        None,
    )
    .unwrap();
    let result = bootstrap(&state, &["main"], params(None));
    assert_eq!(result.workspaces.len(), 1);
    assert_eq!(result.workspaces[0].workspace_id, workspace.workspace_id);
    assert!(result.sessions.is_empty());
    assert_eq!(result.summary.workspace_count, 1);
    assert_eq!(result.summary.session_count, 0);
}

#[test]
fn catalog_bootstrap_should_page_globally_and_survive_a_cold_reload() {
    let temp = tempfile::tempdir().unwrap();
    let state = profile_state(temp.path(), &[("main", None), ("other", None)]);
    let workspace_a_root = temp.path().join("workspace-a");
    let workspace_b_root = temp.path().join("workspace-b");
    std::fs::create_dir(&workspace_a_root).unwrap();
    std::fs::create_dir(&workspace_b_root).unwrap();
    let workspace_a = super::super::workspace_registry::register_workspace(
        &state,
        "main",
        workspace_a_root.to_str().unwrap(),
        None,
    )
    .unwrap();
    let workspace_b = super::super::workspace_registry::register_workspace(
        &state,
        "main",
        workspace_b_root.to_str().unwrap(),
        None,
    )
    .unwrap();

    let shared = SessionKey("api:shared".to_owned());
    let first_ref = super::super::workspace_registry::ensure_session_ref(
        &state,
        "main",
        &shared,
        Some(&workspace_a),
        &info("api:shared", "2026-01-04T00:00:00Z"),
        false,
    )
    .unwrap();
    let second_ref = super::super::workspace_registry::ensure_session_ref(
        &state,
        "main",
        &shared,
        Some(&workspace_b),
        &info("api:shared", "2026-01-04T00:00:00Z"),
        false,
    )
    .unwrap();
    assert_ne!(first_ref.session_ref, second_ref.session_ref);
    let other_ref = super::super::workspace_registry::ensure_session_ref(
        &state,
        "other",
        &SessionKey("api:other".to_owned()),
        None,
        &info("api:other", "2026-01-03T00:00:00Z"),
        false,
    )
    .unwrap();

    let first = bootstrap(&state, &["main", "other"], params(Some(2)));
    assert_eq!(first.sessions.len(), 2);
    assert_eq!(first.summary.session_count, 3);
    assert_eq!(first.summary.returned_count, 2);
    assert!(first.summary.has_more);
    assert!(first.next_cursor.is_some());
    let mut first_ids = first
        .sessions
        .iter()
        .map(|record| record.workspace_id.clone().unwrap())
        .collect::<Vec<_>>();
    first_ids.sort();
    let mut expected_ids = vec![
        workspace_a.workspace_id.clone(),
        workspace_b.workspace_id.clone(),
    ];
    expected_ids.sort();
    assert_eq!(first_ids, expected_ids);
    assert_eq!(first.sessions[0].profile_id, "main");
    assert_eq!(first.sessions[1].profile_id, "main");

    // A newly inserted row ahead of the first page must not shift the cursor.
    super::super::workspace_registry::ensure_session_ref(
        &state,
        "main",
        &SessionKey("api:newer".to_owned()),
        None,
        &info("api:newer", "2026-01-05T00:00:00Z"),
        false,
    )
    .unwrap();
    let second = bootstrap(
        &state,
        &["main", "other"],
        CatalogBootstrapParams {
            cursor: first.next_cursor.clone(),
            ..params(Some(2))
        },
    );
    assert_eq!(second.sessions.len(), 1);
    assert_eq!(second.sessions[0].session_ref, other_ref.session_ref);
    assert_eq!(second.next_cursor, None);
    assert_eq!(second.summary.session_count, 4);

    // A cold state and a new client see the same server-minted identities;
    // nothing depends on browser-local recents or process memory.
    drop(state);
    let reloaded = profile_state(temp.path(), &[("main", None), ("other", None)]);
    let cold = bootstrap(&reloaded, &["main", "other"], params(Some(10)));
    let cold_refs = refs(&cold.sessions);
    assert!(cold_refs.contains(&first_ref.session_ref.as_str()));
    assert!(cold_refs.contains(&second_ref.session_ref.as_str()));
    assert!(cold_refs.contains(&other_ref.session_ref.as_str()));
    assert_eq!(cold.summary.session_count, 4);
    let resumed = super::super::workspace_registry::resolve_session_ref(
        &reloaded,
        None,
        &second_ref.session_ref,
    )
    .unwrap();
    assert_eq!(resumed.session_id, "api:shared");
    assert_eq!(resumed.workspace_id, Some(workspace_b.workspace_id));
}

#[test]
fn catalog_bootstrap_should_default_to_fifty_rows() {
    let temp = tempfile::tempdir().unwrap();
    let state = profile_state(temp.path(), &[("main", None)]);
    let entries = (0..51)
        .map(|index| {
            let id = format!("api:row-{index:02}");
            (
                SessionKey(id.clone()),
                None,
                info(&id, "2026-01-04T00:00:00Z"),
            )
        })
        .collect();
    super::super::workspace_registry::index_session_refs(&state, "main", entries).unwrap();
    let first = bootstrap(&state, &["main"], params(None));
    assert_eq!(first.sessions.len(), 50);
    assert_eq!(first.summary.session_count, 51);
    assert_eq!(first.summary.returned_count, 50);
    assert!(first.summary.has_more);
    let second = bootstrap(
        &state,
        &["main"],
        CatalogBootstrapParams {
            cursor: first.next_cursor,
            ..params(None)
        },
    );
    assert_eq!(second.sessions.len(), 1);
    assert_eq!(second.next_cursor, None);
}

#[test]
fn catalog_bootstrap_should_filter_authorized_profiles_without_enumerating_others() {
    let temp = tempfile::tempdir().unwrap();
    let state = profile_state(
        temp.path(),
        &[("main", None), ("child", Some("main")), ("other", None)],
    );
    let user = crate::api::TestAuthIdentity::User {
        id: "main".to_owned(),
        role: crate::user_store::UserRole::User,
    };
    let selected = catalog_profile_ids(&state, Some(&user), None, None, None).unwrap();
    assert_eq!(selected, ["main"]);
    let denied = catalog_profile_ids(&state, Some(&user), None, None, Some("other")).unwrap_err();
    assert_eq!(denied.data.unwrap()["kind"], json!("auth_scope_violation"));
    let mismatch =
        catalog_profile_ids(&state, Some(&user), Some("main"), None, Some("child")).unwrap_err();
    assert_eq!(
        mismatch.data.unwrap()["kind"],
        json!("auth_scope_violation")
    );

    let admin = catalog_profile_ids(
        &state,
        Some(&crate::api::TestAuthIdentity::Admin),
        None,
        None,
        None,
    )
    .unwrap();
    assert_eq!(admin, ["_main", "child", "main", "other"]);
    let scoped_admin = catalog_profile_ids(
        &state,
        Some(&crate::api::TestAuthIdentity::Admin),
        None,
        Some("other"),
        None,
    )
    .unwrap();
    assert_eq!(scoped_admin, ["other"]);
}

#[tokio::test]
async fn catalog_bootstrap_rpc_should_preserve_external_and_profile_denials() {
    let temp = tempfile::tempdir().unwrap();
    let state = Arc::new(profile_state(
        temp.path(),
        &[("main", None), ("other", None)],
    ));
    let (tx, mut rx) = mpsc::channel(4);
    let ws = WsConnection::new(tx);
    handle_catalog_bootstrap(
        &ws,
        &state,
        None,
        Some("main"),
        None,
        true,
        "external".to_owned(),
        HashSet::new(),
        CatalogBootstrapParams::default(),
    )
    .await;
    let response = match rx.recv().await.unwrap() {
        axum::extract::ws::Message::Text(text) => {
            serde_json::from_str::<Value>(text.as_str()).unwrap()
        }
        other => panic!("unexpected frame: {other:?}"),
    };
    assert_eq!(
        response["error"]["data"]["kind"],
        json!("external_method_denied")
    );

    let user = crate::api::TestAuthIdentity::User {
        id: "main".to_owned(),
        role: crate::user_store::UserRole::User,
    };
    handle_catalog_bootstrap(
        &ws,
        &state,
        Some(&user),
        Some("main"),
        None,
        false,
        "unauthorized".to_owned(),
        HashSet::new(),
        CatalogBootstrapParams {
            profile_id: Some("other".to_owned()),
            ..CatalogBootstrapParams::default()
        },
    )
    .await;
    let response = match rx.recv().await.unwrap() {
        axum::extract::ws::Message::Text(text) => {
            serde_json::from_str::<Value>(text.as_str()).unwrap()
        }
        other => panic!("unexpected frame: {other:?}"),
    };
    assert_eq!(
        response["error"]["data"]["kind"],
        json!("auth_scope_violation")
    );

    handle_catalog_bootstrap(
        &ws,
        &state,
        Some(&crate::api::TestAuthIdentity::Admin),
        Some("main"),
        None,
        false,
        "empty-success".to_owned(),
        HashSet::new(),
        CatalogBootstrapParams::default(),
    )
    .await;
    let response = match rx.recv().await.unwrap() {
        axum::extract::ws::Message::Text(text) => {
            serde_json::from_str::<Value>(text.as_str()).unwrap()
        }
        other => panic!("unexpected frame: {other:?}"),
    };
    assert_eq!(response["error"], Value::Null, "{response}");
    let result: octos_core::ui_protocol::CatalogBootstrapResult =
        serde_json::from_value(response["result"].clone()).unwrap();
    assert_eq!(result.connection_scope.profile_ids, ["main"]);
    assert!(result.sessions.is_empty());
    assert_eq!(result.summary.returned_count, 0);
}
