use super::*;
use serde_json::json;

#[test]
fn compare_protocol_compatible_for_full_protocol_with_known_features() {
    // The full-protocol capabilities advertise every known feature, so any
    // subset (here: the whole known registry) is satisfied.
    let server = UiProtocolCapabilities::full_protocol();
    let required: Vec<&str> = vec![
        UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1,
        UI_PROTOCOL_FEATURE_USER_QUESTION_V1,
    ];
    assert_eq!(
        compare_protocol(&server, required),
        ProtocolCompat::Compatible
    );
}

#[test]
fn compare_protocol_empty_required_is_always_compatible() {
    let server = UiProtocolCapabilities::full_protocol();
    let required: Vec<&str> = Vec::new();
    assert_eq!(
        compare_protocol(&server, required),
        ProtocolCompat::Compatible
    );
    assert!(compare_protocol(&server, Vec::<&str>::new()).is_compatible());
}

#[test]
fn compare_protocol_reports_missing_features_in_request_order() {
    let mut server = UiProtocolCapabilities::full_protocol();
    server
        .supported_features
        .retain(|f| f != UI_PROTOCOL_FEATURE_USER_QUESTION_V1);
    let required = vec![
        UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1, // present
        UI_PROTOCOL_FEATURE_USER_QUESTION_V1,  // removed → missing
    ];
    assert_eq!(
        compare_protocol(&server, required),
        ProtocolCompat::MissingFeatures(vec![UI_PROTOCOL_FEATURE_USER_QUESTION_V1.to_owned()])
    );
}

#[test]
fn compare_protocol_schema_incompatible_when_server_older() {
    if UI_PROTOCOL_SCHEMA_VERSION == 0 {
        return; // can't model an older schema below zero
    }
    let mut server = UiProtocolCapabilities::full_protocol();
    server.version.schema_version = UI_PROTOCOL_SCHEMA_VERSION - 1;
    assert_eq!(
        compare_protocol(&server, [UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1]),
        ProtocolCompat::SchemaIncompatible {
            server: UI_PROTOCOL_SCHEMA_VERSION - 1,
            client: UI_PROTOCOL_SCHEMA_VERSION,
        }
    );
}

#[test]
fn compare_protocol_allows_newer_server_schema() {
    // A server ahead of the client (additive forward-compat) is fine.
    let mut server = UiProtocolCapabilities::full_protocol();
    server.version.schema_version = UI_PROTOCOL_SCHEMA_VERSION + 5;
    assert_eq!(
        compare_protocol(&server, [UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1]),
        ProtocolCompat::Compatible
    );
}

#[test]
fn compare_protocol_schema_incompatible_on_wrong_protocol_family() {
    let mut server = UiProtocolCapabilities::full_protocol();
    server.version.protocol = "octos-ui/v2alpha".into();
    // Even with a same/newer schema number, a different family can't bridge.
    match compare_protocol(&server, [UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1]) {
        ProtocolCompat::SchemaIncompatible { client, .. } => {
            assert_eq!(client, UI_PROTOCOL_SCHEMA_VERSION);
        }
        other => panic!("expected SchemaIncompatible, got {other:?}"),
    }
}

#[test]
fn reasoning_effort_level_wire_shape() {
    // Snake-case wire strings, incl. the DeepSeek "max" tier.
    assert_eq!(
        serde_json::to_value(ReasoningEffortLevel::Max).unwrap(),
        json!("max")
    );
    assert_eq!(
        serde_json::to_value(ReasoningEffortLevel::High).unwrap(),
        json!("high")
    );
    assert_eq!(
        serde_json::from_value::<ReasoningEffortLevel>(json!("low")).unwrap(),
        ReasoningEffortLevel::Low
    );
    // Absent on turn/start is the common case; present round-trips.
    let params = TurnStartParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: TurnId::new(),
        input: vec![],
        media: vec![],
        topic: None,
        rewrite_for: None,
        reasoning_effort: Some(ReasoningEffortLevel::Max),
        live_video: false,
    };
    let wire = serde_json::to_value(&params).unwrap();
    assert_eq!(wire["reasoning_effort"], json!("max"));
    let back: TurnStartParams = serde_json::from_value(wire).unwrap();
    assert_eq!(back.reasoning_effort, Some(ReasoningEffortLevel::Max));
    // Omitted optional fields deserialize to their defaults (backward
    // compatible): no reasoning_effort, and `live_video` false.
    let legacy = json!({
        "session_id": "local:demo",
        "turn_id": "00000000-0000-0000-0000-000000000001",
        "input": []
    });
    let parsed: TurnStartParams = serde_json::from_value(legacy).unwrap();
    assert_eq!(parsed.reasoning_effort, None);
    assert!(!parsed.live_video);
}

#[test]
fn turn_start_live_video_roundtrips_and_is_omitted_when_false() {
    // Explicit true is carried on the wire and read back.
    let on = json!({
        "session_id": "local:demo",
        "turn_id": "00000000-0000-0000-0000-000000000001",
        "input": [],
        "live_video": true
    });
    let parsed: TurnStartParams = serde_json::from_value(on).unwrap();
    assert!(parsed.live_video);
    // Default false is omitted from the serialized form (no wire bloat).
    let params = TurnStartParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: TurnId(Uuid::from_u128(1)),
        input: vec![],
        media: vec![],
        topic: None,
        rewrite_for: None,
        reasoning_effort: None,
        live_video: false,
    };
    let wire = serde_json::to_value(&params).unwrap();
    assert!(wire.get("live_video").is_none());
}

#[test]
fn should_surface_persisted_reasoning_effort_on_session_open() {
    // The persisted per-session effort is surfaced back to a
    // reconnecting/restarting TUI via `SessionOpened.reasoning_effort` so
    // the client can restore its local `/thinking` state and mark its menu.
    let opened = SessionOpened {
        session_id: SessionKey("local:demo".into()),
        active_profile_id: None,
        workspace_root: None,
        context: None,
        context_state: None,
        cursor: None,
        panes: None,
        capabilities: UiProtocolCapabilities::first_server_slice(),
        reasoning_effort: Some(ReasoningEffortLevel::High),
    };
    let wire = serde_json::to_value(&opened).expect("serialize SessionOpened");
    assert_eq!(wire["reasoning_effort"], json!("high"));
    let back: SessionOpened = serde_json::from_value(wire).expect("round-trip");
    assert_eq!(back.reasoning_effort, Some(ReasoningEffortLevel::High));

    // Omitted on the wire when None, and older payloads (no field) decode
    // to None — additive + backward-compatible.
    let none_opened = SessionOpened {
        reasoning_effort: None,
        ..opened.clone()
    };
    let none_wire = serde_json::to_value(&none_opened).expect("serialize None");
    assert!(
        none_wire.get("reasoning_effort").is_none(),
        "reasoning_effort must be omitted from the wire when None"
    );
    let legacy = json!({
        "session_id": "local:demo",
        "capabilities": serde_json::to_value(
            UiProtocolCapabilities::first_server_slice()
        )
        .unwrap()
    });
    let parsed: SessionOpened =
        serde_json::from_value(legacy).expect("legacy payload without field decodes");
    assert_eq!(parsed.reasoning_effort, None);
}

#[test]
fn ui_command_method_matches_expected_transport_name() {
    let cmd = UiCommand::TurnInterrupt(TurnInterruptParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: TurnId::new(),
    });

    assert_eq!(cmd.method(), methods::TURN_INTERRUPT);
}

#[test]
fn protocol_version_and_first_server_capabilities_round_trip() {
    let capabilities = UiProtocolCapabilities::first_server_slice();

    assert!(capabilities.version.is_supported_by_current_runtime());
    assert_eq!(
        capabilities.capabilities_schema_version,
        UI_PROTOCOL_CAPABILITIES_SCHEMA_VERSION
    );
    assert!(capabilities.supports_method(methods::SESSION_OPEN));
    assert!(capabilities.supports_method(methods::TURN_START));
    assert!(capabilities.supports_method(methods::TURN_INTERRUPT));
    assert!(capabilities.supports_method(methods::APPROVAL_RESPOND));
    assert!(capabilities.supports_method(methods::DIFF_PREVIEW_GET));
    assert!(capabilities.supports_method(methods::TASK_OUTPUT_READ));
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1));
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1));
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1));
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_SESSION_SANDBOX_V1));
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1));
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1));
    assert!(capabilities.supports_method(methods::TASK_LIST));
    assert!(capabilities.supports_method(methods::TASK_CANCEL));
    assert!(capabilities.supports_method(methods::TASK_RESTART_FROM_NODE));
    assert!(capabilities.supports_method(methods::TASK_ARTIFACT_LIST));
    assert!(capabilities.supports_method(methods::TASK_ARTIFACT_READ));
    assert!(capabilities.unsupported.is_empty());

    let json = serde_json::to_string(&capabilities).expect("serialize capabilities");
    let decoded: UiProtocolCapabilities =
        serde_json::from_str(&json).expect("deserialize capabilities");

    assert_eq!(decoded, capabilities);
    assert!(
        decoded
            .supported_notifications
            .contains(&methods::SESSION_OPEN.to_owned())
    );
    // #1477: the typed visual lifecycle events are advertised as supported
    // notifications, so a negotiating client knows to expect them.
    assert!(
        decoded
            .supported_notifications
            .contains(&methods::VISUAL_GENERATING.to_owned())
    );
    assert!(
        decoded
            .supported_notifications
            .contains(&methods::VISUAL_SUCCEEDED.to_owned())
    );
    assert!(
        decoded
            .supported_notifications
            .contains(&methods::VISUAL_FAILED.to_owned())
    );
}

#[test]
fn capabilities_accept_absent_supported_features() {
    let legacy = json!({
        "version": UiProtocolVersion::current(),
        "capabilities_schema_version": 1,
        "supported_methods": [methods::SESSION_OPEN],
        "supported_notifications": [methods::SESSION_OPEN]
    });

    let decoded: UiProtocolCapabilities =
        serde_json::from_value(legacy).expect("legacy capabilities decode");

    assert!(decoded.supported_features.is_empty());
    assert!(!decoded.supports_feature(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1));
    assert!(!decoded.supports_feature(UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1));
    assert!(!decoded.supports_feature(UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1));
    assert!(!decoded.supports_feature(UI_PROTOCOL_FEATURE_SESSION_SANDBOX_V1));
    assert!(!decoded.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1));
    assert!(!decoded.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1));
}

#[test]
fn full_protocol_capabilities_advertise_harness_task_control() {
    let capabilities = UiProtocolCapabilities::full_protocol();

    assert!(capabilities.supports_method(methods::TASK_LIST));
    assert!(capabilities.supports_method(methods::TASK_CANCEL));
    assert!(capabilities.supports_method(methods::TASK_RESTART_FROM_NODE));
    assert!(capabilities.supports_method(methods::TASK_OUTPUT_READ));
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1));
    assert!(capabilities.supports_method(methods::TASK_ARTIFACT_LIST));
    assert!(capabilities.supports_method(methods::TASK_ARTIFACT_READ));
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1));
    assert!(capabilities.unsupported.is_empty());
}

#[test]
fn session_open_params_topic_cwd_and_sandbox_are_additive_and_round_trip() {
    let params = SessionOpenParams {
        session_id: SessionKey("local:demo".into()),
        topic: Some("research".into()),
        profile_id: Some("coding".into()),
        cwd: Some("/repo".into()),
        sandbox: Some(SessionSandboxParams {
            enabled: Some(true),
            network_access: Some(false),
            read_allow_paths: vec!["/repo/docs".into()],
        }),
        after: None,
    };

    let wire = serde_json::to_value(&params).expect("serialize session/open params");
    assert_eq!(wire["topic"], json!("research"));
    assert_eq!(wire["cwd"], json!("/repo"));
    assert_eq!(wire["sandbox"]["enabled"], json!(true));
    assert_eq!(wire["sandbox"]["network_access"], json!(false));
    assert_eq!(wire["sandbox"]["read_allow_paths"], json!(["/repo/docs"]));

    let decoded: SessionOpenParams =
        serde_json::from_value(wire).expect("deserialize session/open params");
    assert_eq!(decoded, params);

    let legacy = json!({
        "session_id": "local:demo",
        "profile_id": "coding"
    });
    let decoded_legacy: SessionOpenParams =
        serde_json::from_value(legacy).expect("legacy session/open params");
    assert!(decoded_legacy.topic.is_none());
    assert!(decoded_legacy.cwd.is_none());
    assert!(decoded_legacy.sandbox.is_none());
}

#[test]
fn profile_local_create_params_new_shape_requested_id_round_trips() {
    // New shape: a meaningful requested_id, NO username/email/name.
    let params = ProfileLocalCreateParams {
        requested_id: Some("glm".into()),
        name: String::new(),
        username: String::new(),
        email: String::new(),
        make_default: None,
    };
    let wire = serde_json::to_value(&params).expect("serialize profile/local/create params");
    assert_eq!(wire["requested_id"], json!("glm"));
    let decoded: ProfileLocalCreateParams =
        serde_json::from_value(wire).expect("round-trip decode");
    assert_eq!(decoded, params);

    // Raw new-shape JSON that omits username/email/name entirely still
    // deserializes (the newly-optional fields default to empty).
    let new_shape = json!({ "requested_id": "deepseek" });
    let decoded_new: ProfileLocalCreateParams =
        serde_json::from_value(new_shape).expect("new-shape decode without username/email");
    assert_eq!(decoded_new.requested_id.as_deref(), Some("deepseek"));
    assert!(decoded_new.name.is_empty());
    assert!(decoded_new.username.is_empty());
    assert!(decoded_new.email.is_empty());
}

#[test]
fn profile_local_create_params_legacy_shape_still_deserializes() {
    // Old client shape: {name, username, email}, NO requested_id.
    let legacy = json!({
        "name": "Ada Lovelace",
        "username": "ada",
        "email": "ada@example.com"
    });
    let decoded: ProfileLocalCreateParams =
        serde_json::from_value(legacy).expect("legacy profile/local/create params decode");
    assert!(decoded.requested_id.is_none());
    assert_eq!(decoded.name, "Ada Lovelace");
    assert_eq!(decoded.username, "ada");
    assert_eq!(decoded.email, "ada@example.com");

    // A `None` requested_id serializes to exactly the legacy wire shape
    // (the key is skipped), so an OLDER server sees the bytes unchanged.
    let wire = serde_json::to_value(&decoded).expect("serialize legacy-shaped params");
    assert!(wire.get("requested_id").is_none());
    assert_eq!(wire["name"], json!("Ada Lovelace"));
    assert_eq!(wire["username"], json!("ada"));
    assert_eq!(wire["email"], json!("ada@example.com"));
}

#[test]
fn session_opened_pane_snapshot_round_trips() {
    let session_id = SessionKey("local:demo".into());
    let opened = SessionOpened {
        session_id: session_id.clone(),
        active_profile_id: Some("coding".into()),
        workspace_root: Some("/repo".into()),
        context: None,
        context_state: None,
        cursor: None,
        panes: Some(UiPaneSnapshot {
            session_id: session_id.clone(),
            generated_at: None,
            workspace: Some(UiWorkspacePaneSnapshot {
                root: "/repo".into(),
                readable_roots: vec!["/repo".into()],
                writable_roots: vec!["/repo".into()],
                contract: vec!["feature pane.snapshots.v1".into()],
                entries: vec![UiWorkspacePaneEntry {
                    path: "src/lib.rs".into(),
                    label: "lib.rs".into(),
                    depth: 1,
                    kind: "file".into(),
                    detail: Some("12 KB".into()),
                }],
                limitations: Vec::new(),
            }),
            artifacts: Some(UiArtifactPaneSnapshot {
                items: vec![UiArtifactPaneItem {
                    title: "lib.rs".into(),
                    kind: "file".into(),
                    path: Some("src/lib.rs".into()),
                    uri: None,
                    source: Some("workspace".into()),
                    status: "12 KB".into(),
                    source_task_id: None,
                    preview_id: None,
                    size_bytes: Some(12_288),
                    updated_at: None,
                }],
                limitations: Vec::new(),
            }),
            git: Some(UiGitPaneSnapshot {
                repo_root: Some("/repo".into()),
                branch: Some("coding-green".into()),
                head: Some("abc1234".into()),
                clean: false,
                status: vec![UiGitStatusItem {
                    code: "M".into(),
                    path: "src/lib.rs".into(),
                    detail: "modified".into(),
                }],
                history: vec![UiGitHistoryItem {
                    commit: "abc1234".into(),
                    summary: "pane snapshots".into(),
                }],
                limitations: Vec::new(),
            }),
            limitations: Vec::new(),
        }),
        capabilities: UiProtocolCapabilities::first_server_slice(),
        reasoning_effort: None,
    };

    let wire = serde_json::to_value(&opened).expect("serialize session/open panes");
    assert_eq!(wire["workspace_root"], json!("/repo"));
    assert_eq!(wire["panes"]["workspace"]["root"], json!("/repo"));
    assert_eq!(
        wire["panes"]["artifacts"]["items"][0]["title"],
        json!("lib.rs")
    );
    assert_eq!(wire["panes"]["git"]["branch"], json!("coding-green"));

    let decoded: SessionOpened =
        serde_json::from_value(wire).expect("deserialize session/open panes");
    assert_eq!(decoded, opened);
}

// ----- UPCR-2026-007: capability advertisement on `SessionOpened` -----

#[test]
fn session_open_result_includes_capabilities_field() {
    // Golden: `SessionOpened` serializes a `capabilities` payload that
    // covers protocol version, method/notification surface, and the
    // negotiated feature set so clients can discover the contract
    // in-band per spec § 4 / UPCR-2026-007.
    let opened = SessionOpened {
        session_id: SessionKey("local:demo".into()),
        active_profile_id: None,
        workspace_root: None,
        context: None,
        context_state: None,
        cursor: None,
        panes: None,
        capabilities: UiProtocolCapabilities::first_server_slice(),
        reasoning_effort: None,
    };
    let wire = serde_json::to_value(&opened).expect("serialize SessionOpened");
    let capabilities = wire
        .get("capabilities")
        .expect("SessionOpened must serialize a capabilities field");
    assert_eq!(capabilities["version"]["protocol"], json!(UI_PROTOCOL_V1));
    assert_eq!(
        capabilities["capabilities_schema_version"],
        json!(UI_PROTOCOL_CAPABILITIES_SCHEMA_VERSION)
    );
    assert!(
        capabilities["supported_methods"]
            .as_array()
            .expect("supported_methods array")
            .iter()
            .any(|method| method == &json!(methods::SESSION_OPEN))
    );
    let supported_features = capabilities["supported_features"]
        .as_array()
        .expect("supported_features array");
    for feature in UI_PROTOCOL_KNOWN_FEATURES {
        if *feature == UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2 {
            continue;
        }
        assert!(
            supported_features
                .iter()
                .any(|advertised| advertised == &json!(*feature)),
            "first_server_slice must advertise {feature}"
        );
    }
    assert!(
        !supported_features
            .iter()
            .any(|advertised| advertised == &json!(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2)),
        "strictly opt-in v2 must not change the no-header SessionOpened wire"
    );

    // Older payloads (e.g. ledger replays from before the field
    // existed) decode successfully because the field carries
    // `serde(default = "first_server_slice")`.
    let legacy = json!({
        "session_id": "local:demo",
    });
    let decoded: SessionOpened =
        serde_json::from_value(legacy).expect("legacy SessionOpened decode");
    assert_eq!(
        decoded.capabilities,
        UiProtocolCapabilities::first_server_slice()
    );
}

#[test]
fn negotiated_capabilities_advertise_full_protocol_when_no_features_requested() {
    // No header => `for_negotiated_features([])` returns the
    // first-slice baseline with an empty `supported_features` so the
    // server does not silently advertise flags the client did not ask
    // for. The no-header fallback handled by callers is the
    // `first_server_slice` default; this test pins the empty-request
    // intersection contract.
    let none: [&str; 0] = [];
    let capabilities = UiProtocolCapabilities::for_negotiated_features(none);
    assert!(capabilities.supported_features.is_empty());
    assert!(capabilities.supports_method(methods::SESSION_OPEN));
    assert!(capabilities.supports_method(methods::TURN_START));
    // Capability-gated methods MUST NOT leak when their gating feature
    // is not in the negotiated set — otherwise a client would call
    // them and receive `method_not_supported`.
    assert!(!capabilities.supports_method(methods::TASK_LIST));
    assert!(!capabilities.supports_method(methods::TASK_CANCEL));
    assert!(!capabilities.supports_method(methods::TASK_RESTART_FROM_NODE));
    assert!(!capabilities.supports_method(methods::TASK_ARTIFACT_LIST));
    assert!(!capabilities.supports_method(methods::TASK_ARTIFACT_READ));
}

#[test]
fn negotiated_capabilities_intersect_requested_with_known_features() {
    // Client asked only for pane snapshots — the server returns just
    // that feature, never leaking the typed-approval / cwd / task-
    // control flags the client did not negotiate.
    let capabilities = UiProtocolCapabilities::for_negotiated_features([
        UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1,
        "made.up.feature.v9",
    ]);
    assert_eq!(
        capabilities.supported_features,
        vec![UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1.to_owned()]
    );
    assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1));
    assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1));
    assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1));
    assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1));
    // Task-control methods are gated by harness.task_control.v1 — they
    // must not appear in the advertised method set when the gating
    // feature is not negotiated.
    assert!(!capabilities.supports_method(methods::TASK_LIST));
    assert!(!capabilities.supports_method(methods::TASK_CANCEL));
    assert!(!capabilities.supports_method(methods::TASK_RESTART_FROM_NODE));
    assert!(!capabilities.supports_method(methods::TASK_ARTIFACT_LIST));
    assert!(!capabilities.supports_method(methods::TASK_ARTIFACT_READ));
    // Unconditional methods stay present.
    assert!(capabilities.supports_method(methods::SESSION_OPEN));
    assert!(capabilities.supports_method(methods::TURN_START));
    assert!(capabilities.supports_method(methods::TASK_OUTPUT_READ));
}

#[test]
fn negotiated_capabilities_advertise_task_control_methods_when_feature_requested() {
    // Pre-condition for the gating change: when the client *did*
    // request `harness.task_control.v1`, the server's negotiated
    // method set includes the task-control RPCs so the spec § 7
    // "expose only when feature flag is advertised" rule is honoured
    // bidirectionally.
    let capabilities = UiProtocolCapabilities::for_negotiated_features([
        UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1,
    ]);
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1));
    assert!(capabilities.supports_method(methods::TASK_LIST));
    assert!(capabilities.supports_method(methods::TASK_CANCEL));
    assert!(capabilities.supports_method(methods::TASK_RESTART_FROM_NODE));
}

#[test]
fn negotiated_capabilities_hide_task_and_agent_artifact_methods_without_feature() {
    // #965/#1084 — task artifact methods have their own harness feature
    // gate, while legacy agent artifact aliases stay under agent control.
    let capabilities = UiProtocolCapabilities::for_negotiated_features(Vec::<String>::new());
    assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1));
    assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1));
    assert!(!capabilities.supports_method(methods::TASK_ARTIFACT_LIST));
    assert!(!capabilities.supports_method(methods::TASK_ARTIFACT_READ));
    assert!(!capabilities.supports_method(methods::AGENT_ARTIFACT_LIST));
    assert!(!capabilities.supports_method(methods::AGENT_ARTIFACT_READ));
}

#[test]
fn negotiated_capabilities_advertise_task_artifact_methods_when_feature_requested() {
    let capabilities = UiProtocolCapabilities::for_negotiated_features([
        UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1,
    ]);
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1));
    assert!(capabilities.supports_method(methods::TASK_ARTIFACT_LIST));
    assert!(capabilities.supports_method(methods::TASK_ARTIFACT_READ));
    assert!(!capabilities.supports_method(methods::AGENT_ARTIFACT_LIST));
    assert!(!capabilities.supports_method(methods::AGENT_ARTIFACT_READ));
}

#[test]
fn negotiated_capabilities_advertise_agent_artifact_methods_when_agent_control_requested() {
    let capabilities = UiProtocolCapabilities::for_negotiated_features([
        UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1,
        UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1,
    ]);
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1));
    assert!(capabilities.supports_method(methods::AGENT_ARTIFACT_LIST));
    assert!(capabilities.supports_method(methods::AGENT_ARTIFACT_READ));
    assert!(!capabilities.supports_method(methods::TASK_ARTIFACT_LIST));
    assert!(!capabilities.supports_method(methods::TASK_ARTIFACT_READ));
}

#[test]
fn ui_protocol_v1_wire_contract_is_golden() {
    assert_eq!(UI_PROTOCOL_V1, "octos-ui/v1alpha1");
    assert_eq!(UI_PROTOCOL_SCHEMA_VERSION, 1);
    assert_eq!(UI_PROTOCOL_CAPABILITIES_SCHEMA_VERSION, 2);
    assert_eq!(JSON_RPC_VERSION, "2.0");
    assert_eq!(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1, "approval.typed.v1");
    assert_eq!(UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1, "pane.snapshots.v1");
    assert_eq!(
        UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
        "session.workspace_cwd.v1"
    );
    assert_eq!(
        UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1,
        "harness.task_control.v1"
    );
    assert_eq!(
        UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1,
        "harness.task_artifacts.v1"
    );
    assert_eq!(
        UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1,
        "state.session_hydrate.v1"
    );
    assert_eq!(UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1, "state.thread_graph.v1");
    assert_eq!(
        UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1,
        "state.turn_state_get.v1"
    );
    assert_eq!(
        UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1,
        "projection.envelope.v1"
    );
    assert_eq!(
        UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2,
        "projection.envelope.v2"
    );
    assert_eq!(
        UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1,
        "auxiliary.rest_to_ws.v1"
    );
    assert_eq!(UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1, "coding.autonomy.v1");
    assert_eq!(
        UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1,
        "coding.agent_control.v1"
    );
    assert_eq!(
        UI_PROTOCOL_FEATURE_CODING_GOAL_RUNTIME_V1,
        "coding.goal_runtime.v1"
    );
    assert_eq!(
        UI_PROTOCOL_FEATURE_CODING_LOOP_RUNTIME_V1,
        "coding.loop_runtime.v1"
    );
    assert_eq!(UI_PROTOCOL_FEATURE_REVIEW_START_V1, "review.start.v1");
    assert_eq!(
        UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1,
        "context.lifecycle.v1"
    );
    assert_eq!(
        UI_PROTOCOL_FEATURE_HARNESS_TASK_SUPERVISION_INSPECTION_V1,
        "harness.task_supervision_inspection.v1"
    );
    assert_eq!(
        UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1,
        "harness.task_artifacts.v1"
    );

    assert_eq!(
        UI_PROTOCOL_COMMAND_METHODS,
        &[
            "profile/local/create",
            "session/open",
            "turn/start",
            "turn/interrupt",
            "approval/respond",
            "approval/scopes/list",
            "session/btw",
            "user_question/respond",
            "permission/profile/list",
            "permission/profile/set",
            "diff/preview/get",
            "task/list",
            "task/cancel",
            "task/restart_from_node",
            "task/output/read",
            "session/hydrate",
            "session/rollback",
            "session/fork",
            "thread/graph/get",
            "turn/state/get",
            "agent/list",
            "agent/status/read",
            "agent/output/read",
            "agent/artifact/list",
            "agent/artifact/read",
            "task/artifact/list",
            "task/artifact/read",
            "agent/interrupt",
            "agent/close",
            "session/goal/get",
            "session/goal/set",
            "session/goal/clear",
            "loop/create",
            "loop/list",
            "loop/delete",
            "loop/pause",
            "loop/resume",
            "loop/fire_now",
            "review/start",
            "session/list",
            "session/snapshot",
            "session/messages_page",
            "session/status.get",
            "session/files.list",
            "session/tasks.list",
            "session/workspace.get",
            "session/title.set",
            "session/delete",
            "system/status.get",
            "content/list",
            "content/delete",
            "content/bulk_delete",
            "memory/overview",
            "memory/entity",
            "cron/list",
            "cron/toggle",
            "router/set_mode",
            "router/get_metrics",
            "launch/resolve",
        ]
    );
    assert_eq!(
        UI_PROTOCOL_NOTIFICATION_METHODS,
        &[
            "session/open",
            "turn/started",
            "turn/completed",
            "turn/error",
            "message/delta",
            "message/reasoning_delta",
            "tool/started",
            "tool/progress",
            "tool/completed",
            "approval/requested",
            "approval/auto_resolved",
            "approval/decided",
            "approval/cancelled",
            "user_question/requested",
            "task/updated",
            "plan/updated",
            "task/output/delta",
            "progress/updated",
            "warning",
            "protocol/replay_lossy",
            "turn/spawn_complete",
            "file/attached",
            "visual/generating",
            "visual/succeeded",
            "visual/failed",
            "voice/exit",
            "voice/audio_chunk",
            "projection/envelope",
            "session/event",
            "router/status",
            "router/failover",
            "queue/state",
            "agent/updated",
            "agent/output/delta",
            "agent/artifact/updated",
            "session/goal/updated",
            "session/goal/cleared",
            "loop/updated",
            "loop/fired",
            "loop/completed",
            "context/compaction_completed",
            "context/compaction_started",
            "context/normalization_reported",
        ]
    );
    assert_eq!(
        UI_PROTOCOL_FIRST_SERVER_METHODS,
        &[
            "session/open",
            "turn/start",
            "turn/interrupt",
            "approval/respond",
            "approval/scopes/list",
            "session/btw",
            "user_question/respond",
            "permission/profile/list",
            "permission/profile/set",
            "diff/preview/get",
            "task/list",
            "task/cancel",
            "task/restart_from_node",
            "task/output/read",
            "session/hydrate",
            "session/rollback",
            "session/fork",
            "thread/graph/get",
            "turn/state/get",
            "agent/list",
            "agent/status/read",
            "agent/output/read",
            "agent/artifact/list",
            "agent/artifact/read",
            "task/artifact/list",
            "task/artifact/read",
            "agent/interrupt",
            "agent/close",
            "session/goal/get",
            "session/goal/set",
            "session/goal/clear",
            "loop/create",
            "loop/list",
            "loop/delete",
            "loop/pause",
            "loop/resume",
            "loop/fire_now",
            "review/start",
            "session/list",
            "session/snapshot",
            "session/messages_page",
            "session/status.get",
            "session/files.list",
            "session/tasks.list",
            "session/workspace.get",
            "session/title.set",
            "session/delete",
            "system/status.get",
            "content/list",
            "content/delete",
            "content/bulk_delete",
            "memory/overview",
            "memory/entity",
            "cron/list",
            "cron/toggle",
            "router/set_mode",
            "router/get_metrics",
            "launch/resolve",
        ]
    );
    assert_eq!(UI_PROTOCOL_FIRST_SERVER_UNSUPPORTED_METHODS.len(), 0);
}

