//! UPCR-2026-034 — host-owned app peers: `peer/prepare` host binding, model
//! parity and resume, `peer/model/set`, `peer/context/{open,close}`, and the
//! session-level enforcement of workspace, memory namespace and closure.
use super::*;

struct Fixture {
    _tmp: tempfile::TempDir,
    state: Arc<AppState>,
    runtime: Arc<crate::runtime::ProfileRuntime>,
    data_dir: PathBuf,
    apps: PathBuf,
    system: SessionKey,
    /// Host tokens returned at creation, by peer name.
    tokens: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

fn tok(fx: &Fixture, name: &str) -> Value {
    fx.tokens
        .lock()
        .unwrap()
        .get(name)
        .map(|t| json!(t))
        .unwrap_or(Value::Null)
}

fn remember(fx: &Fixture, name: &str, result: &Value) {
    if let Some(token) = result["host_token"].as_str() {
        fx.tokens
            .lock()
            .unwrap()
            .insert(name.to_owned(), token.to_owned());
    }
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let strong: crate::config::SubProviderConfig = serde_json::from_value(json!({
        "key": "strong",
        "provider": "openai",
        "model": "gpt-4o",
        "api_key_env": "HOST_PEER_TEST_KEY",
    }))
    .unwrap();
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
                        api_key_env: Some("HOST_PEER_TEST_KEY".to_string()),
                        api_type: None,
                    }),
                    ..Default::default()
                }),
                fallbacks: Vec::new(),
            }),
            sub_providers: vec![strong],
            env_vars: [("HOST_PEER_TEST_KEY".to_string(), "test-key".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(data_dir.join("memory")).unwrap();
    // The SYSTEM agent's private memory: no app peer may see it.
    std::fs::write(
        data_dir.join("memory/MEMORY.md"),
        "- SYSTEM-PRIVATE-FACT: the owner's bank PIN hint\n",
    )
    .unwrap();
    let runtime = crate::runtime::ProfileRuntime::bootstrap(
        &profile,
        &data_dir,
        None,
        crate::runtime::BootstrapRole::Serve,
    )
    .await
    .expect("bootstrap dev runtime");
    let mut state = AppState::empty_for_tests();
    state.profiles.insert("dev".to_string(), runtime.clone());
    let apps = tmp.path().join("apps");
    std::fs::create_dir_all(apps.join("rinx")).unwrap();
    std::fs::create_dir_all(apps.join("notes")).unwrap();
    Fixture {
        state: Arc::new(state),
        runtime,
        data_dir,
        apps,
        system: SessionKey::with_profile_topic("dev", "api", "host", "system"),
        tokens: Default::default(),
        _tmp: tmp,
    }
}

fn rpc(method: &str, params: Value) -> RpcRequest<Value> {
    RpcRequest::new("host-1".to_string(), method, params)
}

async fn prepare_app(
    fx: &Fixture,
    name: &str,
    app: &str,
    ns: &str,
    resume: bool,
) -> Result<Value, RpcError> {
    raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": format!("You are the {name} app's assistant."),
                "names": [name],
                "cwd": fx.apps.join(app).to_string_lossy(),
                "session_id": fx.system,
                "memory_namespace": ns,
                "resume": resume,
                "host_token": tok(fx, name),
            }),
        ),
        None,
    )
    .await
    .inspect(|result| remember(fx, name, result))
}

async fn segment(runtime: &crate::runtime::SessionRuntime) -> String {
    runtime.agent.refresh_prompt_segments().await;
    runtime
        .agent
        .prompt_segment_snapshot(octos_agent::MEMORY_SEGMENT_NAME)
        .unwrap_or_default()
}

