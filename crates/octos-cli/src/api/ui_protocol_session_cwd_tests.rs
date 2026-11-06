use super::*;

/// M11-E acceptance §1: a session opened with a custom `cwd` materializes
/// a `SessionRuntime` whose `workspace_root` IS that cwd, and the
/// session's `ReadFileTool` reads files from that cwd. This is the
/// "supplied workspace, not the daemon cwd" invariant from the issue
/// — the legacy `clone_session_tools` path could only honor it
/// indirectly through the global `session_workspaces()` map.
#[tokio::test]
async fn appui_session_with_custom_cwd_reads_supplied_workspace() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (state, profile_runtime) = state_with_profile(temp.path(), "m11e-custom-cwd").await;

    // Pre-seed the supplied workspace with a sentinel file the session
    // is expected to read back.
    let supplied_cwd = temp.path().join("supplied-workspace");
    std::fs::create_dir_all(&supplied_cwd).expect("create supplied cwd");
    let sentinel = supplied_cwd.join("hello.txt");
    std::fs::write(&sentinel, "session-A reads its own workspace\n").expect("seed sentinel");

    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();

    let session_id = SessionKey::with_profile("m11e-custom-cwd", "api", "custom-cwd-session");
    let features = ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        session_workspace_cwd: true,
        session_sandbox: false,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };

    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("m11e-custom-cwd"),
        None,
        features,
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: Some("m11e-custom-cwd".into()),
            cwd: Some(supplied_cwd.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("session/open with supplied cwd must succeed");

    // The wire response carries the canonical workspace_root so dashboard
    // clients render the right cwd on reconnect.
    let opened_root = outcome
        .result
        .opened
        .workspace_root
        .as_ref()
        .expect("opened workspace_root populated");
    assert_eq!(
        std::fs::canonicalize(opened_root).expect("canonicalize opened root"),
        std::fs::canonicalize(&supplied_cwd).expect("canonicalize supplied cwd"),
    );

    // The session cache must hold a SessionRuntime materialized from
    // THIS supplied cwd — that's the wiring the issue tracks.
    let session_runtime = state
        .session_cache
        .get_or_init(&profile_runtime, session_id.clone(), None)
        .await
        .expect("cached session runtime");
    assert_eq!(
        std::fs::canonicalize(&session_runtime.workspace_root).expect("canonicalize root"),
        std::fs::canonicalize(&supplied_cwd).expect("canonicalize supplied cwd"),
        "SessionRuntime.workspace_root must be the client-supplied cwd"
    );

    // The session's read_file tool sees the supplied workspace, not
    // the daemon cwd / the profile data_dir.
    let result = session_runtime
        .tools
        .execute("read_file", &json!({ "path": "hello.txt" }))
        .await
        .expect("read_file via session tools");
    assert!(result.success, "read_file must succeed: {}", result.output);
    assert!(
        result.output.contains("session-A reads its own workspace"),
        "expected session A's sentinel content, got: {}",
        result.output
    );
}

#[test]
fn session_sandbox_requires_negotiated_feature() {
    let params = SessionOpenParams {
        session_ref: None,
        client_commands: None,
        session_id: SessionKey::new("api", "sandbox-feature-required"),
        topic: None,
        profile_id: None,
        cwd: None,
        sandbox: Some(SessionSandboxParams {
            enabled: None,
            network_access: Some(false),
            read_allow_paths: Vec::new(),
        }),
        after: None,
    };

    let err = validate_requested_session_sandbox(
        ConnectionUiFeatures::default(),
        &params,
        &octos_agent::SandboxConfig::default(),
    )
    .expect_err("sandbox override must require feature negotiation");
    assert_eq!(
        err.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("feature_required"))
    );
    assert_eq!(
        err.data.as_ref().and_then(|data| data.get("feature")),
        Some(&json!(UI_PROTOCOL_FEATURE_SESSION_SANDBOX_V1))
    );
}

#[test]
fn session_sandbox_can_narrow_network_but_not_widen() {
    let features = ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        session_sandbox: true,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };
    let params = SessionOpenParams {
        session_ref: None,
        client_commands: None,
        session_id: SessionKey::new("api", "sandbox-network-narrow"),
        topic: None,
        profile_id: None,
        cwd: None,
        sandbox: Some(SessionSandboxParams {
            enabled: None,
            network_access: Some(false),
            read_allow_paths: Vec::new(),
        }),
        after: None,
    };
    let inherited = octos_agent::SandboxConfig {
        allow_network: true,
        ..octos_agent::SandboxConfig::default()
    };

    let narrowed = validate_requested_session_sandbox(features, &params, &inherited)
        .expect("network:false narrows network-enabled profile")
        .expect("override present");
    assert!(!narrowed.allow_network);

    let widening = SessionOpenParams {
        session_ref: None,
        client_commands: None,
        session_id: SessionKey::new("api", "sandbox-network-widen"),
        topic: None,
        profile_id: None,
        cwd: None,
        sandbox: Some(SessionSandboxParams {
            enabled: None,
            network_access: Some(true),
            read_allow_paths: Vec::new(),
        }),
        after: None,
    };
    let err = validate_requested_session_sandbox(
        features,
        &widening,
        &octos_agent::SandboxConfig::default(),
    )
    .expect_err("network:true widens a network-denied profile");
    assert_eq!(
        err.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("session_sandbox_would_widen_profile"))
    );
    assert_eq!(
        err.data.as_ref().and_then(|data| data.get("field")),
        Some(&json!("network_access"))
    );
}

