//! Linux sandbox using bubblewrap (bwrap).

use std::path::{Path, PathBuf};

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
    /// TARGETED write to a repo's `.git` common dir (from a fleet worker's
    /// `FsGrant::Host`). `Some(<repo>/.git)` adds a single `--bind <repo>/.git
    /// <repo>/.git` (rw) ON TOP OF the usual system-ro / tmpfs / cwd binds, so a
    /// Host-granted worktree worker's `git commit` can reach `<repo>/.git`
    /// (objects/refs/worktree-admin) which lives OUTSIDE its checkout cwd —
    /// WITHOUT binding all of `/` (which would expose host AF_UNIX sockets like
    /// `SSH_AUTH_SOCK` / `/var/run/docker.sock`, conferring signing identity /
    /// host-root ABOVE the worker's grant). Default `None` = today's cwd-only
    /// writable behaviour. The operator's explicit grant, NOT a fence.
    pub(crate) repo_git_write: Option<PathBuf>,
}

impl Sandbox for BwrapSandbox {
    fn workspace_scratch_writable(&self) -> bool {
        // A #1976 fence already degraded `workspace_write` to `false` at
        // construction (`fence_degraded_workspace_write`), so this single
        // flag covers both the read-only profile and the fenced one.
        self.workspace_write
    }

    fn supports_repo_git_write(&self) -> bool {
        // bwrap can `--bind <repo>/.git <repo>/.git` (rw) and read-binds the
        // system dirs, so a Host-granted worker gets both the `.git` write AND the
        // reads `git commit` needs — the fleet worktree flow is viable.
        true
    }

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
        //
        // This tmpfs is ALWAYS mounted — including for a `repo_git_write`
        // (`FsGrant::Host`) worktree worker. That is what keeps host /tmp AF_UNIX
        // sockets OUT of the sandbox; a repo whose `.git` lives under /tmp still
        // commits durably because the targeted `.git` bind below is emitted AFTER
        // the tmpfs and re-exposes just that path (bwrap creates the dest path).
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

        // TARGETED repo `.git` write (fleet worktree worker granted
        // `FsGrant::Host`): rw-bind ONLY the repo's `.git` common dir so a
        // `git commit` in the checkout can reach objects/refs/worktree-admin (all
        // OUTSIDE the cwd). Emitted AFTER the system-ro binds + tmpfs + cwd bind
        // so it wins for its own path (bwrap: last bind wins) even when `.git`
        // lives under /tmp/... (bwrap creates the destination path). This is the
        // operator's explicit grant — a NARROW bind, NOT `--bind / /`: no host
        // AF_UNIX socket (SSH_AUTH_SOCK, docker.sock) is ever exposed, so a
        // full-FS worker cannot hijack the controller's signing identity /
        // host-root. `allow_network` is unaffected (the boundary stays
        // `--unshare-net` below).
        if let Some(git_dir) = &self.repo_git_write {
            let git_str = git_dir.to_string_lossy();
            cmd.arg("--bind").arg(&*git_str).arg(&*git_str);
        }

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
            repo_git_write: None,
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
            repo_git_write: None,
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
            repo_git_write: None,
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
            repo_git_write: None,
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
            repo_git_write: None,
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
            repo_git_write: None,
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
            repo_git_write: None,
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
            repo_git_write: None,
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
            repo_git_write: None,
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

    #[test]
    fn worktree_sandbox_does_not_bind_root_or_expose_sockets() {
        // HIGH (controller-hijack, fix 2): a fleet worktree worker granted
        // `FsGrant::Host` must get a TARGETED `.git` write bind, NOT `--bind / /`.
        // Binding all of `/` would expose host AF_UNIX sockets (SSH_AUTH_SOCK,
        // /var/run/docker.sock) that confer signing identity / host-root ABOVE
        // the worker's grant. It also skipped the /tmp tmpfs, exposing host /tmp
        // sockets. The fix: bind cwd (rw) + `<repo>/.git` (rw), keep tmpfs.
        let sb = BwrapSandbox {
            allow_network: true,
            workspace_write: true,
            repo_git_write: Some(PathBuf::from("/srv/controller-repo/.git")),
        };
        let cmd = sb.wrap_command("git commit -am wip", Path::new("/work/fleet/f/t"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        // (1) NO `--bind / /` — the host root (and its sockets) is never mounted.
        assert!(
            !args
                .windows(3)
                .any(|w| w[0] == "--bind" && w[1] == "/" && w[2] == "/"),
            "a worktree worker must NOT `--bind / /` (would expose host sockets), args: {args:?}",
        );

        // (2) The repo's `.git` common dir IS rw-bound (targeted), so `git commit`
        //     can reach objects/refs/worktree-admin outside the cwd.
        let git_idx = args
            .windows(3)
            .position(|w| {
                w[0] == "--bind"
                    && w[1] == "/srv/controller-repo/.git"
                    && w[2] == "/srv/controller-repo/.git"
            })
            .expect("worktree worker must rw-bind <repo>/.git");

        // (3) The cwd checkout is still rw-bound.
        let cwd_idx = args
            .windows(3)
            .position(|w| {
                w[0] == "--bind" && w[1] == "/work/fleet/f/t" && w[2] == "/work/fleet/f/t"
            })
            .expect("worktree worker must rw-bind its checkout cwd");

        // (4) The /tmp + /var/tmp tmpfs is RE-ADDED (that's what keeps host /tmp
        //     sockets out), and precedes the `.git` bind so a `.git` under /tmp is
        //     re-exposed on top of the fresh tmpfs (bwrap creates the dest path).
        let tmpfs_tmp = args
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && w[1] == "/tmp")
            .expect("the /tmp tmpfs must be present");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--tmpfs" && w[1] == "/var/tmp"),
            "the /var/tmp tmpfs must be present, args: {args:?}",
        );
        assert!(
            tmpfs_tmp < cwd_idx && tmpfs_tmp < git_idx,
            "the tmpfs must precede the cwd + .git binds so they overlay it, args: {args:?}",
        );

        assert!(
            sb.supports_repo_git_write(),
            "bwrap supports repo .git write"
        );
    }

    #[test]
    fn default_binds_neither_root_nor_git_dir() {
        // Backward compatibility: without `repo_git_write` no extra write bind is
        // emitted (unchanged cwd-only behaviour), and never `--bind / /`.
        let sb = BwrapSandbox {
            allow_network: false,
            workspace_write: true,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/ws"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            !args
                .windows(3)
                .any(|w| w[0] == "--bind" && w[1] == "/" && w[2] == "/"),
            "default (no repo_git_write) must NOT rw-bind the host root, args: {args:?}",
        );
        // Exactly one rw `--bind` (the cwd) — no second targeted bind.
        let rw_binds = args
            .windows(3)
            .filter(|w| w[0] == "--bind" && w[1] == w[2])
            .count();
        assert_eq!(
            rw_binds, 1,
            "default must rw-bind ONLY the cwd, args: {args:?}"
        );
    }
}
