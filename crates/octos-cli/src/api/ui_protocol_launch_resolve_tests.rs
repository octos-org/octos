#[tokio::test]
async fn launch_resolve_activates_empty_folder_then_resumes_after_store_exists() {
    use octos_core::ui_protocol::{LaunchDecisionKind, LaunchResolveParams};

    let tmp = tempfile::tempdir().unwrap();
    let profile = crate::profiles::UserProfile {
        id: "dev".to_string(),
        name: "Dev".to_string(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: crate::profiles::ProfileConfig {
            llm: Some(crate::profiles::LlmProfileConfig {
                primary: Some(crate::profiles::LlmModelSelectionConfig {
                    family_id: Some("openai".to_string()),
                    model_id: Some("gpt-4o-mini".to_string()),
                    route: Some(crate::profiles::LlmRouteConfig {
                        route_id: None,
                        label: None,
                        base_url: None,
                        api_key_env: Some("LAUNCH_RESOLVE_TEST_KEY".to_string()),
                        api_type: None,
                    }),
                    ..Default::default()
                }),
                fallbacks: Vec::new(),
            }),
            env_vars: [(
                "LAUNCH_RESOLVE_TEST_KEY".to_string(),
                "test-key".to_string(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let runtime = crate::runtime::ProfileRuntime::bootstrap(
        &profile,
        &data_dir,
        None,
        crate::runtime::BootstrapRole::Serve,
    )
    .await
    .expect("bootstrap dev runtime");

    let mut state = AppState::empty_for_tests();
    state.profiles.insert("dev".to_string(), runtime);
    state.session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(4, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let state = Arc::new(state);
    let cap = ConnectionUiFeatures::stdio_defaults();

    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let params = LaunchResolveParams {
        cwd: project.to_string_lossy().into_owned(),
        profile_id: Some("dev".to_string()),
    };

    // Empty folder → activate a new session for the launching profile.
    let activate = resolve_launch_result(&state, Some("dev"), cap, &params).unwrap();
    assert_eq!(activate.decision, LaunchDecisionKind::Activate);
    assert_eq!(activate.resolved_profile.as_deref(), Some("dev"));

    // Once dev has a store here, a relaunch resumes it.
    let canon = std::fs::canonicalize(&project).unwrap();
    std::fs::create_dir_all(
        crate::runtime::session::project_sessions_root(&canon, "dev").join("sessions"),
    )
    .unwrap();
    let resume = resolve_launch_result(&state, Some("dev"), cap, &params).unwrap();
    assert_eq!(resume.decision, LaunchDecisionKind::Resume);
    assert_eq!(resume.resolved_profile.as_deref(), Some("dev"));

    // Without the workspace-cwd feature the probe is rejected.
    let no_cap = ConnectionUiFeatures::default();
    assert!(resolve_launch_result(&state, Some("dev"), no_cap, &params).is_err());
}

#[tokio::test]
async fn admin_launch_resolve_ignores_foreign_sticky_but_honors_explicit_profile() {
    use octos_core::ui_protocol::{LaunchDecisionKind, LaunchResolveParams};

    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::profiles::ProfileStore::open_unified(tmp.path()).unwrap());
    for id in ["coding", "e2e-test"] {
        store
            .save(&crate::profiles::UserProfile {
                id: id.into(),
                name: id.into(),
                enabled: true,
                data_dir: None,
                parent_id: None,
                public_subdomain: None,
                config: crate::profiles::ProfileConfig::default(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .unwrap();
    }
    store.set_default_profile("coding").unwrap();
    let mut state = AppState::empty_for_tests();
    state.profile_store = Some(store);
    state.session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(4, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let state = Arc::new(state);
    let cap = ConnectionUiFeatures::stdio_defaults();
    let params = |cwd: &std::path::Path, profile_id: Option<&str>| LaunchResolveParams {
        cwd: cwd.to_string_lossy().into_owned(),
        profile_id: profile_id.map(ToOwned::to_owned),
    };

    let shared = tmp.path().join("shared");
    std::fs::create_dir_all(
        crate::runtime::session::project_sessions_root(&shared, "e2e-test").join("sessions"),
    )
    .unwrap();
    std::fs::write(shared.join(".octos").join("active-profile"), "e2e-test").unwrap();
    let implicit = resolve_launch_result(&state, None, cap, &params(&shared, None))
        .expect("legacy foreign sticky must not capture an admin launch");
    assert_eq!(implicit.decision, LaunchDecisionKind::CrossProfile);
    assert_eq!(implicit.resolved_profile.as_deref(), Some("coding"));
    assert_eq!(implicit.existing_profiles, ["e2e-test"]);

    let explicit = resolve_launch_result(&state, None, cap, &params(&shared, Some("e2e-test")))
        .expect("an admin may still explicitly operate another profile");
    assert_eq!(explicit.decision, LaunchDecisionKind::Resume);
    assert_eq!(explicit.resolved_profile.as_deref(), Some("e2e-test"));

    let polluted = tmp.path().join("polluted");
    std::fs::create_dir_all(
        crate::runtime::session::project_sessions_root(&polluted, "e2e-test").join("sessions"),
    )
    .unwrap();
    let old_store = resolve_launch_result(&state, None, cap, &params(&polluted, None))
        .expect("an old store has no admin recency ownership");
    assert_eq!(old_store.decision, LaunchDecisionKind::CrossProfile);
    assert_eq!(old_store.resolved_profile.as_deref(), Some("coding"));

    let own = tmp.path().join("own");
    std::fs::create_dir_all(
        crate::runtime::session::project_sessions_root(&own, "coding").join("sessions"),
    )
    .unwrap();
    std::fs::write(
        own.join(".octos").join("active-profile"),
        r#"{"version":1,"profile_id":"coding","owner":{"kind":"admin"}}"#,
    )
    .unwrap();
    let admin_sticky = resolve_launch_result(&state, None, cap, &params(&own, None))
        .expect("an admin-owned marker remains sticky");
    assert_eq!(admin_sticky.decision, LaunchDecisionKind::Resume);
    assert_eq!(admin_sticky.resolved_profile.as_deref(), Some("coding"));
}

/// The persisted `default-profile` pointer drives a bare launch: with no
/// `--profile` and no folder-sticky profile, `launch/resolve` resolves to
/// the pointer even when it is not the first-sorted profile. A stale pointer
/// (naming a profile that no longer exists) is ignored, falling back to the
/// derived default.
#[tokio::test]
async fn launch_resolve_prefers_persisted_default_profile() {
    use octos_core::ui_protocol::{LaunchDecisionKind, LaunchResolveParams};

    let tmp = tempfile::tempdir().unwrap();
    // The default-profile pointer lives in its own octos home; each profile
    // bootstraps in a separate data dir because the redb episode store takes
    // an exclusive lock and two profiles cannot share one.
    let home = tmp.path().join("home");

    let make_profile = |id: &str| crate::profiles::UserProfile {
        id: id.to_string(),
        name: id.to_string(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: crate::profiles::ProfileConfig {
            llm: Some(crate::profiles::LlmProfileConfig {
                primary: Some(crate::profiles::LlmModelSelectionConfig {
                    family_id: Some("openai".to_string()),
                    model_id: Some("gpt-4o-mini".to_string()),
                    route: Some(crate::profiles::LlmRouteConfig {
                        route_id: None,
                        label: None,
                        base_url: None,
                        api_key_env: Some("LAUNCH_DEFAULT_TEST_KEY".to_string()),
                        api_type: None,
                    }),
                    ..Default::default()
                }),
                fallbacks: Vec::new(),
            }),
            env_vars: [(
                "LAUNCH_DEFAULT_TEST_KEY".to_string(),
                "test-key".to_string(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let mut state = AppState::empty_for_tests();
    for id in ["alpha", "zeta"] {
        let profile_dir = tmp.path().join(format!("profile-{id}"));
        std::fs::create_dir_all(&profile_dir).unwrap();
        let runtime = crate::runtime::ProfileRuntime::bootstrap(
            &make_profile(id),
            &profile_dir,
            None,
            crate::runtime::BootstrapRole::Serve,
        )
        .await
        .expect("bootstrap profile");
        state.profiles.insert(id.to_string(), runtime);
    }
    let store = Arc::new(crate::profiles::ProfileStore::open_unified(&home).unwrap());
    // Point the global default at "zeta" — NOT the first-sorted profile.
    store.set_default_profile("zeta").unwrap();
    state.profile_store = Some(store.clone());
    state.session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(4, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let state = Arc::new(state);
    let cap = ConnectionUiFeatures::stdio_defaults();

    let project = tmp.path().join("fresh");
    std::fs::create_dir_all(&project).unwrap();
    // Bare launch (no `--profile`) in an empty folder on an unscoped admin or
    // stdio connection. The persisted default must win over derived sort order;
    // authenticated users are covered by a separate regression and narrowed there.
    let params = LaunchResolveParams {
        cwd: project.to_string_lossy().into_owned(),
        profile_id: None,
    };
    let decision = resolve_launch_result(&state, None, cap, &params).unwrap();
    assert_eq!(decision.decision, LaunchDecisionKind::Activate);
    assert_eq!(
        decision.resolved_profile.as_deref(),
        Some("zeta"),
        "the persisted default must win over the derived profile order"
    );

    // A stale pointer (unknown profile) is ignored → falls back to the
    // derived default (alpha).
    store.set_default_profile("ghost").unwrap();
    let stale = resolve_launch_result(&state, None, cap, &params).unwrap();
    assert_eq!(stale.resolved_profile.as_deref(), Some("alpha"));
}

#[tokio::test]
async fn launch_resolve_confines_folder_defaults_and_explicit_profiles_to_identity() {
    use octos_core::ui_protocol::{LaunchDecisionKind, LaunchResolveParams};

    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::profiles::ProfileStore::open_unified(tmp.path()).unwrap());
    for id in ["solouser", "coding"] {
        store
            .save(&crate::profiles::UserProfile {
                id: id.into(),
                name: id.into(),
                enabled: true,
                data_dir: None,
                parent_id: None,
                public_subdomain: None,
                config: crate::profiles::ProfileConfig::default(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .unwrap();
    }
    store.set_default_profile("coding").unwrap();
    let mut state = AppState::empty_for_tests();
    state.profile_store = Some(store);
    state.session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(4, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let state = Arc::new(state);
    let cap = ConnectionUiFeatures::stdio_defaults();
    let params = |cwd: &std::path::Path, profile_id: Option<&str>| LaunchResolveParams {
        cwd: cwd.to_string_lossy().into_owned(),
        profile_id: profile_id.map(ToOwned::to_owned),
    };

    let fresh = tmp.path().join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    let activated = resolve_launch_result(&state, Some("solouser"), cap, &params(&fresh, None))
        .expect("a user activates only its own profile");
    assert_eq!(activated.decision, LaunchDecisionKind::Activate);
    assert_eq!(activated.resolved_profile.as_deref(), Some("solouser"));
    assert!(activated.existing_profiles.is_empty());
    let absent = resolve_launch_result(&state, Some("missing-user"), cap, &params(&fresh, None))
        .expect("an absent authenticated profile stays in onboarding");
    assert_eq!(absent.decision, LaunchDecisionKind::NoProfile);
    assert!(absent.resolved_profile.is_none());
    assert!(absent.existing_profiles.is_empty());
    validate_session_scope(
        &SessionKey::with_profile(
            activated.resolved_profile.as_deref().unwrap(),
            "api",
            "web-t0178",
        ),
        None,
        Some("solouser"),
    )
    .expect("resolved identity must pass session/open scope");

    let shared = tmp.path().join("shared");
    std::fs::create_dir_all(
        crate::runtime::session::project_sessions_root(&shared, "coding").join("sessions"),
    )
    .unwrap();
    std::fs::write(shared.join(".octos").join("active-profile"), "coding").unwrap();
    let scoped = resolve_launch_result(&state, Some("solouser"), cap, &params(&shared, None))
        .expect("foreign sticky/default profiles stay out of a user decision");
    assert_eq!(scoped.decision, LaunchDecisionKind::Activate);
    assert_eq!(scoped.resolved_profile.as_deref(), Some("solouser"));
    assert!(
        scoped.existing_profiles.is_empty(),
        "ordinary users must not learn another profile's folder history"
    );

    let own = tmp.path().join("own");
    std::fs::create_dir_all(
        crate::runtime::session::project_sessions_root(&own, "solouser").join("sessions"),
    )
    .unwrap();
    let resumed = resolve_launch_result(&state, Some("solouser"), cap, &params(&own, None))
        .expect("the user's own folder store remains resumable");
    assert_eq!(resumed.decision, LaunchDecisionKind::Resume);
    assert_eq!(resumed.resolved_profile.as_deref(), Some("solouser"));

    let error = resolve_launch_result(
        &state,
        Some("solouser"),
        cap,
        &params(&fresh, Some("coding")),
    )
    .expect_err("an explicit foreign profile is rejected before resolution");
    let data = error.data.as_ref().unwrap();
    assert_eq!(data["auth_scope_violation"], json!(true));
    assert_eq!(data["expected_profile_id"], json!("solouser"));
    assert_eq!(data["actual_profile_id"], json!("coding"));

    let admin = resolve_launch_result(&state, None, cap, &params(&shared, None))
        .expect("admin keeps its implicit default/explicit profile behavior");
    assert_eq!(admin.decision, LaunchDecisionKind::Resume);
    assert_eq!(admin.resolved_profile.as_deref(), Some("coding"));

    let stdio_folder = tmp.path().join("stdio-legacy-sticky");
    std::fs::create_dir_all(
        crate::runtime::session::project_sessions_root(&stdio_folder, "solouser").join("sessions"),
    )
    .unwrap();
    std::fs::write(
        stdio_folder.join(".octos").join("active-profile"),
        "solouser",
    )
    .unwrap();
    let stdio = resolve_launch_result_with_sticky_policy(
        &state,
        None,
        false,
        cap,
        &params(&stdio_folder, None),
    )
    .expect("stdio retains generic sticky behavior, not admin-owned-only markers");
    assert_eq!(stdio.decision, LaunchDecisionKind::Resume);
    assert_eq!(stdio.resolved_profile.as_deref(), Some("solouser"));
}

#[tokio::test]
async fn launch_resolve_uses_stored_profiles_without_a_loaded_runtime() {
    // Regression (launch-flow soak against a real `octos serve --stdio`): in
    // solo `--stdio`, profile runtimes materialize LAZILY on `session/open`,
    // so `state.profiles` is EMPTY at bare-launch time. launch/resolve must
    // still see a profile that EXISTS in the persistent `ProfileStore` and
    // must NOT require a loaded runtime — otherwise every folder falls
    // through to `activate` (Resume/CrossProfile unreachable) and a
    // zero-profile machine gets a spurious `cwd_runtime_unavailable` instead
    // of `NoProfile`. `state.profiles` stays empty for the WHOLE test.
    use octos_core::ui_protocol::{LaunchDecisionKind, LaunchResolveParams};

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut state = AppState::empty_for_tests();
    state.profile_store = Some(Arc::new(
        crate::profiles::ProfileStore::open_unified(&home).unwrap(),
    ));
    let state = Arc::new(state);
    let cap = ConnectionUiFeatures::stdio_defaults();
    let resolve = |cwd: &std::path::Path| {
        resolve_launch_result(
            &state,
            None,
            cap,
            &LaunchResolveParams {
                cwd: cwd.to_string_lossy().into_owned(),
                profile_id: None,
            },
        )
    };

    // A valid folder with ZERO stored profiles → NoProfile, NOT an error.
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let no_profile = resolve(&empty).unwrap();
    assert_eq!(no_profile.decision, LaunchDecisionKind::NoProfile);
    assert_eq!(no_profile.resolved_profile, None);

    // Persist a launchable brain to the STORE only — no runtime loaded.
    state
        .profile_store
        .as_ref()
        .unwrap()
        .save(&crate::profiles::UserProfile {
            id: "ghost".to_string(),
            name: "Ghost".to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: crate::profiles::ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .unwrap();

    // Empty folder + the sole stored profile is ghost → Activate{ghost}
    // (default falls back to the only known profile), NOT an error.
    let activate = resolve(&empty).unwrap();
    assert_eq!(activate.decision, LaunchDecisionKind::Activate);
    assert_eq!(activate.resolved_profile.as_deref(), Some("ghost"));

    // A folder holding ghost's activated store + sticky marker → Resume{ghost},
    // even though ghost's runtime was never loaded into `state.profiles`.
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".octos").join("ghost").join("sessions")).unwrap();
    std::fs::write(project.join(".octos").join("active-profile"), "ghost").unwrap();
    let resume = resolve(&project).unwrap();
    assert_eq!(resume.decision, LaunchDecisionKind::Resume);
    assert_eq!(resume.resolved_profile.as_deref(), Some("ghost"));
}