#[tokio::test]
async fn should_stage_and_resume_a_host_owned_app_peer_with_a_persisted_system_originator() {
    let fx = fixture().await;
    let staged = prepare_app(&fx, "Rinx", "rinx", "app/rinx/acct-1", true)
        .await
        .expect("stage");
    assert_eq!(staged["slug"], "rinx");
    assert_eq!(staged["resumed"], false);
    assert_eq!(staged["memory_namespace"], "app/rinx/acct-1");
    assert_eq!(staged["model"], json!({ "lane": "primary" }));
    let peer_dir = fx.data_dir.join("peers/rinx");
    assert_eq!(
        std::fs::read_to_string(peer_dir.join("originator")).unwrap(),
        fx.system.0,
        "the system agent session is the durable originator"
    );

    // Reopening the app resumes the SAME peer instead of minting another.
    let resumed = prepare_app(&fx, "Rinx", "rinx", "app/rinx/acct-1", true)
        .await
        .expect("resume");
    assert_eq!(resumed["slug"], "rinx");
    assert_eq!(resumed["resumed"], true);
    assert_eq!(resumed["cwd"], staged["cwd"]);

    // Without resume the name stays taken (existing peer/prepare behavior).
    assert!(
        prepare_app(&fx, "Rinx", "rinx", "app/rinx/acct-1", false)
            .await
            .is_err()
    );

    // A different account namespace is a different binding, never a silent rebind.
    let err = prepare_app(&fx, "Rinx", "rinx", "app/rinx/acct-2", true)
        .await
        .expect_err("namespace change");
    assert_eq!(err.data.as_ref().unwrap()["kind"], "peer_binding_mismatch");

    // Another session cannot resume (or take over) the system agent's peer.
    let err = raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": "hijack", "names": ["Rinx"],
                "cwd": fx.apps.join("rinx").to_string_lossy(),
                "session_id": SessionKey::with_profile_topic("dev", "api", "host", "other"),
                "memory_namespace": "app/rinx/acct-1", "resume": true,
            }),
        ),
        None,
    )
    .await
    .expect_err("foreign originator");
    assert_eq!(
        err.data.as_ref().unwrap()["kind"],
        "peer_originator_mismatch"
    );
}

#[tokio::test]
async fn should_reject_incomplete_host_bindings() {
    let fx = fixture().await;
    for params in [
        json!({ "brief": "b", "names": ["X"], "cwd": fx.apps.join("rinx").to_string_lossy(), "memory_namespace": "app/x" }),
        json!({ "brief": "b", "names": ["X"], "session_id": fx.system, "cwd": fx.apps.join("rinx").to_string_lossy(), "memory_namespace": "../x" }),
        json!({ "brief": "b", "names": ["X", "Y"], "n": 2, "session_id": fx.system, "cwd": fx.apps.join("rinx").to_string_lossy(), "memory_namespace": "app/x" }),
        json!({ "brief": "b", "names": ["X"], "session_id": fx.system, "cwd": fx.apps.join("rinx").to_string_lossy(), "resume": true }),
    ] {
        assert!(
            raw_peer_prepare(
                &fx.state,
                &rpc(APPUI_METHOD_PEER_PREPARE, params.clone()),
                None
            )
            .await
            .is_err(),
            "{params}"
        );
    }
}

