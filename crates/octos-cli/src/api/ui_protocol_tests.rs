#[path = "ui_protocol_test_support.rs"]
pub(crate) mod test_support;
use self::test_support::{recv_rpc_json, ws_connection_for_test};
use super::*;
// `UiProtocolContractStores`'s audit writer moved with the contract stores to
// `crate::approvals_audit`; the parent module no longer imports these two names
// directly, so name them here.
use octos_core::ui_protocol::{
    ApprovalDecision, ApprovalId, ApprovalRespondParams, PreviewId, QuestionId, UserQuestionAnswer,
};

#[test]
fn spec_section6_catalog_lists_every_advertised_method() {
    fn is_method_char(c: char) -> bool {
        // `.` is part of a method token (dotted M12 names like
        // `session/status.get`), NOT a boundary — otherwise a drifted
        // `session/status.get.v2` catalog entry would still satisfy the
        // advertised `session/status.get`.
        c.is_ascii_alphanumeric() || c == '_' || c == '/' || c == '.'
    }
    // Bounded substring match within an entry's method-token head, so
    // `artifact/list` is not satisfied by `agent/artifact/list`; backticks,
    // commas, and spaces are all boundaries.
    fn head_has_method(head: &str, method: &str) -> bool {
        let bytes = head.as_bytes();
        let mut from = 0;
        while let Some(rel) = head[from..].find(method) {
            let i = from + rel;
            let before_ok = i == 0 || !is_method_char(bytes[i - 1] as char);
            let after = i + method.len();
            let after_ok = after >= bytes.len() || !is_method_char(bytes[after] as char);
            if before_ok && after_ok {
                return true;
            }
            from = i + 1;
        }
        false
    }

    // A method counts as cataloged only when it appears in the method-token
    // HEAD of a `- ` list entry — the entry's first line PLUS any indented
    // continuation lines, truncated at the first description delimiter (`(`
    // annotation or `—` prose). Grouped entries wrap their comma-separated
    // method list across continuation lines, so those must be folded in;
    // but a method named only inside another entry's parenthetical/prose
    // (e.g. `session/hydrate` inside the `session/rollback` bullet) must NOT
    // count, or genuine drift for that method would hide.
    fn catalog_lists(section: &str, method: &str) -> bool {
        fn head(entry: &str) -> &str {
            let cut = [entry.find('('), entry.find('—')]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(entry.len());
            &entry[..cut]
        }
        let mut entries: Vec<String> = Vec::new();
        let mut current: Option<String> = None;
        for line in section.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("- ") {
                if let Some(entry) = current.take() {
                    entries.push(entry);
                }
                current = Some(rest.to_string());
            } else if !trimmed.is_empty() && line.starts_with(char::is_whitespace) {
                // Continuation of the active grouped entry, if any.
                if let Some(entry) = current.as_mut() {
                    entry.push(' ');
                    entry.push_str(trimmed);
                }
            } else {
                // Blank / non-indented line ends the active entry.
                if let Some(entry) = current.take() {
                    entries.push(entry);
                }
            }
        }
        if let Some(entry) = current.take() {
            entries.push(entry);
        }
        entries
            .iter()
            .any(|entry| head_has_method(head(entry), method))
    }

    let spec_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md"
    );
    let spec = std::fs::read_to_string(spec_path)
        .unwrap_or_else(|e| panic!("cannot read UI Protocol spec at {spec_path}: {e}"));

    // Slice the §6 catalog: its header up to the next top-level section.
    let start = spec
        .find("## 6. Envelope Model")
        .expect("UI Protocol spec is missing `## 6. Envelope Model`");
    let rest = &spec[start..];
    // Bound §6 at the NEXT top-level (`## `) heading, whatever its number
    // or title. Hardcoding `## 7.` would fail OPEN if §7 is ever renamed or
    // renumbered — the slice would run to end-of-spec, and later semantic
    // sections repeat these method names in list entries, so a deleted §6
    // entry could still be satisfied. Fail closed: §6 must be followed by
    // another top-level section.
    let end = rest
        .find("\n## ")
        .expect("UI Protocol spec §6 must be followed by a `## ` section heading");
    let section6 = &rest[..end];

    let missing: Vec<&str> = octos_core::ui_protocol::UI_PROTOCOL_COMMAND_METHODS
        .iter()
        .chain(octos_core::ui_protocol::UI_PROTOCOL_NOTIFICATION_METHODS.iter())
        .chain(octos_core::ui_protocol::UI_PROTOCOL_FIRST_SERVER_METHODS.iter())
        .chain(APPUI_EXTRA_METHODS.iter())
        .copied()
        .filter(|method| !catalog_lists(section6, method))
        .collect();

    assert!(
        missing.is_empty(),
        "UI Protocol spec §6 catalog is missing advertised method(s): {missing:?}\n\
             Add them under `## 6. Envelope Model` in \
             api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md — the catalog is a \
             hand-maintained mirror and must stay a superset of \
             UI_PROTOCOL_COMMAND_METHODS / UI_PROTOCOL_NOTIFICATION_METHODS / \
             UI_PROTOCOL_FIRST_SERVER_METHODS / APPUI_EXTRA_METHODS."
    );
}

fn local_profile_state(dir: &Path) -> AppState {
    AppState {
        profile_store: Some(Arc::new(
            crate::profiles::ProfileStore::open_unified(dir).unwrap(),
        )),
        user_store: Some(Arc::new(crate::user_store::UserStore::open(dir).unwrap())),
        deployment_mode: crate::config::DeploymentMode::Local,
        // Solo profile creation is opt-in; the TUI/WS tests exercise the
        // supported path, so enable it here.
        solo_login_enabled: true,
        ..AppState::empty_for_tests()
    }
}

// ===========================================================================
// #2166 — typed AppUI inference-parameter schema (reject / round-trip /
// ownership). The old schema silently DROPPED any extra field the client
// sent (temperature, top_p, max_output_tokens, context_window, nested
// reasoning objects) while answering `applied: true`.
// ===========================================================================

fn local_profile_state_with_sessions(dir: &Path) -> Arc<AppState> {
    Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(dir).expect("session manager"),
        ))),
        ..local_profile_state(dir)
    })
}

fn local_profile_params(
    name: &str,
    username: &str,
    email: &str,
) -> octos_core::ui_protocol::ProfileLocalCreateParams {
    octos_core::ui_protocol::ProfileLocalCreateParams {
        requested_id: None,
        name: name.into(),
        username: username.into(),
        email: email.into(),
        make_default: None,
    }
}

#[test]
fn stdio_capabilities_omit_auth_bound_methods_and_report_unsupported() {
    // M18 conformance (e2e/fixtures/appui-conformance/README.md): stdio must
    // not advertise auth-bound methods it will refuse; each is explicitly
    // reported as unsupported instead. WebSocket keeps the real auth chain.
    let state = AppState::empty_for_tests();
    let websocket = ConnectionUiFeatures::default().advertised_capabilities(&state);
    let stdio = ConnectionUiFeatures::stdio_defaults().advertised_capabilities(&state);
    for method in APPUI_STDIO_AUTH_BOUND_UNAVAILABLE_METHODS {
        assert!(
            websocket.supports_method(method),
            "websocket keeps {method}"
        );
        assert!(!stdio.supports_method(method), "stdio omits {method}");
        assert!(
            stdio.unsupported_report(method).is_some(),
            "stdio explicitly reports {method} unsupported"
        );
    }
}

#[tokio::test]
async fn stdio_multi_profile_open_status_reads_isolated_runtime_policy_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state_with_sessions(dir.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", "ada", "ada@example.com"),
    )
    .expect("create ada profile");
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Grace Hopper", "grace", "grace@example.com"),
    )
    .expect("create grace profile");

    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let features = ConnectionUiFeatures::stdio_defaults();
    let ada_session = SessionKey::with_profile_topic("ada", "local", "tui-a", "coding");
    let grace_session = SessionKey::with_profile_topic("grace", "local", "tui-b", "coding");

    for session_id in [ada_session.clone(), grace_session.clone()] {
        let profile_id = session_id.profile_id().expect("profile-qualified session");
        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            &PendingQuestionStore::default(),
            ConnectionId::next(),
            Some(profile_id),
            None,
            features,
            SessionOpenParams {
                session_ref: None,
                client_commands: None,
                session_id: session_id.clone(),
                topic: None,
                profile_id: None,
                cwd: None,
                sandbox: None,
                after: None,
            },
        )
        .await
        .expect("profile-qualified session/open succeeds");
        assert_eq!(
            outcome.result.opened.active_profile_id.as_deref(),
            Some(profile_id)
        );
    }

    let ada_status = raw_session_status_result(
        &state,
        &RpcRequest::<Value>::new(
            "ada-status",
            APPUI_METHOD_SESSION_STATUS_READ,
            json!({ "session_id": ada_session }),
        ),
        features,
        Some("grace"),
    )
    .await
    .expect("ada status");
    let grace_status = raw_session_status_result(
        &state,
        &RpcRequest::<Value>::new(
            "grace-status",
            APPUI_METHOD_SESSION_STATUS_READ,
            json!({ "session_id": grace_session }),
        ),
        features,
        Some("ada"),
    )
    .await
    .expect("grace status");

    assert_eq!(ada_status["profile_id"], json!("ada"));
    assert_eq!(
        ada_status["runtime_policy_stamp"]["profile_id"],
        json!("ada")
    );
    assert_eq!(grace_status["profile_id"], json!("grace"));
    assert_eq!(
        grace_status["runtime_policy_stamp"]["profile_id"],
        json!("grace")
    );
}

