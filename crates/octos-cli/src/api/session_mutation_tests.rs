use super::*;
use crate::user_store::UserRole;

use crate::process_manager::ProcessManager;
use crate::profiles::{ChannelCredentials, ProfileConfig, ProfileStore, UserProfile};

#[derive(Debug)]
struct GatewayCall {
    method: String,
    path: String,
    authorization: Option<String>,
}

struct GatewayStub {
    port: u16,
    calls: Arc<std::sync::Mutex<Vec<GatewayCall>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for GatewayStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl GatewayStub {
    async fn start(status: StatusCode, token: Option<&str>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let expected = token.map(|token| format!("Bearer {token}"));
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let authorization = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            recorded.lock().unwrap().push(GatewayCall {
                method: request.method().to_string(),
                path: request.uri().path().to_owned(),
                authorization: authorization.clone(),
            });
            let response = if expected.is_some() && authorization != expected {
                StatusCode::UNAUTHORIZED
            } else {
                status
            };
            async move { response }
        });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { port, calls, task }
    }
}

async fn legacy_fixture(profile_id: &str, present: bool) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ProfileStore::open_unified(&dir.path().join("profiles")).unwrap());
    let manager = Arc::new(ProcessManager::new(store.clone()));
    let mut sessions = octos_bus::SessionManager::open(&dir.path().join("data")).unwrap();
    if present {
        sessions
            .add_message(
                &SessionKey::with_profile(profile_id, "api", "web-legacy"),
                Message::user("keep the local result"),
            )
            .await
            .unwrap();
    }
    let state = Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(sessions))),
        profile_store: Some(store),
        process_manager: Some(manager),
        http_client: reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap(),
        ..AppState::empty_for_tests()
    });
    (dir, state)
}

async fn register_gateway(state: &AppState, profile_id: &str, port: u16, token: Option<&str>) {
    register_gateway_at(state, profile_id, port, token, None).await;
}

async fn register_gateway_at(
    state: &AppState,
    profile_id: &str,
    port: u16,
    token: Option<&str>,
    data_dir: Option<String>,
) {
    let profile = UserProfile {
        id: profile_id.into(),
        name: profile_id.into(),
        enabled: true,
        data_dir,
        parent_id: None,
        public_subdomain: None,
        config: ProfileConfig {
            channels: vec![ChannelCredentials::Api {
                port,
                auth_token: token.map(str::to_owned),
            }],
            ..Default::default()
        },
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let store = state.profile_store.as_ref().unwrap();
    std::fs::create_dir_all(store.resolve_data_dir(&profile)).unwrap();
    if profile_id != MAIN_PROFILE_ID {
        store.save(&profile).unwrap();
    }
    let mut command = tokio::process::Command::new("unused-test-gateway");
    command
        .env_remove("OCTOS_AUTH_TOKEN")
        .env_remove("OCTOS_DATA_DIR")
        .env("HOME", store.octos_home_dir());
    state
        .process_manager
        .as_ref()
        .unwrap()
        .register_api_port_for_test(&profile, port, &command)
        .await;
}

async fn legacy_mutation(
    state: &Arc<AppState>,
    deleting: bool,
    identity: AuthIdentity,
) -> Response {
    let id = SessionKey("web-legacy".into());
    if deleting {
        delete(state, &id, Some(identity)).await
    } else {
        rename(state, &id, Some(identity)).await
    }
}

#[tokio::test]
async fn session_mutation_local_success_survives_gateway_http_failures() {
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::NOT_FOUND,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
    ] {
        let gateway = GatewayStub::start(status, None).await;
        for deleting in [false, true] {
            let (dir, state) = legacy_fixture(MAIN_PROFILE_ID, true).await;
            register_gateway(&state, MAIN_PROFILE_ID, gateway.port, None).await;
            let response = legacy_mutation(&state, deleting, AuthIdentity::Admin).await;
            let fresh = octos_bus::SessionManager::open(&dir.path().join("data")).unwrap();
            let row = fresh
                .load(&SessionKey::with_profile(
                    MAIN_PROFILE_ID,
                    "api",
                    "web-legacy",
                ))
                .await;
            if deleting {
                assert!(row.is_none());
            } else {
                assert_eq!(row.unwrap().title.as_deref(), Some("新的标题"));
            }
            assert_eq!(
                response.status(),
                StatusCode::NO_CONTENT,
                "deleting={deleting}, upstream={status}"
            );
        }
    }
}