#[test]
fn session_sandbox_read_paths_must_stay_within_profile_allowlist() {
    let temp = tempfile::tempdir().expect("tempdir");
    let allowed = temp.path().join("allowed");
    let nested = allowed.join("nested");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&nested).expect("nested dir");
    std::fs::create_dir_all(&outside).expect("outside dir");
    let features = ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        session_sandbox: true,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };
    let inherited = octos_agent::SandboxConfig {
        read_allow_paths: vec![allowed.to_string_lossy().into_owned()],
        ..octos_agent::SandboxConfig::default()
    };

    let narrowed = SessionOpenParams {
        session_ref: None,
        client_commands: None,
        session_id: SessionKey::new("api", "sandbox-read-narrow"),
        topic: None,
        profile_id: None,
        cwd: None,
        sandbox: Some(SessionSandboxParams {
            enabled: None,
            network_access: None,
            read_allow_paths: vec![nested.to_string_lossy().into_owned()],
        }),
        after: None,
    };
    let sandbox = validate_requested_session_sandbox(features, &narrowed, &inherited)
        .expect("nested read path narrows profile allowlist")
        .expect("override present");
    assert_eq!(sandbox.read_allow_paths.len(), 1);
    assert!(Path::new(&sandbox.read_allow_paths[0]).ends_with("nested"));

    let widening = SessionOpenParams {
        session_ref: None,
        client_commands: None,
        session_id: SessionKey::new("api", "sandbox-read-widen"),
        topic: None,
        profile_id: None,
        cwd: None,
        sandbox: Some(SessionSandboxParams {
            enabled: None,
            network_access: None,
            read_allow_paths: vec![outside.to_string_lossy().into_owned()],
        }),
        after: None,
    };
    let err = validate_requested_session_sandbox(features, &widening, &inherited)
        .expect_err("outside read path widens profile allowlist");
    assert_eq!(
        err.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("session_sandbox_would_widen_profile"))
    );
    assert_eq!(
        err.data.as_ref().and_then(|data| data.get("field")),
        Some(&json!("read_allow_paths"))
    );
}

#[tokio::test]
async fn session_sandbox_open_override_materializes_distinct_session_policies() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (state, profile_runtime) = state_with_profile_llm_and_sandbox(
        temp.path(),
        "m11-session-sandbox",
        Arc::new(M11EStubLlm),
        octos_agent::SandboxConfig {
            allow_network: true,
            ..octos_agent::SandboxConfig::default()
        },
    )
    .await;
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let features = ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        session_sandbox: true,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };
    let gamma = SessionKey::with_profile("m11-session-sandbox", "api", "gamma");
    let delta = SessionKey::with_profile("m11-session-sandbox", "api", "delta");

    let _ = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("m11-session-sandbox"),
        None,
        features,
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: gamma.clone(),
            topic: None,
            profile_id: Some("m11-session-sandbox".into()),
            cwd: None,
            sandbox: Some(SessionSandboxParams {
                enabled: None,
                network_access: Some(false),
                read_allow_paths: Vec::new(),
            }),
            after: None,
        },
    )
    .await
    .expect("gamma session/open with network false sandbox override");
    let _ = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("m11-session-sandbox"),
        None,
        features,
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: delta.clone(),
            topic: None,
            profile_id: Some("m11-session-sandbox".into()),
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("delta session/open with default sandbox");

    let gamma_runtime = state
        .session_cache
        .get_or_init(&profile_runtime, gamma, None)
        .await
        .expect("gamma runtime");
    let delta_runtime = state
        .session_cache
        .get_or_init(&profile_runtime, delta, None)
        .await
        .expect("delta runtime");

    assert!(profile_runtime.default_sandbox.allow_network);
    assert!(!gamma_runtime.sandbox.allow_network);
    assert!(delta_runtime.sandbox.allow_network);
    assert_ne!(gamma_runtime.workspace_root, delta_runtime.workspace_root);
}

