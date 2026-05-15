//! Command approval policy.
//!
//! This module provides command approval before execution.
//! It's designed to be extended with codex-execpolicy when available.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

use crate::sandbox::{SandboxConfig, SandboxMode};

/// Decision for a command execution request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Allow the command to execute.
    Allow,
    /// Deny the command.
    Deny,
    /// Ask the user for approval.
    Ask,
}

/// Runtime approval behavior for commands that would otherwise ask a user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Ask an interactive client when a command policy returns [`Decision::Ask`].
    #[default]
    Ask,
    /// Never ask. Commands that would ask fail directly at the tool boundary.
    Never,
}

impl ApprovalPolicy {
    /// Whether this policy permits an interactive approval prompt.
    pub fn allows_prompt(self) -> bool {
        !matches!(self, Self::Never)
    }
}

/// Effective filesystem reach for cwd-bound tools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemScope {
    /// File tools must stay under the session workspace root.
    #[default]
    Workspace,
    /// File tools may target host paths outside the session workspace root.
    Host,
}

impl FilesystemScope {
    pub fn is_host(self) -> bool {
        matches!(self, Self::Host)
    }
}

/// Whether native file mutation tools are available.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccessMode {
    /// Reads and directory/search tools only. Write/edit tools fail directly.
    ReadOnly,
    /// Reads and writes are allowed, subject to [`FilesystemScope`].
    #[default]
    ReadWrite,
}

impl FileAccessMode {
    pub fn allows_write(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

/// Network policy recorded by the permission profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// Keep the inherited sandbox network setting.
    #[default]
    Inherit,
    /// Force network access on for the effective sandbox.
    Allowed,
}

/// User-facing permission profile resolved by the runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionProfile {
    /// Read-only workspace access.
    ReadOnly,
    /// Read/write access inside the workspace.
    #[default]
    WorkspaceWrite,
    /// Codex-style dangerous mode: no approvals, no sandbox, host filesystem.
    DangerFullAccess,
}

impl PermissionProfile {
    pub fn is_dangerous(self) -> bool {
        matches!(self, Self::DangerFullAccess)
    }
}

/// Runtime context used to gate dangerous profiles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    /// Local single-user coding mode.
    Solo,
    /// Local server/dashboard mode that may still host multiple profiles.
    #[default]
    Local,
    /// Tenant tunnel mode.
    Tenant,
    /// Hosted/cloud relay mode.
    Cloud,
}

impl RuntimeMode {
    pub fn allows_dangerous(self) -> bool {
        matches!(self, Self::Solo)
    }
}

/// Error returned when a requested permission profile is disallowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionProfileError {
    pub requested: PermissionProfile,
    pub runtime_mode: RuntimeMode,
}

impl fmt::Display for PermissionProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "permission profile {:?} is not allowed in {:?} runtime mode",
            self.requested, self.runtime_mode
        )
    }
}

impl std::error::Error for PermissionProfileError {}

/// Effective runtime permissions after profile + runtime-mode gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectivePermissions {
    pub permission_profile: PermissionProfile,
    pub approval_policy: ApprovalPolicy,
    pub filesystem_scope: FilesystemScope,
    pub file_access: FileAccessMode,
    pub network: NetworkPolicy,
}

impl Default for EffectivePermissions {
    fn default() -> Self {
        Self::workspace_write()
    }
}

impl EffectivePermissions {
    /// Workspace read/write defaults. Approval behavior remains interactive.
    pub fn workspace_write() -> Self {
        Self {
            permission_profile: PermissionProfile::WorkspaceWrite,
            approval_policy: ApprovalPolicy::Ask,
            filesystem_scope: FilesystemScope::Workspace,
            file_access: FileAccessMode::ReadWrite,
            network: NetworkPolicy::Inherit,
        }
    }

    /// Workspace read-only defaults. Approval behavior remains interactive.
    pub fn read_only() -> Self {
        Self {
            permission_profile: PermissionProfile::ReadOnly,
            approval_policy: ApprovalPolicy::Ask,
            filesystem_scope: FilesystemScope::Workspace,
            file_access: FileAccessMode::ReadOnly,
            network: NetworkPolicy::Inherit,
        }
    }

    /// Dangerous full host access. This is only valid after runtime-mode gating.
    pub fn danger_full_access() -> Self {
        Self {
            permission_profile: PermissionProfile::DangerFullAccess,
            approval_policy: ApprovalPolicy::Never,
            filesystem_scope: FilesystemScope::Host,
            file_access: FileAccessMode::ReadWrite,
            network: NetworkPolicy::Allowed,
        }
    }