#[tokio::test]
async fn session_mutation_remote_only_failure_is_not_reported_as_success() {
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::NOT_FOUND,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
    ] {
        let gateway = GatewayStub::start(status, None).await;
        let (_dir, state) = legacy_fixture(MAIN_PROFILE_ID, false).await;
        register_gateway(&state, MAIN_PROFILE_ID, gateway.port, None).await;
        for deleting in [false, true] {
            assert_eq!(
                legacy_mutation(&state, deleting, AuthIdentity::Admin)
                    .await
                    .status(),
                status
            );
        }
    }
}

#[tokio::test]
async fn session_mutation_unreachable_gateway_preserves_only_actual_local_success() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    for present in [false, true] {
        for deleting in [false, true] {
            let (_dir, state) = legacy_fixture(MAIN_PROFILE_ID, present).await;
            register_gateway(&state, MAIN_PROFILE_ID, port, None).await;
            let expected = if present {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::BAD_GATEWAY
            };
            assert_eq!(
                legacy_mutation(&state, deleting, AuthIdentity::Admin)
                    .await
                    .status(),
                expected
            );
        }
    }
}

#[tokio::test]
async fn session_mutation_gateway_uses_identity_profile_and_its_bearer_token() {
    let gateway = GatewayStub::start(StatusCode::NO_CONTENT, Some("owner-token")).await;
    let foreign = GatewayStub::start(StatusCode::BAD_GATEWAY, None).await;
    let (dir, state) = legacy_fixture(MAIN_PROFILE_ID, true).await;
    register_gateway(&state, "coding", gateway.port, Some("owner-token")).await;
    register_gateway(&state, "other", foreign.port, Some("other-token")).await;
    for deleting in [false, true] {
        let identity = AuthIdentity::User {
            id: "coding".into(),
            role: UserRole::User,
        };
        assert_eq!(
            legacy_mutation(&state, deleting, identity).await.status(),
            StatusCode::NO_CONTENT
        );
    }
    {
        let calls = gateway.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (&*calls[0].method, &*calls[0].path),
            ("PATCH", "/sessions/web-legacy/title")
        );
        assert_eq!(
            (&*calls[1].method, &*calls[1].path),
            ("DELETE", "/sessions/web-legacy")
        );
        assert!(
            calls
                .iter()
                .all(|call| call.authorization.as_deref() == Some("Bearer owner-token"))
        );
        assert!(foreign.calls.lock().unwrap().is_empty());
    }
    let main = octos_bus::SessionManager::open(&dir.path().join("data"))
        .unwrap()
        .load(&SessionKey::with_profile(
            MAIN_PROFILE_ID,
            "api",
            "web-legacy",
        ))
        .await
        .unwrap();
    assert_ne!(main.title.as_deref(), Some("新的标题"));
}

#[tokio::test]
async fn session_mutation_missing_owner_gateway_never_uses_another_profile() {
    let foreign = GatewayStub::start(StatusCode::NO_CONTENT, None).await;
    let (_dir, state) = legacy_fixture(MAIN_PROFILE_ID, true).await;
    register_gateway(&state, "other", foreign.port, None).await;
    for deleting in [false, true] {
        assert_eq!(
            legacy_mutation(&state, deleting, AuthIdentity::Admin)
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
    }
    assert!(foreign.calls.lock().unwrap().is_empty());
}

async fn fixture(key: &SessionKey) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let mut sessions = octos_bus::SessionManager::open(dir.path()).unwrap();
    sessions
        .add_message(key, Message::user("keep this conversation"))
        .await
        .unwrap();
    let state = Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(sessions))),
        ..AppState::empty_for_tests()
    });
    (dir, state)
}

async fn rename(
    state: &Arc<AppState>,
    key: &SessionKey,
    identity: Option<AuthIdentity>,
) -> Response {
    update_session_title(
        State(state.clone()),
        HeaderMap::new(),
        identity.map(Extension),
        axum::extract::Path(key.0.clone()),
        Json(UpdateTitleRequest {
            title: "新的标题".to_owned(),
        }),
    )
    .await
}

