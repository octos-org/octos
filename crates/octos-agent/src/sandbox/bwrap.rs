//! Linux sandbox using bubblewrap (bwrap).

use std::path::{Path, PathBuf};

use tokio::process::Command;

use super::{BLOCKED_ENV_VARS, Sandbox};

type HiddenMounts = (Vec<PathBuf>, Vec<(PathBuf, PathBuf)>);
type HiddenMountsResult = std::io::Result<HiddenMounts>;

/// Linux sandbox using bubblewrap (bwrap).
pub struct BwrapSandbox {
    pub(crate) allow_network: bool,
    /// When `false`, the working directory is bound read-only
    /// (`--ro-bind`) so shell commands cannot mutate the workspace.
    /// Default constructions use `true` (read-write `--bind`) to preserve
    /// the historical writable-workspace behaviour.
    pub(crate) workspace_write: bool,
    /// A host-created deliverable directory owned by one spawn. Bound rw AFTER
    /// the read-only cwd bind so it is the sole shell-write exception; a
    /// missing source makes bwrap fail closed instead of running unconfined.
    pub(crate) deliverable_root: Option<PathBuf>,
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
    /// UPCR-2026-034 `read_parent` (see `SandboxConfig::read_only_view`).
    pub(crate) read_only_view: Option<Box<super::SandboxReadOnlyView>>,
    /// UPCR-2026-R1EFC56A3 (see `SandboxConfig::hidden_folders`).
    pub(crate) hidden_folders: Option<octos_core::HiddenFolders>,
}

/// How the sandbox hides `hidden`: each existing folder inside a `bound`
/// folder (bound as given; compared resolved) is covered by an empty tmpfs,
/// and a bound folder inside a hidden one is covered whole. Returns the
/// folders to cover and, for a writable `cwd`, the folders between it and a
/// hidden folder (source, destination), to bind over themselves first: a
/// mount point cannot be renamed or removed, so the shell cannot move a
/// hidden folder out from under the next command's mounts.
fn hidden_mounts(
    hidden: &octos_core::HiddenFolders,
    cwd: &Path,
    view_root: Option<&Path>,
    cwd_writable: bool,
) -> HiddenMountsResult {
    let mut covers = std::collections::BTreeSet::new();
    let mut pins = std::collections::BTreeSet::new();
    let bound = std::iter::once(cwd).chain(view_root);
    let bound: Vec<(&Path, PathBuf)> = bound
        .filter_map(|path| Some((path, std::fs::canonicalize(path).ok()?)))
        .collect();
    let command_roots: Vec<PathBuf> = std::iter::once(cwd.to_path_buf())
        .chain(view_root.map(Path::to_path_buf))
        .collect();
    for registered in hidden.folders() {
        let folder = match std::fs::canonicalize(&registered) {
            Ok(folder) => folder,
            Err(error) => match hidden.resolve_missing_folder(&registered, &command_roots) {
                Ok(true) => std::fs::canonicalize(&registered).map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!(
                            "hidden folder {} was restored but still cannot resolve: {error}",
                            registered.display()
                        ),
                    )
                })?,
                Ok(false) => continue,
                Err(reason) => {
                    return Err(std::io::Error::other(format!(
                        "cannot resolve hidden folder {}: {error}; {reason}",
                        registered.display()
                    )));
                }
            },
        };
        for (path, real) in &bound {
            if let Ok(below) = folder.strip_prefix(real) {
                covers.insert(path.join(below));
                if cwd_writable && *path == cwd {
                    let mut between = PathBuf::new();
                    for part in below.parent().into_iter().flat_map(Path::components) {
                        between.push(part);
                        pins.insert((real.join(&between), path.join(&between)));
                    }
                }
            } else if real.starts_with(&folder) {
                covers.insert(path.to_path_buf());
            }
        }
    }
    Ok((covers.into_iter().collect(), pins.into_iter().collect()))
}

impl Sandbox for BwrapSandbox {
    fn backend_name(&self) -> &'static str {
        "bwrap"
    }

    fn supports_repo_git_write(&self) -> bool {
        // bwrap can `--bind <repo>/.git <repo>/.git` (rw) and read-binds the
        // system dirs, so a Host-granted worker gets both the `.git` write AND the
        // reads `git commit` needs — the fleet worktree flow is viable.
        true
    }

    fn runs_posix_sh(&self) -> bool {
        // `wrap_command` terminates in `sh -c` (Linux).
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

        // A request context's read-only view of its peer's folder: the folder
        // read-only, each other context's folder hidden under an empty
        // tmpfs. Before the cwd bind, which lands on top (the context's own
        // folder lies inside a hidden one).
        if let Some(view) = &self.read_only_view {
            if view.root.exists() {
                cmd.arg("--ro-bind").arg(&view.root).arg(&view.root);
                for excluded in &view.excluded {
                    cmd.arg("--tmpfs").arg(excluded);
                }
            }
        }

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

        // #1796: a read-only spawn still owns its host-seeded deliverable
        // directory. Re-bind exactly that directory rw on top of the cwd ro
        // bind (they normally name the same path). This does not expose a
        // sibling workspace or any other host directory.
        if let Some(root) = &self.deliverable_root {
            cmd.arg("--bind").arg(root).arg(root);
        }

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

        // Folders the shell never sees (every profile's chats folder, for a
        // session that is not a chat): an empty tmpfs over each one inside a
        // bound folder, after the binds so it lands on top, with the folders
        // leading to it pinned in place.
        if let Some(hidden) = &self.hidden_folders {
            let view_root = self.read_only_view.as_ref().map(|view| view.root.as_path());
            let (covers, pins) = match hidden_mounts(hidden, cwd, view_root, self.workspace_write) {
                Ok(mounts) => mounts,
                Err(error) => {
                    return super::refusing_command(
                        &format!(
                            "sandbox bwrap: {error}. Refusing to run the command because a hidden folder could not be covered."
                        ),
                        cwd,
                    );
                }
            };
            for (source, destination) in pins {
                cmd.arg("--bind").arg(source).arg(destination);
            }
            for cover in covers {
                cmd.arg("--tmpfs").arg(cover);
            }
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
    fn test_bwrap_sandbox_env_sanitization() {
        let sb = BwrapSandbox {
            read_only_view: None,
            hidden_folders: None,
            allow_network: false,
            workspace_write: true,
            deliverable_root: None,
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
    fn should_refuse_when_hidden_folder_was_renamed() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("cwd");
        let hidden_path = temp.path().join("hidden");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&hidden_path).unwrap();
        std::fs::rename(&hidden_path, temp.path().join("renamed")).unwrap();
        let hidden = octos_core::HiddenFolders::default();
        hidden.add(hidden_path).unwrap();
        let sb = BwrapSandbox {
            read_only_view: None,
            hidden_folders: Some(hidden),
            allow_network: false,
            workspace_write: true,
            deliverable_root: None,
            repo_git_write: None,
        };
        let cmd = sb.wrap_command("echo must-not-run", &cwd);
        assert_ne!(cmd.as_std().get_program(), "bwrap");
        assert!(
            cmd.as_std()
                .get_args()
                .all(|arg| arg != "echo must-not-run")
        );
    }
}
