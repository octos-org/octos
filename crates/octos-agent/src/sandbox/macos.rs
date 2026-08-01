//! macOS sandbox using sandbox-exec.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use super::{BLOCKED_ENV_VARS, DEFAULT_READ_ALLOW_PATHS, Sandbox};

/// Pick a scratch temp dir for a READ-ONLY workspace that is provably OUTSIDE
/// the (already-canonicalized) `real_cwd`.
///
/// `candidate` is the natural choice derived from `std::env::temp_dir()` (which
/// honours `$TMPDIR`). A hostile parent can set `$TMPDIR` to a path UNDER the
/// workspace — either an ABSOLUTE path inside it, or a RELATIVE path like "tmp"
/// that the sandboxed process resolves against its cwd to `<cwd>/tmp`. If we
/// used it we would (a) mutate the read-only workspace when creating the dir and
/// (b) grant SBPL write inside it — both defeat read-only (codex P2, rounds
/// 3+5). So we first ABSOLUTIZE the candidate against `real_cwd` (a relative
/// candidate is joined onto cwd, an absolute one is left as-is), then
/// canonicalize its location; if it falls inside `real_cwd` we fall back to a
/// base rooted at `/private/tmp` (the real path of `/tmp` on macOS, independent
/// of `$TMPDIR`) which is guaranteed outside any normal workspace. The returned
/// path is always ABSOLUTE. This function performs NO filesystem mutation — the
/// caller creates the dir only after the path is validated safe.
fn read_only_scratch_dir(candidate: &Path, real_cwd: &Path) -> std::path::PathBuf {
    // Absolutize the candidate FIRST. A hostile `$TMPDIR` can be RELATIVE (e.g.
    // "tmp" or "./sub/tmp"); the sandboxed process resolves it relative to its
    // cwd — i.e. it lands at `<cwd>/tmp`, inside the read-only workspace. If we
    // canonicalized it while still relative, the absolute `real_cwd`-contains
    // check below would MISS it (absolute vs relative never match) and we would
    // accept it, then `create_dir_all` would mutate the read-only workspace
    // (codex P2, round 5). Joining a relative candidate onto `real_cwd` models
    // where it truly resolves so the containment check rejects it. An absolute
    // candidate is unchanged by the join.
    let candidate_abs = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        real_cwd.join(candidate)
    };

    // Resolve the candidate to a real path. It usually does not exist yet, so
    // canonicalize the nearest existing ancestor and re-attach the tail.
    let candidate_real = canonicalize_lexical(&candidate_abs);

    if !candidate_real.starts_with(real_cwd) {
        // Return the ABSOLUTIZED candidate (never the raw relative one) so the
        // caller creates the dir at the location we actually validated.
        return candidate_abs;
    }

    // Candidate is inside the read-only workspace: fall back to a location that
    // does not depend on $TMPDIR. `/private/tmp` is the canonical macOS temp
    // root; guard against the pathological case where the workspace itself is
    // under it by only using it when it is outside cwd.
    let unique = format!("octos-sandbox-ro.{}", std::process::id());
    for base in ["/private/tmp", "/private/var/tmp"] {
        let fallback = Path::new(base).join(&unique);
        if !canonicalize_lexical(&fallback).starts_with(real_cwd) {
            return fallback;
        }
    }
    // Extremely unlikely: even the system temp roots are inside cwd. Return the
    // first fallback anyway — the SBPL grant is scoped to it, and the caller's
    // "outside cwd" invariant is enforced by the write-rule guard below.
    Path::new("/private/tmp").join(unique)
}

/// Canonicalize `path` as far as it exists, re-attaching any non-existent tail.
/// Used to decide containment for a scratch dir that has not been created yet
/// (`canonicalize` fails on non-existent paths, so we resolve the longest
/// existing ancestor and re-append the remaining components).
fn canonicalize_lexical(path: &Path) -> std::path::PathBuf {
    let mut ancestor = path;
    // Names peeled off while walking up, in leaf-to-root order.
    let mut peeled: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(mut real) = std::fs::canonicalize(ancestor) {
            for name in peeled.iter().rev() {
                real.push(name);
            }
            return real;
        }
        match ancestor.parent() {
            Some(parent) if parent != ancestor => {
                if let Some(name) = ancestor.file_name() {
                    peeled.push(name);
                }
                ancestor = parent;
            }
            // Reached the root without finding an existing ancestor.
            _ => return path.to_path_buf(),
        }
    }
}