/// #436 P1 #1 — on reconnect the peer opens under a NEW client-chosen wire key;
/// a queued injection must be RE-HOMED to the new session, not stranded on the
/// closed one.
#[tokio::test]
async fn peer_send_input_rehomes_to_reopened_peer_wire_on_reconnect() {
    use crate::autonomy::agent_orchestrator::PeerSendInputEnqueueOutcome;
    let orchestrator = default_agent_orchestrator();
    let profile_id = "test-peer-reconnect-rehome";
    let slug = "reconnect-peer";
    let s1 = SessionKey::with_profile_topic(profile_id, "api", "conn-1", &format!("peer-{slug}"));
    let s2 = SessionKey::with_profile_topic(profile_id, "api", "conn-2", &format!("peer-{slug}"));
    // An empty state has no registered profile runtime, so the close-marker
    // skip is a no-op here — retarget behaves exactly as before this fix.
    let state = Arc::new(AppState::empty_for_tests());

    // Peer opens as S1; an injection is queued against S1.
    register_peer_wire_session(&state, &s1);
    let outcome = orchestrator.enqueue_peer_send_input_continuation(
        &s1,
        profile_id,
        slug,
        "call-x",
        "resume work",
    );
    assert_eq!(outcome, PeerSendInputEnqueueOutcome::Queued);
    assert_eq!(
        orchestrator.pending_continuation_count_for_session_for_test(&s1, profile_id),
        1
    );

    // Peer reopens as a NEW wire key S2 → the stranded injection is re-homed.
    register_peer_wire_session(&state, &s2);
    assert_eq!(
        orchestrator.pending_continuation_count_for_session_for_test(&s1, profile_id),
        0,
        "the injection must not remain stranded on the closed S1",
    );
    assert_eq!(
        orchestrator.pending_continuation_count_for_session_for_test(&s2, profile_id),
        1,
        "the injection must be re-homed to the reopened S2",
    );
    assert_eq!(
        peer_wire_registry().resolve(&peer_wire_key(profile_id, slug)),
        Some(s2.clone()),
        "registry reflects latest-wins wire",
    );
    // The re-homed continuation still renders the original text verbatim.
    let drained = orchestrator.drain_ready_continuations_for_session(
        &s2,
        profile_id,
        crate::autonomy::master_continuation_scheduler::MasterContinuationRuntimeState::idle(),
        8,
    );
    assert_eq!(drained.len(), 1);
    assert_eq!(master_continuation_prompt(&drained[0]), "resume work");
}

// ---------------------------------------------------------------------------
// #2067 — cross-profile goal frames must not cross the profile scope filter.
//
// `SessionGoalUpdated` / `SessionGoalCleared` are appended DURABLY (the
// `record_autonomy_rpc_evidence` -> `send_notification_durable` path at the raw
// autonomy RPC dispatch), so they reach every connection subscribed to the wire
// session key through all three delivery boundaries: the `replay.retain` in
// `open_session_result`, the session/open replay send loop, and the live
// forwarder pump. Before the fix all three passed them through `_ => true`.
// ---------------------------------------------------------------------------

/// Drain every frame `handle_session_open` queued, up to and including the
/// `session/open` NOTIFICATION it direct-sends last (distinguished from the
/// RPC result frame by having no `id`). Frames the replay loop filtered out
/// simply never appear.
async fn drain_session_open_frames(rx: &mut mpsc::Receiver<WsMessage>) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("session/open frames arrive within the timeout")
            .expect("ws stays open");
        let WsMessage::Text(text) = &frame else {
            panic!("expected a text frame, got {frame:?}");
        };
        let value: Value = serde_json::from_str(text).expect("json frame");
        let opened = value.get("id").is_none()
            && value.get("method").and_then(Value::as_str)
                == Some(octos_core::ui_protocol::methods::SESSION_OPEN);
        frames.push(value);
        if opened {
            return frames;
        }
    }
}

/// Read the next frame, or `None` when nothing arrives within `ms`.
async fn next_frame_within(rx: &mut mpsc::Receiver<WsMessage>, ms: u64) -> Option<Value> {
    match tokio::time::timeout(std::time::Duration::from_millis(ms), rx.recv()).await {
        Ok(Some(WsMessage::Text(text))) => Some(serde_json::from_str(&text).expect("json frame")),
        Ok(other) => panic!("expected a text frame, got {other:?}"),
        Err(_) => None,
    }
}

/// #2067 round 2 (H4) — `session/open` is ledger-appended for BROADCAST
/// (`open_session_result` tags it with the opening connection id precisely so
/// other connections observe it), and it carries `workspace_root`, the context
/// snapshot and pane snapshots. On a shared wire key those are another
/// tenant's paths and buffers.
///
/// Filtering it cannot starve its own opener: the opener's forwarder skips the
/// broadcast copy via `from_connection`, and its own frame arrives through the
/// UNFILTERED `send_ledger_event_durable` direct send at the end of
/// `handle_session_open`. And the scopes agree by construction —
/// `active_profile_id` is the same `Option` that `ledger_profile_id` is
/// `unwrap_or(_main)`-ed from, which is exactly how this filter normalizes.
#[tokio::test]
async fn should_drop_cross_profile_session_opened_frames_when_connection_scopes_another_profile() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = local_profile_state_with_sessions(temp.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Alpha Owner", "alpha", "alpha@example.com"),
    )
    .expect("create alpha profile");
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Beta Owner", "beta", "beta@example.com"),
    )
    .expect("create beta profile");

    let (ws, mut rx) = ws_connection_for_test(64);
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let approvals = PendingApprovalStore::default();
    let questions = PendingQuestionStore::default();
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let session_id = SessionKey("web-shared-opened".into());

    let beta_opened = |workspace_root: &str| {
        UiNotification::SessionOpened(SessionOpened {
            session_id: session_id.clone(),
            active_profile_id: Some("beta".to_owned()),
            workspace_root: Some(workspace_root.to_owned()),
            context: None,
            context_state: None,
            cursor: None,
            panes: None,
            capabilities: UiProtocolCapabilities::first_server_slice(),
            reasoning_effort: None,
            accepted_client_commands: None,
        })
    };

    // Beta opened this shared key first; its open frame is durable history.
    ledger.append_notification(beta_opened("/home/beta/secret-workspace"));

    let opened = handle_session_open(
        &ws,
        &state,
        &ledger,
        &approvals,
        &questions,
        &forwarders,
        Some("alpha"),
        Some("alpha"),
        ConnectionUiFeatures::default(),
        "open-alpha-shared".into(),
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        },
        false,
    )
    .await;
    assert!(opened, "alpha must be able to open the shared session");

    // The drain stops at the first `session/open` NOTIFICATION. Alpha must see
    // exactly one — its own — and never beta's workspace root.
    let frames = drain_session_open_frames(&mut rx).await;
    let opened_frames: Vec<&Value> = frames
        .iter()
        .filter(|frame| {
            frame.get("id").is_none()
                && frame.get("method").and_then(Value::as_str)
                    == Some(octos_core::ui_protocol::methods::SESSION_OPEN)
        })
        .collect();
    assert_eq!(
        opened_frames.len(),
        1,
        "alpha must receive exactly one session/open notification: {frames:#?}"
    );
    assert_eq!(
        opened_frames[0]["params"]["active_profile_id"],
        json!("alpha"),
        "the session/open alpha receives must be its OWN, not beta's replayed frame"
    );

    // Live: beta re-opens the shared key from another connection.
    ledger.append_notification(beta_opened("/home/beta/second-workspace"));
    assert!(
        next_frame_within(&mut rx, 250).await.is_none(),
        "beta's live session/open must not reach an alpha-scoped connection"
    );

    abort_live_forwarders(&forwarders, &ledger).await;
}

/// #2067 round 2 (H4) — `background/activity` carries RAW observed text (a
/// monitor's stdout lines, a fleet summary) and the ledger is its ENTIRE
/// delivery mechanism: the sink appends it over a detached `WsConnection`
/// whose writer is drained to nowhere, so every real client gets it from
/// replay or its live forwarder — both of which run this filter.
///
/// Filtering is safe because no production emitter passes `None`: the monitor
/// origin stamps `AutonomyMonitorRecord.profile_id` and the fleet origin
/// stamps `FleetRecord.profile_id`, both concrete and both normalized through
/// `resolve_autonomy_profile_id`. The monitor origin in particular carries the
/// SAME id as `monitor/fired`, which is already filtered — so this arm adds no
/// new starvation surface for it.
#[tokio::test]
async fn should_drop_cross_profile_background_activity_frames_when_connection_scopes_another_profile()
 {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = local_profile_state_with_sessions(temp.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Alpha Owner", "alpha", "alpha@example.com"),
    )
    .expect("create alpha profile");
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Beta Owner", "beta", "beta@example.com"),
    )
    .expect("create beta profile");

    let (ws, mut rx) = ws_connection_for_test(64);
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let approvals = PendingApprovalStore::default();
    let questions = PendingQuestionStore::default();
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let session_id = SessionKey("web-shared-activity".into());
    // `background/activity` is capability-gated; negotiate it or the frames
    // never reach the wire for reasons unrelated to profile scope.
    let features = ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        background_activity: true,
        ..Default::default()
    };

    let activity = |profile_id: &str, text: &str| {
        UiNotification::BackgroundActivity(octos_core::ui_protocol::BackgroundActivityEvent {
            session_id: session_id.clone(),
            profile_id: Some(profile_id.to_owned()),
            origin_kind: "monitor".to_owned(),
            origin_id: format!("monitor-{profile_id}"),
            origin_label: Some(format!("{profile_id}-tail")),
            text: text.to_owned(),
            emitted_at_ms: 1_760_000_000_000,
            dropped_count: None,
            suppressed: false,
        })
    };

    ledger.append_notification(activity("beta", "beta secret log line"));
    ledger.append_notification(activity("alpha", "alpha log line"));

    let opened = handle_session_open(
        &ws,
        &state,
        &ledger,
        &approvals,
        &questions,
        &forwarders,
        Some("alpha"),
        Some("alpha"),
        features,
        "open-alpha-activity".into(),
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        },
        false,
    )
    .await;
    assert!(opened, "alpha must be able to open the shared session");

    let frames = drain_session_open_frames(&mut rx).await;
    let replayed: Vec<String> = frames
        .iter()
        .filter(|frame| {
            frame.get("method").and_then(Value::as_str)
                == Some(octos_core::ui_protocol::methods::BACKGROUND_ACTIVITY)
        })
        .map(|frame| {
            frame["params"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        replayed,
        vec!["alpha log line".to_owned()],
        "replay must not put another tenant's observed text on an alpha connection"
    );

    // Live forwarder leg.
    ledger.append_notification(activity("beta", "beta live log line"));
    ledger.append_notification(activity("alpha", "alpha live log line"));

    let live = next_frame_within(&mut rx, 2_000)
        .await
        .expect("alpha's own live activity must still be forwarded");
    assert_eq!(
        live.get("method").and_then(Value::as_str),
        Some(octos_core::ui_protocol::methods::BACKGROUND_ACTIVITY),
        "the only live activity frame alpha may see is its own: {live}"
    );
    assert_eq!(live["params"]["text"], json!("alpha live log line"));
    assert!(
        next_frame_within(&mut rx, 250).await.is_none(),
        "beta's live activity must not reach an alpha-scoped connection"
    );

    abort_live_forwarders(&forwarders, &ledger).await;
}

fn state_with_sessions(data_dir: &std::path::Path) -> Arc<AppState> {
    Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir).expect("session manager"),
        ))),
        ..AppState::empty_for_tests()
    })
}