/// M11-E acceptance §2: two AppUI sessions opened on the SAME profile
/// with DIFFERENT cwds must not see each other's files. This is the
/// multi-tenant scope invariant codex flagged on PR #868 — pre-M11
/// the per-session view was cloned off a single `base_agent`-bound
/// registry, leaving the workspace_root vulnerable to cross-session
/// leakage if any caller forgot to rebind. With `SessionRuntime.tools`
/// the only path, two sessions hold two distinct `Arc<ToolRegistry>`
/// instances each pinned at bootstrap time.
#[tokio::test]
async fn two_appui_sessions_on_same_profile_with_different_cwds_isolated() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (state, profile_runtime) = state_with_profile(temp.path(), "m11e-multi-cwd").await;

    let cwd_a = temp.path().join("session-a");
    let cwd_b = temp.path().join("session-b");
    std::fs::create_dir_all(&cwd_a).expect("create cwd-a");
    std::fs::create_dir_all(&cwd_b).expect("create cwd-b");

    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();

    let session_a = SessionKey::with_profile("m11e-multi-cwd", "api", "session-a");
    let session_b = SessionKey::with_profile("m11e-multi-cwd", "api", "session-b");

    let features = ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        session_workspace_cwd: true,
        session_sandbox: false,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };

    // Open session A with cwd_a.
    let _ = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("m11e-multi-cwd"),
        None,
        features,
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_a.clone(),
            topic: None,
            profile_id: Some("m11e-multi-cwd".into()),
            cwd: Some(cwd_a.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("session A open");

    // Open session B with cwd_b.
    let _ = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("m11e-multi-cwd"),
        None,
        features,
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_b.clone(),
            topic: None,
            profile_id: Some("m11e-multi-cwd".into()),
            cwd: Some(cwd_b.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("session B open");

    let rt_a = state
        .session_cache
        .get_or_init(&profile_runtime, session_a.clone(), None)
        .await
        .expect("session A runtime");
    let rt_b = state
        .session_cache
        .get_or_init(&profile_runtime, session_b.clone(), None)
        .await
        .expect("session B runtime");

    // Two sessions on the same profile must hold distinct
    // `Arc<ToolRegistry>` instances (codex multi-tenant scope note).
    assert!(
        !Arc::ptr_eq(&rt_a.tools, &rt_b.tools),
        "per-session tool registries must be distinct Arcs"
    );
    assert_ne!(rt_a.workspace_root, rt_b.workspace_root);

    // Session A writes a.txt under cwd_a; session B writes b.txt under cwd_b.
    rt_a.tools
        .execute(
            "write_file",
            &json!({ "path": "a.txt", "content": "from session A\n" }),
        )
        .await
        .expect("write_file under session A's workspace");
    rt_b.tools
        .execute(
            "write_file",
            &json!({ "path": "b.txt", "content": "from session B\n" }),
        )
        .await
        .expect("write_file under session B's workspace");

    // Cross-read MUST fail: session B cannot see a.txt; session A
    // cannot see b.txt. Per-session isolation enforced by the
    // workspace-bound registries built at SessionRuntime::bootstrap.
    let a_reads_a = rt_a
        .tools
        .execute("read_file", &json!({ "path": "a.txt" }))
        .await
        .expect("session A reads its own a.txt");
    assert!(a_reads_a.success, "{}", a_reads_a.output);
    assert!(a_reads_a.output.contains("from session A"));

    let b_reads_b = rt_b
        .tools
        .execute("read_file", &json!({ "path": "b.txt" }))
        .await
        .expect("session B reads its own b.txt");
    assert!(b_reads_b.success, "{}", b_reads_b.output);
    assert!(b_reads_b.output.contains("from session B"));

    // Cross-read MUST fail or return error output: session A cannot
    // see b.txt; session B cannot see a.txt. Per-session isolation
    // enforced by the workspace-bound registries built at
    // SessionRuntime::bootstrap.
    let a_cross = rt_a
        .tools
        .execute("read_file", &json!({ "path": "b.txt" }))
        .await
        .expect("read_file always returns a ToolResult");
    let a_cross_lower = a_cross.output.to_lowercase();
    assert!(
        !a_cross.success
            || a_cross_lower.contains("not found")
            || a_cross_lower.contains("no such")
            || a_cross_lower.contains("error"),
        "session A must NOT be able to read session B's b.txt; got: success={} output={}",
        a_cross.success,
        a_cross.output
    );

    let b_cross = rt_b
        .tools
        .execute("read_file", &json!({ "path": "a.txt" }))
        .await
        .expect("read_file always returns a ToolResult");
    let b_cross_lower = b_cross.output.to_lowercase();
    assert!(
        !b_cross.success
            || b_cross_lower.contains("not found")
            || b_cross_lower.contains("no such")
            || b_cross_lower.contains("error"),
        "session B must NOT be able to read session A's a.txt; got: success={} output={}",
        b_cross.success,
        b_cross.output
    );

    // Filesystem invariant: a.txt physically lives under cwd_a, b.txt
    // physically lives under cwd_b. Catches a regression where the
    // per-session tool somehow wrote to the legacy
    // `session_workspaces()`-resolved path instead of the workspace
    // the SessionRuntime was bootstrapped against.
    assert!(cwd_a.join("a.txt").exists());
    assert!(cwd_b.join("b.txt").exists());
    assert!(!cwd_a.join("b.txt").exists());
    assert!(!cwd_b.join("a.txt").exists());
}

/// M11-E codex round-1 MEDIUM: a second `session/open` for the same
/// session_id but a DIFFERENT cwd must NOT silently rebind a
/// running session's workspace. The cache's `get_or_init` is
/// single-flight per key; a same-key hit returns the cached
/// `Arc<SessionRuntime>` and ignores the new `workspace_hint`.
/// The `SessionOpened.workspace_root` reply MUST reflect the
/// CACHED runtime (not the just-requested cwd) — otherwise the
/// SPA renders one cwd while the next turn uses another, which is
/// exactly the wire/state divergence codex flagged.
#[tokio::test]
async fn second_session_open_with_new_cwd_reports_cached_workspace_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (state, _profile_runtime) = state_with_profile(temp.path(), "m11e-rebind-attempt").await;

    let first_cwd = temp.path().join("first");
    let second_cwd = temp.path().join("second");
    std::fs::create_dir_all(&first_cwd).expect("create first");
    std::fs::create_dir_all(&second_cwd).expect("create second");

    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey::with_profile("m11e-rebind-attempt", "api", "single-key");
    let features = ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        session_workspace_cwd: true,
        session_sandbox: false,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };

    let first_open = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("m11e-rebind-attempt"),
        None,
        features,
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: Some("m11e-rebind-attempt".into()),
            cwd: Some(first_cwd.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("first open");

    let second_open = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("m11e-rebind-attempt"),
        None,
        features,
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: Some("m11e-rebind-attempt".into()),
            // Different cwd — must NOT take effect; cache is sticky.
            cwd: Some(second_cwd.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("second open");

    let first_root = first_open
        .result
        .opened
        .workspace_root
        .expect("first workspace_root populated");
    let second_root = second_open
        .result
        .opened
        .workspace_root
        .expect("second workspace_root populated");
    assert_eq!(
        std::fs::canonicalize(&first_root).expect("canonicalize first"),
        std::fs::canonicalize(&second_root).expect("canonicalize second"),
        "second open of the same session must report the cached \
             workspace_root, not the just-requested cwd",
    );
    assert_eq!(
        std::fs::canonicalize(&second_root).expect("canonicalize"),
        std::fs::canonicalize(&first_cwd).expect("canonicalize"),
        "cached workspace_root must equal the FIRST open's cwd",
    );
}

/// M11-E codex round-3 HIGH: when `state.profiles` is non-empty but
/// the ROUTED profile is not in it, `validate_session_workspace_allowed`
/// must still reject the cwd with `cwd_runtime_unavailable`.
/// Otherwise the wire reply would advertise the requested
/// workspace_root while the turn dispatcher's legacy fallback uses
/// `base_agent`'s root — exactly the divergence the prior round
/// closed for the empty-profiles case. The fix routes the active
/// profile id into the validator so it can resolve the SPECIFIC
/// runtime rather than checking the map non-empty.
#[tokio::test]
async fn session_open_with_cwd_for_unregistered_profile_is_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (state, _profile_runtime) = state_with_profile(temp.path(), "m11e-registered").await;

    let cwd = temp.path().join("repo");
    std::fs::create_dir_all(&cwd).expect("create cwd");

    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();

    // Build a SessionKey under a profile id that is NOT in
    // `state.profiles` so the active profile resolution misses.
    let session_id = SessionKey::with_profile("m11e-not-registered", "api", "x");
    let features = ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        session_workspace_cwd: true,
        session_sandbox: false,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };

    let error = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        // No connection identity so the routed id falls to the
        // session-id-embedded "m11e-not-registered".
        None,
        None,
        features,
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id,
            topic: None,
            profile_id: None,
            cwd: Some(cwd.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect_err("session/open for unregistered profile must reject");

    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("profile_unresolved")),
        "unregistered profile must surface profile_unresolved before cwd policy; \
             got error data {:?}",
        error.data,
    );
}