#[test]
fn ui_protocol_v1_representative_wire_payloads_are_golden() {
    let turn_id = TurnId(Uuid::from_u128(1));
    let approval_id = ApprovalId(Uuid::from_u128(2));
    let preview_id = PreviewId(Uuid::from_u128(3));
    let task_id = TaskId(Uuid::from_u128(4));

    assert_eq!(
        serde_json::to_value(UiProtocolCapabilities::first_server_slice())
            .expect("capabilities json"),
        json!({
            "version": {
                "protocol": "octos-ui/v1alpha1",
                "schema_version": 1,
                "jsonrpc": "2.0"
            },
            "capabilities_schema_version": 2,
            "supported_methods": [
                "session/open",
                "turn/start",
                "turn/interrupt",
                "approval/respond",
                "approval/scopes/list",
                "session/btw",
                "user_question/respond",
                "permission/profile/list",
                "permission/profile/set",
                "diff/preview/get",
                "task/list",
                "task/cancel",
                "task/restart_from_node",
                "task/output/read",
                "session/hydrate",
                "session/rollback",
                "session/fork",
                "thread/graph/get",
                "turn/state/get",
                "agent/list",
                "agent/status/read",
                "agent/output/read",
                "agent/artifact/list",
                "agent/artifact/read",
                "task/artifact/list",
                "task/artifact/read",
                "agent/interrupt",
                "agent/close",
                "session/goal/get",
                "session/goal/set",
                "session/goal/clear",
                "loop/create",
                "loop/list",
                "loop/delete",
                "loop/pause",
                "loop/resume",
                "loop/fire_now",
                "review/start",
                "session/list",
                "session/snapshot",
                "session/messages_page",
                "session/status.get",
                "session/files.list",
                "session/tasks.list",
                "session/workspace.get",
                "session/title.set",
                "session/delete",
                "system/status.get",
                "content/list",
                "content/delete",
                "content/bulk_delete",
                "memory/overview",
                "memory/entity",
                "cron/list",
                "cron/toggle",
                "router/set_mode",
                "router/get_metrics",
                "launch/resolve"
            ],
            "supported_notifications": [
                "session/open",
                "turn/started",
                "turn/completed",
                "turn/error",
                "message/delta",
                "message/reasoning_delta",
                "tool/started",
                "tool/progress",
                "tool/completed",
                "approval/requested",
                "approval/auto_resolved",
                "approval/decided",
                "approval/cancelled",
                "user_question/requested",
                "task/updated",
                "plan/updated",
                "task/output/delta",
                "progress/updated",
                "warning",
                "protocol/replay_lossy",
                "turn/spawn_complete",
                "file/attached",
                "visual/generating",
                "visual/succeeded",
                "visual/failed",
                "voice/exit",
                "voice/audio_chunk",
                "projection/envelope",
                "session/event",
                "router/status",
                "router/failover",
                "queue/state",
                "agent/updated",
                "agent/output/delta",
                "agent/artifact/updated",
                "session/goal/updated",
                "session/goal/cleared",
                "loop/updated",
                "loop/fired",
                "loop/completed",
                "context/compaction_completed",
                "context/compaction_started",
                "context/normalization_reported"
            ],
            "supported_features": [
                "approval.typed.v1",
                "pane.snapshots.v1",
                "session.workspace_cwd.v1",
                "session.sandbox.v1",
                "harness.task_control.v1",
                "state.session_hydrate.v1",
                "state.thread_graph.v1",
                "state.turn_state_get.v1",
                "event.spawn_complete.v1",
                "event.file_attached.v1",
                "projection.envelope.v1",
                "auxiliary.rest_to_ws.v1",
                "coding.autonomy.v1",
                "coding.agent_control.v1",
                "coding.goal_runtime.v1",
                "coding.loop_runtime.v1",
                "review.start.v1",
                "context.lifecycle.v1",
                "harness.task_supervision_inspection.v1",
                "harness.task_artifacts.v1",
                "user_question.v1",
                "event.voice_audio.v1",
                "plan.todos.v1"
            ]
        })
    );

    let turn_start = UiCommand::TurnStart(TurnStartParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: turn_id.clone(),
        input: vec![InputItem::Text {
            text: "hello".into(),
        }],
        media: Vec::new(),
        topic: None,
        rewrite_for: None,
        reasoning_effort: None,
        live_video: false,
    })
    .into_rpc_request("req-turn-start")
    .expect("serialize turn/start");
    assert_eq!(
        serde_json::to_value(turn_start).expect("turn/start json"),
        json!({
            "jsonrpc": "2.0",
            "id": "req-turn-start",
            "method": "turn/start",
            "params": {
                "session_id": "local:demo",
                "turn_id": turn_id,
                "input": [
                    {
                        "kind": "text",
                        "text": "hello"
                    }
                ]
            }
        })
    );

    let approval_response = UiCommand::ApprovalRespond(ApprovalRespondParams::new(
        SessionKey("local:demo".into()),
        approval_id.clone(),
        ApprovalDecision::Approve,
    ))
    .into_rpc_request("req-approval")
    .expect("serialize approval/respond");
    assert_eq!(
        serde_json::to_value(approval_response).expect("approval/respond json"),
        json!({
            "jsonrpc": "2.0",
            "id": "req-approval",
            "method": "approval/respond",
            "params": {
                "session_id": "local:demo",
                "approval_id": approval_id,
                "decision": "approve"
            }
        })
    );

    let diff_result = UiRpcResult::DiffPreviewGet(DiffPreviewGetResult {
        status: DiffPreviewGetStatus::Ready,
        source: DiffPreviewSource::PendingStore,
        preview: DiffPreview {
            session_id: SessionKey("local:demo".into()),
            preview_id: preview_id.clone(),
            title: Some("preview".into()),
            files: vec![DiffPreviewFile {
                path: "src/lib.rs".into(),
                old_path: None,
                status: DiffPreviewFileStatus::Modified,
                hunks: vec![DiffPreviewHunk {
                    header: "@@ -1 +1 @@".into(),
                    lines: vec![
                        DiffPreviewLine {
                            kind: DiffPreviewLineKind::Context,
                            content: "fn demo() {".into(),
                            old_line: Some(1),
                            new_line: Some(1),
                        },
                        DiffPreviewLine {
                            kind: DiffPreviewLineKind::Added,
                            content: "    println!(\"hello\");".into(),
                            old_line: None,
                            new_line: Some(2),
                        },
                    ],
                }],
            }],
        },
    })
    .into_rpc_response("req-diff")
    .expect("serialize diff result");
    assert_eq!(
        serde_json::to_value(diff_result).expect("diff result json"),
        json!({
            "jsonrpc": "2.0",
            "id": "req-diff",
            "result": {
                "status": "ready",
                "source": "pending_store",
                "preview": {
                    "session_id": "local:demo",
                    "preview_id": preview_id,
                    "title": "preview",
                    "files": [
                        {
                            "path": "src/lib.rs",
                            "status": "modified",
                            "hunks": [
                                {
                                    "header": "@@ -1 +1 @@",
                                    "lines": [
                                        {
                                            "kind": "context",
                                            "content": "fn demo() {",
                                            "old_line": 1,
                                            "new_line": 1
                                        },
                                        {
                                            "kind": "added",
                                            "content": "    println!(\"hello\");",
                                            "new_line": 2
                                        }
                                    ]
                                }
                            ]
                        }
                    ]
                }
            }
        })
    );

    let task_output = UiRpcResult::TaskOutputRead(TaskOutputReadResult {
        session_id: SessionKey("local:demo".into()),
        task_id: task_id.clone(),
        source: TaskOutputReadSource::RuntimeProjection,
        cursor: OutputCursor { offset: 0 },
        next_cursor: OutputCursor { offset: 6 },
        text: "output".into(),
        bytes_read: 6,
        total_bytes: 6,
        truncated: false,
        complete: true,
        live_tail_supported: false,
        is_snapshot_projection: true,
        task_status: "completed".into(),
        runtime_state: "completed".into(),
        lifecycle_state: "completed".into(),
        runtime_detail: None,
        output_files: vec![],
        limitations: vec![TaskOutputReadLimitation {
            code: "snapshot_projection".into(),
            message: "served from task snapshot".into(),
        }],
    })
    .into_rpc_response("req-task")
    .expect("serialize task output result");
    assert_eq!(
        serde_json::to_value(task_output).expect("task output json"),
        json!({
            "jsonrpc": "2.0",
            "id": "req-task",
            "result": {
                "session_id": "local:demo",
                "task_id": task_id,
                "source": "runtime_projection",
                "cursor": { "offset": 0 },
                "next_cursor": { "offset": 6 },
                "text": "output",
                "bytes_read": 6,
                "total_bytes": 6,
                "truncated": false,
                "complete": true,
                "live_tail_supported": false,
                "is_snapshot_projection": true,
                "task_status": "completed",
                "runtime_state": "completed",
                "lifecycle_state": "completed",
                "limitations": [
                    {
                        "code": "snapshot_projection",
                        "message": "served from task snapshot"
                    }
                ]
            }
        })
    );

    // M9 review fix MEDIUM #4 (UPCR-2026-004): pin the literal wire form
    // for `task/updated` carrying the new `cancelled` lifecycle state so a
    // future rename of the variant or a serializer regression that drops
    // the snake_case shape is caught by the representative-payload golden
    // gate, not just by the variant-level round-trip tests at the bottom
    // of this module.
    let task_cancelled = UiNotification::TaskUpdated(TaskUpdatedEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        task_id: task_id.clone(),
        tool_call_id: None,
        title: "spawn_only_runner".into(),
        state: TaskRuntimeState::Cancelled,
        runtime_detail: Some("user cancelled".into()),
        source: None,
        role: None,
        summary: None,
        artifact_count: None,
        runtime_policy_stamp: None,
        turn_id: None,
    })
    .into_rpc_notification()
    .expect("serialize task/updated cancelled");
    assert_eq!(
        serde_json::to_value(task_cancelled).expect("task/updated cancelled json"),
        json!({
            "jsonrpc": "2.0",
            "method": "task/updated",
            "params": {
                "session_id": "local:demo",
                "task_id": task_id,
                "title": "spawn_only_runner",
                "state": "cancelled",
                "runtime_detail": "user cancelled"
            }
        })
    );

    let warning = UiNotification::Warning(WarningEvent {
        session_id: SessionKey("local:demo".into()),
        turn_id: Some(turn_id),
        code: "mock_warning".into(),
        message: "mock payload".into(),
    })
    .into_rpc_notification()
    .expect("serialize warning");
    assert_eq!(
        serde_json::to_value(warning).expect("warning json"),
        json!({
            "jsonrpc": "2.0",
            "method": "warning",
            "params": {
                "session_id": "local:demo",
                "turn_id": TurnId(Uuid::from_u128(1)),
                "code": "mock_warning",
                "message": "mock payload"
            }
        })
    );
}

#[test]
fn generic_and_typed_approval_payloads_round_trip() {
    let session_id = SessionKey("local:demo".into());
    let turn_id = TurnId(Uuid::from_u128(1));
    let approval_id = ApprovalId(Uuid::from_u128(2));

    let generic = ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        turn_id.clone(),
        "shell",
        "Approval requested",
        "Run cargo test?",
    );
    let generic_json = serde_json::to_value(&generic).expect("generic approval json");
    assert!(generic_json.get("approval_kind").is_none());
    assert!(generic_json.get("typed_details").is_none());
    assert_eq!(
        serde_json::from_value::<ApprovalRequestedEvent>(generic_json)
            .expect("generic approval decodes"),
        generic
    );

    let command = ApprovalTypedDetails::command(
        ApprovalCommandDetails {
            argv: vec!["cargo".into(), "test".into()],
            command_line: Some("cargo test".into()),
            cwd: Some("/Users/yuechen/home/octos".into()),
            env_keys: vec!["RUST_LOG".into()],
            tool_call_id: Some("tool-1".into()),
        },
        Some(ApprovalSandboxDetails {
            mode: Some("workspace_write".into()),
            filesystem_access: Some("workspace_write".into()),
            network_access: Some(false),
            writable_roots: vec!["/Users/yuechen/home/octos".into()],
        }),
    );
    assert_typed_approval_round_trips(
        ApprovalRequestedEvent {
            approval_kind: Some(approval_kinds::COMMAND.into()),
            risk: Some("medium".into()),
            typed_details: Some(command),
            render_hints: Some(ApprovalRenderHints {
                default_decision: Some("deny".into()),
                primary_label: Some("Approve".into()),
                secondary_label: Some("Deny".into()),
                danger: Some(false),
                monospace_fields: vec![
                    "typed_details.command.command_line".into(),
                    "typed_details.command.cwd".into(),
                ],
            }),
            ..generic.clone()
        },
        approval_kinds::COMMAND,
    );

    assert_typed_approval_round_trips(
        ApprovalRequestedEvent {
            approval_kind: Some(approval_kinds::DIFF.into()),
            typed_details: Some(ApprovalTypedDetails {
                kind: approval_kinds::DIFF.into(),
                command: None,
                sandbox: None,
                diff: Some(ApprovalDiffDetails {
                    preview_id: PreviewId(Uuid::from_u128(3)),
                    operation: Some("apply".into()),
                    file_count: Some(2),
                    additions: Some(14),
                    deletions: Some(5),
                    summary: Some("Update approval reducer tests".into()),
                }),
                filesystem: None,
                network: None,
                sandbox_escalation: None,
            }),
            ..generic.clone()
        },
        approval_kinds::DIFF,
    );

    assert_typed_approval_round_trips(
        ApprovalRequestedEvent {
            approval_kind: Some(approval_kinds::FILESYSTEM.into()),
            typed_details: Some(ApprovalTypedDetails {
                kind: approval_kinds::FILESYSTEM.into(),
                command: None,
                sandbox: None,
                diff: None,
                filesystem: Some(ApprovalFilesystemDetails {
                    operation: "write".into(),
                    paths: vec!["docs/example.md".into()],
                    outside_workspace: false,
                    writable_roots: vec!["/Users/yuechen/home/octos".into()],
                }),
                network: None,
                sandbox_escalation: None,
            }),
            ..generic.clone()
        },
        approval_kinds::FILESYSTEM,
    );

    assert_typed_approval_round_trips(
        ApprovalRequestedEvent {
            approval_kind: Some(approval_kinds::NETWORK.into()),
            typed_details: Some(ApprovalTypedDetails {
                kind: approval_kinds::NETWORK.into(),
                command: None,
                sandbox: None,
                diff: None,
                filesystem: None,
                network: Some(ApprovalNetworkDetails {
                    operation: "connect".into(),
                    hosts: vec!["api.openai.com".into()],
                    ports: vec![443],
                    urls: vec!["https://api.openai.com/v1/responses".into()],
                }),
                sandbox_escalation: None,
            }),
            ..generic.clone()
        },
        approval_kinds::NETWORK,
    );

    assert_typed_approval_round_trips(
        ApprovalRequestedEvent {
            approval_kind: Some(approval_kinds::SANDBOX_ESCALATION.into()),
            typed_details: Some(ApprovalTypedDetails {
                kind: approval_kinds::SANDBOX_ESCALATION.into(),
                command: None,
                sandbox: None,
                diff: None,
                filesystem: None,
                network: None,
                sandbox_escalation: Some(ApprovalSandboxEscalationDetails {
                    from: Some(ApprovalSandboxEscalationEndpoint {
                        mode: Some("workspace_write".into()),
                        network_access: Some(false),
                    }),
                    to: Some(ApprovalSandboxEscalationEndpoint {
                        mode: Some("danger_full_access".into()),
                        network_access: Some(true),
                    }),
                    requested_permissions: vec![
                        "filesystem_unrestricted".into(),
                        "network_access".into(),
                    ],
                    justification: Some("Run integration tests".into()),
                    suggested_prefix_rule: vec!["cargo".into(), "test".into()],
                }),
            }),
            ..generic
        },
        approval_kinds::SANDBOX_ESCALATION,
    );
}

fn assert_typed_approval_round_trips(event: ApprovalRequestedEvent, expected_kind: &str) {
    let value = serde_json::to_value(&event).expect("typed approval json");
    assert_eq!(value["approval_kind"], json!(expected_kind));
    assert_eq!(value["typed_details"]["kind"], json!(expected_kind));
    assert_eq!(
        serde_json::from_value::<ApprovalRequestedEvent>(value).expect("typed approval decodes"),
        event
    );
}

#[test]
fn unknown_typed_approval_kind_decodes_for_generic_fallback() {
    let value = json!({
        "session_id": "local:demo",
        "approval_id": ApprovalId(Uuid::from_u128(2)),
        "turn_id": TurnId(Uuid::from_u128(1)),
        "tool_name": "future",
        "title": "Future approval",
        "body": "Fallback body remains actionable",
        "approval_kind": "future_kind",
        "typed_details": {
            "kind": "future_kind"
        }
    });

    let decoded: ApprovalRequestedEvent =
        serde_json::from_value(value).expect("unknown typed approval decodes");

    assert_eq!(decoded.approval_kind.as_deref(), Some("future_kind"));
    assert_eq!(
        decoded
            .typed_details
            .as_ref()
            .map(|details| details.kind.as_str()),
        Some("future_kind")
    );
    assert_eq!(decoded.title, "Future approval");
    assert_eq!(decoded.body, "Fallback body remains actionable");
}

// ---- UPCR-2026-023 AskUserQuestion protocol round-trips ----

#[test]
fn user_question_methods_and_feature_are_registered() {
    assert_eq!(methods::USER_QUESTION_RESPOND, "user_question/respond");
    assert_eq!(methods::USER_QUESTION_REQUESTED, "user_question/requested");
    assert_eq!(UI_PROTOCOL_FEATURE_USER_QUESTION_V1, "user_question.v1");
    assert!(UI_PROTOCOL_COMMAND_METHODS.contains(&methods::USER_QUESTION_RESPOND));
    assert!(UI_PROTOCOL_NOTIFICATION_METHODS.contains(&methods::USER_QUESTION_REQUESTED));
    assert!(UI_PROTOCOL_KNOWN_FEATURES.contains(&UI_PROTOCOL_FEATURE_USER_QUESTION_V1));
    assert_eq!(
        method_capability_gate(methods::USER_QUESTION_RESPOND),
        Some(UI_PROTOCOL_FEATURE_USER_QUESTION_V1)
    );
}

#[test]
fn full_protocol_advertises_user_question_feature() {
    // `full_protocol()` must agree with the known/first-server feature
    // lists, both of which already include `user_question.v1`. A client
    // that handshakes against `full_protocol()` must see the question
    // capability or it will never negotiate `user_question.v1` and the
    // tool silently degrades to its fallback.
    let caps = UiProtocolCapabilities::full_protocol();
    assert!(
        caps.supported_features
            .iter()
            .any(|f| f == UI_PROTOCOL_FEATURE_USER_QUESTION_V1),
        "full_protocol() must advertise user_question.v1; got {:?}",
        caps.supported_features
    );
}

#[test]
fn user_question_requested_event_round_trips_with_structured_questions() {
    let event = UserQuestionRequestedEvent::new(
        SessionKey("local:demo".into()),
        QuestionId(Uuid::from_u128(7)),
        TurnId(Uuid::from_u128(1)),
        "Pick a framework",
        "The agent needs you to choose a target framework and runtimes.",
        vec![
            UserQuestion {
                header: "Framework".into(),
                question: "Which web framework should I scaffold?".into(),
                options: vec![
                    UserQuestionOption {
                        label: "axum".into(),
                        description: "tower-based async framework".into(),
                    },
                    UserQuestionOption {
                        label: "actix".into(),
                        description: "actor-based framework".into(),
                    },
                ],
                multi_select: false,
                allow_free_text: true,
            },
            UserQuestion {
                header: "Runtimes".into(),
                question: "Which runtimes should CI cover?".into(),
                options: vec![
                    UserQuestionOption {
                        label: "stable".into(),
                        description: "latest stable toolchain".into(),
                    },
                    UserQuestionOption {
                        label: "nightly".into(),
                        description: "nightly toolchain".into(),
                    },
                    UserQuestionOption {
                        label: "msrv".into(),
                        description: "minimum supported rust version".into(),
                    },
                ],
                multi_select: true,
                allow_free_text: true,
            },
        ],
    );

    let notification = UiNotification::UserQuestionRequested(event.clone());
    assert_eq!(notification.method(), methods::USER_QUESTION_REQUESTED);
    let wire = notification
        .clone()
        .into_rpc_notification()
        .expect("serialize user_question/requested");
    assert_eq!(wire.method, methods::USER_QUESTION_REQUESTED);
    // snake_case wire field names + mandatory title/body fallback.
    assert_eq!(wire.params["title"], json!("Pick a framework"));
    assert_eq!(wire.params["questions"][0]["header"], json!("Framework"));
    assert_eq!(wire.params["questions"][0]["multi_select"], json!(false));
    assert_eq!(wire.params["questions"][1]["multi_select"], json!(true));
    assert_eq!(wire.params["questions"][1]["allow_free_text"], json!(true));
    assert_eq!(
        wire.params["questions"][0]["options"][0]["label"],
        json!("axum")
    );

    let decoded =
        UiNotification::from_rpc_notification(wire).expect("decode user_question/requested");
    assert_eq!(decoded, notification);
}

// #1477 voice rich output: the typed visual lifecycle events carry the
// right method + snake_case wire fields (server→client only).
#[test]
fn visual_generating_and_failed_wire_contract() {
    let generating = UiNotification::VisualGenerating(VisualGeneratingEvent {
        session_id: SessionKey("local:voice".into()),
        topic: None,
        turn_id: TurnId(Uuid::from_u128(1)),
        kind: "illustrated".into(),
    });
    assert_eq!(generating.method(), methods::VISUAL_GENERATING);
    let wire = generating
        .clone()
        .into_rpc_notification()
        .expect("serialize visual/generating");
    assert_eq!(wire.method, methods::VISUAL_GENERATING);
    assert_eq!(wire.params["kind"], json!("illustrated"));
    // Round-trip: decode must reconstruct the same notification.
    let decoded = UiNotification::from_rpc_notification(wire).expect("decode visual/generating");
    assert_eq!(decoded, generating);

    let succeeded = UiNotification::VisualSucceeded(VisualSucceededEvent {
        session_id: SessionKey("local:voice".into()),
        topic: None,
        turn_id: TurnId(Uuid::from_u128(1)),
        kind: "html".into(),
        files: vec!["visual-abc.html".into()],
    });
    assert_eq!(succeeded.method(), methods::VISUAL_SUCCEEDED);
    let wire = succeeded
        .clone()
        .into_rpc_notification()
        .expect("serialize visual/succeeded");
    assert_eq!(wire.method, methods::VISUAL_SUCCEEDED);
    assert_eq!(wire.params["kind"], json!("html"));
    assert_eq!(wire.params["files"], json!(["visual-abc.html"]));
    let decoded = UiNotification::from_rpc_notification(wire).expect("decode visual/succeeded");
    assert_eq!(decoded, succeeded);

    let failed = UiNotification::VisualFailed(VisualFailedEvent {
        session_id: SessionKey("local:voice".into()),
        topic: None,
        turn_id: TurnId(Uuid::from_u128(1)),
        reason: Some("timed out".into()),
    });
    assert_eq!(failed.method(), methods::VISUAL_FAILED);
    let wire = failed
        .clone()
        .into_rpc_notification()
        .expect("serialize visual/failed");
    assert_eq!(wire.method, methods::VISUAL_FAILED);
    assert_eq!(wire.params["reason"], json!("timed out"));
    let decoded = UiNotification::from_rpc_notification(wire).expect("decode visual/failed");
    assert_eq!(decoded, failed);
}

#[test]
fn voice_exit_wire_contract() {
    // UPCR-2026-025: the typed exit notification carries session_id + turn_id
    // (and an optional topic); it round-trips intact and the topic is stamped
    // from a topic-scoped session key, mirroring the visual/* lifecycle.
    let exit = UiNotification::VoiceExit(VoiceExitEvent {
        session_id: SessionKey("local:voice#exit".into()),
        topic: None,
        turn_id: TurnId(Uuid::from_u128(42)),
    });
    assert_eq!(exit.method(), methods::VOICE_EXIT);
    let wire = exit
        .clone()
        .into_rpc_notification()
        .expect("serialize voice/exit");
    assert_eq!(wire.method, methods::VOICE_EXIT);
    // Topic is stamped from the `#exit` suffix of the session key on the wire.
    assert_eq!(wire.params["topic"], json!("exit"));
    let decoded = UiNotification::from_rpc_notification(wire).expect("decode voice/exit");
    // Equality holds after the topic was stamped from the session key.
    assert_eq!(decoded.method(), methods::VOICE_EXIT);
    assert_eq!(decoded.session_id().0, "local:voice#exit");
    assert_eq!(decoded.topic(), Some("exit"));
}