/// macOS sandbox using sandbox-exec.
pub struct MacosSandbox {
    pub(crate) allow_network: bool,
    /// When non-empty, restrict file-read* to these paths + cwd.
    /// Empty = allow all reads (backward compatible).
    pub(crate) read_allow_paths: Vec<String>,
    /// When `false`, the workspace cwd is NOT granted `file-write*`
    /// (read-only workspace for shell). `/dev/null` stays writable so
    /// shell redirections and git still work. Default constructions use
    /// `true` to preserve the historical writable-workspace behaviour.
    pub(crate) workspace_write: bool,
    /// TARGETED write to a repo's `.git` common dir (from a fleet worker's
    /// `FsGrant::Host`). `Some(<repo>/.git)` emits `(allow file-write* (subpath
    /// "<repo>/.git"))` ALONGSIDE the cwd grant — NOT a global `(allow
    /// file-write*)` (which would let a full-FS worker rewrite ANY host file,
    /// e.g. a LaunchAgent, ABOVE its grant) — so a Host-granted worktree worker's
    /// `git commit` can reach `<repo>/.git` (outside its cwd). Viable ONLY under
    /// an unrestricted-read profile (empty `read_allow_paths` → global
    /// `(allow file-read*)`), since git must also READ `<repo>/.git`; the pool
    /// gate (`supports_repo_git_write`) enforces that. Default `None` = today's
    /// cwd-only writable behaviour. The operator's explicit grant, NOT a fence.
    pub(crate) repo_git_write: Option<PathBuf>,
}

impl Sandbox for MacosSandbox {
    fn supports_repo_git_write(&self) -> bool {
        // A `(subpath "<repo>/.git")` rule grants the `.git` WRITE, but
        // `git commit` must also READ `<repo>/.git`. That read only holds under an
        // UNRESTRICTED-read profile (empty `read_allow_paths` → global
        // `(allow file-read*)`); a restricted-read profile would grant the write
        // but deny the read, so the worktree flow must fall back to scratch.
        self.read_allow_paths.is_empty()
    }

    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        let cwd_str = cwd.to_string_lossy();

        // Reject paths with control characters or SBPL metacharacters to prevent
        // sandbox profile injection. Fail closed: error instead of running unsandboxed.
        if cwd_str
            .bytes()
            .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
        {
            tracing::error!("cwd contains SBPL metacharacters, refusing to execute");
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg("echo 'sandbox error: cwd contains invalid characters' >&2; exit 1");
            return cmd;
        }

        // Path is validated above -- no escaping needed since \ and " are rejected.
        let cwd_escaped = &cwd_str;

        let network_rule = if self.allow_network {
            "(allow network*)"
        } else {
            "(deny network*)"
        };