/// M11-E codex round-1 HIGH (filed against octos-agent, not blocking
/// M11-E): symlink-based directory escape is an octos-agent
/// `resolve_path` + `read_no_follow`/`write_no_follow` property,
/// not a UI-Protocol property. M11-E binds each session's tools to
/// its own workspace_root via `SessionRuntime::bootstrap`; the
/// remaining gap is that the tool layer follows directory
/// symlinks (only the final path component is checked).
///
/// This test pins the CURRENT octos-agent behavior so a future
/// `read_no_follow`/`write_no_follow` hardening flips it green
/// automatically. We document the gap in the PR body and propose
/// it as a follow-up octos-agent issue (NOT a downstream M11
/// ticket — the tool layer is the right home).
///
/// Marking `#[ignore]` rather than failing the suite: the gap is
/// pre-existing, M11-E neither introduces nor fixes it, and a
/// failing test here would block landing M11-E for a problem
/// that lives in a different crate.
#[tokio::test]
#[ignore = "octos-agent gap: directory symlinks escape per-session workspace; tracked as follow-up issue, NOT blocking M11-E"]
async fn parent_directory_symlink_escapes_per_session_workspace_documents_gap() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (state, profile_runtime) = state_with_profile(temp.path(), "m11e-symlink").await;

    let cwd_a = temp.path().join("session-a");
    let cwd_b = temp.path().join("session-b");
    std::fs::create_dir_all(&cwd_a).expect("create cwd-a");
    std::fs::create_dir_all(&cwd_b).expect("create cwd-b");
    std::fs::write(cwd_b.join("secret.txt"), "B's private data\n").expect("seed b");

    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_a = SessionKey::with_profile("m11e-symlink", "api", "session-a");
    let features = ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        session_workspace_cwd: true,
        session_sandbox: false,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };

    let _ = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("m11e-symlink"),
        None,
        features,
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_a.clone(),
            topic: None,
            profile_id: Some("m11e-symlink".into()),
            cwd: Some(cwd_a.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("session A open");

    // Plant a directory symlink inside session A's workspace
    // pointing at session B's workspace. The path-normalize check
    // in `resolve_path` and the O_NOFOLLOW guard in
    // `read_no_follow` both pass — only the FINAL path component is
    // checked for the symlink-rejection invariant.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&cwd_b, cwd_a.join("escape")).expect("plant symlink");
    }
    #[cfg(not(unix))]
    {
        // The gap is Unix-specific (Windows tool fallback uses a
        // separate symlink-check pattern). Skip the planting on
        // non-Unix.
        return;
    }

    let rt_a = state
        .session_cache
        .get_or_init(&profile_runtime, session_a.clone(), None)
        .await
        .expect("cached session A runtime");

    let result = rt_a
        .tools
        .execute("read_file", &json!({ "path": "escape/secret.txt" }))
        .await
        .expect("read_file returns a ToolResult");

    // CURRENT BEHAVIOR (M11-E lock-in): octos-agent follows the
    // parent directory symlink and reads B's file. The
    // `#[ignore]` keeps this from failing CI while we file the
    // octos-agent issue; flip to `assert!(!result.success)` when
    // the tool-layer fix lands.
    assert!(
        result.success && result.output.contains("B's private data"),
        "octos-agent should currently follow the directory symlink \
             (M11-E gap documentation); got success={} output={}",
        result.success,
        result.output
    );
}

