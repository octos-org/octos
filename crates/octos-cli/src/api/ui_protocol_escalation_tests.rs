use super::*;
use octos_agent::ToolApprovalRequester;
use octos_core::ui_protocol::{
    ApprovalSandboxEscalationDetails, ApprovalSandboxEscalationEndpoint,
};

fn escalation_request() -> (ToolApprovalRequest, ApprovalSandboxEscalationDetails) {
    (
        ToolApprovalRequest {
            tool_id: "exec-escalation-1".into(),
            tool_name: "exec_command".into(),
            title: "Run outside the sandbox once?".into(),
            body: "Run this exact command once with the server account's access.".into(),
            command: Some("echo fixture".into()),
            cwd: Some("/workspace".into()),
            // The transport must force this even if an in-process caller forgot.
            once_only: false,
            host_tool: None,
        },
        ApprovalSandboxEscalationDetails {
            from: Some(ApprovalSandboxEscalationEndpoint {
                mode: Some("confined".into()),
                network_access: None,
            }),
            to: Some(ApprovalSandboxEscalationEndpoint {
                mode: Some("none".into()),
                network_access: Some(true),
            }),
            requested_permissions: vec!["filesystem and network as the server account".into()],
            justification: Some("Read a user-requested file.".into()),
            suggested_prefix_rule: Vec::new(),
        },
    )
}

