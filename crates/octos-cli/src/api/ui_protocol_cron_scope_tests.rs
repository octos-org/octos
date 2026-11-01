use super::*;

#[tokio::test]
async fn cron_rpc_scopes_list_toggle_and_validation_to_the_session_profile() {
    let dir = tempfile::tempdir().unwrap();
    let profile_store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
    let coding = panel_user_profile("coding");
    let admin = panel_user_profile("admin");
    profile_store.save(&coding).unwrap();
    profile_store.save(&admin).unwrap();
    for (profile, job_id) in [(&coding, "coding-job"), (&admin, "admin-job")] {
        let data_dir = profile_store.resolve_data_dir(profile);
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let store = octos_bus::CronStore {
            version: 1,
            jobs: vec![panel_cron_job(job_id, true)],
        };
        tokio::fs::write(
            data_dir.join("cron.json"),
            serde_json::to_string_pretty(&store).unwrap(),
        )
        .await
        .unwrap();
    }
    let state = Arc::new(AppState {
        profile_store: Some(profile_store),
        ..AppState::empty_for_tests()
    });
    let headers = HeaderMap::new();
    let identity = AuthIdentity::Admin;
    let session = SessionKey::with_profile("coding", "api", "web-t0153");
    let (ws, mut rx) = ws_connection_for_test(16);

    handle_cron_list(
        &ws,
        &state,
        &headers,
        Some(&identity),
        true,
        "cron-scoped-list".into(),
        CronListParams {
            session_id: Some(session.clone()),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["count"], json!(1));
    assert_eq!(frame["result"]["jobs"][0]["id"], json!("coding-job"));

    handle_cron_toggle(
        &ws,
        &state,
        &headers,
        Some(&identity),
        true,
        "cron-scoped-toggle".into(),
        CronToggleParams {
            job_id: "coding-job".into(),
            enabled: false,
            session_id: Some(session.clone()),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["job"]["enabled"], json!(false));

    handle_cron_list(
        &ws,
        &state,
        &headers,
        Some(&identity),
        true,
        "cron-unscoped-list".into(),
        CronListParams::default(),
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["jobs"][0]["id"], json!("admin-job"));
    assert_eq!(frame["result"]["jobs"][0]["enabled"], json!(true));

    handle_cron_create(
        &ws,
        &state,
        &headers,
        Some(&identity),
        true,
        "cron-invalid-expression".into(),
        octos_core::ui_protocol::CronCreateParams {
            session_id: session,
            schedule: octos_core::ui_protocol::CronCreateSchedule::Cron {
                expr: "0 9 * * *".into(),
            },
            message: "invalid schedule".into(),
            deliver: false,
            channel: None,
            chat_id: None,
            mode: octos_core::ui_protocol::CronCreateMode::Notify,
            timezone: None,
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["error"]["data"]["rest_status"], json!(400));
    assert!(
        frame["error"]["data"]["detail"]
            .as_str()
            .unwrap()
            .contains("six fields including seconds")
    );
}