/// M11-F deliverable D — restore `appui.default_session_cwd` Tier-2
/// fallback that M11-E's `clone_session_tools` deletion took out.
///
/// Pre-resolution order on `session.open`:
///   1. Tier 1 — client-supplied `cwd` (already wired via
///      `session.workspace_cwd.v1` + `validate_session_workspace_allowed`).
///   2. Tier 2 — operator-configured `appui.default_session_cwd`
///      mirrored on `AppState::appui_default_session_cwd`. **This
///      test pins that wiring.**
///   3. Tier 3 — `SessionRuntime::bootstrap`'s
///      `<profile.data_dir>/users/<encoded base>/workspace` default.
///
/// Scenario: a client that does NOT advertise
/// `session.workspace_cwd.v1` opens an AppUI session with no `cwd`.
/// `AppState::appui_default_session_cwd` is set to an operator
/// directory. The materialized `SessionRuntime.workspace_root` MUST
/// equal the operator default — not the profile-data-relative
/// Tier-3 fallback — and the wire `workspace_root` must reflect it.
#[tokio::test]
async fn appui_session_without_client_cwd_respects_operator_default_session_cwd() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (mut state_inner, profile_runtime) = {
        let (state, profile_runtime) = state_with_profile(temp.path(), "m11f-tier2-default").await;
        // We need to mutate AppState (set appui_default_session_cwd)
        // before sharing it. `state_with_profile` returns an
        // `Arc<AppState>`; for the test we unwrap via
        // `Arc::try_unwrap` knowing this is the only reference.
        (
            Arc::try_unwrap(state)
                .map_err(|_| "state Arc must be unique for test setup")
                .expect("unique Arc"),
            profile_runtime,
        )
    };

    // Operator-configured Tier-2 default. The directory exists and
    // contains a sentinel the session is expected to read back via
    // its own (workspace-bound) read_file tool.
    let operator_default = temp.path().join("operator-default-workspace");
    std::fs::create_dir_all(&operator_default).expect("create operator default");
    std::fs::write(
        operator_default.join("hello.txt"),
        "tier-2 operator default visible to session\n",
    )
    .expect("seed sentinel");
    state_inner.appui_default_session_cwd = Some(operator_default.clone());
    let state = Arc::new(state_inner);

    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();

    let session_id = SessionKey::with_profile("m11f-tier2-default", "api", "tier2-no-client-cwd");
    // IMPORTANT: client does NOT advertise `session.workspace_cwd.v1`
    // and does NOT send a cwd — this is the exact octos-app shape
    // that the M11-E deletion of `clone_session_tools` broke.
    let features = ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        session_workspace_cwd: false,
        session_sandbox: false,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };

    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("m11f-tier2-default"),
        None,
        features,
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: Some("m11f-tier2-default".into()),
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("session/open with operator default cwd must succeed");

    // Wire response carries the operator-default workspace root.
    let opened_root = outcome
        .result
        .opened
        .workspace_root
        .as_ref()
        .expect("opened workspace_root populated");
    assert_eq!(
        std::fs::canonicalize(opened_root).expect("canonicalize opened root"),
        std::fs::canonicalize(&operator_default).expect("canonicalize operator default"),
        "Tier-2: SessionOpened.workspace_root must equal appui.default_session_cwd",
    );

    // The cached SessionRuntime is bound to the operator default —
    // not the Tier-3 profile-data-relative fallback.
    let session_runtime = state
        .session_cache
        .get_or_init(&profile_runtime, session_id.clone(), None)
        .await
        .expect("cached session runtime");
    assert_eq!(
        std::fs::canonicalize(&session_runtime.workspace_root).expect("canonicalize runtime root"),
        std::fs::canonicalize(&operator_default).expect("canonicalize operator default"),
        "Tier-2: SessionRuntime.workspace_root must equal appui.default_session_cwd",
    );

    // End-to-end: a read_file against the relative sentinel resolves
    // inside the operator default, proving the per-session
    // ToolRegistry was rebound to that root (not the Tier-3
    // `<profile_data_dir>/users/<encoded>/workspace`).
    let result = session_runtime
        .tools
        .execute("read_file", &json!({ "path": "hello.txt" }))
        .await
        .expect("read_file via session tools");
    assert!(
        result.success,
        "read_file must succeed under operator default: {}",
        result.output
    );
    assert!(
        result
            .output
            .contains("tier-2 operator default visible to session"),
        "expected operator-default sentinel content, got: {}",
        result.output
    );
}

