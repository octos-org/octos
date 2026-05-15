use octos_agent::{
    ApprovalPolicy, EffectivePermissions, FilesystemScope, PermissionProfile, RuntimeMode,
    SandboxConfig, SandboxMode, ToolRegistry, create_sandbox,
};

#[test]
fn never_workspace_permissions_keep_workspace_sandbox_semantics() {
    let inherited = SandboxConfig::default();
    let permissions =
        EffectivePermissions::workspace_write().with_approval_policy(ApprovalPolicy::Never);
    let sandbox = permissions.apply_to_sandbox(&inherited);

    assert_eq!(permissions.approval_policy, ApprovalPolicy::Never);
    assert_eq!(permissions.filesystem_scope, FilesystemScope::Workspace);
    assert!(sandbox.enabled);
    assert_eq!(sandbox.mode, SandboxMode::Auto);
    assert!(!sandbox.allow_network);
}

#[test]
fn dangerous_profile_is_gated_to_solo_runtime() {
    assert!(
        EffectivePermissions::for_runtime(PermissionProfile::DangerFullAccess, RuntimeMode::Solo)
            .is_ok()
    );

    for runtime_mode in [RuntimeMode::Local, RuntimeMode::Tenant, RuntimeMode::Cloud] {
        let err =
            EffectivePermissions::for_runtime(PermissionProfile::DangerFullAccess, runtime_mode)
                .expect_err("dangerous must be rejected outside solo mode");
        assert_eq!(err.runtime_mode, runtime_mode);
        assert_eq!(err.requested, PermissionProfile::DangerFullAccess);
    }
}

#[test]
fn dangerous_permissions_disable_sandbox_and_allow_network() {
    let inherited = SandboxConfig {
        enabled: true,
        mode: SandboxMode::Docker,
        allow_network: false,
        ..SandboxConfig::default()
    };

    let sandbox = EffectivePermissions::danger_full_access().apply_to_sandbox(&inherited);
    assert!(!sandbox.enabled);
    assert_eq!(sandbox.mode, SandboxMode::None);
    assert!(sandbox.allow_network);

    let tmp = tempfile::tempdir().unwrap();
    let wrapped = create_sandbox(&sandbox).wrap_command("printf ok", tmp.path());
    let program = wrapped.as_std().get_program().to_string_lossy().to_string();
    #[cfg(windows)]
    assert_eq!(program, "cmd");
    #[cfg(not(windows))]
    assert_eq!(program, "sh");
}

#[tokio::test]
async fn workspace_file_tools_reject_absolute_paths_outside_cwd() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), "outside\n").unwrap();

    let permissions =
        EffectivePermissions::workspace_write().with_approval_policy(ApprovalPolicy::Never);
    let sandbox = create_sandbox(&permissions.apply_to_sandbox(&SandboxConfig::default()));
    let registry =
        ToolRegistry::with_builtins_and_permissions(workspace.path(), sandbox, permissions);

    let read = registry
        .execute(
            "read_file",
            &serde_json::json!({ "path": outside.path().to_string_lossy() }),
        )
        .await
        .unwrap();
    assert!(!read.success);
    assert!(read.output.contains("outside working directory"));

    let write = registry
        .execute(
            "write_file",
            &serde_json::json!({
                "path": outside.path().to_string_lossy(),
                "content": "blocked\n"
            }),
        )
        .await
        .unwrap();
    assert!(!write.success);
    assert!(write.output.contains("outside working directory"));
    assert_eq!(
        std::fs::read_to_string(outside.path()).unwrap(),
        "outside\n"
    );
}

#[tokio::test]
async fn read_only_permissions_deny_native_file_writes() {
    let workspace = tempfile::tempdir().unwrap();

    let permissions = EffectivePermissions::read_only().with_approval_policy(ApprovalPolicy::Never);
    let sandbox = create_sandbox(&permissions.apply_to_sandbox(&SandboxConfig::default()));
    let registry =
        ToolRegistry::with_builtins_and_permissions(workspace.path(), sandbox, permissions);

    let result = registry
        .execute(
            "write_file",
            &serde_json::json!({
                "path": "inside.txt",
                "content": "blocked\n"
            }),
        )
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.output.contains("read-only filesystem access"));
    assert!(!workspace.path().join("inside.txt").exists());
}

#[tokio::test]
async fn dangerous_host_scope_file_tools_can_touch_outside_cwd() {
    let workspace = tempfile::tempdir().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path().join("outside.txt");

    let permissions =
        EffectivePermissions::for_runtime(PermissionProfile::DangerFullAccess, RuntimeMode::Solo)
            .unwrap();
    let sandbox = create_sandbox(&permissions.apply_to_sandbox(&SandboxConfig::default()));
    let registry =
        ToolRegistry::with_builtins_and_permissions(workspace.path(), sandbox, permissions);

    let write = registry
        .execute(
            "write_file",
            &serde_json::json!({
                "path": outside.to_string_lossy(),
                "content": "host\n"
            }),
        )
        .await
        .unwrap();
    assert!(write.success, "write_file failed: {}", write.output);

    let read = registry
        .execute(
            "read_file",
            &serde_json::json!({ "path": outside.to_string_lossy() }),
        )
        .await
        .unwrap();
    assert!(read.success, "read_file failed: {}", read.output);
    assert!(read.output.contains("host"));
}

#[tokio::test]
async fn approval_never_returns_direct_tool_failure_for_ask_commands() {
    let workspace = tempfile::tempdir().unwrap();
    let permissions =
        EffectivePermissions::workspace_write().with_approval_policy(ApprovalPolicy::Never);
    let sandbox = create_sandbox(&permissions.apply_to_sandbox(&SandboxConfig::default()));
    let registry =
        ToolRegistry::with_builtins_and_permissions(workspace.path(), sandbox, permissions);

    let result = registry
        .execute(
            "shell",
            &serde_json::json!({ "command": "sudo printf nope" }),
        )
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.output.contains("approval_policy is never"));
    assert!(!result.output.contains("without interactive approval"));
}

#[tokio::test]
async fn dangerous_shell_uses_allow_all_policy() {
    let workspace = tempfile::tempdir().unwrap();
    let permissions =
        EffectivePermissions::for_runtime(PermissionProfile::DangerFullAccess, RuntimeMode::Solo)
            .unwrap();
    let sandbox = create_sandbox(&permissions.apply_to_sandbox(&SandboxConfig::default()));
    let registry =
        ToolRegistry::with_builtins_and_permissions(workspace.path(), sandbox, permissions);

    let result = registry
        .execute(
            "shell",
            &serde_json::json!({ "command": "printf danger-ok # rm -rf /" }),
        )
        .await
        .unwrap();

    assert!(result.success, "shell failed: {}", result.output);
    assert!(result.output.contains("danger-ok"));
}