#[test]
fn user_question_respond_params_round_trip_multi_question_and_free_text() {
    let params = UserQuestionRespondParams {
        session_id: SessionKey("local:demo".into()),
        question_id: QuestionId(Uuid::from_u128(7)),
        answers: vec![
            // single-select: one label
            UserQuestionAnswer {
                selected_labels: vec!["axum".into()],
                free_text: None,
            },
            // multi-select: several labels
            UserQuestionAnswer {
                selected_labels: vec!["stable".into(), "msrv".into()],
                free_text: None,
            },
            // free-text-only: empty labels + free_text
            UserQuestionAnswer {
                selected_labels: Vec::new(),
                free_text: Some("rocket, please".into()),
            },
        ],
        client_note: Some("answered from TUI".into()),
    };

    let command = UiCommand::UserQuestionRespond(params.clone());
    assert_eq!(command.method(), methods::USER_QUESTION_RESPOND);
    let request = command
        .clone()
        .into_rpc_request("req-uq-1")
        .expect("serialize user_question/respond");
    assert_eq!(request.method, methods::USER_QUESTION_RESPOND);
    // free-text-only answer omits the empty selected_labels but keeps free_text.
    assert_eq!(
        request.params["answers"][2]["free_text"],
        json!("rocket, please")
    );
    assert!(
        request.params["answers"][2]
            .get("selected_labels")
            .is_none()
    );

    let decoded = UiCommand::from_rpc_request(request).expect("decode user_question/respond");
    assert_eq!(decoded, command);
}

#[test]
fn user_question_respond_decodes_minimal_and_unknown_fields() {
    // Minimal params: client_note omitted, free-text-only answer with no
    // selected_labels, plus an unknown forward-compat sibling field.
    let value = json!({
        "session_id": "local:demo",
        "question_id": QuestionId(Uuid::from_u128(7)),
        "answers": [
            { "free_text": "something else" }
        ],
        "future_field": { "anything": true }
    });
    let decoded: UserQuestionRespondParams =
        serde_json::from_value(value).expect("minimal user_question/respond decodes");
    assert_eq!(decoded.client_note, None);
    assert_eq!(decoded.answers.len(), 1);
    assert!(decoded.answers[0].selected_labels.is_empty());
    assert_eq!(
        decoded.answers[0].free_text.as_deref(),
        Some("something else")
    );
}

#[test]
fn user_question_requested_keeps_generic_fallback_on_unknown_fields() {
    // A client that does not understand `questions` must still get the
    // mandatory generic title/body, and an unknown extra field must not
    // break decoding (forward-compat).
    let value = json!({
        "session_id": "local:demo",
        "question_id": QuestionId(Uuid::from_u128(7)),
        "turn_id": TurnId(Uuid::from_u128(1)),
        "title": "Generic fallback title",
        "body": "Generic fallback body the user can still answer via free text.",
        "questions": [],
        "future_render_hint": "wizard"
    });
    let decoded: UserQuestionRequestedEvent =
        serde_json::from_value(value).expect("unknown-field event decodes");
    assert_eq!(decoded.title, "Generic fallback title");
    assert_eq!(
        decoded.body,
        "Generic fallback body the user can still answer via free text."
    );
    assert!(decoded.questions.is_empty());
}

#[test]
fn approval_respond_accepts_legacy_and_typed_metadata() {
    let legacy = json!({
        "session_id": "local:demo",
        "approval_id": ApprovalId(Uuid::from_u128(2)),
        "decision": "approve"
    });
    let legacy: ApprovalRespondParams =
        serde_json::from_value(legacy).expect("legacy approval/respond decodes");
    assert_eq!(legacy.approval_scope, None);
    assert_eq!(legacy.client_note, None);

    let typed = json!({
        "session_id": "local:demo",
        "approval_id": ApprovalId(Uuid::from_u128(2)),
        "decision": "deny",
        "approval_scope": "request",
        "client_note": "Denied for this invocation"
    });
    let typed: ApprovalRespondParams =
        serde_json::from_value(typed).expect("typed approval/respond decodes");
    assert_eq!(
        typed.approval_scope.as_deref(),
        Some(approval_scopes::REQUEST)
    );
    assert_eq!(
        typed.client_note.as_deref(),
        Some("Denied for this invocation")
    );
}

#[test]
fn ui_command_builds_and_parses_json_rpc_request() {
    let command = UiCommand::TurnStart(TurnStartParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: TurnId(Uuid::from_u128(1)),
        input: vec![InputItem::Text {
            text: "hello".into(),
        }],
        media: Vec::new(),
        topic: None,
        rewrite_for: None,
        reasoning_effort: None,
        live_video: false,
    });

    let request = command
        .clone()
        .into_rpc_request("req-1")
        .expect("serialize command params");

    assert_eq!(request.jsonrpc, JSON_RPC_VERSION);
    assert_eq!(request.id, "req-1");
    assert_eq!(request.method, methods::TURN_START);

    let wire = serde_json::to_value(&request).expect("serialize request");
    assert_eq!(wire["jsonrpc"], json!(JSON_RPC_VERSION));
    assert_eq!(wire["params"]["session_id"], json!("local:demo"));
    assert_eq!(wire["params"]["input"][0]["kind"], json!("text"));
    assert!(wire["params"].get("kind").is_none());
    // UPCR-2026-015 (M9-β-1): the three new optional fields are
    // ABSENT on the wire when at their default (empty / None).
    // This locks the back-compat shape — pre-β-1 servers and
    // clients see exactly the bytes they used to.
    assert!(
        wire["params"].get("media").is_none(),
        "empty media MUST be omitted on the wire"
    );
    assert!(
        wire["params"].get("topic").is_none(),
        "absent topic MUST be omitted on the wire"
    );
    assert!(
        wire["params"].get("rewrite_for").is_none(),
        "absent rewrite_for MUST be omitted on the wire"
    );

    let decoded_request: RpcRequest<Value> =
        serde_json::from_value(wire).expect("deserialize request");
    let decoded = UiCommand::from_rpc_request(decoded_request).expect("parse request params");

    assert_eq!(decoded, command);
}

/// UPCR-2026-015 (M9-β-1): a `turn/start` carrying media references
/// round-trips bit-for-bit. The `FileRef` shape mirrors
/// `Payload::UserMessage.files` (γ-1 PR #848 / UPCR-2026-014).
#[test]
fn turn_start_round_trips_with_media_field() {
    let command = UiCommand::TurnStart(TurnStartParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: TurnId(Uuid::from_u128(2)),
        input: vec![InputItem::Text {
            text: "look at this".into(),
        }],
        media: vec![
            FileRef {
                path: "/tmp/upload-1.png".into(),
                mime: "image/png".into(),
                size_bytes: 4096,
            },
            FileRef {
                path: "/tmp/voice.mp3".into(),
                mime: "audio/mpeg".into(),
                size_bytes: 32_768,
            },
        ],
        topic: None,
        rewrite_for: None,
        reasoning_effort: None,
        live_video: false,
    });

    let wire = serde_json::to_value(
        command
            .clone()
            .into_rpc_request("req-media")
            .expect("serialize"),
    )
    .expect("to_value");

    let media = wire["params"]
        .get("media")
        .and_then(|v| v.as_array())
        .expect("media array on the wire");
    assert_eq!(media.len(), 2);
    assert_eq!(media[0].get("path"), Some(&json!("/tmp/upload-1.png")));
    assert_eq!(media[0].get("mime"), Some(&json!("image/png")));
    assert_eq!(media[0].get("size_bytes"), Some(&json!(4096)));
    assert_eq!(media[1].get("path"), Some(&json!("/tmp/voice.mp3")));

    let decoded_request: RpcRequest<Value> = serde_json::from_value(wire).expect("deserialize");
    let decoded = UiCommand::from_rpc_request(decoded_request).expect("parse");
    assert_eq!(decoded, command);
}

/// UPCR-2026-015 (M9-β-1): `topic` field surfaces as a flat string
/// on the wire (parallel to `task/list.topic`). The server folds
/// it into the resolved `SessionKey` before scope validation.
#[test]
fn turn_start_round_trips_with_topic_field() {
    let command = UiCommand::TurnStart(TurnStartParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: TurnId(Uuid::from_u128(3)),
        input: vec![InputItem::Text {
            text: "build me a deck".into(),
        }],
        media: Vec::new(),
        topic: Some("slides".into()),
        rewrite_for: None,
        reasoning_effort: None,
        live_video: false,
    });

    let wire = serde_json::to_value(
        command
            .clone()
            .into_rpc_request("req-topic")
            .expect("serialize"),
    )
    .expect("to_value");

    assert_eq!(wire["params"]["topic"], json!("slides"));
    assert!(
        wire["params"].get("media").is_none(),
        "empty media stays omitted"
    );
    assert!(
        wire["params"].get("rewrite_for").is_none(),
        "absent rewrite_for stays omitted"
    );

    let decoded_request: RpcRequest<Value> = serde_json::from_value(wire).expect("deserialize");
    let decoded = UiCommand::from_rpc_request(decoded_request).expect("parse");
    assert_eq!(decoded, command);
}

/// UPCR-2026-015 (M9-β-1): `rewrite_for` carries the
/// `client_message_id` of an existing queued user message that
/// this turn replaces in place (the `/queue` slash-command flow).
#[test]
fn turn_start_round_trips_with_rewrite_for_field() {
    let command = UiCommand::TurnStart(TurnStartParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: TurnId(Uuid::from_u128(4)),
        input: vec![InputItem::Text {
            text: "edited prompt".into(),
        }],
        media: Vec::new(),
        topic: None,
        rewrite_for: Some("cmid-queued-original".into()),
        reasoning_effort: None,
        live_video: false,
    });

    let wire = serde_json::to_value(
        command
            .clone()
            .into_rpc_request("req-rewrite")
            .expect("serialize"),
    )
    .expect("to_value");

    assert_eq!(wire["params"]["rewrite_for"], json!("cmid-queued-original"));
    assert!(
        wire["params"].get("media").is_none(),
        "empty media stays omitted"
    );
    assert!(
        wire["params"].get("topic").is_none(),
        "absent topic stays omitted"
    );

    let decoded_request: RpcRequest<Value> = serde_json::from_value(wire).expect("deserialize");
    let decoded = UiCommand::from_rpc_request(decoded_request).expect("parse");
    assert_eq!(decoded, command);
}

/// UPCR-2026-015 (M9-β-1): the three β-1 fields can co-exist on
/// one envelope (e.g. a `/queue` rewrite that swaps in new media
/// and lands under a topic-scoped session). All three round-trip
/// together.
#[test]
fn turn_start_round_trips_with_all_beta1_fields() {
    let command = UiCommand::TurnStart(TurnStartParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: TurnId(Uuid::from_u128(5)),
        input: vec![InputItem::Text {
            text: "redo with this image".into(),
        }],
        media: vec![FileRef {
            path: "/tmp/replacement.png".into(),
            mime: "image/png".into(),
            size_bytes: 8192,
        }],
        topic: Some("research".into()),
        rewrite_for: Some("cmid-original".into()),
        reasoning_effort: None,
        live_video: false,
    });

    let wire = serde_json::to_value(
        command
            .clone()
            .into_rpc_request("req-all")
            .expect("serialize"),
    )
    .expect("to_value");

    assert_eq!(wire["params"]["topic"], json!("research"));
    assert_eq!(wire["params"]["rewrite_for"], json!("cmid-original"));
    let media = wire["params"]["media"].as_array().expect("media");
    assert_eq!(media.len(), 1);
    assert_eq!(media[0]["path"], json!("/tmp/replacement.png"));

    let decoded_request: RpcRequest<Value> = serde_json::from_value(wire).expect("deserialize");
    let decoded = UiCommand::from_rpc_request(decoded_request).expect("parse");
    assert_eq!(decoded, command);
}

#[test]
fn task_control_commands_build_and_parse_json_rpc_requests() {
    let session_id = SessionKey("local:demo".into());
    let task_id = TaskId(Uuid::from_u128(42));

    let list = UiCommand::TaskList(TaskListParams {
        session_id: session_id.clone(),
        topic: Some("default".into()),
    });
    assert_eq!(list.method(), methods::TASK_LIST);
    let list_wire = list
        .clone()
        .into_rpc_request("task-list")
        .expect("serialize task/list");
    assert_eq!(list_wire.method, methods::TASK_LIST);
    assert_eq!(list_wire.params["session_id"], json!("local:demo"));
    assert_eq!(
        UiCommand::from_rpc_request(list_wire).expect("decode task/list"),
        list
    );

    let cancel = UiCommand::TaskCancel(TaskCancelParams {
        task_id: task_id.clone(),
        session_id: Some(session_id.clone()),
        profile_id: Some("coding".into()),
    });
    assert_eq!(cancel.method(), methods::TASK_CANCEL);
    let cancel_wire = cancel
        .clone()
        .into_rpc_request("task-cancel")
        .expect("serialize task/cancel");
    assert_eq!(cancel_wire.params["task_id"], json!(task_id));
    assert_eq!(cancel_wire.params["profile_id"], json!("coding"));
    assert_eq!(
        UiCommand::from_rpc_request(cancel_wire).expect("decode task/cancel"),
        cancel
    );

    let restart = UiCommand::TaskRestartFromNode(TaskRestartFromNodeParams {
        task_id: TaskId(Uuid::from_u128(43)),
        node_id: Some("node-7".into()),
        session_id: Some(session_id),
        profile_id: None,
    });
    assert_eq!(restart.method(), methods::TASK_RESTART_FROM_NODE);
    let restart_wire = restart
        .clone()
        .into_rpc_request("task-restart")
        .expect("serialize task/restart_from_node");
    assert_eq!(restart_wire.params["node_id"], json!("node-7"));
    assert_eq!(
        UiCommand::from_rpc_request(restart_wire).expect("decode task/restart_from_node"),
        restart
    );

    let artifact_list = UiCommand::TaskArtifactList(TaskArtifactListParams {
        session_id: SessionKey("local:demo".into()),
        task_id: task_id.clone(),
        profile_id: Some("coding".into()),
        agent_id: None,
    });
    assert_eq!(artifact_list.method(), methods::TASK_ARTIFACT_LIST);
    let artifact_list_wire = artifact_list
        .clone()
        .into_rpc_request("task-artifact-list")
        .expect("serialize task/artifact/list");
    assert_eq!(artifact_list_wire.params["task_id"], json!(task_id));
    assert_eq!(
        UiCommand::from_rpc_request(artifact_list_wire).expect("decode task/artifact/list"),
        artifact_list
    );

    let artifact_read = UiCommand::TaskArtifactRead(TaskArtifactReadParams {
        session_id: SessionKey("local:demo".into()),
        task_id: TaskId(Uuid::from_u128(45)),
        artifact_id: Some("summary".into()),
        path: None,
        cursor: None,
        limit_bytes: Some(1024),
        profile_id: None,
        agent_id: Some("agent-1".into()),
    });
    assert_eq!(artifact_read.method(), methods::TASK_ARTIFACT_READ);
    let artifact_read_wire = artifact_read
        .clone()
        .into_rpc_request("task-artifact-read")
        .expect("serialize task/artifact/read");
    assert_eq!(artifact_read_wire.params["artifact_id"], json!("summary"));
    assert_eq!(artifact_read_wire.params["agent_id"], json!("agent-1"));
    assert_eq!(
        UiCommand::from_rpc_request(artifact_read_wire).expect("decode task/artifact/read"),
        artifact_read
    );
}

#[test]
fn typed_rpc_results_map_from_methods_and_round_trip() {
    let opened = SessionOpened {
        session_id: SessionKey("local:demo".into()),
        active_profile_id: Some("coding".into()),
        workspace_root: None,
        context: None,
        context_state: None,
        cursor: Some(UiCursor {
            stream: "events".into(),
            seq: 42,
        }),
        panes: None,
        capabilities: UiProtocolCapabilities::first_server_slice(),
        reasoning_effort: None,
    };

    let session_result = UiRpcResult::SessionOpen(SessionOpenResult::new(opened));
    assert_eq!(session_result.kind(), UiResultKind::SessionOpen);
    assert_eq!(session_result.method(), Some(methods::SESSION_OPEN));

    let response = session_result
        .clone()
        .into_rpc_response("open-1")
        .expect("serialize session/open result");
    assert_eq!(response.id, "open-1");
    assert_eq!(response.result["opened"]["session_id"], json!("local:demo"));

    let decoded = UiRpcResult::from_method_and_result(methods::SESSION_OPEN, response.result)
        .expect("decode session/open result");
    assert_eq!(decoded, session_result);

    let turn_start = UiRpcResult::TurnStart(TurnStartResult::accepted());
    let value = turn_start
        .clone()
        .into_result_value()
        .expect("serialize turn/start result");
    assert_eq!(value, json!({ "accepted": true }));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TURN_START, value)
            .expect("decode turn/start result"),
        turn_start
    );

    let turn_interrupt = UiRpcResult::TurnInterrupt(TurnInterruptResult::new(false));
    let value = turn_interrupt
        .clone()
        .into_result_value()
        .expect("serialize turn/interrupt result");
    assert_eq!(value, json!({ "interrupted": false }));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TURN_INTERRUPT, value)
            .expect("decode turn/interrupt result"),
        turn_interrupt
    );

    let approval_id = ApprovalId::new();
    let approval =
        UiRpcResult::ApprovalRespond(ApprovalRespondResult::accepted(approval_id.clone()));
    assert_eq!(approval.kind(), UiResultKind::ApprovalRespond);
    assert_eq!(approval.method(), Some(methods::APPROVAL_RESPOND));
    let value = approval
        .clone()
        .into_result_value()
        .expect("serialize approval/respond result");
    assert_eq!(value["approval_id"], json!(approval_id));
    assert_eq!(value["status"], json!("accepted"));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::APPROVAL_RESPOND, value)
            .expect("decode approval/respond result"),
        approval
    );

    let scopes_result = UiRpcResult::ApprovalScopesList(ApprovalScopesListResult {
        scopes: vec![ApprovalScopeEntry {
            session_id: SessionKey("local:demo".into()),
            scope: approval_scopes::SESSION.into(),
            scope_match: "shell".into(),
            decision: ApprovalDecision::Approve,
            turn_id: None,
        }],
    });
    assert_eq!(scopes_result.kind(), UiResultKind::ApprovalScopesList);
    assert_eq!(scopes_result.method(), Some(methods::APPROVAL_SCOPES_LIST));
    let value = scopes_result
        .clone()
        .into_result_value()
        .expect("serialize approval/scopes/list result");
    assert_eq!(value["scopes"][0]["scope"], json!(approval_scopes::SESSION));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::APPROVAL_SCOPES_LIST, value)
            .expect("decode approval/scopes/list result"),
        scopes_result
    );

    assert_eq!(
        first_server_result_kind_for_method(methods::DIFF_PREVIEW_GET),
        Some(UiResultKind::DiffPreviewGet)
    );
    assert_eq!(
        first_server_result_kind_for_method(methods::TASK_LIST),
        Some(UiResultKind::TaskList)
    );
    assert_eq!(
        first_server_result_kind_for_method(methods::TASK_CANCEL),
        Some(UiResultKind::TaskCancel)
    );
    assert_eq!(
        first_server_result_kind_for_method(methods::TASK_RESTART_FROM_NODE),
        Some(UiResultKind::TaskRestartFromNode)
    );
    assert_eq!(
        first_server_result_kind_for_method(methods::TASK_ARTIFACT_LIST),
        Some(UiResultKind::TaskArtifactList)
    );
    assert_eq!(
        first_server_result_kind_for_method(methods::TASK_ARTIFACT_READ),
        Some(UiResultKind::TaskArtifactRead)
    );

    let preview_id = PreviewId::new();
    let diff_result = UiRpcResult::DiffPreviewGet(DiffPreviewGetResult {
        status: DiffPreviewGetStatus::Ready,
        source: DiffPreviewSource::PendingStore,
        preview: DiffPreview {
            session_id: SessionKey("local:demo".into()),
            preview_id: preview_id.clone(),
            title: Some("preview".into()),
            files: vec![DiffPreviewFile {
                path: "src/lib.rs".into(),
                old_path: None,
                status: DiffPreviewFileStatus::Modified,
                hunks: vec![DiffPreviewHunk {
                    header: "@@ -1 +1 @@".into(),
                    lines: vec![DiffPreviewLine {
                        kind: DiffPreviewLineKind::Added,
                        content: "let value = 1;".into(),
                        old_line: None,
                        new_line: Some(1),
                    }],
                }],
            }],
        },
    });
    let value = diff_result
        .clone()
        .into_result_value()
        .expect("serialize diff/preview/get result");
    assert_eq!(value["status"], json!("ready"));
    assert_eq!(value["preview"]["preview_id"], json!(preview_id));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::DIFF_PREVIEW_GET, value)
            .expect("decode diff/preview/get result"),
        diff_result
    );

    let started_at = DateTime::parse_from_rfc3339("2026-04-30T12:00:00Z")
        .expect("parse started_at")
        .with_timezone(&Utc);
    let updated_at = DateTime::parse_from_rfc3339("2026-04-30T12:01:00Z")
        .expect("parse updated_at")
        .with_timezone(&Utc);
    let list_task_id = TaskId(Uuid::from_u128(44));
    let task_list = UiRpcResult::TaskList(TaskListResult {
        session_id: SessionKey("local:demo".into()),
        topic: Some("default".into()),
        tasks: vec![TaskListEntry {
            id: list_task_id.clone(),
            tool_name: "spawn_only_runner".into(),
            tool_call_id: "call-1".into(),
            state: TaskRuntimeState::Running,
            status: "running".into(),
            lifecycle_state: "running".into(),
            runtime_state: "executing_tool".into(),
            source: None,
            role: None,
            summary: None,
            artifact_count: None,
            runtime_policy_stamp: None,
            parent_session_key: Some(SessionKey("local:demo".into())),
            child_session_key: Some(SessionKey("local:demo#child-1".into())),
            child_terminal_state: None,
            child_join_state: None,
            child_joined_at: None,
            child_failure_action: None,
            runtime_detail: Some(json!({ "current_phase": "coding" })),
            workflow_kind: Some("coding".into()),
            current_phase: Some("coding".into()),
            started_at,
            updated_at,
            completed_at: None,
            output_files: vec!["octos-file://task-output".into()],
            error: None,
            session_key: Some(SessionKey("local:demo".into())),
        }],
    });
    assert_eq!(task_list.kind(), UiResultKind::TaskList);
    assert_eq!(task_list.method(), Some(methods::TASK_LIST));
    let value = task_list
        .clone()
        .into_result_value()
        .expect("serialize task/list result");
    assert_eq!(value["tasks"][0]["id"], json!(list_task_id));
    assert_eq!(value["tasks"][0]["state"], json!("running"));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TASK_LIST, value)
            .expect("decode task/list result"),
        task_list
    );

    let task_artifact_list = UiRpcResult::TaskArtifactList(TaskArtifactListResult {
        session_id: SessionKey("local:demo".into()),
        task_id: list_task_id.clone(),
        agent_id: Some("agent-1".into()),
        artifacts: vec![TaskArtifactRecord {
            id: "summary".into(),
            title: "Summary".into(),
            kind: "markdown".into(),
            status: "ready".into(),
            path: None,
            content: None,
            extra: BTreeMap::new(),
        }],
    });
    assert_eq!(task_artifact_list.kind(), UiResultKind::TaskArtifactList);
    assert_eq!(
        task_artifact_list.method(),
        Some(methods::TASK_ARTIFACT_LIST)
    );
    let value = task_artifact_list
        .clone()
        .into_result_value()
        .expect("serialize task/artifact/list result");
    assert_eq!(value["artifacts"][0]["id"], json!("summary"));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TASK_ARTIFACT_LIST, value)
            .expect("decode task/artifact/list result"),
        task_artifact_list
    );

    let task_artifact_read = UiRpcResult::TaskArtifactRead(TaskArtifactReadResult {
        session_id: SessionKey("local:demo".into()),
        task_id: list_task_id.clone(),
        agent_id: Some("agent-1".into()),
        artifact: TaskArtifactRecord {
            id: "summary".into(),
            title: "Summary".into(),
            kind: "markdown".into(),
            status: "ready".into(),
            path: None,
            content: None,
            extra: BTreeMap::new(),
        },
        content: Some("done".into()),
        cursor: Some(OutputCursor { offset: 0 }),
        next_cursor: Some(OutputCursor { offset: 4 }),
        has_more: false,
    });
    assert_eq!(task_artifact_read.kind(), UiResultKind::TaskArtifactRead);
    assert_eq!(
        task_artifact_read.method(),
        Some(methods::TASK_ARTIFACT_READ)
    );
    let value = task_artifact_read
        .clone()
        .into_result_value()
        .expect("serialize task/artifact/read result");
    assert_eq!(value["content"], json!("done"));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TASK_ARTIFACT_READ, value)
            .expect("decode task/artifact/read result"),
        task_artifact_read
    );

    let cancel_result = UiRpcResult::TaskCancel(TaskCancelResult {
        task_id: TaskId(Uuid::from_u128(45)),
        status: TaskRuntimeState::Cancelled,
    });
    assert_eq!(cancel_result.kind(), UiResultKind::TaskCancel);
    assert_eq!(cancel_result.method(), Some(methods::TASK_CANCEL));
    let value = cancel_result
        .clone()
        .into_result_value()
        .expect("serialize task/cancel result");
    assert_eq!(value["status"], json!("cancelled"));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TASK_CANCEL, value)
            .expect("decode task/cancel result"),
        cancel_result
    );

    let restart_result = UiRpcResult::TaskRestartFromNode(TaskRestartFromNodeResult {
        original_task_id: TaskId(Uuid::from_u128(46)),
        new_task_id: TaskId(Uuid::from_u128(47)),
        from_node: Some("node-7".into()),
    });
    assert_eq!(restart_result.kind(), UiResultKind::TaskRestartFromNode);
    assert_eq!(
        restart_result.method(),
        Some(methods::TASK_RESTART_FROM_NODE)
    );
    let value = restart_result
        .clone()
        .into_result_value()
        .expect("serialize task/restart_from_node result");
    assert_eq!(value["from_node"], json!("node-7"));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TASK_RESTART_FROM_NODE, value)
            .expect("decode task/restart_from_node result"),
        restart_result
    );

    let task_result = UiRpcResult::TaskOutputRead(TaskOutputReadResult {
        session_id: SessionKey("local:demo".into()),
        task_id: TaskId::new(),
        source: TaskOutputReadSource::RuntimeProjection,
        cursor: OutputCursor { offset: 0 },
        next_cursor: OutputCursor { offset: 4 },
        text: "done".into(),
        bytes_read: 4,
        total_bytes: 4,
        truncated: false,
        complete: true,
        live_tail_supported: false,
        is_snapshot_projection: true,
        task_status: "failed".into(),
        runtime_state: "delivering_outputs".into(),
        lifecycle_state: "completed".into(),
        runtime_detail: Some(json!({ "phase": "collecting_output" })),
        output_files: vec!["octos-file://output".into()],
        limitations: vec![TaskOutputReadLimitation {
            code: "live_tail_unavailable".into(),
            message: "task/output/delta is not emitted".into(),
        }],
    });
    let value = task_result
        .clone()
        .into_result_value()
        .expect("serialize task/output/read result");
    assert_eq!(value["source"], json!("runtime_projection"));
    assert_eq!(value["next_cursor"]["offset"], json!(4));
    // Audit issue #707 / accepted UPCR-2026-006: clients must be able to
    // distinguish a snapshot projection read from a (future) live-tail
    // read on the wire, not just by inferring it from `live_tail_supported
    // == false` or the `runtime_projection` source label.
    assert_eq!(value["is_snapshot_projection"], json!(true));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TASK_OUTPUT_READ, value)
            .expect("decode task/output/read result"),
        task_result
    );
}

/// Golden: minimal `interrupted: true` round-trip omits the optional
/// diagnostic fields (`reason`, `terminal_state`, `ack_timeout`) so the
/// canonical happy-path wire shape is preserved.
#[test]
fn turn_interrupt_result_minimal_omits_optional_fields() {
    let result = UiRpcResult::TurnInterrupt(TurnInterruptResult::interrupted_ok());
    let value = result
        .clone()
        .into_result_value()
        .expect("serialize turn/interrupt result");
    assert_eq!(value, json!({ "interrupted": true }));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TURN_INTERRUPT, value)
            .expect("decode turn/interrupt result"),
        result
    );
}

/// Golden: declined interrupt with a `reason` string round-trips through
/// serde without dropping the diagnostic field.
#[test]
fn turn_interrupt_result_round_trips_with_reason() {
    let result = UiRpcResult::TurnInterrupt(TurnInterruptResult::declined("turn_id_mismatch"));
    let value = result
        .clone()
        .into_result_value()
        .expect("serialize turn/interrupt result");
    assert_eq!(
        value,
        json!({ "interrupted": false, "reason": "turn_id_mismatch" })
    );
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TURN_INTERRUPT, value)
            .expect("decode turn/interrupt result"),
        result
    );
}