/// A no-cwd AppUI session still gets a derived Tier-3 workspace for tools, but
/// that derived path is not a caller-supplied cwd and must never relocate the
/// transcript store when `appui.sessions_in_cwd` is enabled.
///
/// Regression: `session/open` used to write the derived workspace root into
/// `session_workspaces()`. A later lookup then fed that root back into the
/// runtime cache as a cwd hint, creating a second runtime under
/// `<derived-workspace>/.octos/<profile>` and making the session disappear
/// from the profile-global session list.
#[tokio::test]
async fn appui_no_cwd_workspace_does_not_become_a_transcript_store_hint() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile_id = "no-cwd-store-stability";
    let (state, profile_runtime) = state_with_profile(temp.path(), profile_id).await;
    let mut state_inner = Arc::try_unwrap(state)
        .map_err(|_| "state Arc must be unique for test setup")
        .expect("unique Arc");
    state_inner.session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(8, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let state = Arc::new(state_inner);

    let session_id = SessionKey::with_profile(profile_id, "api", "learn-no-cwd");
    let outcome = open_session_result(
        &state,
        &UiProtocolLedger::new(16),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some(profile_id),
        None,
        ConnectionUiFeatures {
            workspace_session_refs_v1: false,
            session_workspace_cwd: false,
            header_present: true,
            ..ConnectionUiFeatures::default()
        },
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: Some(profile_id.into()),
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("no-cwd session/open must succeed");

    let workspace_root = outcome
        .result
        .opened
        .workspace_root
        .expect("Tier-3 workspace root is still exposed for tools and UI");
    // `SessionOpenResult.workspace_root` is wire-typed String; convert so the
    // comparison against the runtime's PathBuf still type-checks.
    assert_ne!(PathBuf::from(workspace_root), profile_runtime.data_dir);

    let sessions = resolve_sessions_for_lookup(&state, None, Some(profile_id), &session_id)
        .await
        .expect("session manager resolves after open");
    assert_eq!(
        sessions.lock().await.data_dir(),
        profile_runtime.data_dir,
        "derived Tier-3 workspace must not be replayed as a cwd hint",
    );
}

/// The provenance split must not weaken real per-project sessions: an
/// explicitly supplied cwd remains the runtime hint used by later lookup
/// paths, so transcript storage stays under that project's `.octos` root.
#[tokio::test]
async fn appui_explicit_cwd_remains_a_transcript_store_hint() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile_id = "explicit-cwd-store-stability";
    let (state, _profile_runtime) = state_with_profile(temp.path(), profile_id).await;
    let mut state_inner = Arc::try_unwrap(state)
        .map_err(|_| "state Arc must be unique for test setup")
        .expect("unique Arc");
    state_inner.session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(8, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let state = Arc::new(state_inner);

    let workspace = temp.path().join("project");
    std::fs::create_dir_all(&workspace).expect("create explicit workspace");
    let workspace = std::fs::canonicalize(workspace).expect("canonicalize explicit workspace");
    let session_id = SessionKey::with_profile(profile_id, "api", "coding-with-cwd");
    open_session_result(
        &state,
        &UiProtocolLedger::new(16),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some(profile_id),
        None,
        ConnectionUiFeatures {
            workspace_session_refs_v1: false,
            session_workspace_cwd: true,
            header_present: true,
            ..ConnectionUiFeatures::default()
        },
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: Some(profile_id.into()),
            cwd: Some(workspace.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("explicit-cwd session/open must succeed");

    let sessions = resolve_sessions_for_lookup(&state, None, Some(profile_id), &session_id)
        .await
        .expect("explicit-cwd session manager resolves after open");
    assert_eq!(
        sessions.lock().await.data_dir(),
        crate::runtime::session::project_sessions_root(&workspace, profile_id),
        "explicit cwd must remain the transcript-store hint",
    );
}

/// #2162: `session/open` with a raw wire id plus an explicit `profile_id`
/// succeeds on an unscoped connection, but the id itself carries no profile
/// — later lookups must still land in the open's profile manager, not fall
/// through to the legacy global one (`unknown_session`).
///
/// The binding registry is process-global, so every test below keys its
/// sessions on ids unique to that test.
#[tokio::test]
async fn appui_raw_open_profile_binding_keeps_the_session_addressable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile_id = "raw-open-binding";
    let (state, profile_runtime) = state_with_profile(temp.path(), profile_id).await;

    let raw_id = SessionKey::new("api", "binding-addressable");
    open_session_result(
        &state,
        &UiProtocolLedger::new(16),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: raw_id.clone(),
            topic: None,
            profile_id: Some(profile_id.into()),
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("raw-id session/open with an explicit profile must succeed");

    let sessions = resolve_sessions_for_lookup(&state, None, None, &raw_id)
        .await
        .expect("open binding tier resolves the session manager");
    assert_eq!(
        sessions.lock().await.data_dir(),
        profile_runtime.data_dir,
        "lookup must land in the profile the open resolved",
    );
    assert!(
        sessions.lock().await.session_known(&raw_id),
        "the session open created must be known under its raw wire id",
    );
}

/// #2162: `session/status/read` for a raw id on an unscoped connection must
/// resolve the open's profile instead of reporting `_main` as unresolved.
#[tokio::test]
async fn raw_session_status_read_resolves_open_profile_binding() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile_id = "raw-open-status";
    let (state, _profile_runtime) = state_with_profile(temp.path(), profile_id).await;

    let raw_id = SessionKey::new("api", "binding-status");
    open_session_result(
        &state,
        &UiProtocolLedger::new(16),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: raw_id.clone(),
            topic: None,
            profile_id: Some(profile_id.into()),
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("raw-id session/open with an explicit profile must succeed");

    let status = raw_session_status_result(
        &state,
        &RpcRequest::<Value>::new(
            "status-after-raw-open",
            APPUI_METHOD_SESSION_STATUS_READ,
            json!({ "session_id": raw_id.0 }),
        ),
        ConnectionUiFeatures::default(),
        None,
    )
    .await
    .expect("status/read must resolve the open binding, not _main");
    assert_eq!(status["profile_id"], json!(profile_id));
}

/// #2162 round 2: reopening the same raw wire id under another profile
/// rebinds it — the last open owns the wire id.
#[tokio::test]
async fn appui_reopen_raw_id_under_new_profile_rebinds_the_binding() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (state, first_runtime) = state_with_profile(temp.path(), "rebind-first").await;
    let second_dir = tempfile::tempdir().expect("tempdir");
    let (_second_state, second_runtime) =
        state_with_profile(second_dir.path(), "rebind-second").await;
    let mut state_inner = Arc::try_unwrap(state)
        .map_err(|_| "state Arc must be unique for test setup")
        .expect("unique Arc");
    state_inner
        .profiles
        .insert("rebind-second".into(), second_runtime.clone());
    let state = Arc::new(state_inner);

    let raw_id = SessionKey::new("api", "rebind-target");
    let open = |profile_id: &'static str| {
        let state = state.clone();
        let raw_id = raw_id.clone();
        async move {
            open_session_result(
                &state,
                &UiProtocolLedger::new(16),
                &PendingApprovalStore::default(),
                &PendingQuestionStore::default(),
                ConnectionId::next(),
                None,
                None,
                ConnectionUiFeatures::default(),
                SessionOpenParams {
                    session_ref: None,
                    client_commands: None,
                    session_id: raw_id,
                    topic: None,
                    profile_id: Some(profile_id.to_owned()),
                    cwd: None,
                    sandbox: None,
                    after: None,
                },
            )
            .await
            .expect("raw-id session/open with an explicit profile must succeed")
        }
    };
    open("rebind-first").await;
    let sessions = resolve_sessions_for_lookup(&state, None, None, &raw_id)
        .await
        .expect("first binding resolves");
    assert_eq!(sessions.lock().await.data_dir(), first_runtime.data_dir);

    open("rebind-second").await;
    let sessions = resolve_sessions_for_lookup(&state, None, None, &raw_id)
        .await
        .expect("rebound binding resolves");
    assert_eq!(
        sessions.lock().await.data_dir(),
        second_runtime.data_dir,
        "the last open owns the wire id",
    );
}

/// #2162 round 2: a profile-less reopen of a raw id must not steal an
/// existing binding onto `_main` — the open itself resolved the session
/// through the binding and shares one manager with the lookups either way.
#[tokio::test]
async fn appui_profileless_reopen_keeps_the_existing_binding() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile_id = "no-steal";
    let (state, profile_runtime) = state_with_profile(temp.path(), profile_id).await;

    let raw_id = SessionKey::new("api", "no-steal-target");
    for params_profile in [Some(profile_id.to_owned()), None] {
        open_session_result(
            &state,
            &UiProtocolLedger::new(16),
            &PendingApprovalStore::default(),
            &PendingQuestionStore::default(),
            ConnectionId::next(),
            None,
            None,
            ConnectionUiFeatures::default(),
            SessionOpenParams {
                session_ref: None,
                client_commands: None,
                session_id: raw_id.clone(),
                topic: None,
                profile_id: params_profile,
                cwd: None,
                sandbox: None,
                after: None,
            },
        )
        .await
        .expect("session/open must succeed");
    }

    let sessions = resolve_sessions_for_lookup(&state, None, None, &raw_id)
        .await
        .expect("binding survives the profile-less reopen");
    assert_eq!(
        sessions.lock().await.data_dir(),
        profile_runtime.data_dir,
        "the profile-less reopen must not rebind the wire id to _main",
    );
}