#[tokio::test]
async fn sandbox_escalation_requires_fresh_approval_and_cannot_record_a_scope() {
    let temp = tempfile::tempdir().unwrap();
    let session = g1_system_session();
    let state = g1_state(temp.path(), &session).await;
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn = TurnId::new();
    contracts.scopes.record(
        &session,
        ApprovalScopeKind::ApproveForTool,
        match_key_for(ApprovalScopeKind::ApproveForTool, "exec_command", &turn),
        ApprovalDecision::Approve,
    );
    let (writer, _reader) = std::sync::mpsc::sync_channel(64);
    let ws = WsConnection::new_stdio(writer);
    let requester = UiProtocolApprovalRequester {
        ws: ws.clone(),
        ledger: ledger.clone(),
        contracts: contracts.clone(),
        state: state.clone(),
        peers_root: temp.path().join("peers"),
        session_id: session.clone(),
        turn_id: turn.clone(),
        features: ConnectionUiFeatures::stdio_defaults(),
    };
    let task = tokio::spawn(async move {
        let (request, details) = escalation_request();
        requester.request_sandbox_escalation(request, details).await
    });
    let pending = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(
                !task.is_finished(),
                "cached approval must not answer escalation"
            );
            if let Some(event) = contracts
                .approvals
                .pending_for_session(&session)
                .into_iter()
                .next()
            {
                break event;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fresh escalation must park");
    assert_eq!(
        pending.approval_kind.as_deref(),
        Some(approval_kinds::SANDBOX_ESCALATION)
    );
    assert_eq!(pending.risk.as_deref(), Some("high"));
    let details = pending.typed_details.as_ref().unwrap();
    assert_eq!(
        details.command.as_ref().unwrap().command_line.as_deref(),
        Some("echo fixture")
    );
    assert_eq!(
        details
            .sandbox_escalation
            .as_ref()
            .unwrap()
            .to
            .as_ref()
            .unwrap()
            .mode
            .as_deref(),
        Some("none")
    );
    assert_eq!(
        pending
            .render_hints
            .as_ref()
            .unwrap()
            .default_decision
            .as_deref(),
        Some("deny")
    );
    let (foreign_ws, mut foreign_rx) = ws_connection_for_test(64);
    handle_approval_respond(
        &foreign_ws,
        &state,
        &ledger,
        &contracts,
        None,
        None,
        "foreign-escalation".into(),
        ApprovalRespondParams::new(
            session.clone(),
            pending.approval_id.clone(),
            ApprovalDecision::Approve,
        ),
    )
    .await;
    let rejected = recv_rpc_json(&mut foreign_rx).await;
    assert_eq!(
        rejected["error"]["data"]["kind"],
        "sandbox_escalation_owner_only"
    );
    assert!(
        !task.is_finished(),
        "a different connection cannot grant escalation"
    );
    let mut answer = ApprovalRespondParams::new(
        session.clone(),
        pending.approval_id,
        ApprovalDecision::Approve,
    );
    answer.approval_scope = Some("approve_for_session".into());
    handle_approval_respond(
        &ws,
        &state,
        &ledger,
        &contracts,
        None,
        None,
        "escalate-answer".into(),
        answer,
    )
    .await;
    assert_eq!(task.await.unwrap(), ToolApprovalDecision::Approve);
    assert!(
        contracts
            .scopes
            .lookup(&session, "unrelated_tool", &TurnId::new())
            .is_none(),
        "one-time approval must not widen session scope"
    );
    assert!(contracts.approvals.pending_for_session(&session).is_empty());
}

#[tokio::test]
async fn sandbox_escalation_refuses_remote_external_peer_and_untyped_sessions() {
    let temp = tempfile::tempdir().unwrap();
    for mode in [
        "remote",
        "external",
        "host_managed",
        "peer",
        "peer_context",
        "untyped",
    ] {
        let session = if mode == "peer" {
            SessionKey("local:escalation#peer-worker".into())
        } else if mode == "peer_context" {
            SessionKey("local:escalation#peerctx-worker-context".into())
        } else {
            g1_system_session()
        };
        let mut state = g1_state(temp.path(), &session).await;
        if mode == "host_managed" {
            Arc::get_mut(&mut state).unwrap().host_managed = Some(Arc::new(
                crate::api::host_managed::HostManaged::new(
                    "test-host-token-0123456789abcdef0123456789abcdef".into(),
                    None,
                    8765,
                )
                .unwrap(),
            ));
        }
        let (writer, _reader) = std::sync::mpsc::sync_channel(64);
        let (remote, _remote_reader) = ws_connection_for_test(64);
        let ws = if mode == "remote" {
            remote
        } else {
            WsConnection::new_stdio(writer)
        };
        if mode == "external" {
            ws.set_external(true);
        }
        let contracts = Arc::new(UiProtocolContractStores::default());
        let requester = UiProtocolApprovalRequester {
            ws,
            ledger: Arc::new(UiProtocolLedger::new(64)),
            contracts: contracts.clone(),
            state,
            peers_root: temp.path().join("peers"),
            session_id: session.clone(),
            turn_id: TurnId::new(),
            features: if mode == "untyped" {
                ConnectionUiFeatures::default()
            } else {
                ConnectionUiFeatures::stdio_defaults()
            },
        };
        let (request, details) = escalation_request();
        assert_eq!(
            requester.request_sandbox_escalation(request, details).await,
            ToolApprovalDecision::Deny,
            "{mode}"
        );
        assert!(contracts.approvals.pending_for_session(&session).is_empty());
    }
}

#[tokio::test]
async fn sandbox_escalation_cancellation_removes_the_pending_approval() {
    let temp = tempfile::tempdir().unwrap();
    let session = g1_system_session();
    let state = g1_state(temp.path(), &session).await;
    let contracts = Arc::new(UiProtocolContractStores::default());
    let (writer, _reader) = std::sync::mpsc::sync_channel(64);
    let requester = UiProtocolApprovalRequester {
        ws: WsConnection::new_stdio(writer),
        ledger: Arc::new(UiProtocolLedger::new(64)),
        contracts: contracts.clone(),
        state,
        peers_root: temp.path().join("peers"),
        session_id: session.clone(),
        turn_id: TurnId::new(),
        features: ConnectionUiFeatures::stdio_defaults(),
    };
    let task = tokio::spawn(async move {
        let (request, details) = escalation_request();
        requester.request_sandbox_escalation(request, details).await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while contracts.approvals.pending_for_session(&session).is_empty() {
            assert!(!task.is_finished());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(contracts.approvals.pending_for_session(&session).is_empty());
}