    /// Resolve a requested permission profile for a concrete runtime mode.
    pub fn for_runtime(
        requested: PermissionProfile,
        runtime_mode: RuntimeMode,
    ) -> Result<Self, PermissionProfileError> {
        if requested.is_dangerous() && !runtime_mode.allows_dangerous() {
            return Err(PermissionProfileError {
                requested,
                runtime_mode,
            });
        }
        Ok(match requested {
            PermissionProfile::ReadOnly => Self::read_only(),
            PermissionProfile::WorkspaceWrite => Self::workspace_write(),
            PermissionProfile::DangerFullAccess => Self::danger_full_access(),
        })
    }

    /// Override approval behavior without changing sandbox or filesystem scope.
    pub fn with_approval_policy(mut self, approval_policy: ApprovalPolicy) -> Self {
        self.approval_policy = approval_policy;
        self
    }

    /// True only for the explicit dangerous full-access profile.
    pub fn is_dangerous(self) -> bool {
        self.permission_profile.is_dangerous()
    }

    /// Apply this permission profile to an inherited sandbox configuration.
    pub fn apply_to_sandbox(self, inherited: &SandboxConfig) -> SandboxConfig {
        let mut sandbox = inherited.clone();
        if self.is_dangerous() {
            sandbox.enabled = false;
            sandbox.mode = SandboxMode::None;
            sandbox.allow_network = true;
            return sandbox;
        }
        if matches!(self.network, NetworkPolicy::Allowed) {
            sandbox.allow_network = true;
        }
        sandbox
    }

    /// Build the shell command policy for these permissions.
    pub fn shell_command_policy(self) -> Arc<dyn CommandPolicy> {
        if self.is_dangerous() {
            Arc::new(AllowAllPolicy)
        } else {
            Arc::new(SafePolicy::default())
        }
    }
}

/// Policy for approving command execution.
pub trait CommandPolicy: Send + Sync {
    /// Check if a command should be allowed.
    fn check(&self, command: &str, cwd: &std::path::Path) -> Decision;
}

/// Default policy that allows all commands.
/// Use this for trusted environments.
pub struct AllowAllPolicy;

impl CommandPolicy for AllowAllPolicy {
    fn check(&self, _command: &str, _cwd: &std::path::Path) -> Decision {
        Decision::Allow
    }
}

/// Policy that denies a small set of obviously dangerous commands.
///
/// **Not a security boundary.** `SafePolicy` catches common accidents (e.g.,
/// `rm -rf /`, fork bombs) via simple pattern matching on whitespace-normalized
/// command strings. It is trivially bypassable — shell metacharacters, variable
/// expansion (`rm${IFS}-rf${IFS}/`), encoding tricks, and any command not on the
/// short deny list all pass through unblocked.
///
/// Real isolation must come from the sandbox layer ([`super::sandbox`]). Treat
/// `SafePolicy` as defense-in-depth for obvious mistakes, not as a guarantee
/// that dangerous commands cannot execute.
pub struct SafePolicy {
    /// Patterns that should be denied.
    deny_patterns: Vec<String>,
    /// Patterns that should always ask.
    ask_patterns: Vec<String>,
}

impl Default for SafePolicy {
    fn default() -> Self {
        Self {
            deny_patterns: vec![
                "rm -rf /".to_string(),
                "rm -rf /*".to_string(),
                "dd if=".to_string(),
                "mkfs".to_string(),
                ":(){:|:&};:".to_string(), // Fork bomb
                "chmod -R 777 /".to_string(),
            ],
            ask_patterns: vec![
                "sudo".to_string(),
                "rm -rf".to_string(),
                "git push --force".to_string(),
                "git reset --hard".to_string(),
            ],
        }
    }
}