/// Golden: already-terminal interrupt round-trips with `terminal_state`
/// and an `interrupted` boolean derived from the prior terminal state.
#[test]
fn turn_interrupt_result_round_trips_with_terminal_state() {
    let result =
        UiRpcResult::TurnInterrupt(TurnInterruptResult::already_terminal("completed", false));
    let value = result
        .clone()
        .into_result_value()
        .expect("serialize turn/interrupt result");
    assert_eq!(
        value,
        json!({ "interrupted": false, "terminal_state": "completed" })
    );
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TURN_INTERRUPT, value)
            .expect("decode turn/interrupt result"),
        result
    );
}

/// Golden: ack-timed-out interrupt round-trips with `ack_timeout: true`
/// and `interrupted: true` (server captured the interrupt; only client
/// receipt of the terminal event is uncertain).
#[test]
fn turn_interrupt_result_round_trips_with_ack_timeout() {
    let result = UiRpcResult::TurnInterrupt(TurnInterruptResult::ack_timed_out());
    let value = result
        .clone()
        .into_result_value()
        .expect("serialize turn/interrupt result");
    assert_eq!(value, json!({ "interrupted": true, "ack_timeout": true }));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TURN_INTERRUPT, value)
            .expect("decode turn/interrupt result"),
        result
    );
}

/// Spec: unknown optional fields on a `turn/interrupt` result must not
/// break decode for clients on this version (forward-compat).
#[test]
fn turn_interrupt_result_decodes_with_unknown_fields_ignored() {
    let value = json!({
        "interrupted": true,
        "future_extension": "x"
    });
    let decoded = UiRpcResult::from_method_and_result(methods::TURN_INTERRUPT, value)
        .expect("decode turn/interrupt result with unknown field");
    assert_eq!(
        decoded,
        UiRpcResult::TurnInterrupt(TurnInterruptResult::interrupted_ok())
    );
}

#[test]
fn ui_command_parser_reports_invalid_method_and_params() {
    let unknown = RpcRequest::new("req-1", "turn/unknown", json!({}));
    let err = UiCommand::from_rpc_request(unknown).expect_err("reject unknown method");
    assert_eq!(err.code, rpc_error_codes::METHOD_NOT_FOUND);

    let malformed = RpcRequest::new(
        "req-2",
        methods::TURN_INTERRUPT,
        json!({ "session_id": "local:demo" }),
    );
    let err = UiCommand::from_rpc_request(malformed).expect_err("reject malformed params");
    assert_eq!(err.code, rpc_error_codes::INVALID_PARAMS);
    assert!(err.message.contains(methods::TURN_INTERRUPT));
}

#[test]
fn unsupported_capability_report_is_typed_error_data() {
    let legacy_data = json!({ "method": methods::TASK_OUTPUT_READ });
    let legacy: UnsupportedCapabilityReport =
        serde_json::from_value(legacy_data).expect("deserialize legacy unsupported data");
    assert_eq!(legacy.method, methods::TASK_OUTPUT_READ);
    assert_eq!(legacy.reason, "unsupported by this server");

    let error = RpcError::method_not_supported(methods::DIFF_PREVIEW_GET);
    assert_eq!(error.code, rpc_error_codes::METHOD_NOT_SUPPORTED);
    let data = error.data.expect("unsupported error should carry data");
    let report: UnsupportedCapabilityReport =
        serde_json::from_value(data).expect("deserialize typed unsupported data");
    assert_eq!(report.method, methods::DIFF_PREVIEW_GET);

    let result =
        UnsupportedCapabilityResult::method(methods::APPROVAL_RESPOND, "approval is pending");
    let value = UiRpcResult::UnsupportedCapability(result.clone())
        .into_result_value()
        .expect("serialize unsupported result");
    assert_eq!(
        value["unsupported"]["method"],
        json!(methods::APPROVAL_RESPOND)
    );
    let decoded: UnsupportedCapabilityResult =
        serde_json::from_value(value).expect("deserialize unsupported result");
    assert_eq!(decoded, result);
}

#[test]
fn rich_progress_metadata_round_trips_with_extra_fields() {
    let value = json!({
        "kind": "token_cost_update",
        "message": "usage updated",
        "token_cost": {
            "input_tokens": 12,
            "output_tokens": 7,
            "session_cost": 0.0025,
            "currency": "USD"
        },
        "provider": "openai"
    });

    let metadata: UiProgressMetadata =
        serde_json::from_value(value).expect("deserialize rich progress metadata");

    assert_eq!(metadata.kind, progress_kinds::TOKEN_COST_UPDATE);
    assert_eq!(metadata.message.as_deref(), Some("usage updated"));
    assert_eq!(
        metadata
            .token_cost
            .as_ref()
            .and_then(|cost| cost.input_tokens),
        Some(12)
    );
    assert_eq!(
        metadata.extra.get("provider"),
        Some(&Value::String("openai".into()))
    );

    let encoded = serde_json::to_value(&metadata).expect("serialize rich progress metadata");
    assert_eq!(encoded["provider"], json!("openai"));
    assert_eq!(encoded["token_cost"]["session_cost"], json!(0.0025));
}

#[test]
fn rich_progress_event_uses_standalone_notification_method() {
    let metadata = UiProgressMetadata::file_mutation(UiFileMutationNotice::new(
        "src/main.rs",
        file_mutation_operations::WRITE,
    ));
    let event = UiProgressEvent::new(
        SessionKey("local:demo".into()),
        Some(TurnId(Uuid::from_u128(3))),
        metadata,
    );

    let notification = event
        .clone()
        .into_rpc_notification()
        .expect("serialize progress notification");

    assert_eq!(notification.method, methods::PROGRESS_UPDATED);
    assert_eq!(
        notification.params["metadata"]["kind"],
        json!("file_mutation")
    );
    assert_eq!(
        notification.params["metadata"]["file_mutation"]["operation"],
        json!("write")
    );

    let decoded =
        UiProgressEvent::from_rpc_notification(notification).expect("decode progress notification");
    assert_eq!(decoded, event);
}

#[test]
fn rpc_success_and_error_responses_use_json_rpc_v2() {
    let success = RpcResponse::success("req-1", json!({ "ok": true }));
    assert_eq!(success.jsonrpc, JSON_RPC_VERSION);
    assert!(success.is_jsonrpc_v2());

    let error = RpcErrorResponse::new(None, RpcError::parse_error("invalid json"));
    let wire = serde_json::to_value(&error).expect("serialize error response");

    assert_eq!(
        wire,
        json!({
            "jsonrpc": JSON_RPC_VERSION,
            "id": null,
            "error": {
                "code": rpc_error_codes::PARSE_ERROR,
                "message": "invalid json"
            }
        })
    );
}

#[test]
fn ui_notification_builds_and_parses_json_rpc_notification() {
    let event = UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        turn_id: TurnId(Uuid::from_u128(2)),
        text: "partial".into(),
    });

    let notification = event
        .clone()
        .into_rpc_notification()
        .expect("serialize notification params");

    assert_eq!(notification.jsonrpc, JSON_RPC_VERSION);
    assert_eq!(notification.method, methods::MESSAGE_DELTA);

    let wire = serde_json::to_value(&notification).expect("serialize notification");
    assert_eq!(wire["params"]["text"], json!("partial"));
    assert!(wire["params"].get("kind").is_none());

    let decoded_notification: RpcNotification<Value> =
        serde_json::from_value(wire).expect("deserialize notification");
    let decoded = UiNotification::from_rpc_notification(decoded_notification)
        .expect("parse notification params");

    assert_eq!(decoded, event);
}

fn m15_agent_record(session_id: SessionKey) -> UiAgentRecord {
    UiAgentRecord {
        agent_id: "reviewer-api".into(),
        parent_agent_id: Some("master".into()),
        session_id,
        task_id: Some("task_01".into()),
        path: "master/reviewer-api".into(),
        role: "reviewer".into(),
        nickname: "Ada Lovelace".into(),
        title: Some("Ada Lovelace".into()),
        backend_kind: "cli_process".into(),
        status: "running".into(),
        last_task: Some("Running live code review check".into()),
        summary: Some("Running live code review check".into()),
        output_tail: Some("reviewer-api: checking API surface\n".into()),
        cwd: Some("/repo".into()),
        profile_id: "coding".into(),
        runtime_policy_stamp: Some(UiAutonomyRuntimePolicyStamp {
            profile_id: Some("coding".into()),
            sandbox: Some("workspace-write".into()),
            approval_policy: Some("on-request".into()),
            tool_policy_id: Some("coding-v1".into()),
            extra: BTreeMap::new(),
        }),
        artifact_count: 1,
        artifacts: vec![m15_agent_artifact()],
        created_at_ms: 1_778_870_000_000,
        updated_at_ms: 1_778_870_030_000,
    }
}

fn m15_agent_artifact() -> UiAgentArtifact {
    UiAgentArtifact {
        id: "api-report".into(),
        title: "API report".into(),
        kind: "markdown".into(),
        status: "ready".into(),
        path: None,
        content: None,
        extra: BTreeMap::new(),
    }
}

fn m15_goal_record() -> UiGoalRecord {
    UiGoalRecord {
        profile_id: Some("coding".into()),
        goal_id: "goal_01".into(),
        objective: "finish the review and tests".into(),
        status: "active".into(),
        token_budget: 50_000,
        tokens_used: 3_200,
        time_used_seconds: 180,
        created_at_ms: 1_778_870_000_000,
        updated_at_ms: 1_778_870_030_000,
    }
}

fn m15_loop_record(session_id: SessionKey) -> UiLoopRecord {
    UiLoopRecord {
        loop_id: "loop_01".into(),
        session_id,
        profile_id: Some("coding".into()),
        prompt: "check deploy".into(),
        mode: "self_paced".into(),
        interval_seconds: None,
        status: "active".into(),
        next_run_at_ms: Some(1_778_870_600_000),
        last_run_at_ms: Some(1_778_870_000_000),
        expires_at_ms: 1_779_474_800_000,
        created_at_ms: 1_778_870_000_000,
        updated_at_ms: 1_778_870_030_000,
    }
}

#[test]
fn m15_autonomy_notifications_register_methods_and_round_trip() {
    let session_id = SessionKey("coding:local:tui#coding".into());
    let loop_state = m15_loop_record(session_id.clone());
    let cases = vec![
        (
            UiNotification::AgentUpdated(AgentUpdatedEvent {
                session_id: session_id.clone(),
                agent: m15_agent_record(session_id.clone()),
            }),
            methods::AGENT_UPDATED,
        ),
        (
            UiNotification::AgentOutputDelta(AgentOutputDeltaEvent {
                session_id: session_id.clone(),
                agent_id: "reviewer-api".into(),
                cursor: OutputCursor { offset: 42 },
                text: "partial output\n".into(),
            }),
            methods::AGENT_OUTPUT_DELTA,
        ),
        (
            UiNotification::AgentArtifactUpdated(AgentArtifactUpdatedEvent {
                session_id: session_id.clone(),
                agent_id: "reviewer-api".into(),
                artifacts: vec![m15_agent_artifact()],
            }),
            methods::AGENT_ARTIFACT_UPDATED,
        ),
        (
            UiNotification::SessionGoalUpdated(SessionGoalUpdatedEvent {
                session_id: session_id.clone(),
                profile_id: Some("coding".into()),
                goal: m15_goal_record(),
                transition_actor: "user".into(),
            }),
            methods::SESSION_GOAL_UPDATED,
        ),
        (
            UiNotification::SessionGoalCleared(SessionGoalClearedEvent {
                session_id: session_id.clone(),
                profile_id: Some("coding".into()),
                cleared: true,
                goal: None,
                transition_actor: "user".into(),
            }),
            methods::SESSION_GOAL_CLEARED,
        ),
        (
            UiNotification::LoopUpdated(LoopUpdatedEvent {
                session_id: session_id.clone(),
                profile_id: Some("coding".into()),
                loop_id: Some("loop_01".into()),
                loop_state: loop_state.clone(),
                ok: Some(true),
                status: Some("active".into()),
                deleted: None,
            }),
            methods::LOOP_UPDATED,
        ),
        (
            UiNotification::LoopFired(LoopFiredEvent {
                session_id: session_id.clone(),
                profile_id: Some("coding".into()),
                loop_id: "loop_01".into(),
                loop_state: Some(loop_state.clone()),
                fire: Some(UiLoopFire {
                    queued: true,
                    duplicate: Some(false),
                    continuation_id: Some(7),
                    dedupe_key: Some("loop:loop_01".into()),
                    reason: Some("LoopFire".into()),
                    priority: Some(20),
                    message: None,
                    extra: BTreeMap::new(),
                }),
                ok: Some(true),
                status: Some("queued".into()),
            }),
            methods::LOOP_FIRED,
        ),
        (
            UiNotification::LoopCompleted(LoopCompletedEvent {
                session_id,
                profile_id: Some("coding".into()),
                loop_id: "loop_01".into(),
                loop_state: Some(loop_state),
                status: Some("completed".into()),
                completed_at_ms: Some(1_778_870_090_000),
                result: Some(json!({ "message": "iteration completed" })),
                error: None,
            }),
            methods::LOOP_COMPLETED,
        ),
    ];

    for (event, method) in cases {
        assert_eq!(event.method(), method);
        assert!(UI_PROTOCOL_NOTIFICATION_METHODS.contains(&method));

        let rpc = event
            .clone()
            .into_rpc_notification()
            .expect("serialize M15 notification");
        assert_eq!(rpc.method, method);
        let decoded = UiNotification::from_rpc_notification(rpc).expect("decode M15 notification");
        assert_eq!(decoded, event);
    }
}

#[test]
fn m15_agent_fixture_notifications_decode_to_typed_variants() {
    let session_id = SessionKey("coding:local:tui#coding".into());
    let agent = m15_agent_record(session_id.clone());
    let agent_wire = RpcNotification::new(
        methods::AGENT_UPDATED,
        json!({
            "session_id": session_id,
            "agent": agent,
        }),
    );
    let decoded = UiNotification::from_rpc_notification(agent_wire).expect("decode agent/updated");
    assert!(matches!(decoded, UiNotification::AgentUpdated(_)));

    let output_wire = RpcNotification::new(
        methods::AGENT_OUTPUT_DELTA,
        json!({
            "session_id": session_id,
            "agent_id": "reviewer-api",
            "cursor": { "offset": 23 },
            "text": "reviewer-api: finding\n"
        }),
    );
    let decoded =
        UiNotification::from_rpc_notification(output_wire).expect("decode agent/output/delta");
    assert!(matches!(decoded, UiNotification::AgentOutputDelta(_)));

    let artifact_wire = RpcNotification::new(
        methods::AGENT_ARTIFACT_UPDATED,
        json!({
            "session_id": session_id,
            "agent_id": "reviewer-api",
            "artifacts": [{
                "id": "api-report",
                "title": "API report",
                "kind": "markdown",
                "status": "ready"
            }]
        }),
    );
    let decoded = UiNotification::from_rpc_notification(artifact_wire)
        .expect("decode agent/artifact/updated");
    assert!(matches!(decoded, UiNotification::AgentArtifactUpdated(_)));
}

#[test]
fn resumable_notifications_carry_event_ledger_cursors() {
    let session_id = SessionKey("local:demo".into());
    let opened_cursor = UiCursor {
        stream: session_id.0.clone(),
        seq: 7,
    };
    let opened = UiNotification::SessionOpened(SessionOpened {
        session_id: session_id.clone(),
        active_profile_id: None,
        workspace_root: None,
        context: None,
        context_state: None,
        cursor: Some(opened_cursor.clone()),
        panes: None,
        capabilities: UiProtocolCapabilities::first_server_slice(),
        reasoning_effort: None,
    });

    let opened_wire = opened
        .clone()
        .into_rpc_notification()
        .expect("serialize session/open notification");
    assert_eq!(opened_wire.params["cursor"]["stream"], json!(session_id.0));
    assert_eq!(opened_wire.params["cursor"]["seq"], json!(7));
    assert_eq!(
        UiNotification::from_rpc_notification(opened_wire)
            .expect("decode session/open notification"),
        opened
    );

    let completed_cursor = UiCursor {
        stream: session_id.0.clone(),
        seq: 8,
    };
    let completed = UiNotification::TurnCompleted(TurnCompletedEvent {
        session_id,
        topic: None,
        turn_id: TurnId(Uuid::from_u128(9)),
        cursor: Some(completed_cursor),
        tokens_in: None,
        tokens_out: None,
        session_result: None,
    });
    let completed_wire = completed
        .clone()
        .into_rpc_notification()
        .expect("serialize turn/completed notification");
    assert_eq!(completed_wire.method, methods::TURN_COMPLETED);
    assert_eq!(completed_wire.params["cursor"]["seq"], json!(8));
    assert_eq!(
        UiNotification::from_rpc_notification(completed_wire)
            .expect("decode turn/completed notification"),
        completed
    );
}

#[test]
fn notification_round_trips_through_json() {
    let event = UiNotification::Warning(WarningEvent {
        session_id: SessionKey("local:demo".into()),
        turn_id: None,
        code: "mock_warning".into(),
        message: "mock payload".into(),
    });

    let json = serde_json::to_string(&event).expect("serialize event");
    let decoded: UiNotification = serde_json::from_str(&json).expect("deserialize event");

    assert_eq!(decoded, event);
}

#[test]
fn progress_updated_round_trip_minimal() {
    let event = UiNotification::ProgressUpdated(ProgressUpdatedEvent::new(
        SessionKey("local:demo".into()),
        None,
        UiProgressMetadata::new(progress_kinds::STATUS),
    ));

    let notification = event
        .clone()
        .into_rpc_notification()
        .expect("serialize progress/updated notification");
    assert_eq!(notification.method, methods::PROGRESS_UPDATED);

    let wire = serde_json::to_value(&notification).expect("serialize wire");
    assert_eq!(
        wire,
        json!({
            "jsonrpc": "2.0",
            "method": "progress/updated",
            "params": {
                "session_id": "local:demo",
                "metadata": { "kind": "status" }
            }
        })
    );

    let decoded_notification: RpcNotification<Value> =
        serde_json::from_value(wire).expect("deserialize wire");
    let decoded = UiNotification::from_rpc_notification(decoded_notification)
        .expect("decode progress/updated notification");
    assert_eq!(decoded, event);
}

// ----- M9-FIX-08 approval/cancelled wire registration -----

#[test]
fn approval_cancelled_notification_registers_method_and_round_trips() {
    let event = UiNotification::ApprovalCancelled(ApprovalCancelledEvent::turn_interrupted(
        SessionKey("local:demo".into()),
        ApprovalId::new(),
        TurnId::new(),
    ));
    assert_eq!(event.method(), methods::APPROVAL_CANCELLED);
    assert!(UI_PROTOCOL_NOTIFICATION_METHODS.contains(&methods::APPROVAL_CANCELLED));

    let rpc = event
        .clone()
        .into_rpc_notification()
        .expect("serialize approval/cancelled");
    let decoded =
        UiNotification::from_rpc_notification(rpc).expect("deserialize approval/cancelled");
    assert_eq!(decoded, event);
}

#[test]
fn progress_updated_round_trip_with_typed_fields() {
    let mut token_cost = UiTokenCostUpdate::new();
    token_cost.input_tokens = Some(120);
    token_cost.output_tokens = Some(45);
    token_cost.session_cost = Some(0.0035);
    token_cost.currency = Some("USD".into());

    let mut retry = UiRetryBackoff::new();
    retry.attempt = Some(2);
    retry.max_attempts = Some(5);
    retry.backoff_ms = Some(250);
    retry.reason = Some("rate_limited".into());

    let mut metadata = UiProgressMetadata::token_cost(token_cost);
    metadata.iteration = Some(2);
    metadata.retry = Some(retry);

    let turn_id = TurnId(Uuid::from_u128(7));
    let event = UiNotification::ProgressUpdated(ProgressUpdatedEvent::new(
        SessionKey("local:demo".into()),
        Some(turn_id.clone()),
        metadata,
    ));

    let wire = serde_json::to_value(
        event
            .clone()
            .into_rpc_notification()
            .expect("serialize progress/updated"),
    )
    .expect("serialize wire");
    assert_eq!(
        wire,
        json!({
            "jsonrpc": "2.0",
            "method": "progress/updated",
            "params": {
                "session_id": "local:demo",
                "turn_id": turn_id,
                "metadata": {
                    "kind": "token_cost_update",
                    "iteration": 2,
                    "retry": {
                        "attempt": 2,
                        "max_attempts": 5,
                        "backoff_ms": 250,
                        "reason": "rate_limited"
                    },
                    "token_cost": {
                        "input_tokens": 120,
                        "output_tokens": 45,
                        "session_cost": 0.0035,
                        "currency": "USD"
                    }
                }
            }
        })
    );

    let decoded_notification: RpcNotification<Value> =
        serde_json::from_value(wire).expect("deserialize wire");
    let decoded = UiNotification::from_rpc_notification(decoded_notification)
        .expect("decode progress/updated");
    assert_eq!(decoded, event);
}

/// The chat bubble footer renders `model · tokens_in / tokens_out · duration`
/// by reading `metadata.token_cost.model`. The wire shape must survive a
/// round trip so the WebSocket bridge can faithfully relay the model id
/// the agent emit layer attached.
#[test]
fn progress_updated_token_cost_round_trip_preserves_model() {
    let mut token_cost = UiTokenCostUpdate::new();
    token_cost.input_tokens = Some(80);
    token_cost.output_tokens = Some(20);
    token_cost.model = Some("deepseek-v4-pro".into());

    let metadata = UiProgressMetadata::token_cost(token_cost);
    let turn_id = TurnId(Uuid::from_u128(11));
    let event = UiNotification::ProgressUpdated(ProgressUpdatedEvent::new(
        SessionKey("local:demo".into()),
        Some(turn_id.clone()),
        metadata,
    ));

    let wire = serde_json::to_value(
        event
            .clone()
            .into_rpc_notification()
            .expect("serialize progress/updated"),
    )
    .expect("serialize wire");
    assert_eq!(
        wire["params"]["metadata"]["token_cost"]["model"],
        json!("deepseek-v4-pro"),
    );

    let decoded_notification: RpcNotification<Value> =
        serde_json::from_value(wire).expect("deserialize wire");
    let decoded = UiNotification::from_rpc_notification(decoded_notification)
        .expect("decode progress/updated");
    assert_eq!(decoded, event);
}

#[test]
fn approval_decision_unknown_falls_through() {
    let decoded: ApprovalDecision =
        serde_json::from_value(json!("future_decision_kind")).expect("decode unknown decision");
    assert_eq!(
        decoded,
        ApprovalDecision::Unknown("future_decision_kind".into())
    );

    let re_encoded = serde_json::to_value(&decoded).expect("encode unknown decision");
    assert_eq!(re_encoded, json!("future_decision_kind"));

    // Known wire values still hit the typed variants.
    let approve: ApprovalDecision =
        serde_json::from_value(json!("approve")).expect("decode approve");
    assert_eq!(approve, ApprovalDecision::Approve);
    assert_eq!(
        serde_json::to_value(&ApprovalDecision::Deny).expect("encode deny"),
        json!("deny")
    );
}

// ----- Spec §10 typed error taxonomy round-trips (M9-FIX-02) -----

/// Helper: serialize an `RpcError` and decode it back, asserting that
/// `code` survives the trip and `data` is preserved (or absent).
fn round_trip_rpc_error(err: &RpcError) -> RpcError {
    let value = serde_json::to_value(err).expect("serialize RpcError");
    serde_json::from_value(value).expect("deserialize RpcError")
}

#[test]
fn approval_not_pending_carries_recorded_decision() {
    let approve = RpcError::approval_not_pending(ApprovalDecision::Approve);
    let json = serde_json::to_value(&approve).expect("serialize approval_not_pending");
    assert_eq!(json["code"], json!(-32011));
    assert_eq!(json["data"]["recorded_decision"], json!("approve"));
    assert_eq!(
        round_trip_rpc_error(&approve).recorded_decision(),
        Some(ApprovalDecision::Approve),
    );

    let deny = RpcError::approval_not_pending(ApprovalDecision::Deny);
    assert_eq!(
        round_trip_rpc_error(&deny).recorded_decision(),
        Some(ApprovalDecision::Deny),
    );

    // Wrong code must not pretend to carry a recorded decision.
    let mislabeled = RpcError::new(rpc_error_codes::INTERNAL_ERROR, "x")
        .with_data(json!({ "recorded_decision": "approve" }));
    assert_eq!(mislabeled.recorded_decision(), None);
}

#[test]
fn cursor_out_of_range_round_trip() {
    let cursor = UiCursor {
        stream: "local:demo".into(),
        seq: 7,
    };
    let head = UiCursor {
        stream: "local:demo".into(),
        seq: 12,
    };
    let err = RpcError::cursor_out_of_range(&cursor, &head);
    assert_eq!(err.code, rpc_error_codes::CURSOR_OUT_OF_RANGE);
    let data = round_trip_rpc_error(&err).data.expect("carries data");
    assert_eq!(data["cursor"]["seq"], json!(7));
    assert_eq!(data["ledger_head"]["seq"], json!(12));
    assert_eq!(data["cursor"]["stream"], json!("local:demo"));
}

#[test]
fn decode_malformed_result_returns_malformed_result_not_invalid_params() {
    // Bad inbound result must surface MALFORMED_RESULT, never INVALID_PARAMS.
    let bad = json!({ "definitely_not": "a session_open result" });
    let err = UiRpcResult::from_method_and_result(methods::SESSION_OPEN, bad)
        .expect_err("malformed result should fail to decode");
    assert_eq!(err.code, rpc_error_codes::MALFORMED_RESULT);
    assert_ne!(err.code, rpc_error_codes::INVALID_PARAMS);
    assert!(err.message.contains(methods::SESSION_OPEN));
}

#[test]
fn unsupported_capability_result_round_trips() {
    // `from_method_and_result` must reconstruct UnsupportedCapability
    // even though the originating method is `approval/respond`.
    let result = UiRpcResult::UnsupportedCapability(UnsupportedCapabilityResult::method(
        methods::APPROVAL_RESPOND,
        "approval is pending",
    ));
    let value = result
        .clone()
        .into_result_value()
        .expect("serialize unsupported result");
    let decoded = UiRpcResult::from_method_and_result(methods::APPROVAL_RESPOND, value)
        .expect("decode unsupported result");
    assert_eq!(decoded, result);
    assert_eq!(decoded.kind(), UiResultKind::UnsupportedCapability);

    // Regular ApprovalRespond payload must still route to its typed variant.
    let regular = UiRpcResult::ApprovalRespond(ApprovalRespondResult::accepted(ApprovalId::new()))
        .into_result_value()
        .expect("serialize approval respond");
    let decoded_regular = UiRpcResult::from_method_and_result(methods::APPROVAL_RESPOND, regular)
        .expect("decode approval respond");
    assert_eq!(decoded_regular.kind(), UiResultKind::ApprovalRespond);
}

#[test]
fn unknown_id_constructors_round_trip_with_typed_data() {
    // One round-trip per `unknown_*` constant.
    let turn = TurnId(Uuid::from_u128(42));
    let approval = ApprovalId(Uuid::from_u128(7));
    let preview = PreviewId(Uuid::from_u128(11));
    let task = TaskId(Uuid::from_u128(99));
    let cases: [(RpcError, i64, &str, &str, Value); 5] = [
        (
            RpcError::unknown_session("local:demo"),
            -32100,
            "unknown_session",
            "session_id",
            json!("local:demo"),
        ),
        (
            RpcError::unknown_turn(&turn),
            -32101,
            "unknown_turn",
            "turn_id",
            json!(turn.0.to_string()),
        ),
        (
            RpcError::unknown_approval_id(&approval),
            -32102,
            "unknown_approval",
            "approval_id",
            json!(approval.0.to_string()),
        ),
        (
            RpcError::unknown_preview_id(&preview),
            -32103,
            "unknown_preview",
            "preview_id",
            json!(preview.0.to_string()),
        ),
        (
            RpcError::unknown_task_id(&task),
            -32104,
            "unknown_task",
            "task_id",
            json!(task.to_string()),
        ),
    ];
    for (err, code, kind, key, value) in cases {
        assert_eq!(err.code, code);
        let decoded = round_trip_rpc_error(&err);
        assert_eq!(decoded.code, code);
        let data = decoded.data.unwrap();
        assert_eq!(data["kind"], json!(kind));
        assert_eq!(data[key], value);
    }
}