#[tokio::test]
async fn should_select_a_configured_model_for_one_peer_without_touching_the_profile_default() {
    let fx = fixture().await;
    // peer/prepare now takes peer_handoff's `model` lane.
    let staged = raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": "b", "names": ["Rinx"], "session_id": fx.system,
                "cwd": fx.apps.join("rinx").to_string_lossy(),
                "memory_namespace": "app/rinx/acct-1", "model": "strong",
            }),
        ),
        None,
    )
    .await
    .unwrap();
    remember(&fx, "Rinx", &staged);
    assert_eq!(
        staged["model"],
        json!({ "lane": "strong", "provider": "openai", "model": "gpt-4o" })
    );
    assert!(staged["model_note"].is_null());

    // An unknown lane at staging falls back to primary and SAYS so.
    let fallback = prepare_app(&fx, "Notes", "notes", "app/notes/acct-1", false)
        .await
        .unwrap();
    assert_eq!(fallback["model"], json!({ "lane": "primary" }));
    let noted = raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({ "brief": "b", "title": "plain", "cwd": fx.apps.join("notes").to_string_lossy(), "model": "turbo", "profile_id": "dev" }),
        ),
        None,
    )
    .await
    .unwrap();
    assert_eq!(noted["model"], json!({ "lane": "primary" }));
    assert!(noted["model_note"].as_str().unwrap().contains("not found"));

    // peer/model/set changes ONE existing peer between turns.
    let set = raw_peer_model_set(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_MODEL_SET,
            json!({ "session_id": fx.system, "host_token": tok(&fx, "Notes"), "peer": "Notes", "model": "strong" }),
        ),
        None,
    )
    .unwrap();
    assert_eq!(set["model"]["lane"], "strong");
    assert_eq!(set["applies"], "next_turn");
    assert_eq!(
        read_peer_model_lane(&fx.data_dir.join("peers"), "notes").as_deref(),
        Some("strong")
    );
    assert_eq!(
        read_peer_model_lane(&fx.data_dir.join("peers"), "rinx").as_deref(),
        Some("strong")
    );

    // Unknown lanes are refused and change nothing; clearing returns to primary.
    let err = raw_peer_model_set(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_MODEL_SET,
            json!({ "session_id": fx.system, "host_token": tok(&fx, "Notes"), "peer": "Notes", "model": "turbo" }),
        ),
        None,
    )
    .unwrap_err();
    assert_eq!(err.data.as_ref().unwrap()["kind"], "peer_model_unknown");
    assert_eq!(
        read_peer_model_lane(&fx.data_dir.join("peers"), "notes").as_deref(),
        Some("strong")
    );
    let cleared = raw_peer_model_set(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_MODEL_SET,
            json!({ "session_id": fx.system, "host_token": tok(&fx, "Notes"), "peer": "Notes", "model": null }),
        ),
        None,
    )
    .unwrap();
    assert_eq!(cleared["model"], json!({ "lane": "primary" }));

    // Only the owner may change a peer's model.
    let err = raw_peer_model_set(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_MODEL_SET,
            json!({ "session_id": SessionKey::with_profile_topic("dev", "api", "host", "other"), "peer": "Rinx", "model": null }),
        ),
        None,
    )
    .unwrap_err();
    assert_eq!(
        err.data.as_ref().unwrap()["kind"],
        "peer_originator_mismatch"
    );

    // The profile's primary selection and lanes are untouched.
    let lanes = profile_model_lanes(&fx.state, "dev");
    assert_eq!(lanes.len(), 1);
    assert_eq!(fx.runtime.config.sub_providers.len(), 1);
}

#[tokio::test]
async fn should_isolate_app_peer_workspace_and_memory_from_the_system_and_each_other() {
    let fx = fixture().await;
    let rinx = prepare_app(&fx, "Rinx", "rinx", "app/rinx/acct-1", true)
        .await
        .unwrap();
    prepare_app(&fx, "Notes", "notes", "app/notes/acct-1", true)
        .await
        .unwrap();
    let rinx_key = SessionKey(format!("{}#peer-rinx", fx.system.base_key()));
    let notes_key = SessionKey(format!("{}#peer-notes", fx.system.base_key()));

    // A bound peer session runs in its workspace even without a hint...
    let rinx_rt = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, rinx_key.clone(), None)
        .await
        .expect("bound peer session");
    assert_eq!(
        dunce::canonicalize(&rinx_rt.workspace_root).unwrap(),
        PathBuf::from(rinx["cwd"].as_str().unwrap())
    );
    assert_eq!(rinx_rt.memory.namespace.as_deref(), Some("app/rinx/acct-1"));
    // ...and refuses to be reopened anywhere else.
    let elsewhere = crate::runtime::SessionRuntime::bootstrap(
        &fx.runtime,
        rinx_key.clone(),
        Some(fx.apps.join("notes")),
    )
    .await;
    assert!(
        elsewhere.is_err(),
        "a bound peer cannot be opened in another workspace"
    );

    // Automatic memory injection never carries the system's private memory.
    assert!(!segment(&rinx_rt).await.contains("SYSTEM-PRIVATE-FACT"));

    // What Rinx captures stays in Rinx's namespace.
    rinx_rt
        .memory
        .memory_store
        .write_long_term("- RINX-APP-FACT: prefers dark room lists\n")
        .await
        .unwrap();
    assert!(segment(&rinx_rt).await.contains("RINX-APP-FACT"));
    let notes_rt = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, notes_key, None)
        .await
        .unwrap();
    let notes_segment = segment(&notes_rt).await;
    assert!(
        !notes_segment.contains("RINX-APP-FACT"),
        "another app never sees it"
    );
    assert!(!notes_segment.contains("SYSTEM-PRIVATE-FACT"));
    let system_memory = fx.runtime.memory_store.read_long_term().await.unwrap();
    assert!(
        !system_memory.contains("RINX-APP-FACT"),
        "the system memory is untouched"
    );

    // Memory tools are rebound to the namespace; profile-memory writers are gone.
    assert!(rinx_rt.tools.get_tool("save_memory").is_some());
    assert!(rinx_rt.tools.get_tool("memory_note").is_none());
    assert!(rinx_rt.tools.get_tool("run_pipeline").is_none());
    assert!(!Arc::ptr_eq(
        &rinx_rt.memory.memory_store,
        &fx.runtime.memory_store
    ));
    assert!(!Arc::ptr_eq(&rinx_rt.memory.episodes, &fx.runtime.memory));

    // An ordinary session keeps the profile's own memory.
    let plain = crate::runtime::SessionRuntime::bootstrap(
        &fx.runtime,
        SessionKey::with_profile_topic("dev", "api", "host", "chat"),
        None,
    )
    .await
    .unwrap();
    assert!(plain.memory.namespace.is_none());
    assert!(segment(&plain).await.contains("SYSTEM-PRIVATE-FACT"));
}

