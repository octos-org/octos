use super::*;

#[test]
fn approval_types_the_shell_aliases_like_shell() {
    let _guard = tool_risk_registry_test_lock().lock().unwrap_or_else(|e| {
        tool_risk_registry_test_lock().clear_poison();
        e.into_inner()
    });
    clear_tool_risk_registry_for_test();
    for tool in ["bash", "exec_command"] {
        let request = ToolApprovalRequest {
            tool_id: "tool-4".into(),
            tool_name: tool.into(),
            title: "Approve command".into(),
            body: "Run command: rm -rf build".into(),
            command: Some("rm -rf build".into()),
            cwd: Some("/workspace/octos".into()),
            once_only: false,
            host_tool: None,
        };
        let typed = approval_event_from_tool_request(
            request,
            SessionKey("local:test".into()),
            ApprovalId::new(),
            TurnId::new(),
            ConnectionUiFeatures {
                typed_approvals: true,
                ..ConnectionUiFeatures::default()
            },
        );
        assert_eq!(
            typed.approval_kind.as_deref(),
            Some(approval_kinds::COMMAND),
            "{tool}"
        );
        let wire = serde_json::to_value(&typed).unwrap();
        assert_eq!(
            wire["typed_details"]["command"]["command_line"], "rm -rf build",
            "{tool}"
        );
        assert_eq!(
            wire["typed_details"]["command"]["cwd"], "/workspace/octos",
            "{tool}"
        );
    }
    clear_tool_risk_registry_for_test();
}
