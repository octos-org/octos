use super::*;
use crate::profiles::SmartHomeConfig;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ws_connection_for_test(capacity: usize) -> (WsConnection, mpsc::Receiver<WsMessage>) {
    let (tx, rx) = mpsc::channel(capacity);
    (WsConnection::new(tx), rx)
}

async fn recv_rpc_json(rx: &mut mpsc::Receiver<WsMessage>) -> Value {
    match rx.recv().await {
        Some(WsMessage::Text(text)) => serde_json::from_str(&text).expect("RPC frame must be JSON"),
        other => panic!("expected a text RPC frame, got {other:?}"),
    }
}

fn smart_home_profile(id: &str, bridge_url: Option<String>) -> crate::profiles::UserProfile {
    crate::profiles::UserProfile {
        id: id.into(),
        name: id.into(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: crate::profiles::ProfileConfig {
            smart_home: bridge_url.map(|bridge_url| SmartHomeConfig {
                bridge_url: Some(bridge_url),
                token: Some("secret-token".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn user_identity(id: &str) -> AuthIdentity {
    AuthIdentity::User {
        id: id.into(),
        role: crate::user_store::UserRole::User,
    }
}

#[tokio::test]
async fn smart_home_query_service_rest_and_oup_are_equivalent_and_scoped() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/devices"))
        .and(header("Authorization", "Bearer secret-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "source": "test",
            "devices": [{
                "id": "lamp",
                "name": "Lamp",
                "kind": "light",
                "on": true,
                "online": true,
                "token": "must-not-leak"
            }],
            "ok": true
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let profile_store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
    profile_store
        .save(&smart_home_profile("tenant", Some(server.uri())))
        .unwrap();
    profile_store
        .save(&smart_home_profile("other", None))
        .unwrap();
    let state = Arc::new(AppState {
        profile_store: Some(profile_store.clone()),
        ..AppState::empty_for_tests()
    });
    let identity = user_identity("tenant");
    let headers = HeaderMap::new();
    let service = crate::api::smart_home_panel::SmartHomeQueryService::new(state.as_ref());

    let status = service.status(&identity, &headers).await.unwrap();
    assert_eq!(status, json!({ "configured": true }));
    let (ws, mut rx) = ws_connection_for_test(16);
    handle_smart_home_status_get(
        &ws,
        &state,
        &headers,
        Some(&identity),
        false,
        "smart-home-status".into(),
        Default::default(),
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"], status);

    let expected = service.devices(&identity, &headers).await.unwrap();
    assert!(!expected.to_string().contains("must-not-leak"));
    assert!(!expected.to_string().contains("secret-token"));
    handle_smart_home_device_list(
        &ws,
        &state,
        &headers,
        Some(&identity),
        false,
        "smart-home-devices".into(),
        Default::default(),
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["devices"], expected);
    assert!(!frame.to_string().contains("must-not-leak"));

    let missing_identity = user_identity("missing");
    let missing = service
        .devices(&missing_identity, &headers)
        .await
        .unwrap_err();
    assert!(matches!(
        &missing,
        crate::api::smart_home_panel::SmartHomeQueryError::ProfileNotFound
    ));
    assert_eq!(missing.status(), axum::http::StatusCode::NOT_FOUND);
    assert_eq!(missing.reason(), "profile_not_found");
    handle_smart_home_device_list(
        &ws,
        &state,
        &headers,
        Some(&missing_identity),
        false,
        "smart-home-missing-profile".into(),
        Default::default(),
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["error"]["data"]["rest_status"], json!(404));
    assert_eq!(frame["error"]["data"]["detail"], json!("profile_not_found"));

    let other_identity = user_identity("other");
    let not_configured = service
        .devices(&other_identity, &headers)
        .await
        .unwrap_err();
    assert!(matches!(
        &not_configured,
        crate::api::smart_home_panel::SmartHomeQueryError::NotConfigured
    ));
    assert_eq!(not_configured.reason(), "not_configured");

    profile_store
        .save(&smart_home_profile(
            "offline",
            Some("http://127.0.0.1:1".into()),
        ))
        .unwrap();
    let offline_identity = user_identity("offline");
    let offline = service
        .devices(&offline_identity, &headers)
        .await
        .unwrap_err();
    assert!(matches!(
        &offline,
        crate::api::smart_home_panel::SmartHomeQueryError::BridgeUnreachable(_)
    ));
    assert_eq!(offline.status(), axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(offline.reason(), "bridge_unreachable");
    handle_smart_home_device_list(
        &ws,
        &state,
        &headers,
        Some(&offline_identity),
        false,
        "smart-home-offline".into(),
        Default::default(),
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["error"]["data"]["rest_status"], json!(502));
    assert_eq!(
        frame["error"]["data"]["detail"],
        json!("bridge_unreachable")
    );
}

#[tokio::test]
async fn smart_home_query_service_preserves_timeout_as_typed_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/devices"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(6))
                .set_body_json(json!({ "devices": [] })),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let profile_store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
    profile_store
        .save(&smart_home_profile("tenant", Some(server.uri())))
        .unwrap();
    let state = Arc::new(AppState {
        profile_store: Some(profile_store.clone()),
        ..AppState::empty_for_tests()
    });
    let service = crate::api::smart_home_panel::SmartHomeQueryService::new(state.as_ref());
    let error = service
        .devices(&user_identity("tenant"), &HeaderMap::new())
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        crate::api::smart_home_panel::SmartHomeQueryError::BridgeUnreachable(_)
    ));
    assert_eq!(error.status(), axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(error.reason(), "bridge_unreachable");
}
