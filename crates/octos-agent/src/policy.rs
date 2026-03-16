//! Command approval policy.
//!
//! This module provides command approval before execution.
//! It's designed to be extended with codex-execpolicy when available.

use serde::{Deserialize, Serialize};

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

/// Policy that denies potentially dangerous commands.
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

impl CommandPolicy for SafePolicy {
    fn check(&self, command: &str, _cwd: &std::path::Path) -> Decision {
        let normalized = normalize_whitespace(command);

        // Check deny patterns first
        for pattern in &self.deny_patterns {
            if normalized.contains(pattern.as_str()) {
                return Decision::Deny;
            }
        }

        // Check ask patterns
        for pattern in &self.ask_patterns {
            if normalized.contains(pattern.as_str()) {
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
}