#[test]
fn application_error_constructors_round_trip() {
    // One round-trip per remaining application-level constant.
    let cursor_invalid = RpcError::cursor_invalid("malformed cursor");
    assert_eq!(cursor_invalid.code, rpc_error_codes::CURSOR_INVALID);
    assert_eq!(cursor_invalid.code, -32111);
    assert_eq!(round_trip_rpc_error(&cursor_invalid), cursor_invalid);

    let permission = RpcError::permission_denied("sandbox: outside workspace");
    assert_eq!(permission.code, rpc_error_codes::PERMISSION_DENIED);
    assert_eq!(permission.code, -32120);
    assert_eq!(
        round_trip_rpc_error(&permission).message,
        permission.message
    );

    let unsupported = RpcError::unsupported_capability(methods::DIFF_PREVIEW_GET, "flag disabled");
    assert_eq!(unsupported.code, rpc_error_codes::UNSUPPORTED_CAPABILITY);
    assert_eq!(unsupported.code, -32130);
    let unsupported_decoded = round_trip_rpc_error(&unsupported);
    let report: UnsupportedCapabilityReport =
        serde_json::from_value(unsupported_decoded.data.unwrap()).expect("typed report decodes");
    assert_eq!(report.method, methods::DIFF_PREVIEW_GET);
    assert_eq!(report.reason, "flag disabled");

    let not_ready = RpcError::runtime_not_ready("initializing");
    assert_eq!(not_ready.code, rpc_error_codes::RUNTIME_NOT_READY);
    assert_eq!(not_ready.code, -32140);
    assert_eq!(round_trip_rpc_error(&not_ready).message, "initializing");

    let malformed = RpcError::malformed_result("invalid result for foo");
    assert_eq!(malformed.code, rpc_error_codes::MALFORMED_RESULT);
    assert_eq!(malformed.code, -32150);
    assert_eq!(round_trip_rpc_error(&malformed), malformed);

    let plain = RpcError::rate_limited("too many turns", None);
    assert_eq!(plain.code, rpc_error_codes::RATE_LIMITED);
    assert_eq!(plain.code, -32160);
    assert!(round_trip_rpc_error(&plain).data.is_none());

    let hinted = RpcError::rate_limited("too many turns", Some(2_500));
    assert_eq!(
        round_trip_rpc_error(&hinted).data.unwrap()["retry_after_ms"],
        json!(2_500)
    );
}

#[test]
fn closed_string_enums_capture_unknown_wire_values() {
    // ApprovalRespondStatus
    let status: ApprovalRespondStatus =
        serde_json::from_value(json!("queued_for_review")).expect("decode status");
    assert_eq!(
        status,
        ApprovalRespondStatus::Unknown("queued_for_review".into())
    );
    assert_eq!(
        serde_json::to_value(&status).expect("encode status"),
        json!("queued_for_review")
    );
    assert_eq!(
        serde_json::to_value(&ApprovalRespondStatus::Accepted).expect("encode accepted"),
        json!("accepted")
    );

    // DiffPreviewFileStatus
    let file_status: DiffPreviewFileStatus =
        serde_json::from_value(json!("type_changed")).expect("decode file status");
    assert_eq!(
        file_status,
        DiffPreviewFileStatus::Unknown("type_changed".into())
    );
    assert_eq!(
        serde_json::to_value(&file_status).expect("encode file status"),
        json!("type_changed")
    );
    assert_eq!(
        serde_json::to_value(&DiffPreviewFileStatus::Renamed).expect("encode renamed"),
        json!("renamed")
    );
}

#[test]
fn input_item_unknown_kind_falls_through() {
    // Tagged input items with future kinds decode to the Unknown unit
    // variant rather than erroring. Known kinds still decode normally.
    let unknown: InputItem = serde_json::from_value(json!({
        "kind": "voice",
        "audio_url": "https://example.test/clip.wav"
    }))
    .expect("decode unknown input item kind");
    assert_eq!(unknown, InputItem::Unknown);

    let known: InputItem = serde_json::from_value(json!({
        "kind": "text",
        "text": "hello"
    }))
    .expect("decode text input item");
    assert_eq!(
        known,
        InputItem::Text {
            text: "hello".into()
        }
    );
}

#[test]
fn rpc_error_codes_partition_is_disjoint() {
    // Application-layer codes must live in -32100..=-32199; the
    // spec-pinned APPROVAL_NOT_PENDING is the documented exception.
    for code in [
        rpc_error_codes::UNKNOWN_SESSION,
        rpc_error_codes::UNKNOWN_TURN,
        rpc_error_codes::UNKNOWN_APPROVAL_ID,
        rpc_error_codes::UNKNOWN_PREVIEW_ID,
        rpc_error_codes::UNKNOWN_TASK_ID,
        rpc_error_codes::APPROVAL_CANCELLED,
        rpc_error_codes::CURSOR_OUT_OF_RANGE,
        rpc_error_codes::CURSOR_INVALID,
        rpc_error_codes::PERMISSION_DENIED,
        rpc_error_codes::UNSUPPORTED_CAPABILITY,
        rpc_error_codes::RUNTIME_NOT_READY,
        rpc_error_codes::MALFORMED_RESULT,
        rpc_error_codes::RATE_LIMITED,
    ] {
        assert!(
            (-32199..=-32100).contains(&code),
            "{code} outside -32100..=-32199",
        );
    }
    assert_eq!(rpc_error_codes::APPROVAL_NOT_PENDING, -32011);
    assert_eq!(rpc_error_codes::APPROVAL_CANCELLED, -32105);
}

#[test]
fn approval_decided_notification_round_trips_through_wire() {
    let session_id = SessionKey("local:demo".into());
    let approval_id = ApprovalId(Uuid::from_u128(0xa11));
    let turn_id = TurnId(Uuid::from_u128(0xb22));
    let decided_at: DateTime<Utc> = DateTime::parse_from_rfc3339("2026-04-28T12:00:00Z")
        .expect("rfc3339 timestamp")
        .with_timezone(&Utc);
    let event = UiNotification::ApprovalDecided(ApprovalDecidedEvent {
        session_id: session_id.clone(),
        topic: None,
        approval_id: approval_id.clone(),
        turn_id: turn_id.clone(),
        decision: ApprovalDecision::Approve,
        scope: Some(approval_scopes::SESSION.into()),
        decided_at,
        decided_by: "user:abc".into(),
        auto_resolved: true,
        policy_id: Some("policy-1".into()),
        client_note: Some("looks good".into()),
    });

    let wire = event
        .clone()
        .into_rpc_notification()
        .expect("serialize approval/decided");
    assert_eq!(wire.method, methods::APPROVAL_DECIDED);
    assert_eq!(
        wire.params["approval_id"],
        serde_json::to_value(&approval_id).unwrap()
    );
    assert_eq!(wire.params["decision"], json!("approve"));
    assert_eq!(wire.params["auto_resolved"], json!(true));
    assert_eq!(wire.params["policy_id"], json!("policy-1"));

    let decoded = UiNotification::from_rpc_notification(wire).expect("decode approval/decided");
    assert_eq!(decoded, event);

    let body = serde_json::to_string(&event).expect("serialize event");
    let again: UiNotification = serde_json::from_str(&body).expect("deserialize event");
    assert_eq!(again, event);
}

#[test]
fn first_server_capabilities_advertise_approval_cancelled() {
    let capabilities = UiProtocolCapabilities::first_server_slice();
    assert!(
        capabilities
            .supported_notifications
            .iter()
            .any(|method| method == methods::APPROVAL_CANCELLED),
        "approval/cancelled must be advertised so clients can render it",
    );
}

// ----- M9 review fix MEDIUM #4 (UPCR-2026-004): Cancelled task state -----

#[test]
fn task_runtime_state_cancelled_round_trips_as_snake_case_cancelled() {
    // Wire form must be exactly `"cancelled"` so the agent's
    // `TaskLifecycleState::Cancelled` (also `snake_case`-serialized as
    // `"cancelled"`) flows through the protocol mapper without falling
    // back to `Running`. UPCR-2026-004 promises `"cancelled"` (the British
    // spelling) as the wire literal.
    let value = serde_json::to_value(TaskRuntimeState::Cancelled).expect("serialize Cancelled");
    assert_eq!(value, json!("cancelled"));
    let parsed: TaskRuntimeState = serde_json::from_value(value).expect("round-trip Cancelled");
    assert_eq!(parsed, TaskRuntimeState::Cancelled);
}

#[test]
fn task_updated_event_round_trips_with_cancelled_state() {
    let event = UiNotification::TaskUpdated(TaskUpdatedEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        task_id: TaskId(Uuid::from_u128(7)),
        tool_call_id: None,
        title: "spawn_only_runner".into(),
        state: TaskRuntimeState::Cancelled,
        runtime_detail: Some("user cancelled".into()),
        source: None,
        role: None,
        summary: None,
        artifact_count: None,
        runtime_policy_stamp: None,
        turn_id: None,
    });
    let rpc = event
        .clone()
        .into_rpc_notification()
        .expect("serialize task/updated cancelled");
    let decoded =
        UiNotification::from_rpc_notification(rpc).expect("deserialize task/updated cancelled");
    assert_eq!(decoded, event);
}

#[test]
fn plan_updated_event_round_trips_and_advertises() {
    let event = UiNotification::PlanUpdated(PlanUpdatedEvent {
        // No topic in the key → `stamp_topic_from_session` is a no-op, so the
        // absent-`topic` assertion below holds and the round-trip is exact.
        session_id: SessionKey("acct:web:tui".into()),
        topic: None,
        turn_id: None,
        plan: UiPlanRecord {
            items: vec![
                UiPlanItem {
                    id: "1".into(),
                    title: "web P3: PWA manifest + bridge hygiene".into(),
                    status: PlanItemStatus::Completed,
                    priority: Some("P3".into()),
                },
                UiPlanItem {
                    id: "2".into(),
                    title: "memory panel (octos endpoints + web UI)".into(),
                    status: PlanItemStatus::InProgress,
                    priority: None,
                },
            ],
            title: Some("Building memory panel…".into()),
            updated_at_ms: 1_700_000_000_000,
        },
    });
    assert_eq!(event.method(), methods::PLAN_UPDATED);

    let rpc = event
        .clone()
        .into_rpc_notification()
        .expect("serialize plan/updated");
    // Status is snake_case on the wire; an absent `priority`/`topic` stays
    // absent (no `null` leakage that would clobber a cached value).
    assert_eq!(rpc.params["plan"]["items"][1]["status"], "in_progress");
    assert!(rpc.params["plan"]["items"][1].get("priority").is_none());
    assert!(rpc.params.get("topic").is_none());

    let decoded = UiNotification::from_rpc_notification(rpc).expect("deserialize plan/updated");
    assert_eq!(decoded, event);

    // Advertised in both the notification and feature registries.
    assert!(UI_PROTOCOL_NOTIFICATION_METHODS.contains(&methods::PLAN_UPDATED));
    assert!(UI_PROTOCOL_KNOWN_FEATURES.contains(&UI_PROTOCOL_FEATURE_PLAN_TODOS_V1));
}

/// #1123 codex P2 follow-up to #1113: pin that the new M13-B
/// projection fields (source / role / summary / artifact_count /
/// runtime_policy_stamp) round-trip through serde on
/// `TaskUpdatedEvent` AND that absent fields stay absent on the
/// wire (no `null` leakage that would clobber a prior value cached
/// by a stale subscriber).
#[test]
fn task_updated_event_round_trips_m13b_projection_fields() {
    // Populated path: every projection field set, all five must
    // appear on the wire and decode back unchanged.
    let event = TaskUpdatedEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        task_id: TaskId(Uuid::from_u128(0xBEEF)),
        tool_call_id: Some("call-r".into()),
        title: "review".into(),
        state: TaskRuntimeState::Running,
        runtime_detail: None,
        source: Some("model".into()),
        role: Some("reviewer".into()),
        summary: Some("found 1 issue".into()),
        artifact_count: Some(2),
        runtime_policy_stamp: Some(json!({ "approval_policy": "on-request" })),
        // C1 step 4: turn_id round-trips alongside the projection fields.
        turn_id: Some(TurnId(Uuid::from_u128(0xCAFE))),
    };
    let value = serde_json::to_value(&event).expect("serialize task/updated");
    assert_eq!(value.get("source"), Some(&json!("model")));
    assert_eq!(value.get("role"), Some(&json!("reviewer")));
    assert_eq!(value.get("summary"), Some(&json!("found 1 issue")));
    assert_eq!(value.get("artifact_count"), Some(&json!(2)));
    assert_eq!(
        value.get("runtime_policy_stamp"),
        Some(&json!({ "approval_policy": "on-request" })),
    );
    assert_eq!(
        value.get("turn_id"),
        Some(&json!(Uuid::from_u128(0xCAFE).to_string())),
        "turn_id must appear on the wire when set",
    );
    let parsed: TaskUpdatedEvent = serde_json::from_value(value).expect("deserialize task/updated");
    assert_eq!(parsed, event);

    // Absent path: no field set, no field on the wire. A legacy
    // payload that pre-dates the fields still parses thanks to
    // `#[serde(default)]`.
    let bare = TaskUpdatedEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        task_id: TaskId(Uuid::from_u128(0xBEE0)),
        tool_call_id: None,
        title: "review".into(),
        state: TaskRuntimeState::Running,
        runtime_detail: None,
        source: None,
        role: None,
        summary: None,
        artifact_count: None,
        runtime_policy_stamp: None,
        turn_id: None,
    };
    let bare_value = serde_json::to_value(&bare).expect("serialize bare task/updated");
    assert!(bare_value.get("source").is_none(), "absent source omits");
    assert!(bare_value.get("role").is_none(), "absent role omits");
    assert!(bare_value.get("summary").is_none(), "absent summary omits");
    assert!(
        bare_value.get("artifact_count").is_none(),
        "absent artifact_count omits",
    );
    assert!(
        bare_value.get("runtime_policy_stamp").is_none(),
        "absent runtime_policy_stamp omits",
    );
    assert!(
        bare_value.get("turn_id").is_none(),
        "absent turn_id omits (C1 step 4)",
    );
    let legacy_json = json!({
        "session_id": "local:demo",
        "task_id": TaskId(Uuid::from_u128(0xBEE0)),
        "title": "review",
        "state": "running",
    });
    let parsed_legacy: TaskUpdatedEvent =
        serde_json::from_value(legacy_json).expect("deserialize legacy bare");
    assert_eq!(parsed_legacy.source, None);
    assert_eq!(parsed_legacy.role, None);
    assert_eq!(parsed_legacy.summary, None);
    assert_eq!(parsed_legacy.artifact_count, None);
    assert_eq!(parsed_legacy.runtime_policy_stamp, None);
    assert_eq!(parsed_legacy.turn_id, None);
}

// ===== UPCR-2026-009 / -010 / -011 / -012 golden tests (PR G) =====

fn sample_session_id() -> SessionKey {
    SessionKey("local:demo".into())
}

fn sample_turn_id() -> TurnId {
    TurnId(Uuid::from_u128(0x10))
}

fn sample_cursor() -> UiCursor {
    UiCursor {
        stream: "local:demo".into(),
        seq: 142,
    }
}

fn sample_persisted_at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-04-30T12:00:00Z")
        .expect("rfc3339 parse")
        .with_timezone(&Utc)
}

fn sample_user_question_requested_event() -> UserQuestionRequestedEvent {
    UserQuestionRequestedEvent::new(
        sample_session_id(),
        QuestionId(Uuid::from_u128(0x77)),
        sample_turn_id(),
        "Pick a framework",
        "Which framework should I scaffold?",
        vec![UserQuestion {
            header: "Framework".into(),
            question: "Which framework?".into(),
            options: vec![
                UserQuestionOption {
                    label: "axum".into(),
                    description: "tower-based".into(),
                },
                UserQuestionOption {
                    label: "actix".into(),
                    description: "actor-based".into(),
                },
            ],
            multi_select: false,
            allow_free_text: true,
        }],
    )
}

#[test]
fn golden_session_hydrate_params_serde() {
    let params = SessionHydrateParams {
        session_id: sample_session_id(),
        after: Some(UiCursor {
            stream: "local:demo".into(),
            seq: 0,
        }),
        include: vec!["messages".into(), "threads".into()],
    };
    let value = serde_json::to_value(&params).expect("serialize hydrate params");
    assert_eq!(
        value,
        json!({
            "session_id": "local:demo",
            "after": { "stream": "local:demo", "seq": 0 },
            "include": ["messages", "threads"],
        })
    );
    let parsed: SessionHydrateParams =
        serde_json::from_value(value).expect("deserialize hydrate params");
    assert_eq!(parsed, params);
}

#[test]
fn golden_session_hydrate_result_serde() {
    let result = SessionHydrateResult {
        session_id: sample_session_id(),
        cursor: sample_cursor(),
        context: None,
        context_state: None,
        messages: Some(vec![HydratedMessage {
            seq: 17,
            role: "user".into(),
            content: "hello".into(),
            turn_id: Some(sample_turn_id()),
            thread_id: Some("thread-1".into()),
            client_message_id: Some("cmid-1".into()),
            persisted_at: sample_persisted_at(),
            message_id: Some("local:demo:17:1700000000000000000".into()),
            source: Some("user".into()),
            media: vec![],
            reasoning_content: None,
        }]),
        threads: Some(vec![ThreadGraphEntry {
            thread_id: "thread-1".into(),
            root_seq: 17,
            root_client_message_id: Some("cmid-1".into()),
            turn_id: Some(sample_turn_id()),
            message_seqs: vec![17, 18],
            status: thread_status::COMPLETED.into(),
        }]),
        turns: Some(vec![HydratedTurn {
            turn_id: sample_turn_id(),
            state: TurnLifecycleState::Completed,
            started_at: Some(sample_persisted_at()),
            completed_at: Some(sample_persisted_at()),
            thread_id: Some("thread-1".into()),
        }]),
        pending_approvals: Some(vec![]),
        pending_questions: Some(vec![sample_user_question_requested_event()]),
        replayed_envelopes: Some(vec![]),
        replayed_tool_envelopes: Some(vec![]),
    };
    let value = serde_json::to_value(&result).expect("serialize hydrate result");
    let parsed: SessionHydrateResult =
        serde_json::from_value(value).expect("deserialize hydrate result");
    assert_eq!(parsed, result);

    // Sections excluded from `include` are omitted (NOT `null`) per UPCR.
    let messages_only = SessionHydrateResult {
        session_id: sample_session_id(),
        cursor: sample_cursor(),
        context: None,
        context_state: None,
        messages: Some(vec![]),
        threads: None,
        turns: None,
        pending_approvals: None,
        pending_questions: None,
        replayed_envelopes: None,
        replayed_tool_envelopes: None,
    };
    let value = serde_json::to_value(&messages_only).expect("serialize messages-only");
    let object = value.as_object().expect("hydrate result is object");
    assert!(object.contains_key("messages"));
    assert!(!object.contains_key("threads"));
    assert!(!object.contains_key("turns"));
    assert!(!object.contains_key("pending_approvals"));
    // UPCR-2026-023: a client that did not request pending questions never
    // sees the new field — it is omitted, never serialized as `null`.
    assert!(!object.contains_key("pending_questions"));
    // Bug C: a non-negotiated client never sees the new field.
    assert!(!object.contains_key("replayed_envelopes"));
    assert!(!object.contains_key("replayed_tool_envelopes"));
}

#[test]
fn golden_session_rollback_params_serde() {
    let params = SessionRollbackParams {
        session_id: sample_session_id(),
        num_turns: 2,
    };
    let value = serde_json::to_value(&params).expect("serialize rollback params");
    assert_eq!(value, json!({ "session_id": "local:demo", "num_turns": 2 }));
    let parsed: SessionRollbackParams =
        serde_json::from_value(value).expect("deserialize rollback params");
    assert_eq!(parsed, params);
}

#[test]
fn session_rollback_command_and_result_round_trip() {
    // Command decodes from its wire method name.
    let command = UiCommand::SessionRollback(SessionRollbackParams {
        session_id: sample_session_id(),
        num_turns: 1,
    });
    assert_eq!(command.method(), methods::SESSION_ROLLBACK);
    let request = command.clone().into_rpc_request("r1").expect("encode");
    assert_eq!(request.method, methods::SESSION_ROLLBACK);
    let decoded = UiCommand::from_rpc_request(request).expect("decode command");
    assert_eq!(decoded, command);

    // Result carries the trimmed hydrate projection and round-trips through
    // the method-keyed decode path.
    let result = SessionRollbackResult {
        dropped_turns: 1,
        thread: SessionHydrateResult {
            session_id: sample_session_id(),
            cursor: sample_cursor(),
            context: None,
            context_state: None,
            messages: Some(vec![]),
            threads: None,
            turns: None,
            pending_approvals: None,
            pending_questions: None,
            replayed_envelopes: None,
            replayed_tool_envelopes: None,
        },
    };
    let wire = UiRpcResult::SessionRollback(result.clone());
    assert_eq!(wire.method(), Some(methods::SESSION_ROLLBACK));
    assert_eq!(wire.kind(), UiResultKind::SessionRollback);
    let value = wire.into_result_value().expect("encode result");
    let decoded =
        UiRpcResult::from_method_and_result(methods::SESSION_ROLLBACK, value).expect("decode");
    assert_eq!(decoded, UiRpcResult::SessionRollback(result));
    assert_eq!(
        first_server_result_kind_for_method(methods::SESSION_ROLLBACK),
        Some(UiResultKind::SessionRollback)
    );
}

#[test]
fn golden_thread_graph_get_params_serde() {
    let params = ThreadGraphGetParams {
        session_id: sample_session_id(),
        at: Some(sample_cursor()),
    };
    let value = serde_json::to_value(&params).expect("serialize");
    assert_eq!(
        value,
        json!({
            "session_id": "local:demo",
            "at": { "stream": "local:demo", "seq": 142 },
        })
    );
    let parsed: ThreadGraphGetParams = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, params);
}

#[test]
fn golden_thread_graph_get_result_serde() {
    let result = ThreadGraphGetResult {
        session_id: sample_session_id(),
        cursor: sample_cursor(),
        threads: vec![ThreadGraphEntry {
            thread_id: "thread-1".into(),
            root_seq: 17,
            root_client_message_id: Some("cmid-1".into()),
            turn_id: Some(sample_turn_id()),
            message_seqs: vec![17, 18, 19],
            status: thread_status::COMPLETED.into(),
        }],
        orphans: vec![42],
    };
    let value = serde_json::to_value(&result).expect("serialize");
    let parsed: ThreadGraphGetResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, result);
}

#[test]
fn golden_turn_state_get_params_serde() {
    let params = TurnStateGetParams {
        session_id: sample_session_id(),
        turn_id: sample_turn_id(),
    };
    let value = serde_json::to_value(&params).expect("serialize");
    let parsed: TurnStateGetParams = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, params);
}

#[test]
fn golden_turn_state_get_result_serde() {
    let result = TurnStateGetResult {
        session_id: sample_session_id(),
        turn_id: sample_turn_id(),
        state: TurnLifecycleState::Active,
        context: None,
        context_state: None,
        started_at: Some(sample_persisted_at()),
        completed_at: None,
        thread_id: Some("thread-1".into()),
        committed_seqs: vec![17, 18, 19],
    };
    let value = serde_json::to_value(&result).expect("serialize");
    // `state` is snake_case wire form.
    assert_eq!(value.get("state"), Some(&json!("active")));
    let parsed: TurnStateGetResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, result);

    // All five lifecycle states round-trip.
    for state in [
        TurnLifecycleState::Active,
        TurnLifecycleState::Interrupting,
        TurnLifecycleState::Completed,
        TurnLifecycleState::Errored,
        TurnLifecycleState::Interrupted,
        TurnLifecycleState::Unknown,
    ] {
        let r = TurnStateGetResult {
            session_id: sample_session_id(),
            turn_id: sample_turn_id(),
            state,
            context: None,
            context_state: None,
            started_at: None,
            completed_at: None,
            thread_id: None,
            committed_seqs: vec![],
        };
        let v = serde_json::to_value(&r).expect("serialize state");
        let p: TurnStateGetResult = serde_json::from_value(v).expect("deserialize state");
        assert_eq!(p.state, state);
    }
}

#[test]
fn golden_turn_spawn_complete_event_serde() {
    let event = TurnSpawnCompleteEvent {
        session_id: sample_session_id(),
        topic: None,
        turn_id: Some(sample_turn_id()),
        thread_id: Some("thread-1".into()),
        task_id: "task_abc123".into(),
        tool_call_id: Some("call_abc123".into()),
        response_to_client_message_id: Some("cmid-user-1".into()),
        seq: 42,
        message_id: "msg-spawn-1".into(),
        source: "background".into(),
        cursor: UiCursor {
            stream: "local:demo".into(),
            seq: 42,
        },
        persisted_at: sample_persisted_at(),
        content: "Research complete: 3 sources reviewed.".into(),
        media: vec!["research/_report.md".into()],
    };
    let value = serde_json::to_value(&event).expect("serialize");
    // Wire shape matches the spec: required fields land on the
    // top-level object with snake_case keys; absent optional fields
    // omit cleanly.
    assert_eq!(value.get("task_id"), Some(&json!("task_abc123")));
    assert_eq!(value.get("tool_call_id"), Some(&json!("call_abc123")));
    assert_eq!(
        value.get("response_to_client_message_id"),
        Some(&json!("cmid-user-1")),
    );
    assert_eq!(
        value.get("content"),
        Some(&json!("Research complete: 3 sources reviewed.")),
    );
    assert_eq!(value.get("source"), Some(&json!("background")));
    let parsed: TurnSpawnCompleteEvent = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, event);

    // Empty media and absent optionals omit on the wire.
    let bare = TurnSpawnCompleteEvent {
        session_id: sample_session_id(),
        topic: None,
        turn_id: None,
        thread_id: None,
        task_id: "task_zzz".into(),
        tool_call_id: None,
        response_to_client_message_id: None,
        seq: 1,
        message_id: "msg-bare".into(),
        source: "background".into(),
        cursor: UiCursor {
            stream: "local:demo".into(),
            seq: 1,
        },
        persisted_at: sample_persisted_at(),
        content: String::new(),
        media: vec![],
    };
    let bare_v = serde_json::to_value(&bare).expect("serialize bare");
    assert!(bare_v.get("turn_id").is_none(), "absent turn_id omits");
    assert!(bare_v.get("thread_id").is_none(), "absent thread_id omits");
    assert!(
        bare_v.get("tool_call_id").is_none(),
        "absent tool_call_id omits",
    );
    assert!(
        bare_v.get("response_to_client_message_id").is_none(),
        "absent response_to_client_message_id omits",
    );
    assert!(bare_v.get("media").is_none(), "empty media omits");
    let bare_p: TurnSpawnCompleteEvent = serde_json::from_value(bare_v).expect("deserialize bare");
    assert_eq!(bare_p, bare);

    // Wire-level: round-trip via the JSON-RPC notification envelope.
    let notif = UiNotification::TurnSpawnComplete(event.clone());
    let rpc = notif
        .clone()
        .into_rpc_notification()
        .expect("notification serialize");
    assert_eq!(rpc.method, methods::TURN_SPAWN_COMPLETE);
    let decoded = UiNotification::from_rpc_notification(rpc).expect("notification deserialize");
    assert_eq!(decoded, notif);
}