async fn delete(
    state: &Arc<AppState>,
    key: &SessionKey,
    identity: Option<AuthIdentity>,
) -> Response {
    delete_session(
        State(state.clone()),
        HeaderMap::new(),
        identity.map(Extension),
        axum::extract::Path(key.0.clone()),
    )
    .await
}

#[tokio::test]
async fn session_mutation_canonical_key_roundtrips_without_touching_prefixed_alias() {
    let key = SessionKey::with_profile_topic("coding", "api", "web-mutation", "review");
    let (dir, state) = fixture(&key).await;
    let alias = SessionKey::with_profile("_main", "api", &key.0);
    state
        .sessions
        .as_ref()
        .unwrap()
        .lock()
        .await
        .add_message(&alias, Message::user("wrong store candidate"))
        .await
        .unwrap();
    assert_eq!(
        rename(&state, &key, Some(AuthIdentity::Admin))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let fresh = octos_bus::SessionManager::open(dir.path()).unwrap();
    assert_eq!(
        fresh.load(&key).await.unwrap().title.as_deref(),
        Some("新的标题")
    );
    assert_ne!(
        fresh.load(&alias).await.unwrap().title.as_deref(),
        Some("新的标题")
    );

    assert_eq!(
        delete(&state, &key, Some(AuthIdentity::Admin))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let fresh = octos_bus::SessionManager::open(dir.path()).unwrap();
    assert!(fresh.load(&key).await.is_none());
    assert!(fresh.load(&alias).await.is_some());
    // Deleting the same authoritative key again remains idempotent.
    assert_eq!(
        delete(&state, &key, Some(AuthIdentity::Admin))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn session_mutation_enforces_canonical_profile_ownership() {
    let key = SessionKey::with_profile("coding", "api", "web-owner");
    let (dir, state) = fixture(&key).await;
    let other = AuthIdentity::User {
        id: "other".to_owned(),
        role: UserRole::User,
    };
    for identity in [Some(other), None] {
        assert_eq!(
            rename(&state, &key, identity.clone()).await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            delete(&state, &key, identity).await.status(),
            StatusCode::FORBIDDEN
        );
        assert!(
            octos_bus::SessionManager::open(dir.path())
                .unwrap()
                .load(&key)
                .await
                .is_some()
        );
    }
    let owner = AuthIdentity::User {
        id: "coding".to_owned(),
        role: UserRole::User,
    };
    assert_eq!(
        rename(&state, &key, Some(owner.clone())).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        delete(&state, &key, Some(owner)).await.status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn session_mutation_does_not_expand_api_access_to_other_channels() {
    for channel in ["telegram", "cli", "local"] {
        let key = SessionKey::with_profile("coding", channel, "web-other-channel");
        let (dir, state) = fixture(&key).await;
        assert_eq!(
            rename(&state, &key, Some(AuthIdentity::Admin))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            delete(&state, &key, Some(AuthIdentity::Admin))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert!(
            octos_bus::SessionManager::open(dir.path())
                .unwrap()
                .load(&key)
                .await
                .is_some()
        );
    }
}

#[tokio::test]
async fn session_mutation_unknown_canonical_rename_is_not_success() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(dir.path()).unwrap(),
        ))),
        ..AppState::empty_for_tests()
    });
    let key = SessionKey::with_profile("coding", "api", "web-missing");
    assert_eq!(
        rename(&state, &key, Some(AuthIdentity::Admin))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn session_mutation_unprofiled_api_key_roundtrips_exactly() {
    for topic in ["", "review"] {
        for identity in [None, Some(AuthIdentity::Admin)] {
            let key = SessionKey::with_topic("api", "web-standalone", topic);
            let (dir, mut state) = fixture(&key).await;
            // Production serve always has a profile store, even when no LLM
            // is configured and `_main` has no runtime.
            Arc::get_mut(&mut state).unwrap().profile_store = Some(Arc::new(
                ProfileStore::open_unified(&dir.path().join("profiles")).unwrap(),
            ));
            let alias = SessionKey::with_profile(MAIN_PROFILE_ID, "api", &key.0);
            state
                .sessions
                .as_ref()
                .unwrap()
                .lock()
                .await
                .add_message(&alias, Message::user("leave the alias alone"))
                .await
                .unwrap();
            assert_eq!(
                rename(&state, &key, identity.clone()).await.status(),
                StatusCode::NO_CONTENT
            );
            let fresh = octos_bus::SessionManager::open(dir.path()).unwrap();
            assert_eq!(
                fresh.load(&key).await.unwrap().title.as_deref(),
                Some("新的标题")
            );
            assert_ne!(
                fresh.load(&alias).await.unwrap().title.as_deref(),
                Some("新的标题")
            );
            assert_eq!(
                delete(&state, &key, identity.clone()).await.status(),
                StatusCode::NO_CONTENT
            );
            let fresh = octos_bus::SessionManager::open(dir.path()).unwrap();
            assert!(fresh.load(&key).await.is_none());
            assert!(fresh.load(&alias).await.is_some());
            assert_eq!(
                delete(&state, &key, identity.clone()).await.status(),
                StatusCode::NO_CONTENT
            );
            assert_eq!(
                rename(&state, &key, identity).await.status(),
                StatusCode::NOT_FOUND
            );
        }
    }
}

#[tokio::test]
async fn session_mutation_unprofiled_api_key_rejects_tenant_users() {
    let key = SessionKey::with_topic("api", "web-shared", "private");
    let (dir, state) = fixture(&key).await;
    let user = Some(AuthIdentity::User {
        id: "coding".into(),
        role: UserRole::User,
    });
    assert_eq!(
        rename(&state, &key, user.clone()).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        delete(&state, &key, user).await.status(),
        StatusCode::FORBIDDEN
    );
    let fresh = octos_bus::SessionManager::open(dir.path()).unwrap();
    let original = fresh.load(&key).await.unwrap();
    assert_ne!(original.title.as_deref(), Some("新的标题"));
    assert_eq!(original.messages[0].content, "keep this conversation");
}

#[path = "session_mutation_gateway_tests.rs"]
mod gateway_storage;

fn find_jsonl(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_jsonl(&path) {
                return Some(found);
            }
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            return Some(path);
        }
    }
    None
}

#[tokio::test]
async fn session_mutation_storage_failure_is_reported_and_retryable() {
    for deleting in [false, true] {
        // A read-only parent permits loading the title but rejects its atomic
        // rewrite; replacing the JSONL with a directory would test a missing
        // readable session instead of a failed title write.
        #[cfg(not(unix))]
        if !deleting {
            continue;
        }
        let key = SessionKey::with_profile("coding", "api", "web-io-error");
        let (dir, state) = fixture(&key).await;
        let file = find_jsonl(dir.path()).expect("seeded session JSONL");
        let backup = dir.path().join("saved-jsonl");
        #[cfg(unix)]
        let original_permissions = std::fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions();
        if deleting {
            std::fs::rename(&file, &backup).unwrap();
            std::fs::create_dir(&file).unwrap();
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    file.parent().unwrap(),
                    std::fs::Permissions::from_mode(0o500),
                )
                .unwrap();
                // Privileged runners can bypass directory permissions.
                let probe = file.parent().unwrap().join("write-probe");
                if std::fs::write(&probe, b"").is_ok() {
                    std::fs::remove_file(probe).unwrap();
                    std::fs::set_permissions(file.parent().unwrap(), original_permissions).unwrap();
                    continue;
                }
            }
        }
        let response = if deleting {
            delete(&state, &key, Some(AuthIdentity::Admin)).await
        } else {
            rename(&state, &key, Some(AuthIdentity::Admin)).await
        };
        // Restore permissions even if the assertion fails, so TempDir cleanup
        // cannot leave a read-only test fixture behind.
        if deleting {
            std::fs::remove_dir(&file).unwrap();
            std::fs::rename(&backup, &file).unwrap();
        } else {
            #[cfg(unix)]
            std::fs::set_permissions(file.parent().unwrap(), original_permissions).unwrap();
        }
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let response = if deleting {
            delete(&state, &key, Some(AuthIdentity::Admin)).await
        } else {
            rename(&state, &key, Some(AuthIdentity::Admin)).await
        };
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let loaded = octos_bus::SessionManager::open(dir.path())
            .unwrap()
            .load(&key)
            .await;
        if deleting {
            assert!(loaded.is_none());
        } else {
            assert_eq!(loaded.unwrap().title.as_deref(), Some("新的标题"));
        }
    }
}

#[path = "session_mutation_shared_gateway_tests.rs"]
mod shared_gateway;