fn test_active_turn(turn_id: TurnId, abort: AbortHandle) -> ActiveTurn {
    let (tx, _rx) = mpsc::channel::<()>(1);
    ActiveTurn {
        dispatched_prompt: None,
        turn_cancel: tokio_util::sync::CancellationToken::new(),
        owner: None,
        turn_id,
        profile_id: MAIN_PROFILE_ID.to_owned(),
        state: Arc::new(TokioMutex::new(TurnState::Active)),
        interrupt_tx: Arc::new(TokioMutex::new(Some(tx))),
        steer: None,
        abort,
    }
}

#[tokio::test]
async fn session_open_replays_pending_approval_after_reconnect_without_cursor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:test".into());
    let approval_id = ApprovalId::new();
    approvals.request(ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        TurnId::new(),
        "shell",
        "Run command",
        "cargo test",
    ));

    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("open session should replay pending approval");

    assert!(outcome.replay.is_empty());
    assert_eq!(outcome.pending_approvals.len(), 1);
    assert_eq!(outcome.pending_approvals[0].approval_id, approval_id);
    assert_eq!(outcome.pending_approvals[0].title, "Run command");
}

// ---- UPCR-2026-023 pending-question reconnect + capability gating ----

fn sample_pending_question(
    session_id: SessionKey,
    question_id: QuestionId,
    turn_id: TurnId,
) -> UserQuestionRequestedEvent {
    use octos_core::ui_protocol::{UserQuestion, UserQuestionOption};
    UserQuestionRequestedEvent::new(
        session_id,
        question_id,
        turn_id,
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

// ----- UPCR-2026-007: capability advertisement on `SessionOpened` -----

// ===== M12 Phase D-1 auxiliary REST → WS negotiation =====

// ----- M9-γ-1: projection.envelope.v1 capability negotiation -----
//
// γ-1 wires capability negotiation only — no emit site references the
// new field yet, and legacy notifications continue to flow on the wire.
// The tests below mirror the `event.spawn_complete.v1` recipe and
// capture each of the six wiring sites (struct field, header parse,
// stdio defaults, requested-token rebuild, negotiated advertisement,
// notification methods list) so γ-2 emit-site wiring can land
// additively without touching the negotiation surface.

// ────────────────────────────────────────────────────────────────────
// Codex #1336 round-2 BLOCKER 1: direct-send capability filter
// ────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────
// Codex #1336 round-3 BLOCKER 1: M15 live-subagent fixture path
// ────────────────────────────────────────────────────────────────────
//
// Round-2 fix landed the per-connection capability filter on the
// direct-send helpers (`send_notification_ephemeral` /
// `send_notification_durable` / `send_notification_lifecycle`).
// Codex round-2 then flagged that the M15 fixture
// (`run_m15_live_subagent_process`) still emitted a raw
// `message/delta` via `send_raw_notification_ephemeral`, which
// jumps straight to `ws.send_ephemeral` and bypasses the gate.
// The fix routes the delta through
// `emit_envelope_for_legacy_notification` (canonical envelope
// dual-emit) + `send_notification_ephemeral` (filtered legacy
// ephemeral). The next three tests pin that contract.

// M11-E: `session_filesystem_profile_for_workspace` was deleted
// alongside `session_tool_registry`. Its server-wide containment
// semantics (cwd must live under the legacy agent's workspace_root)
// are obsolete in the multi-profile world — coding-agent UIs
// legitimately point sessions at arbitrary repos OUTSIDE the
// profile data_dir. The replacement gate is the path-safety check
// in `validate_session_workspace_path_safety` + the bootstrap-time
// re-check inside `SessionRuntime::bootstrap`. The
// `session_workspace_authorizes_approved_subdir` /
// `session_workspace_rejects_outside_root` tests that locked the
// old containment behavior in place are removed with the helper;
// the new path-safety check is covered by the M11-E acceptance
// tests below + the bootstrap-side coverage in
// `crate::runtime::session::tests`.

// M11-E: `session_tool_registry` and its Tier-1 / Tier-2 fallback
// helpers were deleted. The same Tier-1 invariant ("client-supplied
// cwd wins over the bootstrap default") now lives on
// `SessionRuntime::bootstrap`, exercised by
// `crate::runtime::session::tests::bootstrap_with_two_hints_yields_distinct_workspaces`
// and the M11-E acceptance tests
// `appui_session_with_custom_cwd_reads_supplied_workspace` +
// `two_appui_sessions_on_same_profile_with_different_cwds_isolated`
// below. Tier-2 (operator-default `appui.default_session_cwd`) is
// a known follow-up: the new `SessionRuntime::bootstrap` resolves
// workspace_hint at the per-session layer and does not yet honor a
// profile-scope operator default. Tracked alongside the M11
// shared-`validate_session_workspace_allowed`-helper TODO in
// `crate::runtime::session::validate_workspace_hint`.

/// Mirror of `handle_turn_interrupt`'s post-abort drain step. Used by
/// the interrupt-flow tests below to drive the store + ledger without
/// constructing a real `WsSink`.
fn drain_pending_approvals_for_interrupt(
    ledger: &UiProtocolLedger,
    approvals: &PendingApprovalStore,
    session_id: &SessionKey,
    turn_id: &TurnId,
) -> Vec<ApprovalCancelledEvent> {
    let cancelled = approvals.cancel_pending_for_turn(
        session_id,
        turn_id,
        approval_cancelled_reasons::TURN_INTERRUPTED,
    );
    let mut emitted = Vec::with_capacity(cancelled.len());
    for entry in cancelled {
        let event = ApprovalCancelledEvent::turn_interrupted(
            session_id.clone(),
            entry.approval_id,
            entry.turn_id,
        );
        ledger.append_notification(UiNotification::ApprovalCancelled(event.clone()));
        emitted.push(event);
    }
    emitted
}

#[tokio::test]
async fn cancelled_approval_replays_on_reconnect() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_id = ApprovalId::new();
    let approval = ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        turn_id.clone(),
        "shell",
        "Pending",
        "ls",
    );
    approvals.request(approval.clone());
    // The original approval/requested notification is in the durable
    // ledger (typical lifecycle when M9-FIX-01 is active).
    ledger.append_notification(UiNotification::ApprovalRequested(approval));

    let emitted = drain_pending_approvals_for_interrupt(&ledger, &approvals, &session_id, &turn_id);
    assert_eq!(emitted.len(), 1);

    // A reconnecting client with no cursor must rebuild from the ledger
    // replay; pending_for_session must NOT yield the cancelled approval
    // (otherwise the UI would re-render a fresh card after seeing the
    // cancellation event).
    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("session/open after cancellation");
    assert!(
        outcome.pending_approvals.is_empty(),
        "cancelled approvals must not surface as fresh pending replays",
    );

    // A reconnecting client *with* a pre-cancellation cursor must see
    // the durable approval/cancelled event in the cursor-bounded replay.
    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        },
    )
    .await
    .expect("session/open with cursor 0 replays everything");
    assert!(outcome.replay.iter().any(|event| matches!(
        &event.event,
        UiProtocolLedgerEvent::Notification(UiNotification::ApprovalCancelled(event))
            if event.approval_id == approval_id
                && event.reason == "turn_interrupted"
    )));
}

// TODO(M9-FIX-06): once ScopePolicy lands in this worktree, add a test
// verifying that approve_for_session scopes survive turn/interrupt while
// approve_for_turn and per-call pending entries are cancelled. The
// supervisor will reconcile the test during merge.

// ====================================================================
// M9-FIX-03 — interrupt/turn state-machine + TOCTOU repro
// ====================================================================

// ====================================================================
// M9-FIX-06 — `approval_scope` enforcement (#644)
//
// These tests sit at the `(PendingApprovalStore, ScopePolicy)` integration
// level. They mimic the exact recording sequence that
// `handle_approval_respond` performs after a successful `respond`, then
// probe `ScopePolicy::lookup` to verify auto-resolution. Going through
// `handle_approval_respond` itself would require a real WebSocket sink;
// the routing is exercised by the higher-level e2e suite.
// ====================================================================

// ====================================================================
// M9-FIX-04 — send-error handling + backpressure
// ====================================================================

// ====================================================================
// M9-FIX-07 — approval decision audit log + replay
// ====================================================================

#[tokio::test]
async fn reconnect_after_decision_replays_decided_event() {
    use chrono::Utc;
    use octos_core::ui_protocol::{ApprovalDecidedEvent, ApprovalRequestedEvent};

    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(64);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:reconnect".into());
    let approval_id = ApprovalId::new();
    let turn_id = TurnId::new();

    // Seed a pre-C1 anchor so the reconnect cursor can express "before
    // C1" — the cursor space starts at 1.
    let warmup = ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: turn_id.clone(),
        text: "preamble".into(),
    }));
    let request = ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        turn_id,
        "shell",
        "Run command",
        "cargo test",
    );
    approvals.request(request.clone());
    ledger.append_notification(UiNotification::ApprovalRequested(request));
    let outcome_decide = approvals
        .respond_with_context(ApprovalRespondParams::new(
            session_id.clone(),
            approval_id.clone(),
            ApprovalDecision::Approve,
        ))
        .expect("decide");
    let decided_turn_id = outcome_decide
        .context
        .as_ref()
        .map(|ctx| ctx.turn_id.clone())
        .expect("request was registered");
    ledger.append_notification(UiNotification::ApprovalDecided(ApprovalDecidedEvent {
        session_id: session_id.clone(),
        topic: None,
        approval_id: approval_id.clone(),
        turn_id: decided_turn_id,
        decision: ApprovalDecision::Approve,
        scope: Some("session".into()),
        decided_at: Utc::now(),
        decided_by: "user:tester".into(),
        auto_resolved: false,
        policy_id: None,
        client_note: None,
    }));

    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_ref: None,
            client_commands: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(warmup.cursor.clone()),
        },
    )
    .await
    .expect("reconnect should succeed");

    let mut saw_requested = false;
    let mut saw_decided = false;
    for event in &outcome.replay {
        match &event.event {
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(e))
                if e.approval_id == approval_id =>
            {
                saw_requested = true;
            }
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalDecided(e))
                if e.approval_id == approval_id =>
            {
                saw_decided = true;
                assert_eq!(e.decision, ApprovalDecision::Approve);
                assert_eq!(e.scope.as_deref(), Some("session"));
            }
            _ => {}
        }
    }
    assert!(saw_requested, "replay missing approval/requested");
    assert!(saw_decided, "replay missing approval/decided");
    assert!(outcome.pending_approvals.is_empty());
}