/// Asserts the new `tool_call_id` field round-trips through serde on
/// both `task/updated` and `turn/spawn_complete`. The chip-flip
/// race in the client browser hinged on having this field on the
/// wire, so a regression here would silently re-introduce the bug
/// even if the constructors still type-check.
#[test]
fn task_updated_and_spawn_complete_events_round_trip_tool_call_id() {
    // `TaskUpdatedEvent` with `tool_call_id` set.
    let task_event = TaskUpdatedEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        task_id: TaskId(Uuid::from_u128(0xDEADBEEF)),
        tool_call_id: Some("call_podcast_generate_42".into()),
        title: "podcast_generate".into(),
        state: TaskRuntimeState::Completed,
        runtime_detail: Some("rendered output.mp3".into()),
        source: None,
        role: None,
        summary: None,
        artifact_count: None,
        runtime_policy_stamp: None,
        turn_id: None,
    };
    let task_value = serde_json::to_value(&task_event).expect("serialize task_updated");
    assert_eq!(
        task_value.get("tool_call_id"),
        Some(&json!("call_podcast_generate_42")),
        "tool_call_id must appear on the wire",
    );
    let parsed_task: TaskUpdatedEvent =
        serde_json::from_value(task_value).expect("deserialize task_updated");
    assert_eq!(parsed_task, task_event);

    // `TaskUpdatedEvent` with `tool_call_id == None` omits on the wire
    // (legacy daemons / synthetic paths).
    let task_legacy = TaskUpdatedEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        task_id: TaskId(Uuid::from_u128(1)),
        tool_call_id: None,
        title: "legacy".into(),
        state: TaskRuntimeState::Running,
        runtime_detail: None,
        source: None,
        role: None,
        summary: None,
        artifact_count: None,
        runtime_policy_stamp: None,
        turn_id: None,
    };
    let legacy_value = serde_json::to_value(&task_legacy).expect("serialize legacy");
    assert!(
        legacy_value.get("tool_call_id").is_none(),
        "absent tool_call_id must omit so legacy daemons parse",
    );
    // Defensive: a JSON payload from an even-older daemon that lacks
    // the field entirely still parses via `#[serde(default)]`.
    let bare_legacy_json = json!({
        "session_id": "local:demo",
        "task_id": TaskId(Uuid::from_u128(1)),
        "title": "legacy",
        "state": "running",
    });
    let parsed_legacy: TaskUpdatedEvent =
        serde_json::from_value(bare_legacy_json).expect("deserialize legacy bare");
    assert_eq!(parsed_legacy.tool_call_id, None);

    // `TurnSpawnCompleteEvent` with `tool_call_id` set.
    let spawn_event = TurnSpawnCompleteEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        turn_id: None,
        thread_id: None,
        task_id: "task_podcast_42".into(),
        tool_call_id: Some("call_podcast_generate_42".into()),
        response_to_client_message_id: None,
        seq: 7,
        message_id: "msg-spawn-7".into(),
        source: "background".into(),
        cursor: UiCursor {
            stream: "local:demo".into(),
            seq: 7,
        },
        persisted_at: sample_persisted_at(),
        content: "🎙 podcast delivered".into(),
        media: vec!["output.mp3".into()],
    };
    let spawn_value = serde_json::to_value(&spawn_event).expect("serialize spawn_complete");
    assert_eq!(
        spawn_value.get("tool_call_id"),
        Some(&json!("call_podcast_generate_42")),
        "tool_call_id must appear on the wire",
    );
    let parsed_spawn: TurnSpawnCompleteEvent =
        serde_json::from_value(spawn_value).expect("deserialize spawn_complete");
    assert_eq!(parsed_spawn, spawn_event);

    // Defensive: a `turn/spawn_complete` payload from an older daemon
    // that lacks the field entirely still parses via
    // `#[serde(default)]`.
    let bare_spawn_json = json!({
        "session_id": "local:demo",
        "task_id": "task_legacy",
        "seq": 1,
        "message_id": "msg-legacy",
        "source": "background",
        "cursor": { "stream": "local:demo", "seq": 1 },
        "persisted_at": "2026-01-01T00:00:00Z",
        "content": "done",
    });
    let parsed_legacy_spawn: TurnSpawnCompleteEvent =
        serde_json::from_value(bare_spawn_json).expect("deserialize legacy spawn bare");
    assert_eq!(parsed_legacy_spawn.tool_call_id, None);
}

#[test]
fn golden_capabilities_advertise_spawn_complete_v1() {
    // No header at all -> server falls back to `first_server_slice` and
    // advertises every known feature including `event.spawn_complete.v1`.
    let full = UiProtocolCapabilities::first_server_slice();
    assert!(full.supports_feature(UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1));

    // `full_protocol()` advertises the same.
    let full_proto = UiProtocolCapabilities::full_protocol();
    assert!(full_proto.supports_feature(UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1));

    // Negotiated subset: only `event.spawn_complete.v1` requested.
    let only_spawn =
        UiProtocolCapabilities::for_negotiated_features([UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1]);
    assert!(only_spawn.supports_feature(UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1));

    // The notification method is advertised regardless of negotiated
    // gating today; per-connection
    // emit-time filtering is what enforces the capability.
    assert!(
        UI_PROTOCOL_NOTIFICATION_METHODS.contains(&methods::TURN_SPAWN_COMPLETE),
        "turn/spawn_complete is in the notification method registry",
    );
}

#[test]
fn golden_capabilities_includes_new_features_when_negotiated() {
    // No header at all -> server falls back to `first_server_slice` and
    // advertises every known feature including the four new ones.
    let full = UiProtocolCapabilities::first_server_slice();
    assert!(full.supports_feature(UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1));
    assert!(full.supports_feature(UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1));
    assert!(full.supports_feature(UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1));
    // And the gated methods are visible.
    assert!(full.supports_method(methods::SESSION_HYDRATE));
    assert!(full.supports_method(methods::THREAD_GRAPH_GET));
    assert!(full.supports_method(methods::TURN_STATE_GET));

    // Negotiated subset: only `state.thread_graph.v1` requested.
    let only_thread_graph =
        UiProtocolCapabilities::for_negotiated_features([UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1]);
    assert!(only_thread_graph.supports_feature(UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1));
    assert!(only_thread_graph.supports_method(methods::THREAD_GRAPH_GET));
    assert!(
        !only_thread_graph.supports_feature(UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1),
        "non-requested feature must NOT be advertised"
    );
    assert!(
        !only_thread_graph.supports_method(methods::SESSION_HYDRATE),
        "non-requested method must NOT be advertised"
    );
    assert!(!only_thread_graph.supports_method(methods::TURN_STATE_GET));

    // Negotiated subset: all state-query capabilities requested.
    let all_new = UiProtocolCapabilities::for_negotiated_features([
        UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1,
        UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1,
        UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1,
    ]);
    assert!(all_new.supports_feature(UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1));
    assert!(all_new.supports_feature(UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1));
    assert!(all_new.supports_feature(UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1));
    assert!(all_new.supports_method(methods::SESSION_HYDRATE));
    assert!(all_new.supports_method(methods::THREAD_GRAPH_GET));
    assert!(all_new.supports_method(methods::TURN_STATE_GET));
}

#[test]
fn upcr_009_010_011_command_methods_round_trip_through_rpc_envelope() {
    let hydrate = UiCommand::SessionHydrate(SessionHydrateParams {
        session_id: sample_session_id(),
        after: None,
        include: vec![],
    });
    let rpc = hydrate
        .clone()
        .into_rpc_request("req-1")
        .expect("serialize hydrate");
    assert_eq!(rpc.method, methods::SESSION_HYDRATE);
    let decoded = UiCommand::from_rpc_request(rpc).expect("decode hydrate");
    assert_eq!(decoded, hydrate);

    let graph = UiCommand::ThreadGraphGet(ThreadGraphGetParams {
        session_id: sample_session_id(),
        at: None,
    });
    let rpc = graph
        .clone()
        .into_rpc_request("req-2")
        .expect("serialize graph");
    assert_eq!(rpc.method, methods::THREAD_GRAPH_GET);
    let decoded = UiCommand::from_rpc_request(rpc).expect("decode graph");
    assert_eq!(decoded, graph);

    let state = UiCommand::TurnStateGet(TurnStateGetParams {
        session_id: sample_session_id(),
        turn_id: sample_turn_id(),
    });
    let rpc = state
        .clone()
        .into_rpc_request("req-3")
        .expect("serialize state");
    assert_eq!(rpc.method, methods::TURN_STATE_GET);
    let decoded = UiCommand::from_rpc_request(rpc).expect("decode state");
    assert_eq!(decoded, state);
}

// ===== M12 Phase D-1 auxiliary REST → WS frames =====

#[test]
fn aux_rest_to_ws_v1_feature_string_is_stable() {
    assert_eq!(
        UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1,
        "auxiliary.rest_to_ws.v1"
    );
    assert!(UI_PROTOCOL_KNOWN_FEATURES.contains(&UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1));
}

#[test]
fn aux_rest_to_ws_v1_methods_round_trip_through_rpc_envelope() {
    // Each command is built, serialized via `into_rpc_request`,
    // re-decoded via `from_rpc_request`, and asserted equal. The
    // method string is checked against the `methods::` constant so
    // a typo there shows up here, not at deploy.
    let cases: Vec<(UiCommand, &str)> = vec![
        (
            UiCommand::SessionList(SessionListParams::default()),
            methods::SESSION_LIST,
        ),
        (
            UiCommand::SessionSnapshot(SessionSnapshotParams {
                session_id: "sess-1".into(),
                topic: Some("default".into()),
            }),
            methods::SESSION_SNAPSHOT,
        ),
        (
            UiCommand::SessionMessagesPage(SessionMessagesPageParams {
                session_id: "sess-1".into(),
                limit: Some(50),
                offset: Some(0),
                since_seq: None,
                topic: None,
            }),
            methods::SESSION_MESSAGES_PAGE,
        ),
        (
            UiCommand::SessionStatusGet(SessionStatusGetParams {
                session_id: "sess-1".into(),
                topic: None,
            }),
            methods::SESSION_STATUS_GET,
        ),
        (
            UiCommand::SessionFilesList(SessionFilesListParams {
                session_id: "sess-1".into(),
            }),
            methods::SESSION_FILES_LIST,
        ),
        (
            UiCommand::SessionTasksList(SessionTasksListParams {
                session_id: "sess-1".into(),
                topic: None,
            }),
            methods::SESSION_TASKS_LIST,
        ),
        (
            UiCommand::SessionWorkspaceGet(SessionWorkspaceGetParams {
                session_id: "sess-1".into(),
            }),
            methods::SESSION_WORKSPACE_GET,
        ),
        (
            UiCommand::SessionTitleSet(SessionTitleSetParams {
                session_id: "sess-1".into(),
                title: "Renamed".into(),
            }),
            methods::SESSION_TITLE_SET,
        ),
        (
            UiCommand::SessionDelete(SessionDeleteParams {
                session_id: "sess-1".into(),
            }),
            methods::SESSION_DELETE,
        ),
        (
            UiCommand::SystemStatusGet(SystemStatusGetParams::default()),
            methods::SYSTEM_STATUS_GET,
        ),
        (
            UiCommand::ContentList(ContentListParams {
                filters: serde_json::json!({ "limit": 10 }),
            }),
            methods::CONTENT_LIST,
        ),
        (
            UiCommand::ContentDelete(ContentDeleteParams { id: "c-1".into() }),
            methods::CONTENT_DELETE,
        ),
        (
            UiCommand::ContentBulkDelete(ContentBulkDeleteParams {
                ids: vec!["c-1".into(), "c-2".into()],
            }),
            methods::CONTENT_BULK_DELETE,
        ),
        (
            UiCommand::MemoryOverview(MemoryOverviewParams::default()),
            methods::MEMORY_OVERVIEW,
        ),
        (
            UiCommand::MemoryEntity(MemoryEntityParams {
                name: "acme-corp".into(),
            }),
            methods::MEMORY_ENTITY,
        ),
        (
            UiCommand::CronList(CronListParams::default()),
            methods::CRON_LIST,
        ),
        (
            UiCommand::CronToggle(CronToggleParams {
                job_id: "job-1".into(),
                enabled: false,
            }),
            methods::CRON_TOGGLE,
        ),
    ];
    assert_eq!(
        cases.len(),
        17,
        "17 UiCommand arms cover the 17 auxiliary methods \
             (`session/list`, `session/snapshot`, `session/messages_page`, \
             `session/status.get`, `session/files.list`, `session/tasks.list`, \
             `session/workspace.get`, `session/title.set`, `session/delete`, \
             `system/status.get`, `content/list`, `content/delete`, \
             `content/bulk_delete`, `memory/overview`, `memory/entity`, \
             `cron/list`, `cron/toggle`) — `content/delete` and \
             `content/bulk_delete` are distinct methods"
    );
    for (command, expected_method) in cases {
        let rpc = command
            .clone()
            .into_rpc_request("req")
            .expect("serialize command");
        assert_eq!(rpc.method, expected_method);
        let decoded = UiCommand::from_rpc_request(rpc).expect("decode command");
        assert_eq!(decoded, command);
    }
}

#[test]
fn aux_rest_to_ws_v1_empty_param_methods_accept_null_params() {
    // Wire shape per JSON-RPC 2.0 allows omitting params on
    // no-arg methods. `session/list` and `system/status.get`
    // accept either `{}` or absent params.
    let session_list_null = UiCommand::from_method_and_params(methods::SESSION_LIST, Value::Null)
        .expect("session/list with null params");
    assert!(matches!(session_list_null, UiCommand::SessionList(_)));

    let system_status_null =
        UiCommand::from_method_and_params(methods::SYSTEM_STATUS_GET, Value::Null)
            .expect("system/status.get with null params");
    assert!(matches!(system_status_null, UiCommand::SystemStatusGet(_)));

    let content_list_null = UiCommand::from_method_and_params(methods::CONTENT_LIST, Value::Null)
        .expect("content/list with null params");
    assert!(matches!(content_list_null, UiCommand::ContentList(_)));

    let memory_overview_null =
        UiCommand::from_method_and_params(methods::MEMORY_OVERVIEW, Value::Null)
            .expect("memory/overview with null params");
    assert!(matches!(memory_overview_null, UiCommand::MemoryOverview(_)));

    let cron_list_null = UiCommand::from_method_and_params(methods::CRON_LIST, Value::Null)
        .expect("cron/list with null params");
    assert!(matches!(cron_list_null, UiCommand::CronList(_)));
}

#[test]
fn aux_rest_to_ws_v1_result_dtos_round_trip_via_serde_json() {
    // Each result struct serializes to a stable shape and decodes
    // back. Opaque `Value` fields are forwarded byte-for-byte from
    // the REST handler bodies so they may carry whatever shape the
    // existing REST contract emits.
    let listing = SessionListResult {
        sessions: serde_json::json!([{ "id": "s-1", "message_count": 3 }]),
    };
    let value = serde_json::to_value(&listing).expect("serialize");
    let decoded: SessionListResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded.sessions, listing.sessions);

    let snapshot = SessionSnapshotResult {
        status: serde_json::json!({ "active": false }),
        files: serde_json::json!([]),
        tasks: serde_json::json!([]),
    };
    let value = serde_json::to_value(&snapshot).expect("serialize");
    let decoded: SessionSnapshotResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded.status, snapshot.status);
    assert_eq!(decoded.files, snapshot.files);
    assert_eq!(decoded.tasks, snapshot.tasks);

    let page = SessionMessagesPageResult {
        messages: serde_json::json!([]),
        has_more: false,
        next_offset: 0,
    };
    let value = serde_json::to_value(&page).expect("serialize");
    let decoded: SessionMessagesPageResult = serde_json::from_value(value).expect("deserialize");
    assert!(!decoded.has_more);
    assert_eq!(decoded.next_offset, 0);

    let title = SessionTitleSetResult {
        session_id: "s-1".into(),
        title: "Renamed".into(),
    };
    let value = serde_json::to_value(&title).expect("serialize");
    let decoded: SessionTitleSetResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded, title);

    let delete = SessionDeleteResult::default();
    let value = serde_json::to_value(&delete).expect("serialize");
    let decoded: SessionDeleteResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded, delete);

    let content = ContentListResult {
        entries: serde_json::json!([{ "id": "c-1" }]),
        total: 1,
    };
    let value = serde_json::to_value(&content).expect("serialize");
    let decoded: ContentListResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded.entries, content.entries);
    assert_eq!(decoded.total, content.total);

    let bulk = ContentBulkDeleteResult { deleted: 5 };
    let value = serde_json::to_value(&bulk).expect("serialize");
    let decoded: ContentBulkDeleteResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded, bulk);

    let overview = MemoryOverviewResult {
        overview: serde_json::json!({ "ok": true, "long_term": "# MEMORY" }),
    };
    let value = serde_json::to_value(&overview).expect("serialize");
    let decoded: MemoryOverviewResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded.overview, overview.overview);

    let entity = MemoryEntityResult {
        name: "acme-corp".into(),
        content: "# acme".into(),
        content_truncated: false,
        content_total_bytes: 6,
    };
    let value = serde_json::to_value(&entity).expect("serialize");
    let decoded: MemoryEntityResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded, entity);

    let cron = CronListResult {
        jobs: serde_json::json!([{ "id": "job-1" }]),
        count: 1,
        gateway_running: false,
        truncated: false,
    };
    let value = serde_json::to_value(&cron).expect("serialize");
    let decoded: CronListResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded.jobs, cron.jobs);
    assert_eq!(decoded.count, cron.count);
    assert!(!decoded.gateway_running);

    let toggle = CronToggleResult {
        job: serde_json::json!({ "id": "job-1", "enabled": false }),
    };
    let value = serde_json::to_value(&toggle).expect("serialize");
    let decoded: CronToggleResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded.job, toggle.job);
}

#[test]
fn launch_resolve_round_trips_through_rpc_envelope() {
    let command = UiCommand::LaunchResolve(LaunchResolveParams {
        cwd: "/tmp/project".into(),
        profile_id: Some("glm".into()),
    });
    let rpc = command
        .clone()
        .into_rpc_request("req")
        .expect("serialize command");
    assert_eq!(rpc.method, methods::LAUNCH_RESOLVE);
    let decoded = UiCommand::from_rpc_request(rpc).expect("decode command");
    assert_eq!(decoded, command);
}

#[test]
fn launch_resolve_is_gated_on_session_workspace_cwd_v1() {
    let none = UiProtocolCapabilities::for_negotiated_features(Vec::<String>::new());
    assert!(
        !none.supports_method(methods::LAUNCH_RESOLVE),
        "launch/resolve must NOT be advertised without session.workspace_cwd.v1"
    );
    let with_feature = UiProtocolCapabilities::for_negotiated_features([
        UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
    ]);
    assert!(
        with_feature.supports_method(methods::LAUNCH_RESOLVE),
        "launch/resolve must be advertised once session.workspace_cwd.v1 is negotiated"
    );
}

#[test]
fn launch_resolve_result_serializes_snake_case_decision() {
    let result = LaunchResolveResult {
        decision: LaunchDecisionKind::CrossProfile,
        resolved_profile: Some("deepseek".into()),
        existing_profiles: vec!["glm".into()],
    };
    let value = serde_json::to_value(&result).expect("serialize");
    assert_eq!(value["decision"], "cross_profile");
    let decoded: LaunchResolveResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded, result);
}

#[test]
fn aux_rest_to_ws_v1_methods_are_capability_gated() {
    // The 12 new methods must gate on
    // `auxiliary.rest_to_ws.v1`. A connection that does not
    // negotiate the feature must NOT see them in the advertised
    // `supported_methods`.
    let none = UiProtocolCapabilities::for_negotiated_features(Vec::<String>::new());
    for method in [
        methods::SESSION_LIST,
        methods::SESSION_SNAPSHOT,
        methods::SESSION_MESSAGES_PAGE,
        methods::SESSION_STATUS_GET,
        methods::SESSION_FILES_LIST,
        methods::SESSION_TASKS_LIST,
        methods::SESSION_WORKSPACE_GET,
        methods::SESSION_TITLE_SET,
        methods::SESSION_DELETE,
        methods::SYSTEM_STATUS_GET,
        methods::CONTENT_LIST,
        methods::CONTENT_DELETE,
        methods::CONTENT_BULK_DELETE,
        methods::MEMORY_OVERVIEW,
        methods::MEMORY_ENTITY,
        methods::CRON_LIST,
        methods::CRON_TOGGLE,
    ] {
        assert!(
            !none.supports_method(method),
            "method {method} must NOT be advertised without aux_rest_to_ws_v1"
        );
    }

    let with_feature = UiProtocolCapabilities::for_negotiated_features([
        UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1,
    ]);
    for method in [
        methods::SESSION_LIST,
        methods::SESSION_SNAPSHOT,
        methods::SESSION_MESSAGES_PAGE,
        methods::SESSION_STATUS_GET,
        methods::SESSION_FILES_LIST,
        methods::SESSION_TASKS_LIST,
        methods::SESSION_WORKSPACE_GET,
        methods::SESSION_TITLE_SET,
        methods::SESSION_DELETE,
        methods::SYSTEM_STATUS_GET,
        methods::CONTENT_LIST,
        methods::CONTENT_DELETE,
        methods::CONTENT_BULK_DELETE,
        methods::MEMORY_OVERVIEW,
        methods::MEMORY_ENTITY,
        methods::CRON_LIST,
        methods::CRON_TOGGLE,
    ] {
        assert!(
            with_feature.supports_method(method),
            "method {method} must be advertised when aux_rest_to_ws_v1 is negotiated"
        );
    }
    assert!(with_feature.supports_feature(UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1));
}

#[test]
fn autonomy_methods_require_base_and_group_features() {
    let without_base = UiProtocolCapabilities::for_negotiated_features([
        UI_PROTOCOL_FEATURE_CODING_GOAL_RUNTIME_V1,
    ]);
    assert!(!without_base.supports_feature(UI_PROTOCOL_FEATURE_CODING_GOAL_RUNTIME_V1));
    assert!(!without_base.supports_method(methods::SESSION_GOAL_SET));

    let base_only =
        UiProtocolCapabilities::for_negotiated_features([UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1]);
    assert!(base_only.supports_feature(UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1));
    assert!(!base_only.supports_method(methods::SESSION_GOAL_SET));

    let with_group = UiProtocolCapabilities::for_negotiated_features([
        UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1,
        UI_PROTOCOL_FEATURE_CODING_GOAL_RUNTIME_V1,
    ]);
    assert!(with_group.supports_method(methods::SESSION_GOAL_SET));
}

/// Codex review 2026-05-12 (MEDIUM 2): every M12 Phase D-1
/// request/result DTO must be pinned to a JSON-shape golden, not
/// just to a serde round-trip. A round-trip catches "can encode
/// and decode", but it does NOT catch "the field was renamed but
/// both ends agree on the rename" — exactly the failure mode we
/// care about when the WS bridge has to mirror REST DTOs that
/// live in a different crate. The literal-JSON asserts below
/// force a field rename (or a missing-field default flip) in any
/// REST DTO to fail this test before it lands.
#[test]
fn aux_rest_to_ws_v1_request_dtos_match_json_goldens() {
    // session/list — default (no cwd) params still serialize to the
    // historical empty object. The `cwd` field is
    // `skip_serializing_if = "Option::is_none"`, so an additive optional
    // field does NOT break the pinned wire shape: a no-cwd request is
    // byte-identical to the legacy `{}`.
    assert_eq!(
        serde_json::to_value(SessionListParams::default()).expect("serialize"),
        serde_json::json!({}),
    );
    // Old clients that send a bare `{}` still deserialize (→ cwd: None).
    let parsed: SessionListParams = serde_json::from_value(serde_json::json!({})).expect("decode");
    assert_eq!(parsed, SessionListParams::default());
    assert_eq!(parsed.cwd, None);
    // session/list — WITH the additive cwd (per-project storage). Pin the
    // new wire shape so a rename/type-flip of the field fails here.
    let with_cwd = SessionListParams {
        cwd: Some("/home/me/proj".into()),
    };
    assert_eq!(
        serde_json::to_value(&with_cwd).expect("serialize"),
        serde_json::json!({ "cwd": "/home/me/proj" }),
    );
    let parsed_cwd: SessionListParams =
        serde_json::from_value(serde_json::json!({ "cwd": "/home/me/proj" })).expect("decode");
    assert_eq!(parsed_cwd, with_cwd);

    // session/snapshot
    let p = SessionSnapshotParams {
        session_id: "sess-1".into(),
        topic: Some("topic-x".into()),
    };
    assert_eq!(
        serde_json::to_value(&p).expect("serialize"),
        serde_json::json!({ "session_id": "sess-1", "topic": "topic-x" }),
    );
    // Topic is optional — when absent, it must NOT serialize as
    // `"topic": null`. Drift in the `skip_serializing_if`
    // directive would flip the wire shape silently.
    let p_no_topic = SessionSnapshotParams {
        session_id: "sess-1".into(),
        topic: None,
    };
    assert_eq!(
        serde_json::to_value(&p_no_topic).expect("serialize"),
        serde_json::json!({ "session_id": "sess-1" }),
    );

    // session/messages_page
    let p = SessionMessagesPageParams {
        session_id: "sess-2".into(),
        limit: Some(50),
        offset: Some(10),
        since_seq: Some(100),
        topic: None,
    };
    assert_eq!(
        serde_json::to_value(&p).expect("serialize"),
        serde_json::json!({
            "session_id": "sess-2",
            "limit": 50,
            "offset": 10,
            "since_seq": 100,
        }),
    );

    // session/status.get
    let p = SessionStatusGetParams {
        session_id: "sess-3".into(),
        topic: None,
    };
    assert_eq!(
        serde_json::to_value(&p).expect("serialize"),
        serde_json::json!({ "session_id": "sess-3" }),
    );

    // session/files.list
    assert_eq!(
        serde_json::to_value(SessionFilesListParams {
            session_id: "sess-4".into(),
        })
        .expect("serialize"),
        serde_json::json!({ "session_id": "sess-4" }),
    );

    // session/tasks.list
    assert_eq!(
        serde_json::to_value(SessionTasksListParams {
            session_id: "sess-5".into(),
            topic: Some("t".into()),
        })
        .expect("serialize"),
        serde_json::json!({ "session_id": "sess-5", "topic": "t" }),
    );

    // session/workspace.get
    assert_eq!(
        serde_json::to_value(SessionWorkspaceGetParams {
            session_id: "sess-6".into(),
        })
        .expect("serialize"),
        serde_json::json!({ "session_id": "sess-6" }),
    );

    // session/title.set
    assert_eq!(
        serde_json::to_value(SessionTitleSetParams {
            session_id: "sess-7".into(),
            title: "New title".into(),
        })
        .expect("serialize"),
        serde_json::json!({ "session_id": "sess-7", "title": "New title" }),
    );

    // session/delete
    assert_eq!(
        serde_json::to_value(SessionDeleteParams {
            session_id: "sess-8".into(),
        })
        .expect("serialize"),
        serde_json::json!({ "session_id": "sess-8" }),
    );

    // system/status.get — empty
    assert_eq!(
        serde_json::to_value(SystemStatusGetParams::default()).expect("serialize"),
        serde_json::json!({}),
    );

    // content/list — free-form filters; default object is empty
    assert_eq!(
        serde_json::to_value(ContentListParams::default()).expect("serialize"),
        serde_json::json!({ "filters": null }),
    );
    assert_eq!(
        serde_json::to_value(ContentListParams {
            filters: serde_json::json!({ "category": "image", "limit": 50 }),
        })
        .expect("serialize"),
        serde_json::json!({ "filters": { "category": "image", "limit": 50 } }),
    );

    // content/delete
    assert_eq!(
        serde_json::to_value(ContentDeleteParams { id: "c-1".into() }).expect("serialize"),
        serde_json::json!({ "id": "c-1" }),
    );

    // content/bulk_delete
    assert_eq!(
        serde_json::to_value(ContentBulkDeleteParams {
            ids: vec!["c-1".into(), "c-2".into()],
        })
        .expect("serialize"),
        serde_json::json!({ "ids": ["c-1", "c-2"] }),
    );

    // memory/overview — empty
    assert_eq!(
        serde_json::to_value(MemoryOverviewParams::default()).expect("serialize"),
        serde_json::json!({}),
    );

    // memory/entity
    assert_eq!(
        serde_json::to_value(MemoryEntityParams {
            name: "acme-corp".into(),
        })
        .expect("serialize"),
        serde_json::json!({ "name": "acme-corp" }),
    );

    // cron/list — empty
    assert_eq!(
        serde_json::to_value(CronListParams::default()).expect("serialize"),
        serde_json::json!({}),
    );

    // cron/toggle
    assert_eq!(
        serde_json::to_value(CronToggleParams {
            job_id: "job-1".into(),
            enabled: true,
        })
        .expect("serialize"),
        serde_json::json!({ "job_id": "job-1", "enabled": true }),
    );
}