        // Resolve cwd to its real path (macOS /tmp -> /private/tmp symlink).
        // SBPL subpath rules operate on real paths, so if cwd is /tmp/foo the
        // rule must use /private/tmp/foo or writes will be denied.
        let real_cwd = std::fs::canonicalize(cwd)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| cwd_escaped.to_string());
        // Validate the resolved path too
        if real_cwd
            .bytes()
            .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
        {
            tracing::error!("resolved cwd contains SBPL metacharacters, refusing to execute");
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg("echo 'sandbox error: resolved cwd contains invalid characters' >&2; exit 1");
            return cmd;
        }

        // Build file-read rules: global if no read_allow_paths, restricted otherwise
        let read_rules = if self.read_allow_paths.is_empty() {
            "(allow file-read*)".to_string()
        } else {
            let mut rules = Vec::new();
            // dyld needs to stat "/" during process startup (macOS Sequoia+).
            rules.push("(allow file-read* (literal \"/\"))".to_string());
            // Allow stat()/lstat() globally -- needed for getcwd(), realpath(),
            // and traversing parent directories of allowed subpaths. This only
            // permits metadata operations (file size, permissions, existence);
            // file-read-data (actual content reads) still requires subpath rules.
            rules.push("(allow file-read-metadata)".to_string());
            // Always allow reading the workspace (use canonical path for SBPL)
            rules.push(format!(
                "(allow file-read* (subpath \"{cwd}\"))",
                cwd = real_cwd
            ));
            // Add configured read paths -- validate each for SBPL metacharacters
            // to prevent sandbox profile injection (same check as cwd above).
            for path in &self.read_allow_paths {
                if path.bytes().any(|b| {
                    b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"'
                }) {
                    tracing::error!(
                        path = %path,
                        "read_allow_paths entry contains SBPL metacharacters, skipping"
                    );
                    continue;
                }
                rules.push(format!("(allow file-read* (subpath \"{path}\"))"));
            }
            // Add default system paths
            for path in DEFAULT_READ_ALLOW_PATHS {
                if !self.read_allow_paths.iter().any(|p| p == *path) && Path::new(path).exists() {
                    rules.push(format!("(allow file-read* (subpath \"{path}\"))"));
                }
            }
            rules.join("\n")
        };

        // Workspace write rule. Three cases:
        // - `repo_git_write` (a fleet worktree worker granted `FsGrant::Host`):
        //   grant `file-write*` to the cwd checkout AND a `(subpath "<repo>/.git")`
        //   so `git commit` can reach objects/refs/worktree-admin OUTSIDE the cwd.
        //   This is TARGETED, NOT a global `(allow file-write*)` — a full-FS
        //   worker must not be able to rewrite arbitrary host files (e.g. plant a
        //   LaunchAgent) ABOVE its grant. Only reached under unrestricted reads
        //   (see `supports_repo_git_write`), which the pool gate enforces. The
        //   `.git` path is validated for SBPL metacharacters; on failure it falls
        //   back to cwd-only write (fail closed — the commit fails, caught by the
        //   worker's branch-advance check, rather than injecting a rule).
        // - `workspace_write` (default): grant `file-write*` to the cwd subpath.
        // - neither (read-only profile): OMIT the grant so `(deny default)`
        //   denies the write. `/dev/null` stays writable regardless so shell
        //   redirections and git internals still function.
        let cwd_write_rule = format!("(allow file-write* (subpath \"{cwd}\"))\n", cwd = real_cwd);
        let workspace_write_rule = if let Some(git_dir) = &self.repo_git_write {
            let real_git = std::fs::canonicalize(git_dir)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| git_dir.to_string_lossy().to_string());
            if real_git
                .bytes()
                .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
            {
                tracing::error!(
                    "repo_git_write path contains SBPL metacharacters, granting cwd-only write"
                );
                cwd_write_rule
            } else {
                format!("{cwd_write_rule}(allow file-write* (subpath \"{real_git}\"))\n")
            }
        } else if self.workspace_write {
            cwd_write_rule
        } else {
            String::new()
        };

        // Choose a scratch temp dir for TMPDIR/TEMP/TMP.
        //
        // - Writable workspace: keep the historical `<cwd>/tmp` (covered by the
        //   cwd file-write* grant above).
        // - Read-only workspace (P2, codex): NEVER create or point TMPDIR under
        //   the workspace — that would mutate it BEFORE sandbox-exec even runs.
        //   Use a private dir OUTSIDE the workspace and grant SBPL write to
        //   THAT instead, so read-only truly means no workspace mutation while
        //   tools that need scratch space (Python tempfile, compilers) still
        //   work.
        let user_tmp = if self.workspace_write {
            cwd.join("tmp")
        } else {
            // `std::env::temp_dir()` honours `$TMPDIR`, which a hostile parent
            // could point UNDER the workspace. Route the candidate through
            // `read_only_scratch_dir`, which guarantees a path OUTSIDE the
            // (canonicalized) workspace so we never mutate it (codex P2).
            let candidate =
                std::env::temp_dir().join(format!("octos-sandbox-ro.{}", std::process::id()));
            let real_cwd_path = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            read_only_scratch_dir(&candidate, &real_cwd_path)
        };
        // Create the scratch dir. For the writable case this lives inside the
        // (writable) workspace; for the read-only case it is outside it.
        let _ = std::fs::create_dir_all(&user_tmp);

        // For the read-only case, grant SBPL file-write* to the external temp
        // dir's real path so scratch writes succeed there (not in the
        // workspace). The path is validated for SBPL metacharacters; if it is
        // unexpectedly unsafe we simply omit the grant (fail-closed: scratch
        // writes fail rather than the profile being injectable).
        let external_tmp_write_rule = if self.workspace_write {
            String::new()
        } else {
            let real_tmp_path =
                std::fs::canonicalize(&user_tmp).unwrap_or_else(|_| user_tmp.clone());
            let real_tmp = real_tmp_path.to_string_lossy().to_string();
            // Defence in depth: NEVER grant write to a temp path that resolved
            // inside the workspace — that would re-open the read-only hole even
            // though `read_only_scratch_dir` already steers outside cwd
            // (codex P2, fail-closed).
            let real_cwd_path = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            if real_tmp_path.starts_with(&real_cwd_path) {
                tracing::error!(
                    "external temp dir resolved inside the read-only workspace, omitting write grant"
                );
                String::new()
            } else if real_tmp
                .bytes()
                .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
            {
                tracing::error!(
                    "external temp dir contains SBPL metacharacters, omitting write grant"
                );
                String::new()
            } else {
                format!("(allow file-write* (subpath \"{real_tmp}\"))\n")
            }
        };

        let profile = format!(
            r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow process-info*)