// ====================================================================
// M9-06 — terminal task lifecycle durability under WS backpressure
// ====================================================================

// ====================================================================
// Gap-1 unification: parity contract over the SINGLE terminal sink
// (`route_terminal_event_to_continuation_queue`). The same consumer fn
// is wired via `set_on_terminal` in every runtime mode (WS, gateway,
// headless drain), so driving it directly proves the cross-mode parity:
// exactly one continuation, correct reason, enqueued under the resolved
// runtime profile (ZERO under `_main`), idempotent under repeated
// terminal marks, recovery prompt body unchanged for failure-with-ack.
// ====================================================================

/// Build a production-shaped spawn_only `BackgroundTask` for the matrix.
fn unified_terminal_test_task(
    id: &str,
    session: &SessionKey,
    tool_call_id: &str,
    status: octos_agent::TaskStatus,
    error: Option<&str>,
) -> octos_agent::BackgroundTask {
    let now = chrono::Utc::now();
    let runtime_state = match status {
        octos_agent::TaskStatus::Completed => octos_agent::TaskRuntimeState::Completed,
        octos_agent::TaskStatus::Failed => octos_agent::TaskRuntimeState::Failed,
        octos_agent::TaskStatus::Cancelled => octos_agent::TaskRuntimeState::Cancelled,
        _ => octos_agent::TaskRuntimeState::ExecutingTool,
    };
    octos_agent::BackgroundTask {
        id: id.into(),
        tool_name: "mofa_slides".into(),
        tool_call_id: tool_call_id.into(),
        parent_session_key: Some(session.to_string()),
        child_session_key: None,
        child_terminal_state: None,
        child_join_state: None,
        child_joined_at: None,
        child_failure_action: None,
        task_ledger_path: None,
        status,
        runtime_state,
        runtime_detail: None,
        started_at: now,
        updated_at: now,
        completed_at: Some(now),
        output_files: vec![],
        error: error.map(str::to_owned),
        final_output: None,
        failed_by_observer: false,
        session_key: Some(session.to_string()),
        tool_input: Some(serde_json::json!({"topic": "rust"})),
        originating_client_message_id: Some("cmid-unified".into()),
        source: None,
        role: None,
        summary: None,
        artifact_count: None,
        runtime_policy_stamp: None,
        projection_metadata: None,
        workspace_root: None,
        relaunched_from: None,
    }
}

/// failure-WITH-ack across the unified sink: exactly one recovery
/// continuation under the resolved profile (ZERO under `_main`), the
/// recovery prompt body unchanged, idempotent under repeated marks.
#[tokio::test(flavor = "current_thread")]
async fn unified_terminal_sink_failure_with_ack_queues_recovery_under_profile() {
    let session_id = SessionKey("web:tester#unified-fail-ack".to_owned());
    let profile_id = "test-unified-fail-ack";

    let task = unified_terminal_test_task(
        "01900000-0000-7000-8000-0000000fa001",
        &session_id,
        "call-unified-ack",
        octos_agent::TaskStatus::Failed,
        Some("plugin exited 137 (sigkill). available: a, b, c"),
    );
    let event = octos_agent::TerminalEvent {
        task: task.clone(),
        synth_ack_emitted: true,
        outcome: octos_agent::TerminalOutcome::Failed(octos_agent::SpawnOnlyFailureSignal {
            task_id: task.id.clone(),
            tool_name: task.tool_name.clone(),
            tool_input: task.tool_input.clone().unwrap(),
            error_message: task.error.clone().unwrap(),
            suggested_alternatives: vec!["a".into(), "b".into(), "c".into()],
            parent_session_key: task.parent_session_key.clone(),
            originating_client_message_id: task.originating_client_message_id.clone(),
        }),
    };

    // Drive the unified sink twice — idempotent collapse via dedupe key.
    crate::autonomy::agent_orchestrator::route_terminal_event_to_continuation_queue(
        &event,
        Some(profile_id),
    );
    crate::autonomy::agent_orchestrator::route_terminal_event_to_continuation_queue(
        &event,
        Some(profile_id),
    );

    assert_eq!(
        default_agent_orchestrator()
            .pending_continuation_count_for_session_for_test(&session_id, profile_id),
        1,
        "failure-with-ack must enqueue exactly one recovery continuation (idempotent)",
    );
    assert_eq!(
        default_agent_orchestrator()
            .pending_continuation_count_for_session_for_test(&session_id, MAIN_PROFILE_ID),
        0,
        "no recovery continuation may be stranded under '_main'",
    );

    let drained = default_agent_orchestrator().drain_ready_continuations_for_session(
        &session_id,
        profile_id,
        MasterContinuationRuntimeState::idle(),
        usize::MAX,
    );
    assert_eq!(drained.len(), 1, "exactly one continuation drains");
    assert!(matches!(
        drained[0].reason,
        MasterContinuationReason::External(ref kind)
            if kind == crate::autonomy::agent_orchestrator::SPAWN_ONLY_FAILURE_EXTERNAL_KIND
    ));
    let prompt = master_continuation_prompt(&drained[0]);
    assert!(
        prompt.contains("[system-internal]")
            && prompt.contains("mofa_slides")
            && prompt.contains("plugin exited 137"),
        "recovery prompt body must be unchanged (system-internal + tool + error); got: {prompt}",
    );
}

// ====================================================================
// PR G — UPCR-2026-009 / -010 / -011 / -012 handler tests
// ====================================================================

// ========================================================================
// Live ledger publish-subscribe (issue #760, Phase C blocker)
// ========================================================================

// ========================================================================
// UPCR-2026-014 M9-γ — per-connection envelope/legacy mutual exclusion.
// ========================================================================

// -- Codex PR #761 review fixes ----------------------------------------

// ----- M11-E: UI Protocol per-session workspace wiring -----------------

async fn make_m11e_profile_with_llm_and_sandbox(
    profile_id: &str,
    data_dir: &std::path::Path,
    llm: Arc<dyn octos_llm::LlmProvider>,
    sandbox: octos_agent::SandboxConfig,
) -> Arc<crate::runtime::ProfileRuntime> {
    std::fs::create_dir_all(data_dir).expect("profile data dir");
    let memory = Arc::new(
        octos_memory::EpisodeStore::open(data_dir)
            .await
            .expect("episode store"),
    );
    let memory_store = Arc::new(
        octos_memory::MemoryStore::open(data_dir)
            .await
            .expect("memory store"),
    );
    let tool_config = Arc::new(
        octos_agent::ToolConfigStore::open(data_dir)
            .await
            .expect("tool config store"),
    );
    let base_tools = octos_agent::ToolRegistry::with_builtins_and_sandbox(
        data_dir,
        octos_agent::create_sandbox(&sandbox),
    );
    let recall = Arc::new(
        octos_memory::RecallStore::open(data_dir, octos_memory::RecallConfig::default())
            .expect("recall store"),
    );
    Arc::new(crate::runtime::ProfileRuntime {
        profile_id: profile_id.to_string(),
        data_dir: data_dir.to_path_buf(),
        session_store_root: None,
        config: crate::config::Config::default(),
        artifact_retention: Default::default(),
        llm,
        goal_verifier_llm: None,
        adaptive_router: None,
        runtime_qos_catalog: None,
        credential_pool: None,
        content_classifier: None,
        primary_model_id: "m11e-stub".to_string(),
        provider_name: "stub".to_string(),
        credentials: HashMap::new(),
        skills_dir: None,
        plugin_env_template: Vec::new(),
        tool_policy: None,
        default_sandbox: sandbox,
        max_iterations: None,
        session_defaults: None,
        agent_profile: None,
        format_after_edit: false,
        snapshots: None,
        tool_specs: Arc::new(base_tools),
        plugin_tool_names: Vec::new(),
        skill_actions: Vec::new(),
        mcp_client: None,
        plugin_reload: None,
        plugin_dirs: Vec::new(),
        plugin_prompt_fragments: Vec::new(),
        plugin_hooks: Vec::new(),
        review_config: None,
        human_approval_rules: None,
        system_prompt: "test-system-prompt".to_string(),
        prompt_parts: crate::commands::gateway::prompt::GatewayPromptParts {
            uses_bundled_base: false,
            pre_memory: "test-system-prompt".to_string(),
            post_memory: String::new(),
        },
        memory,
        memory_store,
        recall,
        embedder: None,
        memory_inject_tokens: 2500,
        memory_refresh_enabled: false,
        memory_refresh: None,
        tool_config,
        cron_service: None,
        runtime_lifecycle: None,
        pipeline_factory: None,
        hook_executor: None,
        lane_routing: None,
        voice: crate::config::VoiceConfig::default(),
    })
}

async fn state_with_profile_llm(
    data_dir: &std::path::Path,
    profile_id: &str,
    llm: Arc<dyn octos_llm::LlmProvider>,
) -> (Arc<AppState>, Arc<crate::runtime::ProfileRuntime>) {
    state_with_profile_llm_and_sandbox(
        data_dir,
        profile_id,
        llm,
        octos_agent::SandboxConfig::default(),
    )
    .await
}

async fn state_with_profile_llm_and_sandbox(
    data_dir: &std::path::Path,
    profile_id: &str,
    llm: Arc<dyn octos_llm::LlmProvider>,
    sandbox: octos_agent::SandboxConfig,
) -> (Arc<AppState>, Arc<crate::runtime::ProfileRuntime>) {
    std::fs::create_dir_all(data_dir).expect("data dir");

    let sessions = Arc::new(tokio::sync::Mutex::new(
        octos_bus::SessionManager::open(data_dir).expect("session manager"),
    ));

    let profile_data_dir = data_dir.join("profiles").join(profile_id).join("data");
    let profile_runtime =
        make_m11e_profile_with_llm_and_sandbox(profile_id, &profile_data_dir, llm, sandbox).await;

    let mut profiles = HashMap::new();
    profiles.insert(profile_id.to_string(), profile_runtime.clone());

    let state = Arc::new(AppState {
        profiles,
        sessions: Some(sessions),
        ..AppState::empty_for_tests()
    });

    (state, profile_runtime)
}

