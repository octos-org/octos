//! Linux sandbox using bubblewrap (bwrap).

use std::path::Path;

use tokio::process::Command;

use super::{BLOCKED_ENV_VARS, Sandbox};

/// Linux sandbox using bubblewrap (bwrap).
pub struct BwrapSandbox {
    pub(crate) allow_network: bool,
    /// When `false`, the working directory is bound read-only
    /// (`--ro-bind`) so shell commands cannot mutate the workspace.
    /// Default constructions use `true` (read-write `--bind`) to preserve
    /// the historical writable-workspace behaviour.
    pub(crate) workspace_write: bool,
}

impl Sandbox for BwrapSandbox {
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new("bwrap");

        // Clear dangerous environment variables before entering sandbox
        for var in BLOCKED_ENV_VARS {
            cmd.env_remove(var);
        }

        // Read-only bind system directories
        for dir in &["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
            if Path::new(dir).exists() {
                cmd.arg("--ro-bind").arg(dir).arg(dir);
            }
        }

        // Mount the /tmp AND /var/tmp scratch tmpfs BEFORE binding the workspace.
        // bwrap applies mounts in order, so these must precede the workspace bind:
        // if the workspace lives under /tmp or /var/tmp (e.g. cwd = /tmp/ws) a
        // later `--tmpfs` on that root would SHADOW the workspace bind with a
        // fresh writable tmpfs — hiding the real contents AND, under `--sandbox
        // read-only`, re-permitting `touch $cwd/newfile` (round-4 sibling of the
        // Landlock /tmp-overlap edge). Mounting the tmpfs first means the
        // subsequent workspace bind lands on top and wins.
        //
        // Both /tmp and /var/tmp are covered so bwrap is symmetric with the
        // Landlock helper (which treats both as system temp roots): a read-only
        // cwd under EITHER root is protected, and tools that default to either
        // still get writable scratch (round-5 audit).
        cmd.arg("--tmpfs").arg("/tmp");
        cmd.arg("--tmpfs").arg("/var/tmp");

        // Bind the working directory. Read-write by default; read-only when
        // the permission profile denies workspace writes (`--sandbox
        // read-only`), so shell commands cannot mutate the workspace. This bind
        // is emitted AFTER the /tmp tmpfs so it overlays (and wins over) it.
        let cwd_str = cwd.to_string_lossy();
        let workspace_bind = if self.workspace_write {
            "--bind"
        } else {
            "--ro-bind"
        };
        cmd.arg(workspace_bind).arg(&*cwd_str).arg(&*cwd_str);

        // /dev minimal
        cmd.arg("--dev").arg("/dev");
        cmd.arg("--proc").arg("/proc");

        if !self.allow_network {
            cmd.arg("--unshare-net");
        }

        cmd.arg("--unshare-pid");
        cmd.arg("--die-with-parent");
        cmd.arg("--chdir").arg(&*cwd_str);
        cmd.arg("--").arg("sh").arg("-c").arg(shell_command);

        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bwrap_sandbox_command() {
        let sb = BwrapSandbox {
            allow_network: false,
            workspace_write: true,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "bwrap");
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(args.contains(&"echo hi".to_string()));
    }