#[tokio::test]
async fn should_open_isolated_request_contexts_and_refuse_them_after_close() {
    let fx = fixture().await;
    prepare_app(&fx, "Rinx", "rinx", "app/rinx/acct-1", true)
        .await
        .unwrap();
    let open = |context_id: &str| {
        raw_peer_context_open(
            &fx.state,
            &rpc(
                APPUI_METHOD_PEER_CONTEXT_OPEN,
                json!({ "session_id": fx.system, "host_token": tok(&fx, "Rinx"), "peer": "Rinx", "context_id": context_id }),
            ),
            None,
        )
    };
    let a = open("mini-a").unwrap();
    let b = open("mini-b").unwrap();
    assert_eq!(a["created"], true);
    assert_eq!(
        a["session_id"],
        format!("{}#peerctx-rinx.mini-a", fx.system.base_key())
    );
    assert_eq!(a["memory_namespace"], "app/rinx/acct-1/ctx-mini-a");
    assert_ne!(a["cwd"], b["cwd"]);
    // Idempotent while open.
    assert_eq!(open("mini-a").unwrap()["created"], false);

    let key_a = SessionKey(a["session_id"].as_str().unwrap().to_owned());
    let key_b = SessionKey(b["session_id"].as_str().unwrap().to_owned());
    let rt_a = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, key_a.clone(), None)
        .await
        .unwrap();
    let rt_b = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, key_b, None)
        .await
        .unwrap();
    assert_eq!(
        dunce::canonicalize(&rt_a.workspace_root).unwrap(),
        PathBuf::from(a["cwd"].as_str().unwrap())
    );
    // Simultaneous mini-app requests: separate transcripts and memory.
    rt_a.memory
        .memory_store
        .write_long_term("- MINI-A-ONLY\n")
        .await
        .unwrap();
    assert!(segment(&rt_a).await.contains("MINI-A-ONLY"));
    assert!(!segment(&rt_b).await.contains("MINI-A-ONLY"));
    assert!(!segment(&rt_b).await.contains("SYSTEM-PRIVATE-FACT"));
    assert_ne!(rt_a.session_key, rt_b.session_key);
    // A request context is not an agent: it cannot hand off peers.
    assert!(!peer_handoff_allowed_for_session(&key_a));
    // It runs on its owning peer's model lane.
    assert!(
        peer_lane_provider_for(&key_a, &rt_a).is_none(),
        "primary until a lane is set"
    );
    raw_peer_model_set(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_MODEL_SET,
            json!({ "session_id": fx.system, "host_token": tok(&fx, "Rinx"), "peer": "Rinx", "model": "strong" }),
        ),
        None,
    )
    .unwrap();
    assert_eq!(
        peer_lane_provider_for(&key_a, &rt_a)
            .map(|p| p.model_id().to_owned())
            .as_deref(),
        Some("gpt-4o")
    );

    // A context workspace must stay inside the peer's.
    let escape = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({ "session_id": fx.system, "host_token": tok(&fx, "Rinx"), "peer": "Rinx", "context_id": "mini-c", "cwd": fx.apps.join("notes").to_string_lossy() }),
        ),
        None,
    )
    .unwrap_err();
    assert_eq!(
        escape.data.as_ref().unwrap()["kind"],
        "peer_context_workspace_escape"
    );

    // Only the owner may open or close contexts.
    let foreign = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({ "session_id": SessionKey::with_profile_topic("dev", "api", "host", "other"), "peer": "Rinx", "context_id": "mini-d" }),
        ),
        None,
    )
    .unwrap_err();
    assert_eq!(
        foreign.data.as_ref().unwrap()["kind"],
        "peer_originator_mismatch"
    );

    // Close: the session never runs again, and the id is never reopened.
    let closed = raw_peer_context_close(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_CLOSE,
            json!({ "session_id": fx.system, "host_token": tok(&fx, "Rinx"), "peer": "Rinx", "context_id": "mini-a" }),
        ),
        None,
    )
    .await
    .unwrap();
    assert_eq!(closed["closed"], true);
    assert_eq!(closed["was_open"], true);
    assert!(matches!(
        crate::peers::app_binding::resolve_session_app_binding(&fx.data_dir.join("peers"), &key_a),
        crate::peers::app_binding::SessionAppBinding::Refused(_)
    ));
    assert!(
        crate::runtime::SessionRuntime::bootstrap(&fx.runtime, key_a, None)
            .await
            .is_err(),
        "a closed context cannot be revived"
    );
    assert_eq!(
        open("mini-a").unwrap_err().data.unwrap()["kind"],
        "peer_context_closed"
    );

    // A never-opened context topic is refused outright (fail closed).
    let forged = SessionKey(format!("{}#peerctx-rinx.forged", fx.system.base_key()));
    assert!(
        crate::runtime::SessionRuntime::bootstrap(&fx.runtime, forged, None)
            .await
            .is_err()
    );
}

