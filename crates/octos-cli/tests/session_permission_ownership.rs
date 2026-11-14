//! Ordinary users may operate only on sessions in their authenticated profile.
#![cfg(feature = "api")]

use std::{sync::Arc, time::Duration};

use chrono::Utc;
use futures::{SinkExt, StreamExt};
use octos_cli::{
    api::{AppState, build_router},
    otp::{AuthManager, DashboardAuthConfig},
    profiles::{ProfileStore, UserProfile},
    user_store::{User, UserRole, UserStore},
};
use octos_core::SessionKey;
use serde_json::{Value, json};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

const TOKEN_A: &str = "SESSION-OWNERSHIP-TOKEN-A";
const TOKEN_B: &str = "SESSION-OWNERSHIP-TOKEN-B";
const ADMIN_TOKEN: &str = "SESSION-OWNERSHIP-ADMIN-TOKEN";
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn user(id: &str) -> User {
    User {
        id: id.into(),
        email: format!("{id}@example.test"),
        name: id.into(),
        role: UserRole::User,
        created_at: Utc::now(),
        last_login_at: None,
    }
}

async fn fixture(
    solo: bool,
) -> (
    tempfile::TempDir,
    String,
    tokio::task::JoinHandle<()>,
    String,
    String,
) {
    let dir = tempfile::tempdir().unwrap();
    let profiles = Arc::new(ProfileStore::open_unified(dir.path()).unwrap());
    for id in ["tenant-a", "tenant-b"] {
        let profile: UserProfile = serde_json::from_value(json!({
            "id": id,
            "name": id,
            "enabled": true,
            "created_at": "2026-11-14T00:00:00Z",
            "updated_at": "2026-11-14T00:00:00Z",
            "config": {
                "llm": {"primary": {
                    "family_id": "openai",
                    "model_id": "fixture-model",
                    "route": {
                        "base_url": "http://127.0.0.1:9/v1",
                        "api_key_env": "SESSION_OWNERSHIP_FIXTURE_KEY"
                    }
                }},
                "env_vars": {"SESSION_OWNERSHIP_FIXTURE_KEY": "fixture-only"}
            }
        }))
        .unwrap();
        profiles.save(&profile).unwrap();
    }
    let users = Arc::new(UserStore::open(dir.path()).unwrap());
    users.save(&user("tenant-a")).unwrap();
    users.save(&user("tenant-b")).unwrap();
    let auth = Arc::new(AuthManager::new(
        Some(DashboardAuthConfig {
            smtp: None,
            session_expiry_hours: 1,
            allow_self_registration: false,
            static_tokens: vec![TOKEN_A.into(), TOKEN_B.into()],
        }),
        users.clone(),
    ));
    let mut tokens = Vec::new();
    for (email, token) in [
        ("tenant-a@example.test", TOKEN_A),
        ("tenant-b@example.test", TOKEN_B),
    ] {
        tokens.push(
            auth.verify_otp_with_registration(email, token, false)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = Arc::new(AppState {
        profile_store: Some(profiles),
        user_store: Some(users),
        auth_manager: Some(auth),
        auth_token: Some(ADMIN_TOKEN.into()),
        sessions: Some(Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(dir.path()).unwrap(),
        ))),
        appui_default_session_cwd: Some(workspace),
        solo_login_enabled: solo,
        deployment_mode: octos_cli::config::DeploymentMode::Local,
        ..AppState::empty_for_tests()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, build_router(state).into_make_service())
            .await
            .unwrap();
    });
    (
        dir,
        addr.to_string(),
        server,
        tokens.remove(0),
        tokens.remove(0),
    )
}

async fn connect(addr: &str, token: &str) -> Socket {
    let mut request = format!("ws://{addr}/api/ui-protocol/ws")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request.headers_mut().insert(
        "X-Octos-Ui-Features",
        "session.workspace_cwd.v1,context.lifecycle.v1"
            .parse()
            .unwrap(),
    );
    connect_async(request).await.unwrap().0
}

async fn rpc(ws: &mut Socket, method: &str, params: Value) -> Value {
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":method,"method":method,"params":params})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let frame = ws.next().await.expect("socket closed").expect("frame");
            if let Message::Text(text) = frame {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["id"] == method {
                    return value;
                }
            }
        }
    })
    .await
    .expect("RPC response")
}