    #[test]
    fn test_bwrap_sandbox_env_sanitization() {
        let sb = BwrapSandbox {
            allow_network: false,
            workspace_write: true,
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp"));
        let removed: Vec<String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                if v.is_none() {
                    Some(k.to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect();
        for var in BLOCKED_ENV_VARS {
            assert!(
                removed.iter().any(|r| r == *var),
                "bwrap should env_remove {var}"
            );
        }
    }

    #[test]
    fn should_ro_bind_workspace_when_workspace_write_disabled() {
        // P1 (codex): a read-only permission profile must bind the workspace
        // read-only so shell commands cannot write to it.
        let sb = BwrapSandbox {
            allow_network: false,
            workspace_write: false,
        };
        let cmd = sb.wrap_command("touch newfile", Path::new("/tmp/ws"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // The workspace is bound with --ro-bind, not the read-write --bind.
        let ws_bind_idx = args
            .iter()
            .position(|a| a == "/tmp/ws")
            .expect("workspace path must be bound");
        assert!(
            ws_bind_idx >= 1,
            "workspace bind must have a flag before it"
        );
        assert_eq!(
            args[ws_bind_idx - 1],
            "--ro-bind",
            "read-only profile must --ro-bind the workspace, args: {args:?}"
        );
    }

    #[test]
    fn should_rw_bind_workspace_when_workspace_write_enabled() {
        let sb = BwrapSandbox {
            allow_network: false,
            workspace_write: true,
        };
        let cmd = sb.wrap_command("touch newfile", Path::new("/tmp/ws"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // The FIRST occurrence of the workspace path is the workspace bind
        // (system dirs bound before it never match /tmp/ws).
        let ws_bind_idx = args
            .iter()
            .position(|a| a == "/tmp/ws")
            .expect("workspace path must be bound");
        assert_eq!(
            args[ws_bind_idx - 1],
            "--bind",
            "writable profile must --bind (rw) the workspace, args: {args:?}"
        );
    }

    #[test]
    fn should_tmpfs_tmp_before_workspace_ro_bind_when_cwd_under_tmp() {
        // Round-4 sibling of codex P1 (Landlock /tmp overlap): bwrap applies
        // mounts IN ORDER. If `--tmpfs /tmp` is mounted AFTER `--ro-bind
        // /tmp/ws`, the fresh writable tmpfs SHADOWS the read-only workspace,
        // so `touch /tmp/ws/newfile` would succeed — defeating read-only. When
        // the read-only cwd is under /tmp, the tmpfs must be mounted FIRST so
        // the workspace ro-bind lands on top of it and wins.
        let sb = BwrapSandbox {
            allow_network: false,
            workspace_write: false,
        };
        let cmd = sb.wrap_command("touch newfile", Path::new("/tmp/ws"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        // Index of the `--tmpfs /tmp` mount (flag position).
        let tmpfs_idx = args
            .iter()
            .position(|a| a == "--tmpfs")
            .expect("bwrap must mount a /tmp tmpfs");
        assert_eq!(args.get(tmpfs_idx + 1).map(String::as_str), Some("/tmp"));

        // Index of the workspace ro-bind (the `--ro-bind /tmp/ws /tmp/ws`).
        let ws_bind_idx = args
            .iter()
            .position(|a| a == "/tmp/ws")
            .expect("workspace path must be bound");
        assert_eq!(args[ws_bind_idx - 1], "--ro-bind");

        assert!(
            tmpfs_idx < ws_bind_idx,
            "--tmpfs /tmp must be mounted BEFORE the workspace ro-bind so the \
             tmpfs cannot shadow (and re-permit writes to) the read-only \
             workspace, args: {args:?}"
        );
    }

    #[test]
    fn should_not_tmpfs_shadow_writable_workspace_under_tmp() {
        // For a WRITABLE workspace under /tmp the same ordering keeps behaviour
        // correct: the rw --bind must land on top of the tmpfs so the real
        // workspace contents are visible and writable (not shadowed by an empty
        // tmpfs). Assert tmpfs precedes the workspace bind here too.
        let sb = BwrapSandbox {
            allow_network: false,
            workspace_write: true,
        };
        let cmd = sb.wrap_command("touch newfile", Path::new("/tmp/ws"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let tmpfs_idx = args.iter().position(|a| a == "--tmpfs").unwrap();
        let ws_bind_idx = args.iter().position(|a| a == "/tmp/ws").unwrap();
        assert_eq!(args[ws_bind_idx - 1], "--bind");
        assert!(
            tmpfs_idx < ws_bind_idx,
            "--tmpfs /tmp must precede the workspace bind so it is not shadowed, args: {args:?}"
        );
    }

    #[test]
    fn should_tmpfs_var_tmp_before_workspace_ro_bind_when_cwd_under_var_tmp() {
        // Round-5 audit (bwrap sibling of the Landlock /var/tmp handling): the
        // Landlock backend treats BOTH /tmp and /var/tmp as system temp roots and
        // provides a scratch for a read-only cwd under either. bwrap must be
        // symmetric: a /var/tmp scratch tmpfs must exist AND be mounted BEFORE the
        // workspace ro-bind, so (a) a read-only cwd under /var/tmp is not shadowed
        // (and cannot be re-permitted for writes) and (b) tools that default to
        // /var/tmp still have writable scratch.
        let sb = BwrapSandbox {
            allow_network: false,
            workspace_write: false,
        };
        let cmd = sb.wrap_command("touch newfile", Path::new("/var/tmp/ws"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        // A `--tmpfs /var/tmp` mount must be present.
        let var_tmp_tmpfs_idx = args
            .iter()
            .enumerate()
            .find(|(i, a)| {
                *a == "--tmpfs" && args.get(i + 1).map(String::as_str) == Some("/var/tmp")
            })
            .map(|(i, _)| i)
            .expect("bwrap must mount a /var/tmp tmpfs");

        // The workspace ro-bind must come AFTER the tmpfs so it wins.
        let ws_bind_idx = args
            .iter()
            .position(|a| a == "/var/tmp/ws")
            .expect("workspace path must be bound");
        assert_eq!(args[ws_bind_idx - 1], "--ro-bind");
        assert!(
            var_tmp_tmpfs_idx < ws_bind_idx,
            "--tmpfs /var/tmp must be mounted BEFORE the workspace ro-bind so the \
             tmpfs cannot shadow (and re-permit writes to) the read-only \
             workspace, args: {args:?}"
        );
    }

    #[test]
    fn should_tmpfs_both_temp_roots_before_workspace_bind() {
        // Both temp-root tmpfs mounts must precede the workspace bind regardless
        // of where cwd lives, so ordering is never workspace-dependent.
        let sb = BwrapSandbox {
            allow_network: false,
            workspace_write: false,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/home/u/proj"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let tmp_idx = args
            .iter()
            .enumerate()
            .find(|(i, a)| *a == "--tmpfs" && args.get(i + 1).map(String::as_str) == Some("/tmp"))
            .map(|(i, _)| i)
            .expect("must mount /tmp tmpfs");
        let var_tmp_idx = args
            .iter()
            .enumerate()
            .find(|(i, a)| {
                *a == "--tmpfs" && args.get(i + 1).map(String::as_str) == Some("/var/tmp")
            })
            .map(|(i, _)| i)
            .expect("must mount /var/tmp tmpfs");
        let ws_bind_idx = args
            .iter()
            .position(|a| a == "/home/u/proj")
            .expect("workspace path must be bound");
        assert!(
            tmp_idx < ws_bind_idx && var_tmp_idx < ws_bind_idx,
            "both temp-root tmpfs mounts must precede the workspace bind, args: {args:?}"
        );
    }

    #[test]
    fn test_bwrap_sandbox_allows_network() {
        let sb = BwrapSandbox {
            allow_network: true,
            workspace_write: true,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            !args.contains(&"--unshare-net".to_string()),
            "should not unshare net when network is allowed"
        );
    }
}
