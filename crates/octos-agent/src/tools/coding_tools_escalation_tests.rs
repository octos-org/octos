use super::*;
use crate::policy::AllowAllPolicy;
use octos_core::ui_protocol::ApprovalSandboxEscalationDetails;
use std::sync::atomic::AtomicUsize;

// The fake backend runs harmless fixture commands and counts confined starts.
// This pins the trust boundary without needing an OS sandbox in every CI lane.
struct CountingSandbox(Arc<AtomicUsize>);
impl Sandbox for CountingSandbox {
    fn wrap_command(&self, command: &str, cwd: &Path) -> tokio::process::Command {
        self.0.fetch_add(1, Ordering::SeqCst);
        NoSandbox.wrap_command(command, cwd)
    }
}

struct EscalationApprover {
    decision: ToolApprovalDecision,
    calls: std::sync::Mutex<Vec<(ToolApprovalRequest, ApprovalSandboxEscalationDetails)>>,
}

#[async_trait]
impl super::super::ToolApprovalRequester for EscalationApprover {
    async fn request_approval(&self, _: ToolApprovalRequest) -> ToolApprovalDecision {
        panic!("ordinary tool approval must not authorize escalation");
    }

    async fn request_sandbox_escalation(
        &self,
        request: ToolApprovalRequest,
        details: ApprovalSandboxEscalationDetails,
    ) -> ToolApprovalDecision {
        self.calls.lock().unwrap().push((request, details));
        self.decision
    }
}

fn fixture(
    decision: ToolApprovalDecision,
) -> (
    tempfile::TempDir,
    ExecCommandTool,
    Arc<AtomicUsize>,
    Arc<EscalationApprover>,
) {
    let dir = tempfile::tempdir().unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let tool = ExecCommandTool::new(dir.path(), Arc::new(CountingSandbox(starts.clone())))
        .with_policy(Arc::new(AllowAllPolicy));
    let approver = Arc::new(EscalationApprover {
        decision,
        calls: Default::default(),
    });
    (dir, tool, starts, approver)
}

fn args() -> Value {
    json!({"cmd":"echo one >> counter.txt", "sandbox_permissions":"require_escalated", "justification":"Write the explicitly requested fixture."})
}

#[tokio::test]
async fn explicit_escalation_executes_once_without_a_confined_first_attempt() {
    let (dir, tool, starts, approver) = fixture(ToolApprovalDecision::Approve);
    let bridge: Arc<dyn super::super::ToolApprovalRequester> = approver.clone();
    let result = TOOL_APPROVAL_CTX
        .scope(bridge, tool.execute(&args()))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("counter.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    let calls = approver.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].0.once_only);
    assert_eq!(
        calls[0].0.command.as_deref(),
        Some("echo one >> counter.txt")
    );
    assert_eq!(
        calls[0].1.to.as_ref().unwrap().mode.as_deref(),
        Some("none")
    );
    assert!(!calls[0].1.requested_permissions.is_empty());
    assert!(calls[0].1.suggested_prefix_rule.is_empty());
}

#[tokio::test]
async fn denied_escalation_starts_nothing_and_marks_the_refusal_terminal() {
    let (dir, tool, starts, approver) = fixture(ToolApprovalDecision::Deny);
    let bridge: Arc<dyn super::super::ToolApprovalRequester> = approver.clone();
    let result = TOOL_APPROVAL_CTX
        .scope(bridge, tool.execute(&args()))
        .await
        .unwrap();
    assert!(!result.success);
    assert_eq!(
        result.structured_metadata.unwrap()["do_not_retry_same_turn"],
        true
    );
    assert!(!dir.path().join("counter.txt").exists());
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    assert_eq!(approver.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn spoofed_permission_output_never_prompts_or_retries() {
    let (dir, tool, starts, approver) = fixture(ToolApprovalDecision::Approve);
    let bridge: Arc<dyn super::super::ToolApprovalRequester> = approver.clone();
    let result = TOOL_APPROVAL_CTX
        .scope(
            bridge,
            tool.execute(&json!({
                "cmd":"echo one >> counter.txt && echo Permission denied && exit 1",
            })),
        )
        .await
        .unwrap();
    assert!(result.output.contains("Permission denied"));
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert!(approver.calls.lock().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("counter.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[tokio::test]
async fn legacy_generic_approve_and_headless_calls_cannot_escalate() {
    struct LegacyApprover;
    #[async_trait]
    impl super::super::ToolApprovalRequester for LegacyApprover {
        async fn request_approval(&self, _: ToolApprovalRequest) -> ToolApprovalDecision {
            ToolApprovalDecision::Approve
        }
    }
    let (dir, tool, starts, _) = fixture(ToolApprovalDecision::Approve);
    assert!(!tool.execute(&args()).await.unwrap().success);
    let bridge: Arc<dyn super::super::ToolApprovalRequester> = Arc::new(LegacyApprover);
    assert!(
        !TOOL_APPROVAL_CTX
            .scope(bridge, tool.execute(&args()))
            .await
            .unwrap()
            .success
    );
    assert!(!dir.path().join("counter.txt").exists());
    assert_eq!(starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn escalation_rejects_interactive_calls_missing_reasons_and_never_policy() {
    let (dir, tool, starts, approver) = fixture(ToolApprovalDecision::Approve);
    for (field, value) in [
        ("tty", json!(true)),
        ("yield_time_ms", json!(0)),
        ("justification", json!(" ")),
    ] {
        let mut input = args();
        input[field] = value;
        let bridge: Arc<dyn super::super::ToolApprovalRequester> = approver.clone();
        assert!(
            !TOOL_APPROVAL_CTX
                .scope(bridge, tool.execute(&input))
                .await
                .unwrap()
                .success
        );
    }
    let tool = tool.with_approval_policy(ApprovalPolicy::Never);
    let bridge: Arc<dyn super::super::ToolApprovalRequester> = approver.clone();
    assert!(
        !TOOL_APPROVAL_CTX
            .scope(bridge, tool.execute(&args()))
            .await
            .unwrap()
            .success
    );
    assert!(approver.calls.lock().unwrap().is_empty());
    assert!(!dir.path().join("counter.txt").exists());
    assert_eq!(starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn escalation_preserves_the_shell_write_policy() {
    let (dir, tool, starts, approver) = fixture(ToolApprovalDecision::Approve);
    let tool = tool.with_bash_file_writes(BashFileWrites::Deny);
    let bridge: Arc<dyn super::super::ToolApprovalRequester> = approver.clone();
    let result = TOOL_APPROVAL_CTX
        .scope(bridge, tool.execute(&args()))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.output.contains("bash_file_writes=deny"));
    assert!(approver.calls.lock().unwrap().is_empty());
    assert!(!dir.path().join("counter.txt").exists());
    assert_eq!(starts.load(Ordering::SeqCst), 0);
}