// ---- #peer-awaiting-wake — real requester integration ----
//
// Drive the REAL `UiProtocolApprovalRequester::request_approval` /
// `SessionUserQuestionRequester::request_user_question` from a PEER session and
// assert the peer's park wakes its originator (master) via the process-global
// master-continuation scheduler — the exact seam `peer_fleet_synthesis` uses.
// No live LLM: the requester parks on its oneshot and we cancel it to unwind.

// ---- #1842 — close-while-parked: a CLOSED peer must not park ----------------
//
// Two independent mechanisms, tested separately (the full close-vs-park
// interleaving is not deterministically drivable through the real requester —
// there is no await point between its register and its post-check):
//   (a) `peer_close` ABORTS the peer's in-flight turn through the SAME
//       interrupt routine `turn/interrupt` uses (state `Active` →
//       `Interrupting` + a signal on the turn's `interrupt_tx`), which is the
//       only path `run_standalone_turn` honors — it aborts the INNER
//       `agent_task`, so the peer definitively stops.
//   (b) every park point GATES on the peer's `(profile, slug)` closed state,
//       derived from the RESOLVED runtime profile + the VALIDATED `peer-`
//       topic, with a precheck → register → post-check bracket.

// ----------------------------------------------------------------
// Wave4-A: router/status build + router/set_mode emission tests.
// ----------------------------------------------------------------

// ------------------------------------------------------------------
// DO-NOT-SHIP: a pathological over-cap RPC RESPONSE must NEVER strand
// the client request. When the primary frame for a request reply
// cannot fit (frame_for -> None), the helpers must emit a MINIMAL
// same-id JSON-RPC error reply (guaranteed tiny / under cap) so the
// client always gets a reply to its id — never a silent drop.
// ------------------------------------------------------------------

// ---------------------------------------------------------------------------
// peer/prepare (#1800)
// ---------------------------------------------------------------------------

// ----------------------------------------------------------------------------
// #20a — smart worktree fencing: when `peer_handoff` leaves `worktree`
// UNSPECIFIED, the host auto-fences on a collision predicate hit (① >1 active
// goal, ② master's tree on a non-default branch, ③ an unfenced peer already
// in flight). An explicit `worktree=false` is still honored — with a
// model-visible warning in `model_note` — and an explicit `true` fences
// WITHOUT touching the predicate (zero-cost short-circuit).
// ----------------------------------------------------------------------------

// ----------------------------------------------------------------------------
// #20c — concurrent dual-goal fixtures: the JOINT #20a + #20b behaviour with
// TWO active goals live in the same profile. 20a's single-predicate tests
// prove the predicate fires; these fixtures prove the end-to-end story a
// real campaign hits: goal_02's UNSPECIFIED peer_handoff auto-fences with no
// human steer and no warning, and — with goal_01 owning the main tree — a
// cross-goal `git checkout` on the shared tree is refused while the owner
// goal itself passes through.
// ----------------------------------------------------------------------------

// ----------------------------------------------------------------------------
// #peer-model — per-peer model lane (a peer runs its turns on a named
// `sub_provider` lane configured in the master's profile).
// ----------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// #peer-respond — master answers a BLOCKED peer (human-in-the-loop).
// The process-global pending STORE is the authority for "awaiting input" — not
// the filesystem — so these drive the store + `peer_respond_resolve` /
// `cancel_peer_pending_on_close` / `build_peer_list_callback` directly, WITHOUT
// a live LLM. Each test uses a UNIQUE peer slug so the process-global peer-wire
// registry never collides across parallel tests.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Child stream delta coalescing (#1799 follow-up)
// ---------------------------------------------------------------------------

// --- turn/steer: mid-turn prompt injection (codex parity) ---

/// Await the RPC response frame carrying `id`, skipping interleaved
/// notification frames (turn/started, projection envelopes, ...).
async fn recv_rpc_response_with_id(
    rx: &mut mpsc::Receiver<axum::extract::ws::Message>,
    id: &str,
) -> Value {
    for _ in 0..256 {
        let frame = recv_rpc_json(rx).await;
        if frame.get("id").and_then(Value::as_str) == Some(id) {
            return frame;
        }
    }
    panic!("no rpc response with id {id} within 256 frames");
}

// ---------------------------------------------------------------------------
// #1959 — goal-event generation guard: a stale update can't overtake a clear.
// ---------------------------------------------------------------------------

/// #1973 fix-round — the process-global event ledger's INIT (including disk
/// recovery) runs exactly once even when N callers race the first
/// `event_ledger()` call (global drain vs stdio connection at serve boot).
/// Before, recovery ran BEFORE the `get_or_init` install: two racers could
/// both replay the same JSONL and both synthesize orphan-terminal records,
/// interleaving appends before one loser's ledger was discarded.
///
/// Drives the extracted once-init core (`event_ledger_init_once`) with its
/// own `OnceLock` + 8 threads: exactly one runs the init closure (recovery
/// is inside it), every caller gets the same `Arc`, and the seeded orphan is
/// swept by exactly ONE synthesized terminal.
#[test]
fn event_ledger_recovery_runs_exactly_once_across_concurrent_initializers() {
    let temp = tempfile::tempdir().expect("tempdir");
    let session_id = SessionKey("local:init-race-orphan".into());
    // Seed a durable dir with a session whose last task row is NON-terminal,
    // so the recovery has a real orphan sweep to perform.
    {
        let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(temp.path().into()));
        let task: octos_core::ui_protocol::TaskUpdatedEvent = serde_json::from_value(json!({
            "session_id": session_id.0.clone(),
            "task_id": octos_core::TaskId::new().to_string(),
            "title": "ghost task",
            "state": "running",
        }))
        .expect("task event");
        ledger.append_notification(UiNotification::TaskUpdated(task));
    } // process "dies" — no terminal event

    static ONCE: OnceLock<Arc<UiProtocolLedger>> = OnceLock::new();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let dir: PathBuf = temp.path().into();
        handles.push(std::thread::spawn(move || {
            event_ledger_init_once(&ONCE, Some(dir))
        }));
    }
    let results: Vec<(Arc<UiProtocolLedger>, bool)> = handles
        .into_iter()
        .map(|handle| handle.join().expect("join init thread"))
        .collect();

    let installs = results
        .iter()
        .filter(|(_, installed_now)| *installed_now)
        .count();
    assert_eq!(
        installs, 1,
        "exactly one caller may run the init closure (and thus the recovery)"
    );
    for (ledger, _) in &results {
        assert!(
            Arc::ptr_eq(ledger, &results[0].0),
            "every racer must get the same installed ledger"
        );
    }

    // The single recovery swept the orphan exactly once.
    let (events, _) = results[0]
        .0
        .snapshot_with_cursor(&session_id, None)
        .expect("snapshot");
    let synthesized = events
        .iter()
        .filter(|event| match &event.event {
            UiProtocolLedgerEvent::Notification(UiNotification::TaskUpdated(task)) => {
                task.runtime_detail.as_deref() == Some("orphaned_by_restart")
            }
            _ => false,
        })
        .count();
    assert_eq!(
        synthesized, 1,
        "one recovery → one synthesized orphan terminal, never N interleaved sets"
    );
}

// ---------------------------------------------------------------------------
// #1935 — interactive-turn goal-completion sentinel (serve path). Before this,
// sentinel detection + the independent verifier lived ONLY inside the
// autonomous `goal_context` accountant: an INTERACTIVE turn whose reply ended
// in `<goal:complete>` did nothing (only the explicit goal_update tool
// worked). `run_interactive_sentinel_completion` is the extracted, testable
// sentinel block `run_standalone_turn`'s interactive branch now calls.
// ---------------------------------------------------------------------------

// ---- task-return-unconsumed-steer-inputs: accepted-but-undrained steers are
// returned to the client as `turn/steer_dropped` at turn end ----

// ----------------------------------------------------------------------------
// #27f (R3) — result.md single-writer ownership.
// ----------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// #48b — serve/UI forwarder path: `fallback_switch` rows from
// `spawn_router_failover_forwarder` (same shape as the gateway path).
// ---------------------------------------------------------------------------

/// #48b — marker-prefixed terminal message produces exactly the
/// malformed_exhausted decision (and the CLI appends ONLY that row).

#[tokio::test]
async fn bc9_sf1_disconnect_releases_build_cache_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let peers_root = tmp.path().join("peers");
    let slug = "bc9-disconnect";
    let slot = build_cache_peer::acquire_for_staging(
        &peers_root,
        tmp.path(),
        slug,
        None,
        None,
        &crate::build_cache::BuildCacheConfig {
            min_free_gb: 0,
            ..Default::default()
        },
    )
    .unwrap();
    let path = slot.path.clone();
    build_cache_slot_registry().park(build_cache_slot_registry_key(&peers_root, slug), slot);
    let session = SessionKey::with_profile_topic("cache", "api", "tab", &format!("peer-{slug}"));
    let turn = TurnId::new();
    let state = Arc::new(TokioMutex::new(TurnState::Active));
    build_cache_slot_registry()
        .reserve_staged(
            &build_cache_slot_registry_key(&peers_root, slug),
            &build_cache_turn_owner(&session, &turn, &state),
        )
        .unwrap();
    let active: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let connection: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let handle = tokio::spawn(std::future::pending::<()>());
    let mut entry = test_active_turn(turn.clone(), handle.abort_handle());
    entry.state = state.clone();
    active.lock().await.insert(session.clone(), entry);
    connection.lock().await.insert(
        session,
        ConnectionTurn {
            finished: std::future::pending().boxed().shared(),
            turn_id: turn,
            state,
        },
    );
    abort_connection_turns(
        &active,
        &connection,
        &ScopePolicy::default(),
        &UiProtocolLedger::new(16),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
    )
    .await;
    assert!(
        !path.join("holder.json").exists(),
        "disconnect must return the held cache slot"
    );
    assert!(handle.await.unwrap_err().is_cancelled());
}