/// Collapse consecutive whitespace into single spaces and trim.
fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Check if `pattern` appears in `haystack` at a word boundary.
///
/// A word boundary is start/end of string or a non-alphanumeric character.
/// This prevents "mkfs" from matching inside "unmkfsblah" or "sudo" inside "pseudocode".
fn contains_at_word_boundary(haystack: &str, pattern: &str) -> bool {
    let pat_bytes = pattern.as_bytes();
    let hay_bytes = haystack.as_bytes();
    if pat_bytes.len() > hay_bytes.len() {
        return false;
    }
    for i in 0..=(hay_bytes.len() - pat_bytes.len()) {
        if &hay_bytes[i..i + pat_bytes.len()] == pat_bytes {
            // Check left boundary: start of string or non-alphanumeric
            let left_ok = i == 0 || !hay_bytes[i - 1].is_ascii_alphanumeric();
            // Check right boundary: end of string or non-alphanumeric
            let right_ok = i + pat_bytes.len() == hay_bytes.len()
                || !hay_bytes[i + pat_bytes.len()].is_ascii_alphanumeric();
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

impl CommandPolicy for SafePolicy {
    fn check(&self, command: &str, _cwd: &std::path::Path) -> Decision {
        let normalized = normalize_whitespace(command);

        // Check deny patterns first
        for pattern in &self.deny_patterns {
            if contains_at_word_boundary(&normalized, pattern) {
                return Decision::Deny;
            }
        }

        // Check ask patterns
        for pattern in &self.ask_patterns {
            if contains_at_word_boundary(&normalized, pattern) {
                return Decision::Ask;
            }
        }

        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_allow_all_policy() {
        let policy = AllowAllPolicy;
        assert_eq!(policy.check("rm -rf /", Path::new("/")), Decision::Allow);
    }

    #[test]
    fn test_safe_policy_deny() {
        let policy = SafePolicy::default();
        assert_eq!(policy.check("rm -rf /", Path::new("/tmp")), Decision::Deny);
        assert_eq!(
            policy.check("dd if=/dev/zero of=/dev/sda", Path::new("/tmp")),
            Decision::Deny
        );
    }

    #[test]
    fn test_safe_policy_ask() {
        let policy = SafePolicy::default();
        assert_eq!(
            policy.check("sudo apt install foo", Path::new("/tmp")),
            Decision::Ask
        );
        assert_eq!(
            policy.check("git push --force origin main", Path::new("/tmp")),
            Decision::Ask
        );
    }

    #[test]
    fn test_safe_policy_whitespace_bypass() {
        let policy = SafePolicy::default();
        // Double-space and tab variants must still be caught
        assert_eq!(
            policy.check("rm  -rf  /", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("rm\t-rf\t/", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("git  push  --force origin main", Path::new("/tmp")),
            Decision::Ask
        );
    }

    #[test]
    fn test_safe_policy_allow() {
        let policy = SafePolicy::default();
        assert_eq!(
            policy.check("cargo build", Path::new("/tmp")),
            Decision::Allow
        );
        assert_eq!(
            policy.check("git status", Path::new("/tmp")),
            Decision::Allow
        );
    }

    #[test]
    fn test_safe_policy_word_boundary() {
        let policy = SafePolicy::default();
        // "sudo" should NOT match inside "pseudocode"
        assert_eq!(
            policy.check("pseudocode is fun", Path::new("/tmp")),
            Decision::Allow
        );
        // "mkfs" should NOT match inside "unmkfs"
        assert_eq!(
            policy.check("unmkfs something", Path::new("/tmp")),
            Decision::Allow
        );
        // But standalone "mkfs" should still be caught
        assert_eq!(
            policy.check("mkfs /dev/sda", Path::new("/tmp")),
            Decision::Deny
        );
        // And "sudo" standalone should still be caught
        assert_eq!(policy.check("sudo ls", Path::new("/tmp")), Decision::Ask);
        // Pattern at end of string
        assert_eq!(policy.check("run sudo", Path::new("/tmp")), Decision::Ask);
    }

    #[test]
    fn never_approval_does_not_imply_host_or_sandbox_bypass() {
        let base = SandboxConfig::default();
        let permissions =
            EffectivePermissions::workspace_write().with_approval_policy(ApprovalPolicy::Never);
        let sandbox = permissions.apply_to_sandbox(&base);

        assert_eq!(permissions.approval_policy, ApprovalPolicy::Never);
        assert_eq!(permissions.filesystem_scope, FilesystemScope::Workspace);
        assert_eq!(permissions.file_access, FileAccessMode::ReadWrite);
        assert!(sandbox.enabled);
        assert_eq!(sandbox.mode, SandboxMode::Auto);
        assert!(!sandbox.allow_network);
    }

    #[test]
    fn dangerous_profile_requires_solo_runtime() {
        for runtime_mode in [RuntimeMode::Local, RuntimeMode::Tenant, RuntimeMode::Cloud] {
            let err = EffectivePermissions::for_runtime(
                PermissionProfile::DangerFullAccess,
                runtime_mode,
            )
            .unwrap_err();
            assert_eq!(err.requested, PermissionProfile::DangerFullAccess);
            assert_eq!(err.runtime_mode, runtime_mode);
        }

        let permissions = EffectivePermissions::for_runtime(
            PermissionProfile::DangerFullAccess,
            RuntimeMode::Solo,
        )
        .expect("solo mode may opt into dangerous");
        assert!(permissions.is_dangerous());
        assert_eq!(permissions.approval_policy, ApprovalPolicy::Never);
        assert_eq!(permissions.filesystem_scope, FilesystemScope::Host);
    }

    #[test]
    fn dangerous_profile_disables_sandbox_and_allows_network() {
        let base = SandboxConfig {
            enabled: true,
            mode: SandboxMode::Docker,
            allow_network: false,
            ..SandboxConfig::default()
        };
        let sandbox = EffectivePermissions::danger_full_access().apply_to_sandbox(&base);

        assert!(!sandbox.enabled);
        assert_eq!(sandbox.mode, SandboxMode::None);
        assert!(sandbox.allow_network);
    }
}
