use super::*;

use crate::process_manager::ProcessManager;
use crate::profiles::{ChannelCredentials, ProfileConfig, ProfileStore, UserProfile};

struct GatewayStub {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for GatewayStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl GatewayStub {
    async fn start(status: StatusCode) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new().fallback(move || async move { status });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { port, task }
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

async fn register_gateway(state: &AppState, profile_id: &str, port: u16) {
    let profile = UserProfile {
        id: profile_id.into(),
        name: profile_id.into(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: ProfileConfig {
            channels: vec![ChannelCredentials::Api {
                port,
                auth_token: None,
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

async fn assert_local_mutation(dir: &tempfile::TempDir, present: bool, deleting: bool) {
    let fresh = octos_bus::SessionManager::open(&dir.path().join("data")).unwrap();
    let row = fresh
        .load(&SessionKey::with_profile(
            MAIN_PROFILE_ID,
            "api",
            "web-legacy",
        ))
        .await;
    if !present || deleting {
        assert!(row.is_none(), "present={present}, deleting={deleting}");
    } else {
        assert_eq!(row.unwrap().title.as_deref(), Some("新的标题"));
    }
}

#[tokio::test]
async fn session_mutation_unreachable_gateway_preserves_only_actual_local_success() {
    // TCP port zero is never a connectable service, so no parallel test can
    // occupy it between discovery and the request.
    for present in [false, true] {
        for deleting in [false, true] {
            let (dir, state) = legacy_fixture(MAIN_PROFILE_ID, present).await;
            register_gateway(&state, MAIN_PROFILE_ID, 0).await;
            let response = legacy_mutation(&state, deleting, AuthIdentity::Admin).await;
            assert_local_mutation(&dir, present, deleting).await;
            assert_eq!(
                response.status(),
                if present {
                    StatusCode::NO_CONTENT
                } else {
                    StatusCode::BAD_GATEWAY
                }
            );
        }
    }

    // Real HTTP failures, including 404, must retain their exact mapping;
    // local disk success still survives each failure and is verified above.
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::NOT_FOUND,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
    ] {
        let gateway = GatewayStub::start(status).await;
        for present in [false, true] {
            for deleting in [false, true] {
                let (dir, state) = legacy_fixture(MAIN_PROFILE_ID, present).await;
                register_gateway(&state, MAIN_PROFILE_ID, gateway.port).await;
                let response = legacy_mutation(&state, deleting, AuthIdentity::Admin).await;
                assert_local_mutation(&dir, present, deleting).await;
                assert_eq!(
                    response.status(),
                    if present {
                        StatusCode::NO_CONTENT
                    } else {
                        status
                    },
                    "present={present}, deleting={deleting}, upstream={status}"
                );
            }
        }
    }
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