// ---------------------------------------------------------------------------
// Recall index methods: `memory/search` filter + `memory/ingest` validation
// (docs/adr/personal-memory-tiers.md). Pure helpers; the handlers add only
// the identity → runtime resolution shared with `memory/overview`.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------
// Oversized hydrate: shrink low-value fields first, never blank a reply.
// A long coding session's `session/hydrate` reply is several MiB, spread
// over hundreds of reasoning / tool-output / reply strings. Truncating
// largest-first with a leftover budget blanked the biggest fields outright
// — often the assistant's own replies — with `[oversized field omitted]`.
// ---------------------------------------------------------------------

// --- server/shutdown: stop a local --solo `octos serve` from a UI client ---

// --- session keep-alive: an open Session must not age out of the runtime
// cache under a client that is simply reading (see
// APPUI_SESSION_KEEPALIVE_INTERVAL).

// ── UPCR-2026-036: `octos serve --host-managed` ─────────────────────────────

fn host_managed_peer_session(topic: &str) -> SessionKey {
    SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", "octosense", topic)
}

async fn external_approval_respond(
    external: bool,
    session_id: SessionKey,
) -> (Value, tokio::sync::oneshot::Receiver<ApprovalDecision>) {
    external_approval_respond_as(external, session_id, true).await
}

/// `own_turn`: whether the pending approval was raised by a turn of the
/// answering connection (the server records the owner on the approval).
async fn external_approval_respond_as(
    external: bool,
    session_id: SessionKey,
    own_turn: bool,
) -> (Value, tokio::sync::oneshot::Receiver<ApprovalDecision>) {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let (ws, mut rx) = ws_connection_for_test(32);
    let (host_ws, _host_rx) = ws_connection_for_test(32);
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let approval_id = ApprovalId::new();
    // The same client-chosen turn id either way: ownership is the
    // connection's, never the id's.
    let turn_id = TurnId::new();
    let owner = if own_turn { &ws } else { &host_ws };
    let decision_rx = contracts.approvals.request_runtime_owned(
        ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id,
            "shell",
            "Run command",
            "cargo test",
        ),
        Some(owner.connection_id().0),
    );
    let external_owner = external.then(|| ws.connection_id());
    let mut respond =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
    respond.approval_scope = Some("approve_for_session".into());
    handle_approval_respond(
        &ws,
        &state,
        &ledger,
        &contracts,
        Some(MAIN_PROFILE_ID),
        external_owner,
        "respond".into(),
        respond,
    )
    .await;
    if external {
        assert!(
            contracts.scopes.list_for_session(&session_id).is_empty(),
            "an external answer never records a session-wide scope"
        );
    }
    (recv_rpc_json(&mut rx).await, decision_rx)
}

#[tokio::test]
async fn should_refuse_an_external_answer_to_a_host_owned_peer_approval() {
    for topic in ["peer-rinx", "peerctx-rinx.app-a"] {
        let (reply, mut decision) =
            external_approval_respond(true, host_managed_peer_session(topic)).await;
        assert_eq!(
            reply["error"]["data"]["kind"],
            json!(super::super::host_managed::HOST_OWNED_PEER_ANSWER_DENIED),
            "{topic}: {reply}"
        );
        assert!(
            decision.try_recv().is_err(),
            "{topic}: the approval stays parked for the person"
        );
    }
}

#[tokio::test]
async fn should_let_an_external_client_answer_only_its_own_turns_approvals_once() {
    let session = host_managed_peer_session("system");
    // A host turn's approval on the shared system conversation: refused.
    let (reply, mut decision) = external_approval_respond_as(true, session.clone(), false).await;
    assert_eq!(
        reply["error"]["data"]["kind"],
        json!(super::super::host_managed::EXTERNAL_TURN_DENIED),
        "{reply}"
    );
    assert!(
        decision.try_recv().is_err(),
        "the host's approval stays pending"
    );
    // Its own turn's approval: answered, once (no scope recorded).
    let (reply, decision) = external_approval_respond_as(true, session, true).await;
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(decision.await.unwrap(), ApprovalDecision::Approve);
}

// ---------------------------------------------------------------------------
// OctoSense ADR 0004 G1 (kernel half): an external client's approvals go only
// to that client (UPCR-2026-036).
// ---------------------------------------------------------------------------