/// Codex review 2026-05-12 (MEDIUM 2): JSON golden assertions for
/// every aux result DTO. Pinned wire shapes — a rename of any
/// field below must show up in this test before it reaches a
/// downstream client. Each `assert_eq!` is the contract.
#[test]
fn aux_rest_to_ws_v1_result_dtos_match_json_goldens() {
    // session/list — `{ sessions: <opaque> }`
    assert_eq!(
        serde_json::to_value(SessionListResult {
            sessions: serde_json::json!([{ "id": "s-1" }]),
        })
        .expect("serialize"),
        serde_json::json!({ "sessions": [{ "id": "s-1" }] }),
    );

    // session/snapshot — `{ status, files, tasks }`
    assert_eq!(
        serde_json::to_value(SessionSnapshotResult {
            status: serde_json::json!({ "active": true }),
            files: serde_json::json!([{ "path": "f.txt" }]),
            tasks: serde_json::json!([]),
        })
        .expect("serialize"),
        serde_json::json!({
            "status": { "active": true },
            "files": [{ "path": "f.txt" }],
            "tasks": [],
        }),
    );

    // session/messages_page — `{ messages, has_more, next_offset }`
    assert_eq!(
        serde_json::to_value(SessionMessagesPageResult {
            messages: serde_json::json!([]),
            has_more: true,
            next_offset: 200,
        })
        .expect("serialize"),
        serde_json::json!({
            "messages": [],
            "has_more": true,
            "next_offset": 200,
        }),
    );

    // session/status.get — `{ status: <opaque> }`
    assert_eq!(
        serde_json::to_value(SessionStatusGetResult {
            status: serde_json::json!({ "active": false }),
            context_state: None,
        })
        .expect("serialize"),
        serde_json::json!({ "status": { "active": false } }),
    );

    // session/files.list — `{ files: <opaque> }`
    assert_eq!(
        serde_json::to_value(SessionFilesListResult {
            files: serde_json::json!([{ "path": "a.txt" }]),
        })
        .expect("serialize"),
        serde_json::json!({ "files": [{ "path": "a.txt" }] }),
    );

    // session/tasks.list — `{ tasks: <opaque> }`
    assert_eq!(
        serde_json::to_value(SessionTasksListResult {
            tasks: serde_json::json!([]),
        })
        .expect("serialize"),
        serde_json::json!({ "tasks": [] }),
    );

    // session/workspace.get — `{ contracts: <opaque> }`
    assert_eq!(
        serde_json::to_value(SessionWorkspaceGetResult {
            contracts: serde_json::json!([]),
        })
        .expect("serialize"),
        serde_json::json!({ "contracts": [] }),
    );

    // session/title.set — `{ session_id, title }`
    assert_eq!(
        serde_json::to_value(SessionTitleSetResult {
            session_id: "s-1".into(),
            title: "Hello".into(),
        })
        .expect("serialize"),
        serde_json::json!({ "session_id": "s-1", "title": "Hello" }),
    );

    // session/delete — empty object
    assert_eq!(
        serde_json::to_value(SessionDeleteResult::default()).expect("serialize"),
        serde_json::json!({}),
    );

    // system/status.get — `{ status: <opaque> }`
    assert_eq!(
        serde_json::to_value(SystemStatusGetResult {
            status: serde_json::json!({ "version": "0.1.1" }),
        })
        .expect("serialize"),
        serde_json::json!({ "status": { "version": "0.1.1" } }),
    );

    // content/list — `{ entries, total }`
    assert_eq!(
        serde_json::to_value(ContentListResult {
            entries: serde_json::json!([{ "id": "c-1" }]),
            total: 7,
        })
        .expect("serialize"),
        serde_json::json!({
            "entries": [{ "id": "c-1" }],
            "total": 7,
        }),
    );

    // content/delete — `{ deleted: bool }`
    assert_eq!(
        serde_json::to_value(ContentDeleteResult { deleted: true }).expect("serialize"),
        serde_json::json!({ "deleted": true }),
    );

    // content/bulk_delete — `{ deleted: usize }`
    assert_eq!(
        serde_json::to_value(ContentBulkDeleteResult { deleted: 12 }).expect("serialize"),
        serde_json::json!({ "deleted": 12 }),
    );

    // memory/overview — `{ overview: <opaque REST body> }`
    assert_eq!(
        serde_json::to_value(MemoryOverviewResult {
            overview: serde_json::json!({ "ok": true, "staging_notes": 2 }),
        })
        .expect("serialize"),
        serde_json::json!({ "overview": { "ok": true, "staging_notes": 2 } }),
    );

    // memory/entity — `{ name, content, content_truncated,
    // content_total_bytes }` (truncation metadata is part of the
    // wire contract: capped fields must be DECLARED, never silent).
    assert_eq!(
        serde_json::to_value(MemoryEntityResult {
            name: "acme-corp".into(),
            content: "# acme".into(),
            content_truncated: false,
            content_total_bytes: 6,
        })
        .expect("serialize"),
        serde_json::json!({
            "name": "acme-corp",
            "content": "# acme",
            "content_truncated": false,
            "content_total_bytes": 6,
        }),
    );

    // cron/list — `{ jobs, count, gateway_running, truncated }`
    assert_eq!(
        serde_json::to_value(CronListResult {
            jobs: serde_json::json!([{ "id": "job-1" }]),
            count: 1,
            gateway_running: true,
            truncated: false,
        })
        .expect("serialize"),
        serde_json::json!({
            "jobs": [{ "id": "job-1" }],
            "count": 1,
            "gateway_running": true,
            "truncated": false,
        }),
    );

    // cron/toggle — `{ job: <opaque cron/list entry> }`
    assert_eq!(
        serde_json::to_value(CronToggleResult {
            job: serde_json::json!({ "id": "job-1", "enabled": false }),
        })
        .expect("serialize"),
        serde_json::json!({ "job": { "id": "job-1", "enabled": false } }),
    );
}

/// Codex review 2026-05-12 (MEDIUM 1): the new
/// `RpcError::not_found(resource_type, identifier)` constructor
/// must carry the resource tag + identifier in `data` so clients
/// can distinguish a content-row miss from a session miss without
/// parsing message strings. Pinned via JSON golden.
#[test]
fn rpc_error_not_found_carries_typed_resource_data() {
    let err = RpcError::not_found("content", "c-99");
    assert_eq!(err.code, rpc_error_codes::RESOURCE_NOT_FOUND);
    let value = serde_json::to_value(&err).expect("serialize");
    assert_eq!(value.get("code"), Some(&json!(-32170)));
    let data = value.get("data").expect("data present");
    assert_eq!(data.get("kind"), Some(&json!("not_found")));
    assert_eq!(data.get("resource_type"), Some(&json!("content")));
    assert_eq!(data.get("identifier"), Some(&json!("c-99")));
}

/// Codex review 2026-05-12 (MEDIUM 3): the bulk-delete cap is
/// part of the wire contract and must not drift silently. Pin
/// the constant value to 256 so a future bump shows up as a
/// test diff.
#[test]
fn content_bulk_delete_max_ids_constant_is_pinned() {
    assert_eq!(
        CONTENT_BULK_DELETE_MAX_IDS, 256,
        "wire-contract cap; bump server dispatcher AND any client adapters together",
    );
}

// ===== UPCR-2026-014 M9-γ projection envelope golden tests =====

fn envelope(seq: u64, payload: Payload) -> Envelope {
    Envelope {
        thread_id: "thread-1".into(),
        seq,
        client_message_id: None,
        payload,
    }
}

#[test]
fn golden_envelope_assistant_delta_round_trips() {
    let env = envelope(
        1,
        Payload::AssistantDelta {
            text: "hello".into(),
        },
    );
    let value = serde_json::to_value(&env).expect("serialize");
    assert_eq!(value.get("thread_id"), Some(&json!("thread-1")));
    assert_eq!(value.get("seq"), Some(&json!(1)));
    assert!(
        value.get("client_message_id").is_none(),
        "client_message_id is absent on internal events"
    );
    let payload = value.get("payload").expect("payload");
    assert_eq!(payload.get("type"), Some(&json!("assistant_delta")));
    assert_eq!(
        payload.get("data").and_then(|d| d.get("text")),
        Some(&json!("hello"))
    );
    let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, env);
}

#[test]
fn golden_envelope_user_message_round_trips() {
    // user_message envelopes are the turn root — server-mirrored
    // from the client send. They carry `client_message_id` (and
    // ONLY they do, per UPCR-2026-014 § 14.1) so the optimistic
    // <GhostBubble> overlay can match its server reflection. The
    // projection itself MUST NOT consult the field.
    let env = Envelope {
        thread_id: "thread-1".into(),
        seq: 1,
        client_message_id: Some("cmid-abc".into()),
        payload: Payload::UserMessage {
            text: "Q1 — what's 2+2?".into(),
            files: vec![FileRef {
                path: "/tmp/upload.png".into(),
                mime: "image/png".into(),
                size_bytes: 2048,
            }],
        },
    };
    let value = serde_json::to_value(&env).expect("serialize");
    assert_eq!(value.get("client_message_id"), Some(&json!("cmid-abc")));
    let payload = value.get("payload").expect("payload");
    assert_eq!(payload.get("type"), Some(&json!("user_message")));
    let data = payload.get("data").expect("data");
    assert_eq!(data.get("text"), Some(&json!("Q1 — what's 2+2?")));
    let files = data.get("files").and_then(|f| f.as_array()).expect("files");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].get("path"), Some(&json!("/tmp/upload.png")));
    assert_eq!(files[0].get("mime"), Some(&json!("image/png")));
    assert_eq!(files[0].get("size_bytes"), Some(&json!(2048)));
    let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, env);
}

#[test]
fn golden_envelope_user_message_omits_empty_files() {
    // `files` is omitted on the wire when empty (matches the rest
    // of the protocol's `Vec<_>` skip-empty convention).
    let env = Envelope {
        thread_id: "thread-1".into(),
        seq: 1,
        client_message_id: Some("cmid-1".into()),
        payload: Payload::UserMessage {
            text: "hi".into(),
            files: vec![],
        },
    };
    let value = serde_json::to_value(&env).expect("serialize");
    let data = value
        .get("payload")
        .and_then(|p| p.get("data"))
        .expect("data");
    assert!(
        data.get("files").is_none(),
        "empty files array MUST be omitted on the wire"
    );
    let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, env);
}

#[test]
fn golden_envelope_assistant_delta_omits_client_message_id_on_wire() {
    // Per spec § 14.1 + Envelope doc: client_message_id is ONLY
    // populated on user_message envelopes. Internal events
    // (assistant_delta and friends) leave it None and the wire
    // shape MUST omit the field entirely.
    let env = envelope(
        2,
        Payload::AssistantDelta {
            text: "Q1 answer…".into(),
        },
    );
    let value = serde_json::to_value(&env).expect("serialize");
    assert!(
        value.get("client_message_id").is_none(),
        "client_message_id is absent on non-user_message envelopes"
    );
}

#[test]
fn golden_envelope_assistant_persisted_round_trips() {
    let env = envelope(
        3,
        Payload::AssistantPersisted {
            text: "final answer".into(),
            meta: MessageMeta {
                message_id: "msg-7".into(),
                persisted_at: sample_persisted_at(),
                media: vec!["report.md".into()],
            },
        },
    );
    let value = serde_json::to_value(&env).expect("serialize");
    let payload = value.get("payload").expect("payload");
    assert_eq!(payload.get("type"), Some(&json!("assistant_persisted")));
    let data = payload.get("data").expect("data");
    assert_eq!(data.get("text"), Some(&json!("final answer")));
    assert_eq!(
        data.get("meta").and_then(|m| m.get("message_id")),
        Some(&json!("msg-7"))
    );
    let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, env);
}

#[test]
fn golden_envelope_tool_fidelity_round_trip_and_legacy_decode() {
    // Enriched shape: arguments/output previews + duration survive the
    // wire round-trip.
    let start = envelope(
        4,
        Payload::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "shell".into(),
            arguments_preview: Some("command: \"cargo test\"".into()),
        },
    );
    let end = envelope(
        5,
        Payload::ToolEnd {
            tool_call_id: "tc-1".into(),
            status: EnvelopeToolEndStatus::Complete,
            error: None,
            reason: None,
            output_preview: Some("test result: ok. 815 passed".into()),
            duration_ms: Some(1234),
        },
    );
    for env in [start, end] {
        let value = serde_json::to_value(&env).expect("serialize");
        let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
        assert_eq!(parsed, env);
    }

    // Legacy wire (envelopes persisted before the fidelity fields
    // existed) must still decode — fields default to None. Build the
    // legacy shape by stripping the new keys from a modern envelope so
    // the fixture tracks the real tag/content encoding.
    let strip = |env: &Envelope, keys: &[&str]| -> Envelope {
        let mut value = serde_json::to_value(env).expect("serialize");
        let data = value["payload"]["data"]
            .as_object_mut()
            .expect("payload data object");
        for key in keys {
            data.remove(*key);
        }
        serde_json::from_value(value).expect("legacy envelope decodes")
    };
    let start = envelope(
        8,
        Payload::ToolStart {
            tool_call_id: "tc-9".into(),
            name: "read_file".into(),
            arguments_preview: Some("path: \"x\"".into()),
        },
    );
    match strip(&start, &["arguments_preview"]).payload {
        Payload::ToolStart {
            arguments_preview, ..
        } => assert_eq!(arguments_preview, None),
        other => panic!("expected ToolStart, got {other:?}"),
    }
    let end = envelope(
        9,
        Payload::ToolEnd {
            tool_call_id: "tc-9".into(),
            status: EnvelopeToolEndStatus::Complete,
            error: None,
            reason: None,
            output_preview: Some("ok".into()),
            duration_ms: Some(1),
        },
    );
    match strip(&end, &["output_preview", "duration_ms"]).payload {
        Payload::ToolEnd {
            output_preview,
            duration_ms,
            ..
        } => {
            assert_eq!(output_preview, None);
            assert_eq!(duration_ms, None);
        }
        other => panic!("expected ToolEnd, got {other:?}"),
    }
}

#[test]
fn golden_envelope_tool_start_progress_end_round_trip() {
    let start = envelope(
        4,
        Payload::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "shell".into(),
            arguments_preview: None,
        },
    );
    let progress = envelope(
        5,
        Payload::ToolProgress {
            tool_call_id: "tc-1".into(),
            message: "running…".into(),
        },
    );
    let end_ok = envelope(
        6,
        Payload::ToolEnd {
            tool_call_id: "tc-1".into(),
            status: EnvelopeToolEndStatus::Complete,
            error: None,
            reason: None,
            output_preview: None,
            duration_ms: None,
        },
    );
    let end_err = envelope(
        7,
        Payload::ToolEnd {
            tool_call_id: "tc-2".into(),
            status: EnvelopeToolEndStatus::Error,
            error: Some("boom".into()),
            reason: None,
            output_preview: None,
            duration_ms: None,
        },
    );

    for env in [&start, &progress, &end_ok, &end_err] {
        let value = serde_json::to_value(env).expect("serialize");
        let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
        assert_eq!(&parsed, env);
    }

    // Wire-form discriminator check.
    let start_val = serde_json::to_value(&start).expect("serialize");
    assert_eq!(
        start_val.get("payload").and_then(|p| p.get("type")),
        Some(&json!("tool_start"))
    );
    let end_err_val = serde_json::to_value(&end_err).expect("serialize");
    assert_eq!(
        end_err_val
            .get("payload")
            .and_then(|p| p.get("data"))
            .and_then(|d| d.get("status")),
        Some(&json!("error"))
    );
    // ToolEnd `error` and `reason` fields omitted when None.
    let end_ok_val = serde_json::to_value(&end_ok).expect("serialize");
    let end_ok_data = end_ok_val
        .get("payload")
        .and_then(|p| p.get("data"))
        .expect("tool_end data");
    assert!(end_ok_data.get("error").is_none());
    assert!(end_ok_data.get("reason").is_none());
}

#[test]
fn golden_envelope_tool_end_skipped_and_aborted_round_trip() {
    // Codex M9-γ-1 BLOCK 3: `complete | error` was too lossy. The
    // sealed v1 union now also covers deadline-skip (`skipped`) and
    // user/system-driven cancellation (`aborted`). Optional
    // `reason` carries the human-readable detail.
    let skipped = envelope(
        10,
        Payload::ToolEnd {
            tool_call_id: "tc-3".into(),
            status: EnvelopeToolEndStatus::Skipped,
            error: None,
            reason: Some("deadline elapsed before tool started".into()),
            output_preview: None,
            duration_ms: None,
        },
    );
    let aborted = envelope(
        11,
        Payload::ToolEnd {
            tool_call_id: "tc-4".into(),
            status: EnvelopeToolEndStatus::Aborted,
            error: None,
            reason: Some("user issued turn/interrupt".into()),
            output_preview: None,
            duration_ms: None,
        },
    );
    for (env, expected_status) in [(&skipped, "skipped"), (&aborted, "aborted")] {
        let value = serde_json::to_value(env).expect("serialize");
        let data = value
            .get("payload")
            .and_then(|p| p.get("data"))
            .expect("tool_end data");
        assert_eq!(data.get("status"), Some(&json!(expected_status)));
        assert!(
            data.get("reason").is_some(),
            "reason populated for skipped/aborted"
        );
        assert!(data.get("error").is_none(), "error omitted when None");
        let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
        assert_eq!(&parsed, env);
    }
}

#[test]
fn golden_envelope_file_attached_round_trips() {
    let env = envelope(
        8,
        Payload::FileAttached {
            path: "/tmp/report.md".into(),
            mime: "text/markdown".into(),
            size_bytes: 4096,
        },
    );
    let value = serde_json::to_value(&env).expect("serialize");
    assert_eq!(
        value.get("payload").and_then(|p| p.get("type")),
        Some(&json!("file_attached"))
    );
    let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, env);
}

#[test]
fn golden_envelope_turn_completed_round_trips() {
    let env = envelope(
        9,
        Payload::TurnCompleted {
            token_usage: EnvelopeTokenUsage {
                input_tokens: 100,
                output_tokens: 250,
                reasoning_tokens: 0,
                cache_read_tokens: 80,
                cache_write_tokens: 0,
            },
        },
    );
    let value = serde_json::to_value(&env).expect("serialize");
    assert_eq!(
        value.get("payload").and_then(|p| p.get("type")),
        Some(&json!("turn_completed"))
    );
    let usage = value
        .get("payload")
        .and_then(|p| p.get("data"))
        .and_then(|d| d.get("token_usage"))
        .expect("token_usage");
    assert_eq!(usage.get("input_tokens"), Some(&json!(100)));
    assert_eq!(usage.get("output_tokens"), Some(&json!(250)));
    // Zero fields are omitted on the wire.
    assert!(usage.get("reasoning_tokens").is_none());
    assert!(usage.get("cache_write_tokens").is_none());
    let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, env);
}

#[test]
fn golden_envelope_token_usage_zero_default_round_trips() {
    // turn_completed with all-zero usage emits an empty `token_usage: {}`.
    let env = envelope(
        10,
        Payload::TurnCompleted {
            token_usage: EnvelopeTokenUsage::default(),
        },
    );
    let value = serde_json::to_value(&env).expect("serialize");
    let usage = value
        .get("payload")
        .and_then(|p| p.get("data"))
        .and_then(|d| d.get("token_usage"))
        .expect("token_usage");
    assert!(usage.as_object().expect("object").is_empty());
    let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, env);
}

#[test]
fn projection_envelope_v2_is_flattened_and_carries_the_stage_one_contract() {
    let persisted_at = sample_persisted_at();
    let notification = UiNotification::EnvelopeV2(EnvelopeV2Notification {
        session_id: SessionKey("local:v2-contract#planning".into()),
        topic: Some("planning".into()),
        envelope: EnvelopeV2 {
            thread_id: "thread-v2-parent".into(),
            seq: 7,
            cursor: Some(UiCursor {
                stream: "local:v2-contract".into(),
                seq: 412,
            }),
            turn_id: "turn-v2-parent".into(),
            client_message_id: None,
            payload: PayloadV2::AssistantPersisted {
                text: "final segment".into(),
                assistant_segment_id: "turn-v2-parent:assistant:2".into(),
                meta: MessageMeta {
                    message_id: "msg-v2-7".into(),
                    persisted_at,
                    media: vec!["artifacts/plan.md".into()],
                },
            },
        },
    });

    let rpc = notification
        .clone()
        .into_rpc_notification()
        .expect("v2 wire serializes");
    assert_eq!(rpc.method, methods::PROJECTION_ENVELOPE);
    assert!(rpc.params.get("envelope").is_none(), "wire stays flattened");
    assert_eq!(rpc.params.get("turn_id"), Some(&json!("turn-v2-parent")));
    assert_eq!(
        rpc.params
            .get("cursor")
            .and_then(|cursor| cursor.get("seq")),
        Some(&json!(412)),
        "v2 emit always carries its durable ledger cursor",
    );
    assert_eq!(
        rpc.params
            .get("payload")
            .and_then(|payload| payload.get("type")),
        Some(&json!("assistant_persisted")),
    );
    assert_eq!(
        rpc.params
            .get("payload")
            .and_then(|payload| payload.get("data"))
            .and_then(|data| data.get("assistant_segment_id")),
        Some(&json!("turn-v2-parent:assistant:2")),
    );

    let expected_envelope = match &notification {
        UiNotification::EnvelopeV2(notification) => notification.envelope.clone(),
        _ => unreachable!("test constructed an EnvelopeV2"),
    };
    let mut cursor_absent_rpc = rpc.clone();
    cursor_absent_rpc
        .params
        .as_object_mut()
        .expect("v2 params are an object")
        .remove("cursor");
    let cursor_absent = UiNotification::from_rpc_notification(cursor_absent_rpc)
        .expect("cursor-absent v2 wire still decodes");
    assert!(matches!(
        cursor_absent,
        UiNotification::EnvelopeV2(EnvelopeV2Notification {
            envelope: EnvelopeV2 { cursor: None, .. },
            ..
        })
    ));

    let decoded = UiNotification::from_rpc_notification(rpc).expect("v2 wire decodes");
    match decoded {
        UiNotification::EnvelopeV2(decoded) => {
            // Mirrors v1's established wire behavior: a topic-suffixed
            // storage/session key is normalized to its bare routing key,
            // while the topic stays explicit.
            assert_eq!(decoded.session_id, SessionKey("local:v2-contract".into()));
            assert_eq!(decoded.topic.as_deref(), Some("planning"));
            assert_eq!(decoded.envelope, expected_envelope);
        }
        other => panic!("expected EnvelopeV2, got {other:?}"),
    }
}

#[test]
fn projection_envelope_v2_payloads_cover_terminal_attachment_and_child_completion() {
    let error = TurnTerminalError {
        code: "runtime_error".into(),
        message: "provider stopped the turn".into(),
        data: Some(json!({ "retryable": true })),
    };
    let terminal = PayloadV2::TurnTerminal {
        outcome: TurnTerminalOutcome::Errored,
        error: Some(error),
        token_usage: None,
    };
    let terminal_value = serde_json::to_value(&terminal).expect("terminal serializes");
    assert_eq!(terminal_value.get("type"), Some(&json!("turn_terminal")));
    assert_eq!(
        terminal_value
            .get("data")
            .and_then(|data| data.get("outcome")),
        Some(&json!("errored")),
    );

    let attached = PayloadV2::FileAttached {
        path: "artifacts/report.md".into(),
        mime: "text/markdown".into(),
        size_bytes: 42,
        attachment_owner: AttachmentOwnerV2 {
            assistant_segment_id: Some("turn-v2-parent:assistant:2".into()),
            tool_call_id: Some("call-v2-1".into()),
        },
    };
    let attached_value = serde_json::to_value(&attached).expect("attachment serializes");
    assert_eq!(
        attached_value
            .get("data")
            .and_then(|data| data.get("attachment_owner"))
            .and_then(|owner| owner.get("tool_call_id")),
        Some(&json!("call-v2-1")),
    );

    let child = PayloadV2::BackgroundChildCompleted {
        parent_turn_id: "turn-v2-parent".into(),
        response_to_client_message_id: Some("cmid-v2-parent".into()),
        task_id: "task-v2-child".into(),
        content: "background result".into(),
        tool_call_id: Some("call-v2-1".into()),
        message_id: "msg-v2-child".into(),
        source: "background".into(),
        persisted_at: sample_persisted_at(),
        media: vec!["artifacts/report.md".into()],
    };
    let child_value = serde_json::to_value(&child).expect("child completion serializes");
    assert_eq!(
        child_value
            .get("data")
            .and_then(|data| data.get("parent_turn_id")),
        Some(&json!("turn-v2-parent")),
    );
    assert_eq!(
        child_value
            .get("data")
            .and_then(|data| data.get("response_to_client_message_id")),
        Some(&json!("cmid-v2-parent")),
    );
}

#[test]
fn golden_envelope_capability_feature_flag_registered() {
    // The projection feature flag must be in the known-features
    // registry so capability negotiation honours it.
    assert!(
        UI_PROTOCOL_KNOWN_FEATURES.contains(&UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1),
        "projection.envelope.v1 must be registered for capability negotiation"
    );
    assert!(
        UI_PROTOCOL_KNOWN_FEATURES.contains(&UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2),
        "projection.envelope.v2 must be registered for capability negotiation"
    );
    assert!(
        !UiProtocolCapabilities::first_server_slice()
            .supports_feature(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2),
        "v2 is strictly opt-in and must not alter the no-header capability baseline"
    );
}

#[test]
fn envelope_notification_method_is_projection_envelope() {
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        envelope: Envelope {
            thread_id: "thread-1".into(),
            seq: 1,
            client_message_id: None,
            payload: Payload::AssistantDelta { text: "hi".into() },
        },
    });
    assert_eq!(notif.method(), "projection/envelope");
    assert_eq!(notif.session_id(), &SessionKey("local:demo".into()));
}

#[test]
fn envelope_notification_round_trips_through_rpc_envelope_with_routing() {
    // feat(envelope-wire-routing): the wire now carries `session_id`
    // (the bare base key) + optional `topic` FLATTENED alongside the
    // bare Envelope fields so a multi-session client can route the
    // envelope to the right session. The envelope fields stay at the
    // top level (no `envelope` nesting) so the existing tolerant web
    // SPA bridge — which reads `thread_id`/`seq`/`payload` top-level
    // and ignores unknown keys — keeps decoding it unchanged.
    let envelope = Envelope {
        thread_id: "thread-7".into(),
        seq: 42,
        client_message_id: Some("cmid-x".into()),
        payload: Payload::UserMessage {
            text: "hi".into(),
            files: vec![FileRef {
                path: "/tmp/a.png".into(),
                mime: "image/png".into(),
                size_bytes: 12,
            }],
        },
    };
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:demo".into()),
        topic: Some("planning".into()),
        envelope: envelope.clone(),
    });
    let rpc = notif.into_rpc_notification().expect("serialize");
    assert_eq!(rpc.method, "projection/envelope");
    // Wire shape: flattened — bare Envelope keys PLUS routing keys.
    let params = &rpc.params;
    assert_eq!(
        params.get("session_id"),
        Some(&json!("local:demo")),
        "session_id must reach the wire so the client can route",
    );
    assert_eq!(
        params.get("topic"),
        Some(&json!("planning")),
        "topic must reach the wire for topic-scoped routing",
    );
    // Bare Envelope keys stay at the top level (web-bridge compat).
    assert_eq!(params.get("thread_id"), Some(&json!("thread-7")));
    assert_eq!(params.get("seq"), Some(&json!(42)));
    assert_eq!(params.get("client_message_id"), Some(&json!("cmid-x")));
    // No `envelope` nesting on the wire — the flatten keeps the bare
    // shape the web bridge already reads.
    assert!(
        params.get("envelope").is_none(),
        "wire is flattened, not nested under `envelope`",
    );

    // Round-trip decode: session_id + topic survive byte-for-byte and
    // the envelope is byte-equal.
    let parsed = UiNotification::from_rpc_notification(rpc).expect("decode");
    match parsed {
        UiNotification::Envelope(ev) => {
            assert_eq!(ev.envelope, envelope);
            assert_eq!(
                ev.session_id,
                SessionKey("local:demo".into()),
                "decode must recover the routing session_id from the wire",
            );
            assert_eq!(ev.topic, Some("planning".into()));
        }
        other => panic!("expected Envelope variant, got {other:?}"),
    }
}

