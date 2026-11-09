//! Command approval policy.
//!
//! This module provides command approval before execution.
//! It's designed to be extended with codex-execpolicy when available.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

use crate::sandbox::{MountMode, SandboxConfig, SandboxMode};
use crate::tools::policy::BashFileWrites;

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
    /// #28d — the bash-file-writes knob carried from the session's
    /// ToolPolicy at LOAD time (default `allow` = zero behavior change).
    /// Consumed by the registry when it constructs the shell-family tools.
    pub bash_file_writes: BashFileWrites,
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
            bash_file_writes: BashFileWrites::default(),
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
            bash_file_writes: BashFileWrites::default(),
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
            bash_file_writes: BashFileWrites::default(),
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
    ///
    /// The sandbox is the layer that gates *what commands can write*
    /// (`CommandPolicy` only gates *which commands run*). A read-only
    /// profile must therefore make the shell/exec sandbox deny workspace
    /// writes, not just deny the native write/edit file tools — otherwise
    /// `octos chat --sandbox read-only` still lets `touch newfile` succeed
    /// via the shell (codex P1).
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
        // Mirror the file-tool write policy onto the shell sandbox: a
        // read-only profile mounts/binds the workspace read-only so
        // shell/bash/exec_command cannot write to it either. WorkspaceWrite
        // leaves the inherited (writable) sandbox untouched.
        if !self.file_access.allows_write() {
            sandbox.workspace_write = false;
            // Docker: NARROW a read-write mount to read-only, but never WIDEN
            // a fully-isolated `None` mount (no project access at all) up to
            // `ReadOnly` — that would grant read access the operator withheld
            // (codex P1 regression). Only ReadWrite -> ReadOnly is a narrowing.
            if sandbox.docker.mount_mode == MountMode::ReadWrite {
                sandbox.docker.mount_mode = MountMode::ReadOnly;
            }
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
/// `rm -rf /`, fork bombs). It is trivially bypassable — encoding tricks and
/// any command not on the short deny list pass through unblocked.
///
/// Matching (#1769, lite): a pure-Rust shell tokenizer
/// ([`crate::shell_analysis`]) checks patterns against the command position of
/// each pipeline segment, so quoted literals (`echo "don't rm -rf /"`) no
/// longer false-positive while quoted argv laundering (`rm "-rf" "/"`),
/// wrappers (`xargs rm -rf`, `ssh host '...'`) and `sh -c '...'` scripts
/// (fused `sh -c'...'` included) are still caught. Quoted text that something
/// executes keeps the legacy verdict: pipes into shells (`echo '...' | sh`),
/// shells reading stdin or script files, and inline interpreters
/// (`python -c`, `perl -e`). Unanalyzable constructs (command substitution,
/// `eval`, backslash escapes, `$VAR` in command position) fall back to the
/// legacy whitespace-normalized substring match for that segment — never less
/// strict than before (pinned by a differential corpus test in
/// `shell_analysis`).
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

/// Whether `command` names another process's environment or command line
/// (`/proc/<pid>/environ`, `/proc/<pid>/cmdline`, or anything else under a
/// process's `/proc/<pid>` view): where a parent or sibling process's secrets
/// live.
///
/// BEST-EFFORT DEFENSE IN DEPTH, not a boundary. It reads the command's words
/// as paths: quotes and backslashes are dropped, `.`/`..` are folded, glob
/// patterns (`*`, `?`, `[..]`, `{a,b}`) and `$VAR` words count as matching
/// whatever they could match, and `cd`/`pushd` to a relative or absolute
/// directory is followed, so `cat /pro[c]/1/env*` and
/// `cd /proc && cat 1/environ` are caught as well as the plain spelling. A
/// shell can still build a path this cannot see (`eval`, command
/// substitution, variables set earlier, a script file). The real controls
/// are the sandbox and, for the external clients of
/// `octos serve --host-managed`, having no shell at all.
pub fn names_process_secrets(command: &str) -> bool {
    names_process_secrets_in(command, None)
}

/// [`names_process_secrets`] for a command that runs in `cwd`.
pub fn names_process_secrets_in(command: &str, cwd: Option<&std::path::Path>) -> bool {
    let normalized: String = command
        .chars()
        .filter(|c| !matches!(c, '"' | '\'' | '\\'))
        .collect();
    // The plain spelling, whatever surrounds it.
    if normalized.contains("/proc/")
        && (normalized.contains("environ") || normalized.contains("cmdline"))
    {
        return true;
    }
    let words: Vec<&str> = normalized
        .split(|c: char| {
            c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')' | '`' | '=')
        })
        .filter(|word| !word.is_empty())
        .collect();
    let mut dir: Vec<String> = cwd
        .map(|cwd| path_components(&[], &cwd.to_string_lossy()))
        .unwrap_or_default();
    let mut mentions_proc = false;
    let mut names_secret_file = false;
    let mut index = 0;
    while index < words.len() {
        let word = words[index];
        if matches!(word, "cd" | "pushd") {
            if let Some(target) = words.get(index + 1) {
                dir = path_components(&dir, target);
                if reaches_process_view(&dir) {
                    return true;
                }
                mentions_proc |= dir
                    .first()
                    .is_some_and(|first| glob_could_match(first, "proc"));
            }
            index += 2;
            continue;
        }
        let path = path_components(&dir, word);
        if reaches_process_view(&path) {
            return true;
        }
        mentions_proc |= path
            .first()
            .is_some_and(|first| glob_could_match(first, "proc"));
        // `find /proc -name environ`: a secret file's name, spelled with at
        // least one literal letter (a bare `*` is too common to judge).
        names_secret_file |= word.split('/').any(|part| {
            part.chars().any(|c| c.is_ascii_alphabetic())
                && (glob_could_match(part, "environ") || glob_could_match(part, "cmdline"))
        });
        index += 1;
    }
    mentions_proc && names_secret_file
}

