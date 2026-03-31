//! macOS sandbox using sandbox-exec.

use std::path::Path;

use tokio::process::Command;

use super::{BLOCKED_ENV_VARS, DEFAULT_READ_ALLOW_PATHS, Sandbox};

/// macOS sandbox using sandbox-exec.
pub struct MacosSandbox {
    pub(crate) allow_network: bool,
    /// When non-empty, restrict file-read* to these paths + cwd.
    /// Empty = allow all reads (backward compatible).
    pub(crate) read_allow_paths: Vec<String>,
}

impl Sandbox for MacosSandbox {
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
(allow file-write* (subpath "{cwd}"))
{network_rule}
"#,
            read_rules = read_rules,
            cwd = real_cwd,
            network_rule = network_rule,
        );

        // Create a per-user tmp dir inside the workspace so programs that
        // need temp files (Python tempfile, compilers, etc.) still work.
        let user_tmp = cwd.join("tmp");
        let _ = std::fs::create_dir_all(&user_tmp);

        let mut cmd = Command::new("sandbox-exec");
        // Redirect TMPDIR/TEMP/TMP to the per-user tmp inside the workspace
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_sandbox_command() {
        let sb = MacosSandbox {
            allow_network: true,
            read_allow_paths: Vec::new(),
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
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_sandbox_rejects_control_chars() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
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
