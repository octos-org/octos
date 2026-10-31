use super::*;

#[tokio::test]
async fn execution_environment_read_enforces_scope_and_reuses_cached_probe() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let mut profile = profile_for_runtime_message("env");
    profile.config.llm = Some(crate::profiles::LlmProfileConfig {
        primary: Some(crate::profiles::LlmModelSelectionConfig {
            family_id: Some("openai".to_string()),
            model_id: Some("gpt-4o-mini".to_string()),
            route: Some(crate::profiles::LlmRouteConfig {
                api_key_env: Some("OCTOS_TEST_EXECUTION_ENVIRONMENT_KEY".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        fallbacks: Vec::new(),
    });
    profile.config.env_vars.insert(
        "OCTOS_TEST_EXECUTION_ENVIRONMENT_KEY".to_string(),
        "test-key".to_string(),
    );
    profile.config.sandbox.enabled = false;
    state
        .profile_store
        .as_ref()
        .unwrap()
        .save(&profile)
        .unwrap();
    let state = Arc::new(state);
    let request = RpcRequest::<Value>::new(
        "environment-read",
        octos_core::ui_protocol::methods::SESSION_EXECUTION_ENVIRONMENT_READ,
        json!({ "session_id": "local:execution-environment", "profile_id": "env" }),
    );

    let error = route_rpc_command(request.clone(), ConnectionUiFeatures::default())
        .expect_err("execution environment reads require their negotiated feature");
    assert_eq!(
        error.code,
        octos_core::ui_protocol::rpc_error_codes::METHOD_NOT_SUPPORTED
    );
    raw_session_execution_environment_read(&state, &request, Some("other-profile"))
        .await
        .expect_err("another profile's connection must not read this session environment");

    let first = raw_session_execution_environment_read(&state, &request, Some("env"))
        .await
        .expect("read actual unconfined environment");
    assert_eq!(first["backend"], json!("none"));
    assert_eq!(first["docker_image"], json!(null));
    assert!(first["probed_at"].is_string());
    #[cfg(not(windows))]
    {
        assert_eq!(first["probe_status"], json!("complete"), "{first}");
        for tool in ["node", "npm", "git", "python"] {
            assert!(["available", "missing"].contains(&first["tools"][tool].as_str().unwrap()));
        }
    }
    #[cfg(windows)]
    {
        assert_eq!(first["probe_status"], json!("unsupported"), "{first}");
        assert_eq!(first["probe_error"], json!(null));
        for tool in ["node", "npm", "git", "python"] {
            assert_eq!(first["tools"][tool], json!("unknown"));
        }
    }
    let second = raw_session_execution_environment_read(&state, &request, Some("env"))
        .await
        .expect("reuse cached execution environment");
    assert_eq!(
        second, first,
        "a second read must not execute another probe"
    );
}