#[test]
fn should_advertise_and_dispatch_the_host_peer_methods() {
    for method in [
        APPUI_METHOD_PEER_MODEL_SET,
        APPUI_METHOD_PEER_CONTEXT_OPEN,
        APPUI_METHOD_PEER_CONTEXT_CLOSE,
    ] {
        assert!(APPUI_EXTRA_METHODS.contains(&method), "{method} advertised");
        assert!(
            raw_method_is_dispatched(method, false),
            "{method} dispatched"
        );
    }
}

#[tokio::test]
async fn should_provision_a_kernel_workspace_when_the_host_names_none() {
    // A remote host's local paths mean nothing to the kernel: without `cwd`
    // the kernel provisions the app workspace from the namespace.
    let fx = fixture().await;
    let staged = raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": "b", "names": ["Remote Rinx"], "session_id": fx.system,
                "memory_namespace": "app/rinx/acct-9", "resume": true,
            }),
        ),
        None,
    )
    .await
    .expect("provisioned");
    remember(&fx, "Remote Rinx", &staged);
    let cwd = PathBuf::from(staged["cwd"].as_str().unwrap());
    assert_eq!(
        cwd,
        dunce::canonicalize(fx.data_dir.join("app-workspaces/app/rinx/acct-9")).unwrap()
    );
    let resumed = raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": "b", "names": ["Remote Rinx"], "session_id": fx.system,
                "memory_namespace": "app/rinx/acct-9", "resume": true,
                "host_token": tok(&fx, "Remote Rinx"),
            }),
        ),
        None,
    )
    .await
    .expect("resumed");
    assert_eq!(resumed["resumed"], true);
    assert_eq!(resumed["cwd"], staged["cwd"]);
}

