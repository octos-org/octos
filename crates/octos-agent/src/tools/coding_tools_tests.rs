use super::*;

#[cfg(unix)]
#[tokio::test]
async fn exec_cli_receives_the_host_continuation_identity_on_every_command_path() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ExecCommandTool::new(dir.path(), Arc::new(crate::sandbox::NoSandbox))
        .with_policy(Arc::new(crate::policy::AllowAllPolicy));
    for extra in [
        json!({}),
        json!({"yield_time_ms": 5000}),
        json!({"tty": true, "yield_time_ms": 5000}),
    ] {
        let mut ctx = ToolContext::zero();
        ctx.continuation_owner = Some("session:example:local:tui#peer-maker".into());
        let mut args = extra;
        args["cmd"] = json!("printf '%s' \"$OCTOS_CONTINUATION_OWNER\"");
        let result = TOOL_CTX.scope(ctx, tool.execute(&args)).await.unwrap();
        assert!(result.success);
        assert!(
            result
                .output
                .contains("session:example:local:tui#peer-maker"),
            "{}",
            result.output
        );
    }
}