/// #2162 round 2: a connection's authenticated profile outranks the binding
/// — profile-scoped connections keep their existing isolation.
#[tokio::test]
async fn appui_connection_profile_outranks_the_open_binding() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile_id = "binding-shadowed";
    let (state, profile_runtime) = state_with_profile(temp.path(), profile_id).await;

    let raw_id = SessionKey::new("api", "shadowed-target");
    open_session_result(
        &state,
        &UiProtocolLedger::new(16),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: raw_id.clone(),
            topic: None,
            profile_id: Some(profile_id.into()),
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("raw-id session/open with an explicit profile must succeed");

    let sessions = resolve_sessions_for_lookup(&state, Some("other-profile"), None, &raw_id)
        .await
        .expect("connection-profile lookup resolves");
    assert_ne!(
        sessions.lock().await.data_dir(),
        profile_runtime.data_dir,
        "the connection profile must win over the open binding",
    );
}

/// #2162 round 2 (review MAJOR 2): forking a raw-id parent yields a raw
/// child id — the child must be bound to the profile the parent resolved
/// under, or the fork mints another half-usable identity.
#[tokio::test]
async fn appui_fork_binds_a_raw_child_to_the_parent_profile() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile_id = "fork-bind";
    let (state, profile_runtime) = state_with_profile(temp.path(), profile_id).await;

    let raw_id = SessionKey::new("api", "fork-parent");
    open_session_result(
        &state,
        &UiProtocolLedger::new(16),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: raw_id.clone(),
            topic: None,
            profile_id: Some(profile_id.into()),
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("raw-id session/open with an explicit profile must succeed");

    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_fork(
        &ws,
        &state,
        None,
        None,
        "fork-bind-1".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: raw_id.clone(),
            new_chat_id: "kid".into(),
            copy_messages: None,
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    let child = frame
        .get("result")
        .and_then(|result| result.get("new_session_id"))
        .and_then(Value::as_str)
        .expect("fork must succeed and echo the child id")
        .to_owned();

    let child_key = SessionKey(child);
    assert!(
        child_key.profile_id().is_none(),
        "the parent is a raw id, so the child must be raw too",
    );
    let sessions = resolve_sessions_for_lookup(&state, None, None, &child_key)
        .await
        .expect("child binding resolves the session manager");
    assert_eq!(
        sessions.lock().await.data_dir(),
        profile_runtime.data_dir,
        "the fork child must land in the parent's profile manager",
    );
    assert!(
        sessions.lock().await.session_known(&child_key),
        "the fork child must be known under its raw wire id",
    );
}

#[tokio::test]
async fn should_keep_profile_transcript_store_with_fresh_cache_after_no_cwd_reseed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile_id = "restart-no-cwd";
    let (_state, profile_runtime) = state_with_profile(temp.path(), profile_id).await;
    let workspace = temp.path().join("derived-workspace");
    std::fs::create_dir_all(&workspace).expect("create derived workspace");
    let workspace = std::fs::canonicalize(workspace).expect("canonicalize workspace");
    let wire = SessionKey::with_profile(profile_id, "api", "restart-derived");
    let workspaces = SessionWorkspaceStore::default();
    let orchestrator = InProcessAgentOrchestrator::default();

    reseed_fleet_keeper_candidates(
        &workspaces,
        &orchestrator,
        vec![FleetKeeperSeed {
            wire: wire.clone(),
            scope: None,
            root: workspace.to_string_lossy().into_owned(),
            workspace_has_runtime_hint: Some(false),
        }],
    );
    let binding = workspaces
        .snapshot(profile_id, &wire)
        .expect("reseeded binding");
    let fresh_cache =
        crate::runtime::SessionRuntimeCache::new(8, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true);
    let runtime = fresh_cache
        .get_or_init(&profile_runtime, wire, binding.runtime_hint)
        .await
        .expect("fresh runtime");

    assert_eq!(
        runtime.sessions_root, profile_runtime.data_dir,
        "a no-cwd keeper must remain in the profile-global transcript store after restart"
    );
}