#[tokio::test]
async fn should_require_the_host_token_for_every_control_call() {
    let fx = fixture().await;
    let staged = prepare_app(&fx, "Rinx", "rinx", "app/rinx/acct-1", true)
        .await
        .unwrap();
    let token = staged["host_token"]
        .as_str()
        .expect("minted once")
        .to_owned();
    assert_eq!(token.len(), 64);
    let peer_dir = fx.data_dir.join("peers/rinx");
    let stored = std::fs::read_to_string(peer_dir.join("host_binding.json")).unwrap();
    assert!(!stored.contains(&token), "only the digest is stored");
    // The originator session alone is not enough.
    for params in [
        json!({ "brief": "b", "names": ["Rinx"], "session_id": fx.system,
                "cwd": fx.apps.join("rinx").to_string_lossy(), "memory_namespace": "app/rinx/acct-1", "resume": true }),
        json!({ "brief": "b", "names": ["Rinx"], "session_id": fx.system, "host_token": "guess",
                "cwd": fx.apps.join("rinx").to_string_lossy(), "memory_namespace": "app/rinx/acct-1", "resume": true }),
    ] {
        let err = raw_peer_prepare(&fx.state, &rpc(APPUI_METHOD_PEER_PREPARE, params), None)
            .await
            .unwrap_err();
        assert_eq!(
            err.data.as_ref().unwrap()["kind"],
            "peer_host_token_mismatch"
        );
    }
    let err = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({ "session_id": fx.system, "peer": "Rinx", "context_id": "a" }),
        ),
        None,
    )
    .unwrap_err();
    assert_eq!(
        err.data.as_ref().unwrap()["kind"],
        "peer_host_token_mismatch"
    );
    let err = raw_peer_model_set(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_MODEL_SET,
            json!({ "session_id": fx.system, "peer": "Rinx", "model": "strong" }),
        ),
        None,
    )
    .unwrap_err();
    assert_eq!(
        err.data.as_ref().unwrap()["kind"],
        "peer_host_token_mismatch"
    );
    // Resume never re-issues the token.
    let resumed = prepare_app(&fx, "Rinx", "rinx", "app/rinx/acct-1", true)
        .await
        .unwrap();
    assert!(resumed["host_token"].is_null());
}

#[tokio::test]
async fn should_refuse_a_binding_that_shares_state_with_another_app_peer() {
    let fx = fixture().await;
    prepare_app(&fx, "Rinx", "rinx", "app/rinx/acct-1", true)
        .await
        .unwrap();
    std::fs::create_dir_all(fx.apps.join("rinx/nested")).unwrap();
    for (name, cwd, ns) in [
        ("Twin", fx.apps.join("notes"), "app/rinx/acct-1"),
        (
            "Inside",
            fx.apps.join("notes"),
            "app/rinx/acct-1/ctx-mini-a",
        ),
        ("Parent", fx.apps.join("notes"), "app/rinx"),
        ("Nested", fx.apps.join("rinx/nested"), "app/other/acct-1"),
        ("Around", fx.apps.clone(), "app/other/acct-2"),
    ] {
        let err = raw_peer_prepare(
            &fx.state,
            &rpc(
                APPUI_METHOD_PEER_PREPARE,
                json!({ "brief": "b", "names": [name], "session_id": fx.system,
                        "cwd": cwd.to_string_lossy(), "memory_namespace": ns }),
            ),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.data.as_ref().unwrap()["kind"],
            "peer_binding_conflict",
            "{name}"
        );
    }
    let stores = fx.data_dir.join("memory-namespaces/app");
    std::fs::create_dir_all(&stores).unwrap();
    let err = raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({ "brief": "b", "names": ["Stores"], "session_id": fx.system,
                    "cwd": stores.to_string_lossy(), "memory_namespace": "app/stores" }),
        ),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.data.as_ref().unwrap()["kind"], "peer_binding_conflict");
    // A disjoint app is fine.
    prepare_app(&fx, "Notes", "notes", "app/notes/acct-1", true)
        .await
        .unwrap();
}