(allow sysctl-read)
(allow mach-lookup)
(allow mach-register)
(allow ipc-posix*)
(allow signal)
(allow file-ioctl)
{read_rules}
(allow file-write* (literal "/dev/null"))
{workspace_write_rule}{external_tmp_write_rule}{network_rule}
"#,
            read_rules = read_rules,
            workspace_write_rule = workspace_write_rule,
            external_tmp_write_rule = external_tmp_write_rule,
            network_rule = network_rule,
        );

        let mut cmd = Command::new("sandbox-exec");
        // Redirect TMPDIR/TEMP/TMP to the chosen scratch dir (inside cwd when
        // writable, outside the workspace when read-only).
        cmd.env("TMPDIR", &user_tmp);
        cmd.env("TEMP", &user_tmp);
        cmd.env("TMP", &user_tmp);
        // Clear dangerous environment variables (sandbox-exec inherits parent env)
        for var in BLOCKED_ENV_VARS {
            cmd.env_remove(var);
        }
        cmd.arg("-p")
            .arg(profile)
            .arg("sh")
            .arg("-c")
            .arg(shell_command)
            .current_dir(cwd);
        cmd
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_sandbox_command() {
        let sb = MacosSandbox {
            allow_network: true,
            read_allow_paths: Vec::new(),
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sandbox-exec");
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.iter().any(|a| a.contains("allow network")));

        // Verify /private/tmp is NOT in SBPL write rules (loophole fixed)
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");
        assert!(
            !profile.contains("(allow file-write* (subpath \"/private/tmp\"))"),
            "SBPL should NOT allow writes to /private/tmp (loophole)"
        );
        assert!(
            !profile.contains("(allow file-write* (subpath \"/private/var/folders\"))"),
            "SBPL should NOT allow writes to /private/var/folders"
        );
        assert!(
            profile.contains("(allow file-write* (literal \"/dev/null\"))"),
            "SBPL should allow write access to /dev/null for git and shell redirections"
        );
    }

    #[test]
    fn repo_git_write_emits_git_subpath_not_global() {
        // HIGH (controller-hijack, fix 2): a fleet worktree worker granted
        // `FsGrant::Host` gets `repo_git_write = Some(<repo>/.git)`. The SBPL
        // profile must grant `file-write*` to the cwd checkout AND a `(subpath
        // "<repo>/.git")` — but NEVER a GLOBAL `(allow file-write*)` (which would
        // let a full-FS worker rewrite arbitrary host files, e.g. a LaunchAgent,
        // above its grant). Only reached under unrestricted reads, which
        // `supports_repo_git_write` gates.
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: true,
            repo_git_write: Some(PathBuf::from("/tmp/controller-repo/.git")),
        };
        let cmd = sb.wrap_command("git commit -am wip", Path::new("/tmp/ws"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");
        // The `.git` common dir is granted as a SUBPATH (a non-existent test path
        // is not canonicalized, so it appears verbatim)...
        assert!(
            profile.contains("(allow file-write* (subpath \"/tmp/controller-repo/.git\"))"),
            "repo_git_write must grant a `.git` subpath write, profile: {profile}",
        );
        // ...ALONGSIDE the cwd checkout: exactly TWO `file-write*` subpath rules
        // (cwd + `.git`), asserted by count so the cwd's canonicalized form
        // (`/tmp` → `/private/tmp` on macOS) does not make this platform-sensitive.
        let subpath_writes = profile.matches("(allow file-write* (subpath \"").count();
        assert_eq!(
            subpath_writes, 2,
            "expected exactly cwd + .git subpath write rules, profile: {profile}",
        );
        // ...but NEVER a global `(allow file-write*)` (close-paren right after `*`).
        assert!(
            !profile.contains("(allow file-write*)"),
            "repo_git_write must NOT emit a GLOBAL file-write* grant, profile: {profile}",
        );
        assert!(
            sb.supports_repo_git_write(),
            "macOS with unrestricted reads supports repo .git write",
        );

        // Default (no repo_git_write): only the cwd subpath is writable, no global.
        let plain = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = plain.wrap_command("echo hi", Path::new("/tmp/ws"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");
        assert!(
            !profile.contains("(allow file-write*)"),
            "default profile must NOT emit a global file-write* grant, profile: {profile}",
        );
    }

    #[test]
    fn restricted_reads_do_not_support_repo_git_write() {
        // A restricted-read profile grants the `.git` WRITE but denies the
        // `.git` READ `git commit` needs, so it must NOT be reported as
        // supporting the worktree flow (the pool gate falls back to scratch).
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: vec!["/opt/custom".to_string()],
            workspace_write: true,
            repo_git_write: None,
        };
        assert!(
            !sb.supports_repo_git_write(),
            "a restricted-read macOS profile must not support repo .git write",
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_sandbox_rejects_control_chars() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp/\x01bad"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sh"); // error command, not sandbox-exec
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // Must NOT execute the original command unsandboxed
        assert!(args.iter().any(|a| a.contains("exit 1")));
        assert!(!args.iter().any(|a| a.contains("ls")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_sandbox_rejects_sbpl_metacharacters() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: true,
            repo_git_write: None,
        };
        // Parentheses, backslash, and quote should all be rejected
        for path in &[
            "/tmp/(allow network*)",
            "/tmp/test\\evil",
            "/tmp/test\"evil",
        ] {
            let cmd = sb.wrap_command("ls", Path::new(path));
            let prog = cmd.as_std().get_program().to_string_lossy().to_string();
            assert_eq!(prog, "sh", "should reject path: {path}");
            let args: Vec<_> = cmd
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect();
            assert!(
                args.iter().any(|a| a.contains("exit 1")),
                "should exit 1 for path: {path}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_sandbox_denies_network() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a.contains("deny network")),
            "should deny network when allow_network is false"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_sandbox_accepts_valid_path() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo ok", Path::new("/Users/test/project"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sandbox-exec");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_sandbox_rejects_del_character() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp/evil\x7Fpath"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sh");
    }

    // --- macOS restricted read paths ---

    #[cfg(target_os = "macos")]
    #[test]
    fn should_use_global_file_read_when_no_read_paths() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a.contains("(allow file-read*)\n")),
            "should have global file-read* when read_allow_paths is empty"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_restrict_reads_when_read_paths_configured() {
        // Use a real temp dir so canonicalize works (macOS /tmp -> /private/tmp)
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();
        let real_cwd = std::fs::canonicalize(cwd)
            .unwrap()
            .to_string_lossy()
            .to_string();

        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: vec!["/custom/path".to_string()],
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo hi", cwd);
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");
        assert!(
            !profile.contains("(allow file-read*)\n"),
            "should not have global file-read*"
        );
        assert!(
            profile.contains("(allow file-read-metadata)"),
            "should allow file-read-metadata globally"
        );
        assert!(
            profile.contains(&format!(r#"(allow file-read* (subpath "{real_cwd}"))"#)),
            "should allow reading workspace at canonical path, profile:\n{profile}"
        );
        assert!(
            profile.contains(r#"(allow file-read* (subpath "/custom/path"))"#),
            "should allow reading custom path"
        );
        // `/private/etc` must reach the restricted profile by its CANONICAL path
        // (macOS `/etc` symlink) so system curl/LibreSSL can read
        // `/private/etc/ssl/openssl.cnf` + `cert.pem` under the network-on
        // default — otherwise TLS clients fast-fail "Operation not permitted".
        assert!(
            profile.contains(r#"(allow file-read* (subpath "/private/etc"))"#),
            "should allow reading /private/etc (canonical of /etc) for TLS config, profile:\n{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_reject_read_allow_paths_with_sbpl_metacharacters() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: vec![
                "/safe/path".to_string(),
                "/evil\")\n(allow file-write* (subpath \"/\"))".to_string(),
                "/another/safe".to_string(),
            ],
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");
        assert!(
            profile.contains(r#"(allow file-read* (subpath "/safe/path"))"#),
            "safe path should be allowed"
        );
        assert!(
            profile.contains(r#"(allow file-read* (subpath "/another/safe"))"#),
            "second safe path should be allowed"
        );
        assert!(
            !profile.contains(r#"(allow file-write* (subpath "/"))"#),
            "injected file-write* root rule must not appear in profile"
        );
        assert!(
            !profile.contains("/evil"),
            "evil path should be completely excluded"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_reject_read_allow_paths_with_parens() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: vec!["/path/with(parens)".to_string()],
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");
        assert!(
            !profile.contains("with(parens)"),
            "path with parens should be rejected from SBPL profile"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_reject_read_allow_paths_with_control_chars() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: vec![
                "/path/with\x01control".to_string(),
                "/path/with\x7Fdel".to_string(),
                "/valid/path".to_string(),
            ],
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");
        assert!(
            !profile.contains("control"),
            "path with control char should be rejected"
        );
        assert!(
            !profile.contains("del"),
            "path with DEL char should be rejected"
        );
        assert!(
            profile.contains(r#"(allow file-read* (subpath "/valid/path"))"#),
            "valid path should be present"
        );
    }

    // --- Sandbox execution tests (platform-specific) ---

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn test_macos_sandbox_blocks_write_outside_cwd() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();

        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: vec![],
            workspace_write: true,
            repo_git_write: None,
        };
        let mut cmd = sb.wrap_command(
            "touch /tmp/sandbox_escape_test_file 2>&1; echo exit=$?",
            cwd,
        );
        let output = cmd.output().await.expect("sandbox-exec should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let escaped = std::path::Path::new("/tmp/sandbox_escape_test_file").exists();
        if escaped {
            let _ = std::fs::remove_file("/tmp/sandbox_escape_test_file");
            panic!("sandbox failed to block write outside cwd! stdout={stdout}, stderr={stderr}");
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn test_macos_sandbox_allows_write_inside_cwd() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();

        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: vec![],
            workspace_write: true,
            repo_git_write: None,
        };
        let mut cmd = sb.wrap_command("touch test_file && echo ok", cwd);
        let output = cmd.output().await.expect("sandbox-exec should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("ok"),
            "write inside cwd should succeed, got stdout={stdout}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            cwd.join("test_file").exists(),
            "file should be created inside cwd"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_omit_workspace_write_rule_when_workspace_write_disabled() {
        // P1 (codex): under a read-only profile the SBPL profile must NOT
        // grant file-write* to the workspace cwd (so shell cannot write),
        // while still permitting /dev/null writes.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();
        let real_cwd = std::fs::canonicalize(cwd)
            .unwrap()
            .to_string_lossy()
            .to_string();

        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: false,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("touch newfile", cwd);
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");
        assert!(
            !profile.contains(&format!(r#"(allow file-write* (subpath "{real_cwd}"))"#)),
            "read-only profile must NOT grant file-write* to the workspace, profile:\n{profile}"
        );
        // /dev/null stays writable for shell redirections / git internals.
        assert!(
            profile.contains(r#"(allow file-write* (literal "/dev/null"))"#),
            "/dev/null must stay writable even when workspace is read-only"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_grant_workspace_write_rule_when_workspace_write_enabled() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();
        let real_cwd = std::fs::canonicalize(cwd)
            .unwrap()
            .to_string_lossy()
            .to_string();

        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("touch newfile", cwd);
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");
        assert!(
            profile.contains(&format!(r#"(allow file-write* (subpath "{real_cwd}"))"#)),
            "writable profile must grant file-write* to the workspace, profile:\n{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_not_create_tmp_under_cwd_when_workspace_write_disabled() {
        // P2 (codex): with workspace_write=false, wrap_command must NOT create
        // `<cwd>/tmp` (a workspace mutation that happens BEFORE sandbox-exec
        // even starts) nor point TMPDIR/TEMP/TMP under the workspace. The temp
        // dir must live OUTSIDE the read-only workspace.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();

        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: false,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo hi", cwd);

        // The wrapper must not have created a tmp dir inside the workspace.
        assert!(
            !cwd.join("tmp").exists(),
            "read-only wrapper must NOT create <cwd>/tmp before sandbox-exec"
        );

        // TMPDIR/TEMP/TMP must point OUTSIDE the workspace cwd.
        let envs: std::collections::HashMap<String, Option<String>> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        for key in ["TMPDIR", "TEMP", "TMP"] {
            if let Some(Some(val)) = envs.get(key) {
                assert!(
                    !std::path::Path::new(val).starts_with(cwd),
                    "{key} must not point inside the read-only workspace, got {val}"
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_grant_sbpl_write_to_external_tmp_when_workspace_write_disabled() {
        // P2 (codex): the out-of-workspace temp dir that TMPDIR points at must
        // itself be granted file-write* in the SBPL profile, otherwise tools
        // that need scratch space break under a read-only workspace.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();

        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: false,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo hi", cwd);
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");

        let envs: std::collections::HashMap<String, Option<String>> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        let tmpdir = envs
            .get("TMPDIR")
            .and_then(|v| v.clone())
            .expect("TMPDIR must be set");
        // Canonicalize because SBPL subpath rules use real paths.
        let real_tmp = std::fs::canonicalize(&tmpdir)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or(tmpdir);
        assert!(
            profile.contains(&format!(r#"(allow file-write* (subpath "{real_tmp}"))"#)),
            "external temp dir must be granted file-write* in SBPL, profile:\n{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_reject_scratch_candidate_inside_workspace_when_read_only() {
        // P2 (codex, round 3): `std::env::temp_dir()` honours `$TMPDIR`, which a
        // parent could point UNDER the read-only workspace. `read_only_scratch_dir`
        // must detect a candidate inside the (canonicalized) cwd and fall back to
        // a location provably OUTSIDE the workspace — WITHOUT creating anything
        // under cwd.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();
        let real_cwd = std::fs::canonicalize(cwd).expect("canonicalize cwd");

        // Adversarial candidate: a subdir of the workspace cwd (what $TMPDIR
        // pointing under cwd would yield).
        let evil_candidate = cwd.join("evil-tmp");
        let scratch = read_only_scratch_dir(&evil_candidate, &real_cwd);

        // The chosen scratch dir must be OUTSIDE the workspace...
        let scratch_real = std::fs::canonicalize(&scratch).unwrap_or_else(|_| scratch.clone());
        assert!(
            !scratch_real.starts_with(&real_cwd),
            "read-only scratch must be OUTSIDE the workspace, got {} (cwd {})",
            scratch_real.display(),
            real_cwd.display()
        );
        // ...and choosing it must NOT have created anything inside cwd.
        assert!(
            !evil_candidate.exists(),
            "must NOT create the in-workspace candidate dir"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_keep_scratch_candidate_outside_workspace_when_read_only() {
        // When $TMPDIR is already OUTSIDE the workspace, the candidate is kept.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();
        let real_cwd = std::fs::canonicalize(cwd).expect("canonicalize cwd");

        let outside = tempfile::tempdir().expect("create outside temp dir");
        let good_candidate = outside.path().join("octos-sandbox-ro.123");
        let scratch = read_only_scratch_dir(&good_candidate, &real_cwd);
        assert_eq!(
            scratch, good_candidate,
            "an outside candidate must be used as-is"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_reject_relative_scratch_candidate_when_read_only() {
        // P2 (codex, round 5): a NON-EXISTENT RELATIVE `$TMPDIR` like "tmp" is
        // resolved by the sandboxed process relative to its cwd — i.e. it lands
        // at `<cwd>/tmp`, inside the read-only workspace. A prior lexical
        // canonicalization left it relative, so the absolute `real_cwd`-contains
        // check missed it and it was accepted → `create_dir_all` would mutate the
        // read-only workspace. The relative candidate must be absolutized against
        // cwd BEFORE the containment check and thereby rejected; the chosen
        // scratch must be ABSOLUTE and OUTSIDE cwd.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();
        let real_cwd = std::fs::canonicalize(cwd).expect("canonicalize cwd");

        let relative_candidate = Path::new("tmp");
        let scratch = read_only_scratch_dir(relative_candidate, &real_cwd);

        assert!(
            scratch.is_absolute(),
            "chosen scratch must be absolute, got {}",
            scratch.display()
        );
        // Absolutize a still-relative result against cwd the way the process
        // would, then confirm it is NOT inside the workspace.
        let scratch_abs = if scratch.is_absolute() {
            scratch.clone()
        } else {
            real_cwd.join(&scratch)
        };
        let scratch_real =
            std::fs::canonicalize(&scratch_abs).unwrap_or_else(|_| scratch_abs.clone());
        assert!(
            !scratch_real.starts_with(&real_cwd),
            "a relative $TMPDIR candidate must not resolve inside cwd; got {} (cwd {})",
            scratch_real.display(),
            real_cwd.display()
        );
        // Purity: selecting the scratch must NOT create `<cwd>/tmp`.
        assert!(
            !cwd.join("tmp").exists(),
            "must NOT create <cwd>/tmp when rejecting a relative candidate"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_reject_nested_relative_scratch_candidate_when_read_only() {
        // Same hazard with a deeper relative path "./sub/tmp".
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();
        let real_cwd = std::fs::canonicalize(cwd).expect("canonicalize cwd");

        let relative_candidate = Path::new("./sub/tmp");
        let scratch = read_only_scratch_dir(relative_candidate, &real_cwd);

        assert!(
            scratch.is_absolute(),
            "chosen scratch must be absolute, got {}",
            scratch.display()
        );
        let scratch_real = std::fs::canonicalize(&scratch).unwrap_or_else(|_| scratch.clone());
        assert!(
            !scratch_real.starts_with(&real_cwd),
            "nested relative candidate must not resolve inside cwd; got {} (cwd {})",
            scratch_real.display(),
            real_cwd.display()
        );
        assert!(
            !cwd.join("sub").exists(),
            "must NOT create <cwd>/sub when rejecting a nested relative candidate"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_not_use_tmpdir_inside_workspace_end_to_end_when_read_only() {
        // End-to-end through wrap_command: even though we can't safely mutate
        // $TMPDIR in a #[deny(unsafe_code)] test, assert the wired TMPDIR points
        // OUTSIDE the workspace and no scratch dir was created under cwd.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();
        let real_cwd = std::fs::canonicalize(cwd).expect("canonicalize cwd");

        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
            workspace_write: false,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo hi", cwd);

        assert!(
            !cwd.join("tmp").exists(),
            "read-only wrapper must NOT create <cwd>/tmp"
        );

        let envs: std::collections::HashMap<String, Option<String>> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        for key in ["TMPDIR", "TEMP", "TMP"] {
            let val = envs
                .get(key)
                .and_then(|v| v.clone())
                .unwrap_or_else(|| panic!("{key} must be set"));
            let real_val =
                std::fs::canonicalize(&val).unwrap_or_else(|_| std::path::PathBuf::from(&val));
            assert!(
                !real_val.starts_with(&real_cwd),
                "{key} must point OUTSIDE the read-only workspace, got {val} (real {})",
                real_val.display()
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn should_not_leave_tmp_dir_in_workspace_after_read_only_run() {
        // P2 (codex) end-to-end: running a command under a read-only workspace
        // must leave NO `<cwd>/tmp` behind (the workspace stays untouched).
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();

        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: vec![],
            workspace_write: false,
            repo_git_write: None,
        };
        let mut cmd = sb.wrap_command("echo hello; :", cwd);
        let output = cmd.output().await.expect("sandbox-exec should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !cwd.join("tmp").exists(),
            "read-only run must not create <cwd>/tmp, stdout={stdout}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn should_block_write_inside_cwd_when_workspace_write_disabled() {
        // End-to-end proof: a read-only sandbox denies `touch newfile` even
        // inside the workspace cwd (the P1 footgun: shell writing under
        // `--sandbox read-only`).
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();

        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: vec![],
            workspace_write: false,
            repo_git_write: None,
        };
        let mut cmd = sb.wrap_command("touch newfile 2>&1; echo exit=$?", cwd);
        let output = cmd.output().await.expect("sandbox-exec should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !cwd.join("newfile").exists(),
            "read-only sandbox must block workspace writes, stdout={stdout}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn test_macos_sandbox_restricts_read_paths() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let cwd = tmp.path();

        let home = std::env::var("HOME").expect("HOME must be set");
        let secret_dir = std::path::PathBuf::from(&home).join(".sandbox_test_tmp");
        std::fs::create_dir_all(&secret_dir).expect("create secret dir");
        let secret_file = secret_dir.join("secret.txt");
        std::fs::write(&secret_file, "top-secret-data").expect("write secret");

        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: vec!["/nonexistent/path".to_string()],
            workspace_write: true,
            repo_git_write: None,
        };
        let real_secret =
            std::fs::canonicalize(&secret_file).unwrap_or_else(|_| secret_file.clone());
        let cmd_str = format!("cat {} 2>&1; echo exit=$?", real_secret.display());
        let mut cmd = sb.wrap_command(&cmd_str, cwd);
        let output = cmd.output().await.expect("sandbox-exec should run");
        let stdout = String::from_utf8_lossy(&output.stdout);

        let _ = std::fs::remove_dir_all(&secret_dir);

        assert!(
            !stdout.contains("top-secret-data"),
            "sandbox should block reading files outside allowed paths, got: {stdout}"
        );
    }
}