/// feat(envelope-wire-routing) backward-compat: an OLD bare-envelope
/// wire frame (no `session_id` / `topic` keys — emitted by a server
/// before this change) must still decode without error. The routing
/// fields default to empty/None; the consumer is expected to fall
/// back to ambient connection context for those legacy frames.
#[test]
fn envelope_notification_decodes_legacy_bare_wire_frame_without_routing() {
    // OLD wire shape: bare Envelope, no session_id/topic.
    let legacy_params = json!({
        "thread_id": "thread-legacy",
        "seq": 3,
        "payload": { "type": "assistant_delta", "data": { "text": "hi" } }
    });
    let decoded =
        UiNotification::from_method_and_params(methods::PROJECTION_ENVELOPE, legacy_params)
            .expect("legacy bare-envelope frame must still decode");
    match decoded {
        UiNotification::Envelope(ev) => {
            assert_eq!(
                ev.session_id,
                SessionKey(String::new()),
                "absent session_id defaults to empty for legacy frames",
            );
            assert_eq!(ev.topic, None, "absent topic defaults to None");
            assert_eq!(ev.envelope.thread_id, "thread-legacy");
            assert_eq!(ev.envelope.seq, 3);
        }
        other => panic!("expected Envelope variant, got {other:?}"),
    }
}

/// Codex #1336 round-2 BLOCKER 4: the durable ledger writes records
/// via `serde_json::to_string(&LedgerDiskRecord)`, which chains
/// through the global `Serialize` impl on `EnvelopeNotification`.
/// Before the fix, that global impl stripped `session_id` + `topic`
/// to mirror the wire shape — so disk records lost their routing
/// context and recovery deserialized them with empty/None routing.
/// Topic-scoped envelope replay after restart silently mis-routed.
///
/// Post-fix: the global Serialize/Deserialize is derive-based and
/// preserves ALL fields. The wire shape is opted into only at the
/// JSON-RPC boundary inside `into_rpc_notification`.
#[test]
fn envelope_notification_serde_preserves_routing_fields_for_disk_persistence() {
    // Persistent shape: routing fields survive a JSON round-trip.
    // This is the path the durable ledger uses for its on-disk
    // records, NOT the wire path.
    let original = EnvelopeNotification {
        session_id: SessionKey("local:disk-routing".into()),
        topic: Some("planning".into()),
        envelope: Envelope {
            thread_id: "thread-disk".into(),
            seq: 7,
            client_message_id: None,
            payload: Payload::AssistantDelta {
                text: "persisted delta".into(),
            },
        },
    };

    // Serialize the EnvelopeNotification directly (NOT via
    // into_rpc_notification) — this mirrors how the ledger writes
    // it inside a LedgerDiskRecord. The output MUST contain the
    // routing fields.
    let serialized =
        serde_json::to_value(&original).expect("EnvelopeNotification serializes for disk");
    assert_eq!(
        serialized.get("session_id"),
        Some(&json!("local:disk-routing")),
        "session_id must persist on disk so recovery can rebuild routing",
    );
    assert_eq!(
        serialized.get("topic"),
        Some(&json!("planning")),
        "topic must persist on disk so topic-scoped recovery routes correctly",
    );
    assert!(
        serialized.get("envelope").is_some(),
        "envelope body must be present on disk",
    );

    // Deserialize back — routing fields must round-trip byte-equal.
    let parsed: EnvelopeNotification =
        serde_json::from_value(serialized).expect("EnvelopeNotification deserializes from disk");
    assert_eq!(
        parsed, original,
        "disk round-trip must preserve all fields including routing",
    );

    // Defensive: a `topic: None` envelope omits the field on disk
    // (no behavioural change — just keeps the disk shape compact
    // when topic isn't set).
    let no_topic = EnvelopeNotification {
        session_id: SessionKey("local:disk-no-topic".into()),
        topic: None,
        envelope: original.envelope.clone(),
    };
    let serialized = serde_json::to_value(&no_topic).expect("serialize");
    assert!(
        serialized.get("topic").is_none(),
        "absent topic is omitted on disk; deserialize defaults back to None",
    );
    let parsed: EnvelopeNotification = serde_json::from_value(serialized).expect("deserialize");
    assert_eq!(parsed, no_topic);
}

/// feat(envelope-wire-routing) — wire shape guard. The wire is the
/// FLATTENED form: bare Envelope keys (`thread_id`, `seq`, `payload`,
/// no `envelope` nesting) PLUS the routing keys `session_id` +
/// `topic` so a multi-session client can route. Codex #1336
/// BLOCKER-4's actual invariant — that the DISK derive preserves
/// routing — is pinned by
/// `envelope_notification_serde_preserves_routing_fields_for_disk_persistence`
/// above; that disk path is untouched by un-stripping the wire.
#[test]
fn envelope_notification_into_rpc_notification_flattens_routing_onto_wire() {
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:wire-route".into()),
        topic: Some("planning".into()),
        envelope: Envelope {
            thread_id: "thread-wire".into(),
            seq: 5,
            client_message_id: None,
            payload: Payload::AssistantDelta { text: "x".into() },
        },
    });
    let rpc = notif.into_rpc_notification().expect("serialize");
    assert_eq!(rpc.method, methods::PROJECTION_ENVELOPE);
    let params = &rpc.params;
    assert_eq!(
        params.get("session_id"),
        Some(&json!("local:wire-route")),
        "wire carries session_id for routing",
    );
    assert_eq!(
        params.get("topic"),
        Some(&json!("planning")),
        "wire carries topic for topic-scoped routing",
    );
    // Bare envelope fields stay top-level (no `envelope` nesting) so
    // the existing web-bridge top-level reader is unaffected.
    assert_eq!(params.get("thread_id"), Some(&json!("thread-wire")));
    assert_eq!(params.get("seq"), Some(&json!(5)));
    assert!(
        params.get("envelope").is_none(),
        "wire is flattened, not nested under `envelope`",
    );
}

/// feat(envelope-wire-routing): a `topic: None` envelope omits the
/// `topic` key on the wire (compact shape) but still carries
/// `session_id`. Decode recovers session_id and defaults topic.
#[test]
fn envelope_notification_wire_omits_absent_topic_but_keeps_session_id() {
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:no-topic".into()),
        topic: None,
        envelope: Envelope {
            thread_id: "thread-nt".into(),
            seq: 9,
            client_message_id: None,
            payload: Payload::AssistantDelta { text: "y".into() },
        },
    });
    let rpc = notif.into_rpc_notification().expect("serialize");
    let params = &rpc.params;
    assert_eq!(params.get("session_id"), Some(&json!("local:no-topic")));
    assert!(
        params.get("topic").is_none(),
        "absent topic omitted on the wire",
    );
    let parsed = UiNotification::from_rpc_notification(rpc).expect("decode");
    match parsed {
        UiNotification::Envelope(ev) => {
            assert_eq!(ev.session_id, SessionKey("local:no-topic".into()));
            assert_eq!(ev.topic, None);
        }
        other => panic!("expected Envelope variant, got {other:?}"),
    }
}

/// feat(envelope-wire-routing) — codex BLOCKER: on a TOPIC turn the
/// `turn/start` flow folds the topic into `session_id` as
/// `"base#topic"`, which is carried forward into the emitted
/// `EnvelopeNotification.session_id`. The WIRE `session_id` MUST be
/// normalized to the bare base key (`"base"`) — a client only knows
/// the base key, so a `"base#topic"` wire key misroutes the message
/// and defeats the orphan-chip self-heal. The topic MUST NOT be lost:
/// it is preserved on the wire's separate `topic` field (recovered
/// from the suffix when the explicit `topic` field is empty). The
/// DISK derive on `EnvelopeNotification` keeps `"base#topic"`
/// untouched (pinned by the disk-persistence test above).
#[test]
fn envelope_wire_session_id_is_normalized_to_base_key_with_topic_preserved() {
    let envelope = Envelope {
        thread_id: "thread-topic".into(),
        seq: 11,
        client_message_id: None,
        payload: Payload::AssistantDelta {
            text: "topic delta".into(),
        },
    };

    // Case 1: topic folded into session_id ("base#topic"), explicit
    // `topic` field is None — the suffix must be recovered onto the
    // wire's separate `topic` field while session_id is stripped.
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:demo#research".into()),
        topic: None,
        envelope: envelope.clone(),
    });
    let rpc = notif.into_rpc_notification().expect("serialize");
    let params = &rpc.params;
    assert_eq!(
        params.get("session_id"),
        Some(&json!("local:demo")),
        "wire session_id must be the bare base key, not base#topic",
    );
    assert_eq!(
        params.get("topic"),
        Some(&json!("research")),
        "topic must be preserved on the wire (recovered from suffix)",
    );
    // Decode must round-trip to the bare base key + separate topic.
    let parsed = UiNotification::from_rpc_notification(rpc).expect("decode");
    match parsed {
        UiNotification::Envelope(ev) => {
            assert_eq!(
                ev.session_id,
                SessionKey("local:demo".into()),
                "decode recovers the bare base key from the wire",
            );
            assert_eq!(ev.topic, Some("research".into()));
            assert_eq!(ev.envelope, envelope);
        }
        other => panic!("expected Envelope variant, got {other:?}"),
    }

    // Case 2: topic folded into session_id AND an explicit `topic`
    // field also set — the explicit topic wins, session_id still
    // strips to the base key.
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:demo#research".into()),
        topic: Some("research".into()),
        envelope: envelope.clone(),
    });
    let rpc = notif.into_rpc_notification().expect("serialize");
    let params = &rpc.params;
    assert_eq!(
        params.get("session_id"),
        Some(&json!("local:demo")),
        "wire session_id must be the bare base key even with explicit topic",
    );
    assert_eq!(params.get("topic"), Some(&json!("research")));
}

// ------------------------------------------------------------------
// Wave4-A: router/status, router/failover, queue/state, router/set_mode,
// router/get_metrics round-trip + wire-shape tests.
// ------------------------------------------------------------------

/// Wave4-A: `router/status` notification round-trips through JSON-RPC
/// with deterministic `BTreeMap` ordering and the correct wire tag.
#[test]
fn router_status_notification_round_trips_with_deterministic_order() {
    let mut lane_scores = BTreeMap::new();
    lane_scores.insert("zai/glm-5-turbo".into(), 0.21);
    lane_scores.insert("dashscope/qwen3.5-plus".into(), 0.41);
    lane_scores.insert("ollama/llama3.2".into(), 0.62);

    let mut breakers = BTreeMap::new();
    breakers.insert("zai/glm-5-turbo".into(), "closed".into());
    breakers.insert("dashscope/qwen3.5-plus".into(), "half_open".into());
    breakers.insert("ollama/llama3.2".into(), "open".into());

    let notif = UiNotification::RouterStatus(RouterStatusEvent {
        session_id: SessionKey("local:demo".into()),
        provider_name: "zai/glm-5-turbo".into(),
        mode: "lane".into(),
        qos_ranking: true,
        lane_scores: lane_scores.clone(),
        circuit_breakers: breakers.clone(),
    });

    // Method tag matches the constant.
    assert_eq!(notif.method(), methods::ROUTER_STATUS);

    // Round-trip through JSON-RPC notification envelope.
    let rpc = notif
        .clone()
        .into_rpc_notification()
        .expect("serialize router/status");
    assert_eq!(rpc.method, methods::ROUTER_STATUS);

    let json = serde_json::to_string(&rpc).expect("to_string");
    let parsed_rpc: RpcNotification<Value> = serde_json::from_str(&json).expect("from_str rpc");
    let decoded = UiNotification::from_rpc_notification(parsed_rpc).expect("decode router/status");
    assert_eq!(decoded, notif);

    // BTreeMap ordering is deterministic — the first key in the wire
    // payload must be the lex-smallest, so a re-serialization byte-
    // matches.
    let wire_keys: Vec<String> = serde_json::to_value(&notif)
        .expect("value")
        .get("lane_scores")
        .and_then(|v| v.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default();
    assert_eq!(
        wire_keys,
        vec![
            "dashscope/qwen3.5-plus".to_string(),
            "ollama/llama3.2".to_string(),
            "zai/glm-5-turbo".to_string(),
        ],
        "lane_scores keys must be in BTreeMap (lex-sorted) order"
    );
}

/// Wave4-A: `router/failover` round-trips with the failover metadata
/// the AdaptiveRouter emits when it crosses a lane.
#[test]
fn router_failover_notification_round_trips() {
    let notif = UiNotification::RouterFailover(RouterFailoverEvent {
        session_id: SessionKey("local:demo".into()),
        from_provider: "zai/glm-5-turbo".into(),
        to_provider: "dashscope/qwen3.5-plus".into(),
        reason: "circuit_breaker_open".into(),
        elapsed_ms: 12_345,
    });
    assert_eq!(notif.method(), methods::ROUTER_FAILOVER);

    let rpc = notif
        .clone()
        .into_rpc_notification()
        .expect("serialize router/failover");
    let json = serde_json::to_string(&rpc).expect("to_string");
    let parsed_rpc: RpcNotification<Value> = serde_json::from_str(&json).expect("from_str");
    let decoded =
        UiNotification::from_rpc_notification(parsed_rpc).expect("decode router/failover");
    assert_eq!(decoded, notif);
}

/// Wave4-A: `queue/state` round-trips with `head_client_message_id`
/// both populated (in-flight) and absent (queue idle).
#[test]
fn queue_state_notification_round_trips_with_and_without_head() {
    // In-flight: head_client_message_id present.
    let notif_active = UiNotification::QueueState(QueueStateEvent {
        session_id: SessionKey("local:demo".into()),
        pending_count: 3,
        head_client_message_id: Some("cmid-12345".into()),
    });
    assert_eq!(notif_active.method(), methods::QUEUE_STATE);
    let rpc = notif_active
        .clone()
        .into_rpc_notification()
        .expect("serialize active");
    let json = serde_json::to_string(&rpc).expect("to_string active");
    // head_client_message_id is on the wire when populated.
    assert!(json.contains("head_client_message_id"));
    assert!(json.contains("cmid-12345"));

    let parsed: RpcNotification<Value> = serde_json::from_str(&json).expect("from_str active");
    let decoded = UiNotification::from_rpc_notification(parsed).expect("decode queue/state active");
    assert_eq!(decoded, notif_active);

    // Empty queue: head_client_message_id absent.
    let notif_empty = UiNotification::QueueState(QueueStateEvent {
        session_id: SessionKey("local:demo".into()),
        pending_count: 0,
        head_client_message_id: None,
    });
    let rpc = notif_empty
        .clone()
        .into_rpc_notification()
        .expect("serialize empty");
    let json = serde_json::to_string(&rpc).expect("to_string empty");
    assert!(
        !json.contains("head_client_message_id"),
        "head_client_message_id must be omitted when None — got {json}"
    );

    let parsed: RpcNotification<Value> = serde_json::from_str(&json).expect("from_str empty");
    let decoded = UiNotification::from_rpc_notification(parsed).expect("decode queue/state empty");
    assert_eq!(decoded, notif_empty);
}

/// Wave4-A: `router/set_mode` command round-trips and dispatches
/// through the standard `UiCommand` request shape.
#[test]
fn router_set_mode_command_round_trips() {
    let command = UiCommand::RouterSetMode(RouterSetModeParams {
        session_id: SessionKey("local:demo".into()),
        mode: "hedge".into(),
    });
    assert_eq!(command.method(), methods::ROUTER_SET_MODE);

    let rpc = command
        .clone()
        .into_rpc_request("req-set-mode")
        .expect("serialize router/set_mode");
    assert_eq!(rpc.method, methods::ROUTER_SET_MODE);
    assert_eq!(rpc.params["mode"], json!("hedge"));

    let json = serde_json::to_string(&rpc).expect("to_string");
    let parsed_rpc: RpcRequest<Value> = serde_json::from_str(&json).expect("from_str");
    let decoded = UiCommand::from_rpc_request(parsed_rpc).expect("decode router/set_mode");
    assert_eq!(decoded, command);
}

/// Wave4-A: `router/get_metrics` request + result round-trip. Mirrors
/// the wire shape of the `router/status` notification so clients can
/// reuse the deserializer.
#[test]
fn router_get_metrics_command_and_result_round_trip() {
    let command = UiCommand::RouterGetMetrics(RouterGetMetricsParams {
        session_id: SessionKey("local:demo".into()),
    });
    assert_eq!(command.method(), methods::ROUTER_GET_METRICS);

    let rpc = command
        .clone()
        .into_rpc_request("req-get-metrics")
        .expect("serialize router/get_metrics");
    let json = serde_json::to_string(&rpc).expect("to_string");
    let parsed_rpc: RpcRequest<Value> = serde_json::from_str(&json).expect("from_str");
    let decoded = UiCommand::from_rpc_request(parsed_rpc).expect("decode router/get_metrics");
    assert_eq!(decoded, command);

    // Result round-trips with deterministic BTreeMap order.
    let mut lane_scores = BTreeMap::new();
    lane_scores.insert("a/b".into(), 0.1);
    lane_scores.insert("c/d".into(), 0.2);
    let mut breakers = BTreeMap::new();
    breakers.insert("a/b".into(), "closed".into());
    breakers.insert("c/d".into(), "closed".into());
    let result = RouterGetMetricsResult {
        provider_name: "a/b".into(),
        mode: "lane".into(),
        qos_ranking: false,
        lane_scores: lane_scores.clone(),
        circuit_breakers: breakers.clone(),
    };
    let json = serde_json::to_string(&result).expect("serialize result");
    let parsed: RouterGetMetricsResult = serde_json::from_str(&json).expect("deserialize result");
    assert_eq!(parsed.provider_name, result.provider_name);
    assert_eq!(parsed.lane_scores, lane_scores);
    assert_eq!(parsed.circuit_breakers, breakers);
}

/// Wave4-A: capability advertisement carries the three new notification
/// methods (`router/status`, `router/failover`, `queue/state`) so
/// clients that negotiate at handshake time can subscribe.
#[test]
fn wave4a_router_methods_are_in_capabilities() {
    let caps = UiProtocolCapabilities::first_server_slice();
    assert!(
        caps.supported_notifications
            .contains(&methods::ROUTER_STATUS.to_owned()),
        "router/status must be advertised as a supported notification"
    );
    assert!(
        caps.supported_notifications
            .contains(&methods::ROUTER_FAILOVER.to_owned()),
        "router/failover must be advertised as a supported notification"
    );
    assert!(
        caps.supported_notifications
            .contains(&methods::QUEUE_STATE.to_owned()),
        "queue/state must be advertised as a supported notification"
    );
    assert!(
        caps.supports_method(methods::ROUTER_SET_MODE),
        "router/set_mode must be a supported command method"
    );
    assert!(
        caps.supports_method(methods::ROUTER_GET_METRICS),
        "router/get_metrics must be a supported command method"
    );
}

// -----------------------------------------------------------------
// #1329 — topic-scope routing class fix
//
// The 6 events listed below (ToolStarted/Progress/Completed,
// ApprovalAutoResolved/Decided/Cancelled), plus FileAttached
// (already covered by the P0-A regression), gained an explicit
// `topic: Option<String>` field. `UiNotification::topic()` now
// consults that field FIRST and only falls back to
// `SessionKey.topic()`. Each test:
//   1. Builds the event with an explicit `topic` field — `topic()`
//      returns the field's value, even when `session_id` was
//      stripped to `base_key()` (the P0-A failure mode).
//   2. Builds the same event with a topic-suffixed session_id but
//      NO explicit topic — `topic()` falls back to the suffix
//      (backward compat; `stamp_topic_from_session` then promotes
//      it to the explicit field at append time).
//   3. Builds the event with neither — `topic()` returns `None`.
// -----------------------------------------------------------------

fn topic_session() -> SessionKey {
    SessionKey("local:slides-soak#slides".into())
}

fn bare_session() -> SessionKey {
    SessionKey("local:slides-soak".into())
}

#[test]
fn voice_audio_chunk_round_trips_through_rpc_notification() {
    let event = VoiceAudioChunkEvent {
        session_id: bare_session(),
        topic: None,
        turn_id: TurnId::new(),
        segment_id: "seg-1".into(),
        seq: 0,
        mime: "audio/mpeg".into(),
        audio_b64: "QUJD".into(),
        last: false,
    };
    let notif = UiNotification::VoiceAudioChunk(event.clone());
    assert_eq!(notif.method(), methods::VOICE_AUDIO_CHUNK);

    let rpc = notif.into_rpc_notification().expect("to rpc notification");
    assert_eq!(rpc.method, "voice/audio_chunk");

    let back = UiNotification::from_rpc_notification(rpc).expect("from rpc notification");
    assert_eq!(back, UiNotification::VoiceAudioChunk(event));
}

#[test]
fn tool_started_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        turn_id: TurnId::new(),
        tool_call_id: "tc-1".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    assert_eq!(
        with_field.topic(),
        Some("slides"),
        "explicit topic field wins over base_key() session_id"
    );

    let fallback = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc-2".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    assert_eq!(
        fallback.topic(),
        Some("slides"),
        "missing explicit topic falls back to session_id suffix"
    );

    let neither = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: bare_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc-3".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    assert_eq!(neither.topic(), None, "no topic anywhere → None");
}

#[test]
fn tool_progress_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ToolProgress(ToolProgressEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        turn_id: TurnId::new(),
        tool_call_id: "tc-1".into(),
        message: Some("step 1".into()),
        progress_pct: Some(25.0),
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::ToolProgress(ToolProgressEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc-2".into(),
        message: None,
        progress_pct: None,
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

#[test]
fn tool_completed_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ToolCompleted(ToolCompletedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        turn_id: TurnId::new(),
        tool_call_id: "tc-1".into(),
        tool_name: "shell".into(),
        success: Some(true),
        output_preview: None,
        duration_ms: Some(10),
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::ToolCompleted(ToolCompletedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc-2".into(),
        tool_name: "shell".into(),
        success: Some(true),
        output_preview: None,
        duration_ms: None,
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

#[test]
fn approval_auto_resolved_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ApprovalAutoResolved(ApprovalAutoResolvedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        tool_name: "shell".into(),
        scope: approval_scopes::SESSION.into(),
        scope_match: "exact".into(),
        decision: ApprovalDecision::Approve,
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::ApprovalAutoResolved(ApprovalAutoResolvedEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        tool_name: "shell".into(),
        scope: approval_scopes::SESSION.into(),
        scope_match: "exact".into(),
        decision: ApprovalDecision::Approve,
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

#[test]
fn approval_decided_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ApprovalDecided(ApprovalDecidedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        decision: ApprovalDecision::Approve,
        scope: None,
        decided_at: Utc::now(),
        decided_by: "user:test".into(),
        auto_resolved: false,
        policy_id: None,
        client_note: None,
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::ApprovalDecided(ApprovalDecidedEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        decision: ApprovalDecision::Approve,
        scope: None,
        decided_at: Utc::now(),
        decided_by: "user:test".into(),
        auto_resolved: false,
        policy_id: None,
        client_note: None,
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

#[test]
fn approval_cancelled_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ApprovalCancelled(ApprovalCancelledEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        reason: approval_cancelled_reasons::TURN_INTERRUPTED.into(),
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::ApprovalCancelled(ApprovalCancelledEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        reason: approval_cancelled_reasons::TURN_INTERRUPTED.into(),
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

/// #1329 sibling test: FileAttached gained the same `topic` field
/// as the 6 ApprovalDecided-class events; verify the same access
/// rule (explicit first, suffix fallback). This was the bug the
/// P0-A exemption patched; with explicit field, the exemption is
/// no longer needed.
#[test]
fn file_attached_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::FileAttached(FileAttachedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        turn_id: TurnId::new(),
        path: "/tmp/deck.pptx".into(),
        tool_call_id: Some("tc-slides".into()),
        mime: None,
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::FileAttached(FileAttachedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        path: "/tmp/deck.pptx".into(),
        tool_call_id: None,
        mime: None,
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

/// `stamp_topic_from_session` MUST populate the new explicit
/// `topic` field for the 6 vulnerable variants (and FileAttached)
/// from the SessionKey suffix when the field is absent. This is
/// the safety net that runs in `into_rpc_notification`: even if a
/// caller forgets to stamp, the wire-emit path stamps it for them
/// so a topic-scoped subscriber always routes the event correctly.
#[test]
fn stamp_topic_from_session_populates_new_topic_class_events() {
    // ToolStarted
    let mut event = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    event.stamp_topic_from_session();
    assert_eq!(event.topic(), Some("slides"));
    if let UiNotification::ToolStarted(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    } else {
        panic!("event variant changed unexpectedly");
    }

    // ToolProgress
    let mut event = UiNotification::ToolProgress(ToolProgressEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc".into(),
        message: None,
        progress_pct: None,
    });
    event.stamp_topic_from_session();
    if let UiNotification::ToolProgress(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }

    // ToolCompleted
    let mut event = UiNotification::ToolCompleted(ToolCompletedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc".into(),
        tool_name: "shell".into(),
        success: Some(true),
        output_preview: None,
        duration_ms: None,
    });
    event.stamp_topic_from_session();
    if let UiNotification::ToolCompleted(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }

    // ApprovalAutoResolved
    let mut event = UiNotification::ApprovalAutoResolved(ApprovalAutoResolvedEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        tool_name: "shell".into(),
        scope: approval_scopes::SESSION.into(),
        scope_match: "exact".into(),
        decision: ApprovalDecision::Approve,
    });
    event.stamp_topic_from_session();
    if let UiNotification::ApprovalAutoResolved(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }

    // ApprovalDecided
    let mut event = UiNotification::ApprovalDecided(ApprovalDecidedEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        decision: ApprovalDecision::Approve,
        scope: None,
        decided_at: Utc::now(),
        decided_by: "user:test".into(),
        auto_resolved: false,
        policy_id: None,
        client_note: None,
    });
    event.stamp_topic_from_session();
    if let UiNotification::ApprovalDecided(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }

    // ApprovalCancelled
    let mut event = UiNotification::ApprovalCancelled(ApprovalCancelledEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        reason: approval_cancelled_reasons::TURN_INTERRUPTED.into(),
    });
    event.stamp_topic_from_session();
    if let UiNotification::ApprovalCancelled(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }

    // FileAttached (sibling)
    let mut event = UiNotification::FileAttached(FileAttachedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        path: "/tmp/deck.pptx".into(),
        tool_call_id: None,
        mime: None,
    });
    event.stamp_topic_from_session();
    if let UiNotification::FileAttached(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }
}

/// #1329 wire-shape guarantee: the new `topic` field must
/// serialize when present and stay omitted when absent (so v0
/// clients never see a surprise field). Verified for one
/// representative variant (the same `skip_serializing_if` is
/// applied uniformly across all 7).
#[test]
fn tool_started_topic_field_round_trips_on_the_wire() {
    let event = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        turn_id: TurnId(Uuid::from_u128(0x1329)),
        tool_call_id: "tc-1329".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    let wire = serde_json::to_value(event.clone().into_rpc_notification().expect("serialize"))
        .expect("to_value");
    assert_eq!(wire["params"]["topic"], json!("slides"));

    let decoded: RpcNotification<Value> = serde_json::from_value(wire).expect("deserialize wire");
    let decoded_event = UiNotification::from_rpc_notification(decoded).expect("decode");
    assert_eq!(decoded_event.topic(), Some("slides"));

    // Absent topic stays omitted.
    let bare_event = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: bare_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc-bare".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    let wire = serde_json::to_value(bare_event.into_rpc_notification().expect("serialize bare"))
        .expect("to_value");
    assert!(
        wire["params"].get("topic").is_none(),
        "absent topic field must stay omitted on the wire (no v0 breakage)"
    );
}