fn path_tree(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

async fn exercise_mode(solo: bool) {
    let (dir, addr, server, token_a, token_b) = fixture(solo).await;
    let mode = if solo { "solo" } else { "local" };
    let mut a = connect(&addr, &token_a).await;
    let mut b = connect(&addr, &token_b).await;
    let a_session = SessionKey::with_profile("tenant-a", "api", &format!("owned-a-{mode}"));
    let b_session = SessionKey::with_profile("tenant-b", "api", &format!("owned-b-{mode}"));
    let tree_before = path_tree(dir.path());
    let cold = rpc(
        &mut a,
        "permission/profile/list",
        json!({"session_id":SessionKey::with_profile("tenant-a","api",&format!("cold-unknown-{mode}"))}),
    )
    .await;
    assert_eq!(cold["error"]["data"]["kind"], "unknown_session");
    assert_eq!(
        tree_before,
        path_tree(dir.path()),
        "solo={solo}: cold unknown must not initialize profile databases or directories"
    );
    for (ws, session, profile) in [
        (&mut a, &a_session, "tenant-a"),
        (&mut b, &b_session, "tenant-b"),
    ] {
        let opened = rpc(
            ws,
            "session/open",
            json!({"session_id":session,"profile_id":profile}),
        )
        .await;
        assert!(opened.get("error").is_none(), "solo={solo}: {opened}");
    }
    let before = rpc(
        &mut b,
        "permission/profile/set",
        json!({"session_id":b_session,"update":{"mode":"read_only","network":"deny"}}),
    )
    .await;
    assert_eq!(before["result"]["applied"], true, "solo={solo}: {before}");
    let mut admin = connect(&addr, ADMIN_TOKEN).await;
    let admin_status = rpc(
        &mut admin,
        "session/status/read",
        json!({"session_id":b_session}),
    )
    .await;
    assert!(
        admin_status.get("error").is_none(),
        "solo={solo}: {admin_status}"
    );
    for (method, params) in [
        (
            "permission/profile/set",
            json!({"session_id":b_session,"runtime_mode":"solo","update":{"mode":"workspace_write","network":"deny"}}),
        ),
        ("permission/profile/list", json!({"session_id":b_session})),
        (
            "session/status/read",
            json!({"session_id":b_session,"profile_id":"tenant-a","runtime_mode":"solo"}),
        ),
    ] {
        let mut attacker = connect(&addr, &token_a).await;
        let denied = rpc(&mut attacker, method, params).await;
        assert_eq!(denied["error"]["code"], -32602, "solo={solo}: {denied}");
        assert_eq!(
            denied["error"]["data"]["auth_scope_violation"], true,
            "solo={solo}: {denied}"
        );
    }
    let after = rpc(
        &mut b,
        "permission/profile/list",
        json!({"session_id":b_session}),
    )
    .await;
    assert_eq!(after["result"]["current"], before["result"]["current"]);

    let workspace_policy = dir.path().join("workspace/.octos-workspace.toml");
    let workspace_octos = dir.path().join("workspace/.octos");
    let workspace_before = (
        std::fs::read(&workspace_policy).ok(),
        workspace_octos.exists(),
    );
    for session in [
        SessionKey("".into()),
        SessionKey(format!("never-opened-raw-{mode}")),
        SessionKey::with_profile("tenant-a", "api", &format!("never-opened-{mode}")),
    ] {
        for method in [
            "permission/profile/set",
            "permission/profile/list",
            "session/status/read",
        ] {
            let params = if method == "permission/profile/set" {
                json!({"session_id":session,"update":{"mode":"read_only","network":"deny"}})
            } else {
                json!({"session_id":session})
            };
            let mut owner = connect(&addr, &token_a).await;
            let denied = rpc(&mut owner, method, params).await;
            assert_eq!(
                denied["error"]["data"]["kind"], "unknown_session",
                "solo={solo}: {denied}"
            );
        }
    }
    assert_eq!(
        workspace_before,
        (
            std::fs::read(&workspace_policy).ok(),
            workspace_octos.exists(),
        ),
        "solo={solo}: unknown IDs must not bootstrap a runtime or workspace"
    );
    let own = rpc(
        &mut a,
        "permission/profile/list",
        json!({"session_id":a_session}),
    )
    .await;
    assert!(own.get("error").is_none(), "solo={solo}: {own}");

    // The same raw SPA id under two authenticated profiles is a distinct
    // (profile, session) identity, independent of open-binding LRU eviction.
    let raw = SessionKey(format!("shared-web-{mode}"));
    for (ws, profile, mode) in [
        (&mut a, "tenant-a", "read_only"),
        (&mut b, "tenant-b", "workspace_write"),
    ] {
        let opened = if profile == "tenant-b" {
            // Admin may establish the other profile's raw record/binding;
            // follow-up ownership must still remain with each authenticated user.
            rpc(
                &mut admin,
                "session/open",
                json!({"session_id":raw,"profile_id":profile}),
            )
            .await
        } else {
            rpc(
                ws,
                "session/open",
                json!({"session_id":raw,"profile_id":profile}),
            )
            .await
        };
        assert!(opened.get("error").is_none(), "solo={solo}: {opened}");
        let updated = rpc(
            ws,
            "permission/profile/set",
            json!({"session_id":raw,"update":{"mode":mode,"network":"deny"}}),
        )
        .await;
        assert_eq!(updated["result"]["applied"], true, "solo={solo}: {updated}");
    }
    for (ws, profile, mode) in [
        (&mut a, "tenant-a", "read_only"),
        (&mut b, "tenant-b", "workspace_write"),
    ] {
        let listed = rpc(ws, "permission/profile/list", json!({"session_id":raw})).await;
        assert_eq!(listed["result"]["current"]["mode"], mode, "solo={solo}");
        let status = rpc(ws, "session/status/read", json!({"session_id":raw})).await;
        assert_eq!(status["result"]["profile_id"], profile, "solo={solo}");
        assert!(
            status["result"]["context"].is_object(),
            "solo={solo}: raw status must use its own context manager: {status}"
        );
    }
    server.abort();
}

#[test]
fn should_enforce_session_ownership_for_permission_and_status_methods() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            exercise_mode(true).await;
            exercise_mode(false).await;
        });
}