/// A fresh system-like session per call: the approval side tables are
/// process-wide, so tests never share a session key.
fn g1_system_session() -> SessionKey {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    SessionKey(format!(
        "local:g1-octosense-{}#system",
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Run one approval request of a turn on `ws` in the background; return the
/// task and the approval once it is pending.
async fn g1_raise_approval(
    ws: &WsConnection,
    ledger: &Arc<UiProtocolLedger>,
    contracts: &Arc<UiProtocolContractStores>,
    state: &Arc<AppState>,
    session_id: &SessionKey,
) -> (tokio::task::JoinHandle<ToolApprovalDecision>, ApprovalId) {
    let turn_id = TurnId::new();
    let requester = UiProtocolApprovalRequester {
        ws: ws.clone(),
        ledger: Arc::clone(ledger),
        contracts: Arc::clone(contracts),
        state: Arc::clone(state),
        // Not a `peer-` topic, so the #1842 park gate never resolves.
        peers_root: std::path::PathBuf::from("/nonexistent/peers"),
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        features: ConnectionUiFeatures::default(),
    };
    let task = tokio::spawn(async move {
        <UiProtocolApprovalRequester as octos_agent::ToolApprovalRequester>::request_approval(
            &requester,
            ToolApprovalRequest {
                tool_id: "shell-1".into(),
                tool_name: "shell".into(),
                title: "Run command".into(),
                body: "ls".into(),
                command: Some("ls".into()),
                cwd: None,
                once_only: false,
                host_tool: None,
            },
        )
        .await
    });
    for _ in 0..500 {
        if let Some(pending) = contracts
            .approvals
            .pending_for_session(session_id)
            .into_iter()
            .find(|approval| approval.turn_id == turn_id)
        {
            return (task, pending.approval_id);
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("the approval never became pending");
}

/// The approval ids `session/open` lists as pending for `connection`.
async fn g1_opened_pending(
    state: &Arc<AppState>,
    ledger: &UiProtocolLedger,
    contracts: &UiProtocolContractStores,
    connection: ConnectionId,
    session_id: &SessionKey,
) -> Vec<ApprovalId> {
    open_session_result(
        state,
        ledger,
        &contracts.approvals,
        &contracts.user_questions,
        connection,
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_ref: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: None,
            client_commands: None,
        },
    )
    .await
    .expect("session/open")
    .pending_approvals
    .into_iter()
    .map(|approval| approval.approval_id)
    .collect()
}

async fn g1_state(temp: &std::path::Path, session_id: &SessionKey) -> Arc<AppState> {
    let state = state_with_sessions(temp);
    let sessions = state.sessions.as_ref().expect("sessions");
    sessions.lock().await.get_or_create(session_id).await;
    state
}

fn g1_question_features() -> ConnectionUiFeatures {
    ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        user_question_v1: true,
        ..ConnectionUiFeatures::stdio_defaults()
    }
}

/// Run one `ask_user_question` of a turn on `ws` in the background; return
/// the task and the question once it is pending.
async fn g1_ask_question(
    ws: &WsConnection,
    ledger: &Arc<UiProtocolLedger>,
    contracts: &Arc<UiProtocolContractStores>,
    state: &Arc<AppState>,
    session_id: &SessionKey,
) -> (
    tokio::task::JoinHandle<octos_agent::UserQuestionOutcome>,
    QuestionId,
) {
    use octos_agent::UserQuestionRequester as _;
    let turn_id = TurnId::new();
    let requester = SessionUserQuestionRequester {
        ws: ws.clone(),
        ledger: Arc::clone(ledger),
        contracts: Arc::clone(contracts),
        state: Arc::clone(state),
        peers_root: std::path::PathBuf::from("/nonexistent/peers"),
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
    };
    let task = tokio::spawn(async move {
        requester
            .request_user_question(octos_agent::UserQuestionRequest {
                questions: sample_pending_question(
                    SessionKey("unused".into()),
                    QuestionId::new(),
                    TurnId::new(),
                )
                .questions,
                title: "Pick a framework".to_owned(),
                body: "Which framework?".to_owned(),
            })
            .await
    });
    for _ in 0..500 {
        if let Some(pending) = contracts
            .user_questions
            .pending_for_session(session_id)
            .into_iter()
            .find(|question| question.turn_id == turn_id)
        {
            return (task, pending.question_id);
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("the question never became pending");
}

/// The question ids `session/open` lists as pending for `connection`.
async fn g1_opened_questions(
    state: &Arc<AppState>,
    ledger: &UiProtocolLedger,
    contracts: &UiProtocolContractStores,
    connection: ConnectionId,
    session_id: &SessionKey,
) -> Vec<QuestionId> {
    open_session_result(
        state,
        ledger,
        &contracts.approvals,
        &contracts.user_questions,
        connection,
        None,
        None,
        g1_question_features(),
        SessionOpenParams {
            session_ref: None,
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: None,
            client_commands: None,
        },
    )
    .await
    .expect("session/open")
    .pending_questions
    .into_iter()
    .map(|question| question.question_id)
    .collect()
}

fn g1_answer() -> Vec<UserQuestionAnswer> {
    vec![UserQuestionAnswer {
        selected_labels: vec!["axum".into()],
        free_text: None,
    }]
}

// ---------------------------------------------------------------------------
// #2625: an external client's prompts stay hidden from the host after a
// restart, or once the transport's bounded owner table forgot them: their
// ledger records carry a durable `external_prompt` marker (UPCR-2026-036).
// ---------------------------------------------------------------------------

#[path = "ui_protocol_prompt_durability_tests.rs"]
mod prompt_durability;

#[path = "ui_protocol_ws_lifetime_tests.rs"]
mod ws_lifetime_tests;

include!("ui_protocol_file_mutation_tests.rs");
include!("ui_protocol_terminal_time_tests.rs");

include!("ui_protocol_durability_tests.rs");

// T-0309: work-secret grants-file corruption must not strip ordinary browser
// turns of their existing capability, while the secret path stays fail-closed.
/// #924 allow-side: a request with no Origin header (or an empty/whitespace
/// one) must be ALLOWED — TUI / gateway / scripts send none. The deny tests
/// cannot substitute for this allow-side contract; losing it silently
/// regresses legitimate non-browser clients.
#[test]
fn ws_origin_gate_allows_absent_or_empty_origin() {
    let headers = HeaderMap::new();
    assert_eq!(
        decide_ws_origin_gate(&headers, None, &[], false),
        WsOriginDecision::Allow,
    );
    let mut headers = HeaderMap::new();
    headers.insert(axum::http::header::ORIGIN, "".parse().unwrap());
    assert_eq!(
        decide_ws_origin_gate(&headers, None, &[], false),
        WsOriginDecision::Allow,
    );
    let mut headers = HeaderMap::new();
    headers.insert(axum::http::header::ORIGIN, "   ".parse().unwrap());
    assert_eq!(
        decide_ws_origin_gate(&headers, None, &[], false),
        WsOriginDecision::Allow,
    );
}

/// T-0538: protocol-boundary sanitizer — tool_context rejects path
/// separators, >64 chars, and empty/whitespace input.
#[test]
fn should_normalize_safe_tool_context_at_protocol_boundary() {
    assert_eq!(
        normalize_tool_context(Some("  notebook  ")),
        Some("notebook".to_string())
    );
    assert_eq!(normalize_tool_context(None), None);
    assert_eq!(normalize_tool_context(Some("")), None);
    assert_eq!(normalize_tool_context(Some("notebook/other")), None);
    assert_eq!(normalize_tool_context(Some(&"a".repeat(65))), None);
}

mod corrupt_grants_tests {}

#[test]
fn peer_checkpoint_is_owned_intermediate_and_never_consumed_as_a_final_result() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let profile = "test-owned-checkpoint-v4";
    let slug = "maker";
    let caller = SessionKey::with_profile_topic(profile, "api", "writer", "peer-maker");
    let other = SessionKey::with_profile_topic(profile, "api", "other", "peer-maker");
    let owner = SessionKey::with_profile(profile, "api", "master");
    let dir = root.join(slug);
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("brief.md"), "Make an artifact").unwrap();
    std::fs::write(dir.join("originator"), &owner.0).unwrap();
    peer_wire_registry().register(peer_wire_key(profile, slug), caller.clone());
    assert!(publish_peer_checkpoint(root, profile, &other, "turn-a", "forged").is_err());
    assert!(
        publish_peer_checkpoint(
            root,
            profile,
            &caller,
            "turn-a",
            "attempt-a/cycle.json ready for review"
        )
        .is_ok()
    );
    assert!(!dir.join("result.md").exists());
    let receipts = Arc::new(StdMutex::new(HashMap::new()));
    let gather = build_peer_gather_callback_for_turn(
        root.into(),
        Some((owner, receipts.clone())),
        profile.into(),
    );
    let output = gather(Some(vec![slug.into()])).unwrap();
    assert!(output.contains("attempt-a/cycle.json ready for review"));
    assert!(output.contains("not completion"));
    assert!(receipts.lock().unwrap().is_empty());
    std::fs::write(dir.join("closed"), "closed").unwrap();
    assert!(publish_peer_checkpoint(root, profile, &caller, "turn-b", "after close").is_err());
    evict_peer_wire_session(&caller);
}

// ===========================================================================
// T-0548 / UPCR-2026-RD1002E7E — synchronous memory/entries, memory/edit,
// memory/delete over the session-scoped profile.
// ===========================================================================

/// Create a local solo profile and return its on-disk memory dir.
fn memory_dir_of(state: &AppState, profile_id: &str) -> std::path::PathBuf {
    let store = state.profile_store.as_ref().expect("profile store");
    let profile = store.get(profile_id).unwrap().expect("profile exists");
    store.resolve_data_dir(&profile).join("memory")
}

fn write_memory_entries(memory_dir: &std::path::Path, blocks: &[&str]) {
    std::fs::create_dir_all(memory_dir).unwrap();
    std::fs::write(
        memory_dir.join("MEMORY.md"),
        format!("{}\n", blocks.join("\n\n")),
    )
    .unwrap();
}

fn entry_version_of(memory_dir: &std::path::Path, entry_id: &str) -> String {
    let content = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
    let parsed = crate::memory_consolidate::entry::parse_memory_md(&content).unwrap();
    match parsed {
        crate::memory_consolidate::entry::ParsedMemory::Entries(entries) => entries
            .iter()
            .find(|e| e.id == entry_id)
            .map(|e| e.content_hash())
            .expect("entry exists"),
        _ => panic!("expected entries"),
    }
}

fn memory_user(profile_id: &str) -> AuthIdentity {
    AuthIdentity::User {
        id: profile_id.to_string(),
        role: UserRole::User,
    }
}

const MEM_E1: &str = "likes green tea (updated: 2026-01-01) ^m4k2abq";
const MEM_E2: &str = "lives in Lisbon (updated: 2026-01-05) ^mzzzzz2";

#[tokio::test]
async fn should_edit_memory_entry_verbatim_through_rpc_with_refresh_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state_with_sessions(dir.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada", "ada", "ada@example.com"),
    )
    .expect("create ada");
    // No profile runtime, no LLM, no refresh service: the edit must still
    // land (no model call, independent of the refresh toggle).
    let memory_dir = memory_dir_of(&state, "ada");
    write_memory_entries(&memory_dir, &[MEM_E1, MEM_E2]);
    let session = SessionKey::with_profile_topic("ada", "local", "web-1", "coding");
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_memory_entries(
        &ws,
        &state,
        &HeaderMap::new(),
        Some(&memory_user("ada")),
        true,
        "list-1".into(),
        octos_core::ui_protocol::MemoryEntriesParams {
            session_id: Some(session.to_string()),
        },
    )
    .await;
    let listed = recv_rpc_json(&mut rx).await;
    let entries = listed["result"]["entries"]
        .as_array()
        .expect("entries list");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["id"], json!("^m4k2abq"));
    assert_eq!(entries[0]["text"], json!(MEM_E1));
    assert_eq!(listed["result"]["truncated"], json!(false));
    assert_eq!(listed["result"]["total"], json!(2));
    let version = entries[0]["version"].as_str().unwrap().to_string();
    assert_eq!(version, entry_version_of(&memory_dir, "^m4k2abq"));

    handle_memory_edit(
        &ws,
        &state,
        &HeaderMap::new(),
        Some(&memory_user("ada")),
        true,
        "edit-1".into(),
        octos_core::ui_protocol::MemoryEditParams {
            session_id: Some(session.to_string()),
            entry_id: "^m4k2abq".into(),
            version,
            content: "prefers oolong".into(),
        },
    )
    .await;
    let edited = recv_rpc_json(&mut rx).await;
    assert_eq!(edited["result"]["entry_id"], json!("^m4k2abq"));
    let content = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
    let new_text = match crate::memory_consolidate::entry::parse_memory_md(&content).unwrap() {
        crate::memory_consolidate::entry::ParsedMemory::Entries(entries) => entries[0].text.clone(),
        _ => panic!("expected entries"),
    };
    assert!(
        new_text.starts_with("prefers oolong (updated: ") && new_text.ends_with(" ^m4k2abq"),
        "body is the user text verbatim with the kernel stamp and unchanged id: {new_text}"
    );
    assert_eq!(
        edited["result"]["version"],
        json!(crate::memory_consolidate::entry::sha256_hex(&new_text))
    );
}

#[tokio::test]
async fn should_conflict_on_stale_memory_edit_version_and_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state_with_sessions(dir.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada", "ada", "ada@example.com"),
    )
    .expect("create ada");
    let memory_dir = memory_dir_of(&state, "ada");
    write_memory_entries(&memory_dir, &[MEM_E1]);
    let before = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
    let session = SessionKey::with_profile_topic("ada", "local", "web-1", "coding");
    let (ws, mut rx) = ws_connection_for_test(8);
    let stale = "0".repeat(64);
    handle_memory_edit(
        &ws,
        &state,
        &HeaderMap::new(),
        Some(&memory_user("ada")),
        true,
        "edit-stale".into(),
        octos_core::ui_protocol::MemoryEditParams {
            session_id: Some(session.to_string()),
            entry_id: "^m4k2abq".into(),
            version: stale,
            content: "never stored".into(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(
        frame["error"]["code"],
        json!(octos_core::ui_protocol::rpc_error_codes::RESOURCE_CONFLICT)
    );
    assert_eq!(frame["error"]["data"]["kind"], json!("version_conflict"));
    assert_eq!(
        frame["error"]["data"]["current_version"],
        json!(entry_version_of(&memory_dir, "^m4k2abq"))
    );
    assert_eq!(
        std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap(),
        before,
        "a conflict changes nothing"
    );
}

#[tokio::test]
async fn should_report_not_found_for_another_profiles_memory_entry() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state_with_sessions(dir.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada", "ada", "ada@example.com"),
    )
    .expect("create ada");
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Grace", "grace", "grace@example.com"),
    )
    .expect("create grace");
    let ada_dir = memory_dir_of(&state, "ada");
    write_memory_entries(&ada_dir, &[MEM_E1]);
    write_memory_entries(&memory_dir_of(&state, "grace"), &[MEM_E2]);
    let before = std::fs::read_to_string(ada_dir.join("MEMORY.md")).unwrap();
    // Grace's session addresses Ada's entry id: it must NOT be found in
    // Grace's profile and Ada's file must not change.
    let grace_session = SessionKey::with_profile_topic("grace", "local", "web-9", "coding");
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_memory_delete(
        &ws,
        &state,
        &HeaderMap::new(),
        Some(&memory_user("grace")),
        true,
        "del-x".into(),
        octos_core::ui_protocol::MemoryDeleteParams {
            session_id: Some(grace_session.to_string()),
            entry_id: "^m4k2abq".into(),
            version: entry_version_of(&ada_dir, "^m4k2abq"),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(
        frame["error"]["code"],
        json!(octos_core::ui_protocol::rpc_error_codes::RESOURCE_NOT_FOUND)
    );
    assert_eq!(
        frame["error"]["data"]["resource_type"],
        json!("memory_entry")
    );
    assert_eq!(
        std::fs::read_to_string(ada_dir.join("MEMORY.md")).unwrap(),
        before
    );
    // Malformed ids are invalid_params, not not_found.
    handle_memory_delete(
        &ws,
        &state,
        &HeaderMap::new(),
        Some(&memory_user("grace")),
        true,
        "del-bad".into(),
        octos_core::ui_protocol::MemoryDeleteParams {
            session_id: Some(grace_session.to_string()),
            entry_id: "not-an-id".into(),
            version: "0".repeat(64),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["error"]["code"], json!(-32602));
}

#[tokio::test]
async fn should_delete_memory_entry_through_rpc_and_cascade_to_archives() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state_with_sessions(dir.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada", "ada", "ada@example.com"),
    )
    .expect("create ada");
    let memory_dir = memory_dir_of(&state, "ada");
    write_memory_entries(&memory_dir, &[MEM_E1, MEM_E2]);
    let archive_dir = memory_dir.join("archive");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::write(
        archive_dir.join("2026-01.md"),
        "old tea note (updated: 2025-12-01) ^m4k2abq\n",
    )
    .unwrap();
    let session = SessionKey::with_profile_topic("ada", "local", "web-1", "coding");
    let version = entry_version_of(&memory_dir, "^m4k2abq");
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_memory_delete(
        &ws,
        &state,
        &HeaderMap::new(),
        Some(&memory_user("ada")),
        true,
        "del-1".into(),
        octos_core::ui_protocol::MemoryDeleteParams {
            session_id: Some(session.to_string()),
            entry_id: "^m4k2abq".into(),
            version: version.clone(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["entry_id"], json!("^m4k2abq"));
    assert_eq!(frame["result"]["version"], json!(version));
    let content = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
    assert!(!content.contains("^m4k2abq") && content.contains("^mzzzzz2"));
    let archive = std::fs::read_to_string(archive_dir.join("2026-01.md"));
    assert!(
        archive.is_err() || !archive.as_deref().unwrap().contains("^m4k2abq"),
        "archived copy scrubbed too"
    );
}
// ---------------------------------------------------------------------------
// UPCR-2026-R48CB467B / T-0556 — `profile/skills/install_reviewed`: the
// reviewed-install method exists only on cores that implement the binding,
// and even there it is strict opt-in. An old core (or an unnegotiated
// connection) answers `method_not_supported`, never a silently-weakened
// install; the web simply has no entry point to offer.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reviewed_install_capability_gate_and_announcement() {
    // The feature and method are announced exactly when the profile skills
    // surface exists; a core without the profile store has no entry point.
    let state = AppState::empty_for_tests();
    let capabilities = ConnectionUiFeatures::default().advertised_capabilities(&state);
    assert!(
        !capabilities
            .supported_features
            .iter()
            .any(|feature| feature == "profile.skills.reviewed_install.v1")
    );
    assert!(
        !capabilities
            .supported_methods
            .iter()
            .any(|method| method == "profile/skills/install_reviewed")
    );
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let capabilities = ConnectionUiFeatures::default().advertised_capabilities(&state);
    assert!(
        capabilities
            .supported_features
            .iter()
            .any(|feature| feature == "profile.skills.reviewed_install.v1"),
        "the feature is announced wherever the skills surface exists"
    );
    assert!(
        capabilities
            .supported_methods
            .iter()
            .any(|method| method == "profile/skills/install_reviewed")
    );

    let state = Arc::new(AppState::empty_for_tests());
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let (ws, mut rx) = ws_connection_for_test(8);
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let call = |ws: &WsConnection, features, id: &str, params: Value| {
        let state = Arc::clone(&state);
        let ledger = Arc::clone(&ledger);
        let contracts = Arc::clone(&contracts);
        let active_turns = active_turns.clone();
        let connection_turns = connection_turns.clone();
        let ws = ws.clone();
        let id = id.to_string();
        async move {
            let request = RpcRequest::new(id.clone(), "profile/skills/install_reviewed", params);
            handle_raw_appui_rpc(
                &ws,
                &state,
                &ledger,
                &contracts,
                &active_turns,
                &connection_turns,
                features,
                None,
                id,
                &request,
            )
            .await
        }
    };
    // A connection that never negotiated the feature (and a client of an old
    // core, which lacks the method entirely) stops at the gate.
    let request_params = || {
        json!({
            "package": "demo",
            "repo": "octos-org/demo",
            "source_commit": "a".repeat(40),
            "reviewed_at": "2026-11-10T00:00:00Z",
            "platform": "linux-x86_64",
        })
    };
    assert!(call(&ws, ConnectionUiFeatures::default(), "r1", request_params()).await);
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(
        frame["error"]["code"],
        json!(octos_core::ui_protocol::rpc_error_codes::METHOD_NOT_SUPPORTED)
    );

    // Negotiated: the gate passes and the TYPED schema answers — required
    // identity fields may not be omitted, and unknown fields (a branch
    // substitute) are rejected rather than silently ignored.
    let features = ConnectionUiFeatures::from_requested_feature_tokens(
        ["profile.skills.reviewed_install.v1"],
        false,
    );
    assert!(call(&ws, features, "r2", json!({ "package": "demo" })).await);
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(
        frame["error"]["code"],
        json!(octos_core::ui_protocol::rpc_error_codes::INVALID_PARAMS)
    );
    let mut params = request_params();
    params["branch"] = json!("latest");
    assert!(call(&ws, features, "r3", params).await);
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(
        frame["error"]["code"],
        json!(octos_core::ui_protocol::rpc_error_codes::INVALID_PARAMS),
        "deny_unknown_fields must reject a branch substitute"
    );
}

/// UPCR-2026-R48CB467B / T-0563 — `profile/skills/registry/search` reports
/// the Core host platform (`server_platform`, the SAME `platform_key()`
/// `install_reviewed` validates against) so the web can build a legal
/// reviewed-install request for source-only and multi-platform packages
/// without guessing from the browser.
static REGISTRY_URL_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// UPCR-2026-R48CB467B / T-0563 — `profile/skills/registry/search` reports
/// the Core host platform (`server_platform`, the SAME `platform_key()`
/// `install_reviewed` validates against) so a client can build a legal
/// reviewed-install request for a source-only package (no `binaries` map to
/// read a key from) without guessing from the browser.
#[allow(unsafe_code)]
#[tokio::test]
async fn registry_search_reports_the_core_host_platform_for_reviewed_install() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let _env_lock = REGISTRY_URL_ENV_LOCK.lock().await;
    let host = crate::commands::skills::platform_key();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/registry.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "name": "source-only-pack",
            "description": "no binaries at all",
            "repo": "octos-org/source-only-pack",
            "skills": ["source_only_skill"],
            "source_commit": "a".repeat(40),
            "reviewed_at": "2026-11-10T00:00:00Z",
        }])))
        .mount(&server)
        .await;
    struct EnvGuard(&'static str);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: serialized by REGISTRY_URL_ENV_LOCK for this process.
            unsafe { std::env::remove_var(self.0) };
        }
    }
    // SAFETY: serialized by REGISTRY_URL_ENV_LOCK; restored by EnvGuard.
    unsafe {
        std::env::set_var(
            "OCTOS_SKILLS_REGISTRY_URL",
            format!("{}/registry.json", server.uri()),
        )
    };
    let _env_guard = EnvGuard("OCTOS_SKILLS_REGISTRY_URL");

    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada", "ada", "ada@example.com"),
    )
    .expect("create ada");
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let (ws, mut rx) = ws_connection_for_test(8);
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let call = |method: &str, features, id: &str, params: Value| {
        let (state, ledger, contracts) = (
            Arc::clone(&state),
            Arc::clone(&ledger),
            Arc::clone(&contracts),
        );
        let (active_turns, connection_turns) = (active_turns.clone(), connection_turns.clone());
        let (ws, id, method) = (ws.clone(), id.to_string(), method.to_string());
        async move {
            let request = RpcRequest::new(id.clone(), method, params);
            handle_raw_appui_rpc(
                &ws,
                &state,
                &ledger,
                &contracts,
                &active_turns,
                &connection_turns,
                features,
                None,
                id,
                &request,
            )
            .await
        }
    };

    // Source-only package: the projection still reports the host platform.
    assert!(
        call(
            "profile/skills/registry/search",
            ConnectionUiFeatures::default(),
            "s1",
            json!({"profile_id": "ada"}),
        )
        .await
    );
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["error"], Value::Null, "search must succeed: {frame}");
    let result = &frame["result"];
    let server_platform = result["server_platform"]
        .as_str()
        .expect("server_platform is reported");
    assert!(!server_platform.is_empty() && server_platform.len() <= 64);
    assert_eq!(
        server_platform, host,
        "same platform_key() install validates"
    );

    // A request built from server_platform passes the install platform gate;
    // a foreign platform is rejected BEFORE any registry fetch or disk write.
    let reviewed = ConnectionUiFeatures::from_requested_feature_tokens(
        ["profile.skills.reviewed_install.v1"],
        false,
    );
    let params = |platform: &str| {
        json!({
            "package": "source-only-pack",
            "repo": "octos-org/source-only-pack",
            "source_commit": "a".repeat(40),
            "reviewed_at": "2026-11-10T00:00:00Z",
            "platform": platform,
            "profile_id": "ada",
        })
    };
    assert!(
        call(
            "profile/skills/install_reviewed",
            reviewed,
            "i1",
            params("plan9-8086")
        )
        .await
    );
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(
        frame["error"]["data"]["kind"],
        json!("reviewed_install_platform_unavailable"),
        "wrong platform fails closed before any network/disk work: {frame}"
    );
    assert!(
        call(
            "profile/skills/install_reviewed",
            reviewed,
            "i2",
            params(server_platform)
        )
        .await
    );
    let frame = recv_rpc_json(&mut rx).await;
    assert_ne!(
        frame["error"]["data"]["kind"],
        json!("reviewed_install_platform_unavailable"),
        "the reported platform must be accepted by the install gate: {frame}"
    );
}

#[test]
fn reviewed_install_keeps_the_existing_profile_isolation_scope() {
    // The reviewed path resolves its target profile through the same
    // authenticated-scope gate as every other profile skills method: a
    // connection scoped to one profile can never install into another.
    let error = raw_profile_skill_profile_id(Some("other".to_string()), Some("alice"))
        .expect_err("a foreign profile_id must be refused");
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("auth_scope_violation"))
    );
    assert_eq!(
        raw_profile_skill_profile_id(Some("alice".to_string()), Some("alice"))
            .expect("the connection's own profile is in scope"),
        "alice"
    );
}
