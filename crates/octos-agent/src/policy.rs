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
    fn should_deny_reading_process_environments_and_command_lines() {
        let policy = SafePolicy::default();
        for command in [
            "cat /proc/1234/environ",
            "tr '\\0' '\\n' < /proc/$PPID/environ",
            "cat \"/proc/self/cmdline\"",
            "xxd /proc/1/task/1/environ",
        ] {
            assert_eq!(
                policy.check(command, Path::new("/tmp")),
                Decision::Deny,
                "{command}"
            );
        }
        assert_eq!(
            policy.check("cat /proc/cpuinfo", Path::new("/tmp")),
            Decision::Allow
        );
    }

    #[test]
    fn should_deny_process_secrets_behind_globs_and_relative_paths() {
        let policy = SafePolicy::default();
        for command in [
            "cd /proc && cat 1/environ",
            "cd /proc; cd self; cat environ",
            "pushd /proc/1 >/dev/null; cat env*",
            "cat /pro[c]/1/environ",
            "cat /proc/1/env*",
            "cat /pr?c/[0-9]/c*line",
            "cat /proc/{1,2}/environ",
            "cat /p*/1/*",
            "cat /tmp/../proc/1/environ",
            "cat ../../../proc/1/environ",
            "grep -r KEY /proc/1/",
            "find /proc -name environ",
            "tar cf - /proc/self",
        ] {
            assert_eq!(
                policy.check(command, Path::new("/tmp")),
                Decision::Deny,
                "{command}"
            );
        }
        // The working directory counts too.
        assert_eq!(
            policy.check("cat environ", Path::new("/proc/1")),
            Decision::Deny
        );
        for command in [
            "cat /proc/cpuinfo",
            "cat /proc/meminfo /proc/loadavg",
            "ls /proc",
            "cd /tmp && cat 1/environ.txt.bak",
            "grep -rn environ src/",
            "cargo test -p octos-agent",
        ] {
            assert_eq!(
                policy.check(command, Path::new("/tmp")),
                Decision::Allow,
                "{command}"
            );
        }
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

    // ---- #1769 (lite): quote-aware, command-position tokenizer tests ----

    #[test]
    fn should_allow_dangerous_text_when_inside_quoted_literal() {
        let policy = SafePolicy::default();
        // The motivating false positive: dangerous text inside a quoted
        // argument of a harmless command must no longer trip the denylist.
        assert_eq!(
            policy.check("echo \"don't rm -rf /\"", Path::new("/tmp")),
            Decision::Allow
        );
        assert_eq!(
            policy.check("echo 'rm -rf /'", Path::new("/tmp")),
            Decision::Allow
        );
        assert_eq!(
            policy.check(
                "git commit -m \"fix: don't rm -rf / on cleanup\"",
                Path::new("/tmp")
            ),
            Decision::Allow
        );
        // Ask patterns get the same treatment.
        assert_eq!(
            policy.check("echo \"use sudo carefully\"", Path::new("/tmp")),
            Decision::Allow
        );
        // Separators inside quotes are literal text, not command boundaries.
        assert_eq!(
            policy.check("echo \"a; rm -rf /\"", Path::new("/tmp")),
            Decision::Allow
        );
    }

    #[test]
    fn should_deny_quoted_args_when_command_position_dangerous() {
        let policy = SafePolicy::default();
        // Quoting must not LAUNDER a dangerous command: the argv is
        // identical after unquoting, so this is denied.
        assert_eq!(
            policy.check("rm \"-rf\" \"/\"", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("rm '-rf' '/'", Path::new("/tmp")),
            Decision::Deny
        );
    }

    #[test]
    fn should_catch_dangerous_verb_when_behind_pipe_or_wrapper() {
        let policy = SafePolicy::default();
        assert_eq!(
            policy.check("cat file | xargs rm -rf /", Path::new("/tmp")),
            Decision::Deny
        );
        // Wrapper + quoted args: only the tokenizer catches this one.
        assert_eq!(
            policy.check("ls | xargs \"rm\" \"-rf\" \"/\"", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("cat file.txt | xargs rm -rf", Path::new("/tmp")),
            Decision::Ask
        );
        assert_eq!(
            policy.check("env rm -rf /", Path::new("/tmp")),
            Decision::Deny
        );
    }

    #[test]
    fn should_catch_script_when_sh_dash_c() {
        let policy = SafePolicy::default();
        // The -c payload is a shell script: recurse instead of trusting the
        // quotes (quote-stripping alone would be LESS strict than today).
        assert_eq!(
            policy.check("sh -c 'rm -rf /'", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("bash -lc \"rm -rf /\"", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("sh -c 'echo hi; rm -rf /'", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("sh -c 'echo \"safe\"'", Path::new("/tmp")),
            Decision::Allow
        );
    }

    #[test]
    fn should_deny_command_position_when_after_separators() {
        let policy = SafePolicy::default();
        assert_eq!(
            policy.check("true; rm -rf /", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("true && rm -rf /", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("false || rm -rf /", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("sleep 1 & rm -rf /", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("echo `rm -rf /`", Path::new("/tmp")),
            Decision::Deny
        );
        assert_eq!(
            policy.check("echo $(rm -rf /)", Path::new("/tmp")),
            Decision::Deny
        );
    }

    #[test]
    fn should_stay_strict_when_dynamic_constructs() {
        let policy = SafePolicy::default();
        // eval makes the segment unanalyzable: fall back to the old
        // substring behavior, which scans through the quotes.
        assert_eq!(
            policy.check("eval \"rm -rf /\"", Path::new("/tmp")),
            Decision::Deny
        );
        // Command substitution in the segment: same fallback, so the quoted
        // text stays caught exactly as today.
        assert_eq!(
            policy.check("echo \"rm -rf /\" $(date)", Path::new("/tmp")),
            Decision::Deny
        );
        // Backslash escape outside quotes: unanalyzable segment, fallback.
        assert_eq!(
            policy.check("echo \"rm -rf /\" \\;", Path::new("/tmp")),
            Decision::Deny
        );
        // ${VAR}/$VAR in command position: unanalyzable, fallback keeps
        // today's verdict for the raw text.
        assert_eq!(policy.check("$rm -rf /", Path::new("/tmp")), Decision::Deny);
    }

    #[test]
    fn should_preserve_legacy_verdicts_when_unquoted_text() {
        let policy = SafePolicy::default();
        // Unquoted dangerous text in argument position stays caught (old
        // substring parity — only QUOTED literals were de-false-positived).
        assert_eq!(
            policy.check("echo rm -rf /", Path::new("/tmp")),
            Decision::Deny
        );
        // Fork bomb (pattern containing separators) still denied.
        assert_eq!(
            policy.check(":(){:|:&};:", Path::new("/tmp")),
            Decision::Deny
        );
        // Path-prefixed command still caught via substring parity.
        assert_eq!(
            policy.check("/bin/rm -rf /", Path::new("/tmp")),
            Decision::Deny
        );
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

    #[test]
    fn should_make_shell_sandbox_read_only_when_read_only_profile() {
        // P1 (codex): `--sandbox read-only` must stop shell writes too, not
        // just the native file tools. The resolved sandbox config must be
        // non-writable for the workspace so shell/exec cannot `touch newfile`.
        let base = SandboxConfig {
            enabled: true,
            mode: SandboxMode::Docker,
            allow_network: false,
            ..SandboxConfig::default()
        };
        let sandbox = EffectivePermissions::read_only().apply_to_sandbox(&base);

        // Sandbox stays enabled (it is what enforces the read-only boundary).
        assert!(sandbox.enabled);
        // Workspace writes are denied at the sandbox level.
        assert!(
            !sandbox.workspace_write,
            "ReadOnly profile must make the shell sandbox deny workspace writes"
        );
        // Docker mounts the workspace read-only.
        assert_eq!(
            sandbox.docker.mount_mode,
            crate::sandbox::MountMode::ReadOnly
        );
    }

    #[test]
    fn should_not_widen_docker_none_mount_to_read_only_when_read_only_profile() {
        // P1 (codex) regression guard: an inherited Docker config with
        // `MountMode::None` means the container has NO project access at all
        // (fully isolated). Downgrading it to `ReadOnly` for a read-only
        // profile would WIDEN access (none -> read), a security regression.
        // Only `ReadWrite` may be narrowed to `ReadOnly`; `None` stays `None`.
        let base = SandboxConfig {
            enabled: true,
            mode: SandboxMode::Docker,
            allow_network: false,
            docker: crate::sandbox::DockerConfig {
                mount_mode: crate::sandbox::MountMode::None,
                ..Default::default()
            },
            ..SandboxConfig::default()
        };
        let sandbox = EffectivePermissions::read_only().apply_to_sandbox(&base);

        // workspace_write is still forced false (read-only intent preserved).
        assert!(!sandbox.workspace_write);
        // But the mount mode must NOT be widened from None to ReadOnly.
        assert_eq!(
            sandbox.docker.mount_mode,
            crate::sandbox::MountMode::None,
            "None (fully isolated) must not be widened to ReadOnly by a read-only profile"
        );
    }

    #[test]
    fn should_keep_shell_sandbox_writable_when_workspace_write_profile() {
        // WorkspaceWrite must leave the workspace writable at the sandbox
        // level (default behaviour — the shell can create files in cwd).
        let base = SandboxConfig::default();
        let sandbox = EffectivePermissions::workspace_write().apply_to_sandbox(&base);

        assert!(sandbox.enabled);
        assert!(
            sandbox.workspace_write,
            "WorkspaceWrite must keep the sandbox workspace writable"
        );
        assert_eq!(
            sandbox.docker.mount_mode,
            crate::sandbox::MountMode::ReadWrite
        );
    }

    #[test]
    fn should_leave_sandbox_off_when_danger_full_access_profile() {
        // DangerFullAccess disables the sandbox entirely (unchanged): the
        // workspace_write flag is irrelevant because there is no sandbox.
        let base = SandboxConfig::default();
        let sandbox = EffectivePermissions::danger_full_access().apply_to_sandbox(&base);

        assert!(!sandbox.enabled);
        assert_eq!(sandbox.mode, SandboxMode::None);
    }
}
