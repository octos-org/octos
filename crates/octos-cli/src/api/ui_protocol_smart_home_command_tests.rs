use super::*;
use crate::profiles::SmartHomeConfig;
use wiremock::matchers::{body_string, header, method, path};
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
async fn smart_home_command_service_rest_and_oup_are_equivalent_and_scoped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/devices/lamp"))
        .and(header("Authorization", "Bearer secret-token"))
        .and(body_string("on=true"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/devices/invalid"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(json!({ "error": "invalid command" })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/cameras/cam/stream"))
        .and(body_string("quality=3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "protocol": "webrtc",
            "playback_url": "https://bridge.invalid/playback",
            "stream_url": "https://bridge.invalid/stream"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/cameras/cam/stop"))
        .respond_with(ResponseTemplate::new(200))
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
    profile_store
        .save(&smart_home_profile(
            "offline",
            Some("http://127.0.0.1:1".into()),
        ))
        .unwrap();
    let state = Arc::new(AppState {
        profile_store: Some(profile_store),
        ..AppState::empty_for_tests()
    });
    let identity = user_identity("tenant");
    let headers = HeaderMap::new();
    let service = crate::api::smart_home_panel::SmartHomeCommandService::new(state.as_ref());
    let params = serde_json::Map::from_iter([("on".to_owned(), json!(true))]);

    // Repeating the same command is still exactly two bridge accepts, never a
    // client-side fake dedupe/success. Each OUP reply keeps its own request id.
    assert_eq!(
        service
            .execute(&identity, &headers, "lamp", &params)
            .await
            .unwrap(),
        json!({})
    );
    assert_eq!(
        service
            .execute(&identity, &headers, "lamp", &params)
            .await
            .unwrap(),
        json!({})
    );
    let axum::Json(rest) = crate::api::smart_home_panel::my_smart_home_device_command(
        State(state.clone()),
        headers.clone(),
        axum::Extension(identity.clone()),
        "lamp".into(),
        params.clone(),
    )
    .await
    .unwrap();
    assert_eq!(rest, json!({}));
    let (ws, mut rx) = ws_connection_for_test(16);
    for request_id in ["command-1", "command-2"] {
        handle_smart_home_device_command(
            &ws,
            &state,
            &headers,
            Some(&identity),
            false,
            request_id.into(),
            octos_core::ui_protocol::SmartHomeDeviceCommandParams {
                device_id: "lamp".into(),
                params: json!({ "on": true }),
            },
        )
        .await;
        let frame = recv_rpc_json(&mut rx).await;
        assert_eq!(frame["id"], json!(request_id));
        assert_eq!(frame["result"], json!({}));
    }

    let invalid = service
        .execute(&identity, &headers, "invalid", &params)
        .await
        .unwrap_err();
    assert!(matches!(
        &invalid,
        crate::api::smart_home_panel::SmartHomeCommandError::Bridge(_)
    ));
    assert_eq!(invalid.status(), axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(invalid.reason(), "bridge_error");
    let (rest_status, axum::Json(rest_error)) =
        crate::api::smart_home_panel::my_smart_home_device_command(
            State(state.clone()),
            headers.clone(),
            axum::Extension(identity.clone()),
            "invalid".into(),
            params.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(rest_status, axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(rest_error["reason"], json!("bridge_error"));
    assert_eq!(rest_error["detail"], json!("invalid command"));
    handle_smart_home_device_command(
        &ws,
        &state,
        &headers,
        Some(&identity),
        false,
        "command-invalid".into(),
        octos_core::ui_protocol::SmartHomeDeviceCommandParams {
            device_id: "invalid".into(),
            params: json!({ "on": true }),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], json!("command-invalid"));
    assert_eq!(frame["error"]["data"]["rest_status"], json!(502));
    assert_eq!(frame["error"]["data"]["detail"], json!("bridge_error"));

    let stream = service
        .camera_start(&identity, &headers, "cam", Some(3))
        .await
        .unwrap();
    assert_eq!(stream["protocol"], json!("webrtc"));
    let axum::Json(rest_stream) = crate::api::smart_home_panel::my_smart_home_camera_stream_start(
        State(state.clone()),
        headers.clone(),
        axum::Extension(identity.clone()),
        "cam".into(),
        Some(3),
    )
    .await
    .unwrap();
    assert_eq!(rest_stream, stream);
    handle_smart_home_camera_stream_start(
        &ws,
        &state,
        &headers,
        Some(&identity),
        false,
        "camera-start".into(),
        octos_core::ui_protocol::SmartHomeCameraStreamStartParams {
            device_id: "cam".into(),
            quality: Some(3),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], json!("camera-start"));
    assert_eq!(frame["result"]["stream"], stream);
    assert_eq!(
        service
            .camera_stop(&identity, &headers, "cam")
            .await
            .unwrap(),
        json!({})
    );
    handle_smart_home_camera_stream_stop(
        &ws,
        &state,
        &headers,
        Some(&identity),
        false,
        "camera-stop".into(),
        octos_core::ui_protocol::SmartHomeCameraStreamStopParams {
            device_id: "cam".into(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], json!("camera-stop"));
    assert_eq!(frame["result"], json!({}));

    let other = user_identity("other");
    let not_configured = service
        .execute(&other, &headers, "lamp", &params)
        .await
        .unwrap_err();
    assert!(matches!(
        &not_configured,
        crate::api::smart_home_panel::SmartHomeCommandError::NotConfigured
    ));
    let missing = user_identity("missing");
    let missing_error = service
        .execute(&missing, &headers, "lamp", &params)
        .await
        .unwrap_err();
    assert!(matches!(
        &missing_error,
        crate::api::smart_home_panel::SmartHomeCommandError::ProfileNotFound
    ));

    let offline = user_identity("offline");
    let offline_error = service
        .execute(&offline, &headers, "lamp", &params)
        .await
        .unwrap_err();
    assert!(matches!(
        &offline_error,
        crate::api::smart_home_panel::SmartHomeCommandError::BridgeUnreachable(_)
    ));
    assert_eq!(offline_error.reason(), "bridge_unreachable");
    handle_smart_home_device_command(
        &ws,
        &state,
        &headers,
        Some(&offline),
        false,
        "command-offline".into(),
        octos_core::ui_protocol::SmartHomeDeviceCommandParams {
            device_id: "lamp".into(),
            params: json!({ "on": true }),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], json!("command-offline"));
    assert_eq!(frame["error"]["data"]["rest_status"], json!(502));
    assert_eq!(
        frame["error"]["data"]["detail"],
        json!("bridge_unreachable")
    );

    let lamp_calls = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/devices/lamp")
        .count();
    assert_eq!(lamp_calls, 5, "two service + one REST + two OUP commands");
}

#[tokio::test]
async fn smart_home_command_service_timeout_is_not_reported_as_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/devices/lamp"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(6)))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let profile_store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
    profile_store
        .save(&smart_home_profile("tenant", Some(server.uri())))
        .unwrap();
    let state = Arc::new(AppState {
        profile_store: Some(profile_store),
        ..AppState::empty_for_tests()
    });
    let service = crate::api::smart_home_panel::SmartHomeCommandService::new(state.as_ref());
    let params = serde_json::Map::from_iter([("on".to_owned(), json!(true))]);
    let error = service
        .execute(&user_identity("tenant"), &HeaderMap::new(), "lamp", &params)
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        crate::api::smart_home_panel::SmartHomeCommandError::BridgeUnreachable(_)
    ));
    assert_eq!(error.status(), axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(error.reason(), "bridge_unreachable");
}