#[tokio::test]
async fn should_keep_project_transcript_store_with_fresh_cache_after_explicit_cwd_reseed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile_id = "restart-explicit-cwd";
    let (_state, profile_runtime) = state_with_profile(temp.path(), profile_id).await;
    let workspace = temp.path().join("explicit-workspace");
    std::fs::create_dir_all(&workspace).expect("create explicit workspace");
    let workspace = std::fs::canonicalize(workspace).expect("canonicalize workspace");
    let wire = SessionKey::with_profile(profile_id, "api", "restart-explicit");
    let workspaces = SessionWorkspaceStore::default();
    let orchestrator = InProcessAgentOrchestrator::default();

    reseed_fleet_keeper_candidates(
        &workspaces,
        &orchestrator,
        vec![FleetKeeperSeed {
            wire: wire.clone(),
            scope: None,
            root: workspace.to_string_lossy().into_owned(),
            workspace_has_runtime_hint: Some(true),
        }],
    );
    let binding = workspaces
        .snapshot(profile_id, &wire)
        .expect("reseeded binding");
    let fresh_cache =
        crate::runtime::SessionRuntimeCache::new(8, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true);
    let runtime = fresh_cache
        .get_or_init(&profile_runtime, wire, binding.runtime_hint)
        .await
        .expect("fresh runtime");

    assert_eq!(
        runtime.sessions_root,
        crate::runtime::session::project_sessions_root(&workspace, profile_id),
        "an explicit cwd keeper must remain in the project transcript store after restart"
    );
}