/// `word` as lexical path components, relative words resolved against `dir`
/// (the absolute components of the working directory, when known). Leading
/// `..` of an unresolvable relative path are dropped, so `../../proc/1` is
/// judged as `proc/1`.
fn path_components(dir: &[String], word: &str) -> Vec<String> {
    let mut parts: Vec<String> = if word.starts_with('/') {
        Vec::new()
    } else {
        dir.to_vec()
    };
    for part in word.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part.to_owned()),
        }
    }
    parts
}

/// Whether absolute components `path` could name `/proc/<pid>` or anything
/// under it (`self`, `thread-self`, a number, a glob or a `$VAR`).
fn reaches_process_view(path: &[String]) -> bool {
    let [first, second, ..] = path else {
        return false;
    };
    glob_could_match(first, "proc")
        && (second.starts_with('$')
            || ["self", "thread-self", "1", "4242"]
                .iter()
                .any(|name| glob_could_match(second, name)))
}

/// Whether the shell glob `pattern` (`*`, `?`, `[..]`, one level of `{a,b}`)
/// matches `name`; a `$VAR` pattern matches anything.
fn glob_could_match(pattern: &str, name: &str) -> bool {
    if pattern.contains('$') {
        return true;
    }
    if let (Some(open), Some(close)) = (pattern.find('{'), pattern.find('}')) {
        if open < close {
            let (head, rest) = pattern.split_at(open);
            let (group, tail) = rest[1..].split_at(close - open - 1);
            let tail = &tail[1..];
            return group
                .split(',')
                .any(|choice| glob_could_match(&format!("{head}{choice}{tail}"), name));
        }
    }
    fn matches(pattern: &[char], name: &[char]) -> bool {
        match pattern.split_first() {
            None => name.is_empty(),
            Some(('*', rest)) => (0..=name.len()).any(|skip| matches(rest, &name[skip..])),
            Some(('?', rest)) => !name.is_empty() && matches(rest, &name[1..]),
            Some(('[', rest)) => {
                let Some(end) = rest.iter().skip(1).position(|c| *c == ']').map(|p| p + 1) else {
                    return name.first() == Some(&'[') && matches(rest, &name[1..]);
                };
                let Some((first, remaining)) = name.split_first() else {
                    return false;
                };
                let (negated, set) = match rest[..end].split_first() {
                    Some(('!' | '^', set)) => (true, set),
                    _ => (false, &rest[..end]),
                };
                let mut hit = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        hit |= (set[i]..=set[i + 2]).contains(first);
                        i += 3;
                    } else {
                        hit |= set[i] == *first;
                        i += 1;
                    }
                }
                hit != negated && matches(&rest[end + 1..], remaining)
            }
            Some((c, rest)) => name.first() == Some(c) && matches(rest, &name[1..]),
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    matches(&pattern, &name)
}

impl CommandPolicy for SafePolicy {
    fn check(&self, command: &str, cwd: &std::path::Path) -> Decision {
        if names_process_secrets_in(command, Some(cwd)) {
            return Decision::Deny;
        }
        crate::shell_analysis::evaluate(command, &self.deny_patterns, &self.ask_patterns)
    }
}
