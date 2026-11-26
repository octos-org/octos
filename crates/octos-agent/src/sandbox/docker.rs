//! Docker container sandbox.

use std::path::Path;

use tokio::process::Command;

use super::{BLOCKED_ENV_VARS, DockerConfig, MountMode, Sandbox};

/// Bind mount sources that could lead to container escape or host compromise.
const BLOCKED_DOCKER_BIND_SOURCES: &[&str] = &[
    "/var/run/docker.sock",
    "docker.sock",
    "/etc",
    "/proc",
    "/sys",
    "/dev",
];

/// Check if a bind mount source is dangerous.
pub(crate) fn is_blocked_bind_source(source: &str) -> bool {
    let normalized = source.trim_end_matches('/');
    BLOCKED_DOCKER_BIND_SOURCES.iter().any(|blocked| {
        normalized == *blocked
            || normalized.ends_with("/docker.sock")
            || (normalized.starts_with(blocked)
                && normalized.as_bytes().get(blocked.len()) == Some(&b'/'))
    })
}

/// Docker container sandbox.
pub struct DockerSandbox {
    pub(crate) config: DockerConfig,
    pub(crate) allow_network: bool,
    /// UPCR-2026-R1EFC56A3 (see `SandboxConfig::hidden_folders`). Docker mounts
    /// only the workspace, the extra binds and the build-cache slot, so a
    /// hidden folder outside every mount is absent from the container; one
    /// inside a mount is masked with an empty `--tmpfs` (bwrap parity,
    /// T-0681). A mask mounts over the folder, so a shell cannot rename the
    /// mount point away, and renaming an ancestor carries the mask along.
    /// The reverse containment — a mount source inside a hidden folder —
    /// cannot be masked (a bind over the destination would shadow the tmpfs),
    /// so the command refuses, as on the non-hiding backends.
    pub(crate) hidden_folders: Option<octos_core::HiddenFolders>,
}

/// Resolve each registered hidden folder and return the container paths to
/// mask with an empty tmpfs. Resolution mirrors bwrap: a folder that fails
/// validation or cannot be accounted for is an error, so the wrap fails
/// closed instead of running the command.
fn hidden_masks(
    hidden: &octos_core::HiddenFolders,
    mounts: &[(std::path::PathBuf, String)],
    command_roots: &[std::path::PathBuf],
) -> std::io::Result<Vec<String>> {
    let mut masks = std::collections::BTreeSet::new();
    for registered in hidden.folders() {
        let folder = match std::fs::canonicalize(&registered) {
            Ok(folder) => {
                hidden
                    .resolve_existing_folder(&registered, &folder)
                    .map_err(|reason| {
                        std::io::Error::other(format!(
                            "hidden folder {} failed ownership validation: {reason}",
                            registered.display()
                        ))
                    })?;
                folder
            }
            Err(error) => match hidden.resolve_missing_folder(&registered, command_roots) {
                Ok(true) => {
                    let folder = std::fs::canonicalize(&registered).map_err(|error| {
                        std::io::Error::new(
                            error.kind(),
                            format!(
                                "hidden folder {} was restored but still cannot resolve: {error}",
                                registered.display()
                            ),
                        )
                    })?;
                    hidden
                        .resolve_existing_folder(&registered, &folder)
                        .map_err(|reason| {
                            std::io::Error::other(format!(
                                "hidden folder {} failed ownership validation after restore: {reason}",
                                registered.display()
                            ))
                        })?;
                    folder
                }
                Ok(false) => continue,
                Err(reason) => {
                    return Err(std::io::Error::other(format!(
                        "cannot resolve hidden folder {}: {error}; {reason}",
                        registered.display()
                    )));
                }
            },
        };
        for (source, dest) in mounts {
            let real = std::fs::canonicalize(source).unwrap_or_else(|_| source.clone());
            let dest = dest.trim_end_matches('/');
            if real == folder || real.starts_with(&folder) {
                // The mount source IS the hidden folder or lies inside it: a
                // tmpfs at the destination would be shadowed by the bind
                // itself, so masking cannot work — fail closed like the
                // non-hiding backends instead of exposing the protected
                // subtree.
                return Err(std::io::Error::other(format!(
                    "mount source {} lies inside hidden folder {}",
                    source.display(),
                    folder.display()
                )));
            }
            if let Ok(below) = folder.strip_prefix(&real) {
                // The hidden folder is strictly inside this mount: mask it
                // where it lands.
                masks.insert(format!("{dest}/{}", below.display()));
            }
        }
    }
    Ok(masks.into_iter().collect())
}

impl Sandbox for DockerSandbox {
    fn backend_name(&self) -> &'static str {
        "docker"
    }

    fn docker_image(&self) -> Option<&str> {
        Some(self.config.image.as_str())
    }

    fn is_docker(&self) -> bool {
        true
    }

    fn runs_posix_sh(&self) -> bool {
        // `wrap_command` terminates in an in-container `sh -c` (Linux image),
        // regardless of the host OS.
        true
    }

    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        self.wrap_with_slot(shell_command, cwd, None)
    }

    fn wrap_command_with_build_cache_slot(
        &self,
        shell_command: &str,
        cwd: &Path,
        slot: Option<&Path>,
    ) -> Command {
        self.wrap_with_slot(shell_command, cwd, slot)
    }
}

/// Whether the workspace is ALSO bound at its host path inside the container.
/// The model's prompt names the host workspace path (the UI context knows no
/// container), so a shell-only `/workspace` mount made the model's first
/// `ls <host path>` fail with ENOENT (burn-in r8 R8-F01). Aliasing the same
/// source at its own path adds no new content — both mounts expose the same
/// directory — but an alias that lands exactly on a container system
/// directory (e.g. cwd = `/tmp`) would shadow it, so those stay unmounted.
fn may_alias_host_path(cwd: &Path) -> bool {
    // Aliasing shadows the destination inside the container, so a workspace
    // path that IS a container system directory (or `/`) must never alias;
    // anything deeper (e.g. `/home/me/project`) only shadows itself.
    const SHADOWED: &str = "/ /usr /usr/local /bin /sbin /lib /lib64 /etc /var /opt /root /tmp /proc /sys /dev /run /boot /mnt /media /srv /home /workspace";
    if !cwd.is_absolute() {
        return false;
    }
    let trimmed = cwd.to_string_lossy().trim_end_matches('/').to_owned();
    let normalized = if trimmed.is_empty() {
        "/".to_owned()
    } else {
        trimmed
    };
    !SHADOWED.split(' ').any(|dir| dir == normalized)
}

/// Git `safe.directory` grants for the workspace's container paths, passed
/// through the environment so no config file is written anywhere (T-0686):
/// the container's user (image default, e.g. root in node:22-bookworm) does
/// not own the host bind mount, so git refused with "detected dubious
/// ownership" and `git diff -- a b` silently degraded to a two-file
/// `--no-index` comparison. Scoped to the exact mount destinations — never a
/// wildcard, never a host-side write.
fn add_git_safe_directory_env(cmd: &mut Command, dests: &[String]) {
    if dests.is_empty() {
        return;
    }
    cmd.arg("--env")
        .arg(format!("GIT_CONFIG_COUNT={}", dests.len()));
    for (i, dest) in dests.iter().enumerate() {
        cmd.arg("--env")
            .arg(format!("GIT_CONFIG_KEY_{i}=safe.directory"));
        cmd.arg("--env").arg(format!("GIT_CONFIG_VALUE_{i}={dest}"));
    }
}

impl DockerSandbox {
    fn wrap_with_slot(&self, shell_command: &str, cwd: &Path, slot: Option<&Path>) -> Command {
        let mut cmd = Command::new("docker");
        let mut hidden_mounts: Vec<(std::path::PathBuf, String)> = Vec::new();
        cmd.arg("run").arg("--rm");

        // Resource limits
        if let Some(ref cpu) = self.config.cpu_limit {
            cmd.arg("--cpus").arg(cpu);
        }
        if let Some(ref mem) = self.config.memory_limit {
            cmd.arg("--memory").arg(mem);
        }
        if let Some(pids) = self.config.pids_limit {
            cmd.arg("--pids-limit").arg(pids.to_string());
        }

        // Network
        if !self.allow_network {
            cmd.arg("--network").arg("none");
        }

        // Security hardening
        cmd.arg("--security-opt").arg("no-new-privileges");
        cmd.arg("--cap-drop").arg("ALL");

        // Clear dangerous environment variables (code injection vectors)
        for var in BLOCKED_ENV_VARS {
            cmd.arg("--env").arg(format!("{var}="));
        }

        // Workspace mount -- validate path to prevent volume mount injection via ':'
        let cwd_str = cwd.to_string_lossy();
        if cwd_str.contains(':')
            || cwd_str.contains('\0')
            || cwd_str.contains('\n')
            || cwd_str.contains('\r')
        {
            tracing::error!(
                "cwd contains invalid characters for Docker mount, refusing to execute"
            );
            let mut fail = Command::new("sh");
            fail.arg("-c")
                .arg("echo 'sandbox error: cwd contains invalid characters' >&2; exit 1");
            return fail;
        }
        // Validate cwd against dangerous mount sources
        if is_blocked_bind_source(&cwd_str) {
            tracing::error!(
                cwd = %cwd_str,
                "cwd is a blocked bind mount source, refusing to execute"
            );
            let mut fail = Command::new("sh");
            fail.arg("-c")
                .arg("echo 'sandbox error: dangerous bind mount source blocked' >&2; exit 1");
            return fail;
        }

        let mut workspace_dests: Vec<String> = Vec::new();
        match self.config.mount_mode {
            MountMode::ReadWrite | MountMode::ReadOnly => {
                let ro = if self.config.mount_mode == MountMode::ReadOnly {
                    ":ro"
                } else {
                    ""
                };
                cmd.arg("-v").arg(format!("{cwd_str}:/workspace{ro}"));
                cmd.arg("-w").arg("/workspace");
                hidden_mounts.push((cwd.to_path_buf(), "/workspace".to_owned()));
                workspace_dests.push("/workspace".to_owned());
                if may_alias_host_path(cwd) {
                    cmd.arg("-v").arg(format!("{cwd_str}:{cwd_str}{ro}"));
                    hidden_mounts.push((cwd.to_path_buf(), cwd_str.clone().into_owned()));
                    workspace_dests.push(cwd_str.clone().into_owned());
                }
            }
            MountMode::None => {
                cmd.arg("-w").arg("/tmp");
            }
        }
        add_git_safe_directory_env(&mut cmd, &workspace_dests);

        // Extra bind mounts (validated against dangerous sources)
        for bind in &self.config.extra_binds {
            let mut parts = bind.split(':');
            let source = parts.next().unwrap_or(bind);
            if is_blocked_bind_source(source) {
                tracing::warn!(
                    bind = %bind,
                    "skipping dangerous bind mount source"
                );
                continue;
            }
            let dest = parts
                .next()
                .filter(|dest| !dest.is_empty())
                .unwrap_or(source);
            hidden_mounts.push((std::path::PathBuf::from(source), dest.to_owned()));
            cmd.arg("-v").arg(bind);
        }

        if let Some(slot) = slot {
            let target = std::fs::canonicalize(slot.join("target"));
            let target = match target {
                Ok(target) if target.is_dir() => target,
                _ => return cache_mount_refusal(),
            };
            let source = target.to_string_lossy();
            if source.contains([':', '\0', '\n', '\r']) || is_blocked_bind_source(&source) {
                return cache_mount_refusal();
            }
            // Only artifacts are writable; the claim's control files stay on the host.
            hidden_mounts.push((target.clone(), "/octos-build-cache/target".to_owned()));
            cmd.arg("-v")
                .arg(format!("{source}:/octos-build-cache/target"));
            cmd.arg("--env")
                .arg("CARGO_TARGET_DIR=/octos-build-cache/target");
            cmd.arg("--env").arg("CARGO_INCREMENTAL=0");
        }

        // Mask hidden folders that fall inside a mount with an empty tmpfs.
        // A resolution failure keeps the command fail-closed, as on bwrap.
        if let Some(hidden) = &self.hidden_folders {
            let command_roots = vec![cwd.to_path_buf()];
            match hidden_masks(hidden, &hidden_mounts, &command_roots) {
                Ok(masks) => {
                    for mask in masks {
                        cmd.arg("--tmpfs").arg(mask);
                    }
                }
                Err(error) => {
                    return super::refusing_command(
                        &format!(
                            "sandbox docker: a hidden folder cannot be covered for this command ({error}). Refusing to run the command."
                        ),
                        cwd,
                    );
                }
            }
        }

        cmd.arg(&self.config.image);
        cmd.arg("sh").arg("-c").arg(shell_command);
        cmd
    }
}

fn cache_mount_refusal() -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("echo 'sandbox error: build-cache target cannot be mounted' >&2; exit 1");
    cmd
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Command as Proc;

    /// A locally available test image; None skips (CI without a registry).
    fn image() -> Option<String> {
        let up = Proc::new("docker").arg("info").output();
        if !up.ok()?.status.success() {
            return None;
        }
        let images = [
            "ubuntu:24.04",
            "node:22-bookworm",
            "python:3.12-slim",
            "alpine:latest",
        ];
        for image in images {
            let mut inspect = Proc::new("docker");
            let out = inspect.args(["image", "inspect", image]).output();
            if out.is_ok_and(|out| out.status.success()) {
                return Some(image.to_owned());
            }
        }
        None
    }

    fn sandbox(image: &str, hidden: octos_core::HiddenFolders) -> DockerSandbox {
        let config = DockerConfig {
            image: image.to_owned(),
            ..DockerConfig::default()
        };
        DockerSandbox {
            config,
            allow_network: false,
            hidden_folders: Some(hidden),
        }
    }

    #[test]
    fn should_alias_only_safe_host_workspace_paths() {
        assert!(may_alias_host_path(Path::new(
            "/data2/kamico/octos-dogfood/work"
        )));
        assert!(may_alias_host_path(Path::new("/home/me/project")));
        assert!(may_alias_host_path(Path::new("/Users/me/project")));
        for denied in [
            "/",
            "/tmp",
            "/usr",
            "/var",
            "/etc",
            "/home",
            "/workspace",
            "/proc",
        ] {
            assert!(
                !may_alias_host_path(Path::new(denied)),
                "{denied} must not be aliased"
            );
        }
        assert!(!may_alias_host_path(Path::new("relative/path")));
    }

    /// T-0686: the prompt-visible host workspace path works inside the
    /// container (no ENOENT), git status/diff work without any
    /// `safe.directory='*'` wildcard or config write, and hidden folders stay
    /// masked on the alias mount too.
    #[tokio::test]
    async fn should_alias_host_path_and_scope_git_ownership() {
        let Some(image) = image() else { return };
        let git_ok = Proc::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !git_ok {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let git = |args: &[&str]| {
            let out = Proc::new("git")
                .args(args)
                .current_dir(&work)
                .output()
                .unwrap();
            assert!(out.status.success(), "host git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(work.join("a.txt"), "hello\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(work.join("a.txt"), "hello2\n").unwrap();
        let hidden = octos_core::HiddenFolders::default();
        std::fs::create_dir_all(work.join("chats/abc")).unwrap();
        std::fs::write(work.join("chats/abc/secret.txt"), "s").unwrap();
        hidden.add(work.join("chats")).unwrap();
        let host = work.display();
        let probe = format!(
            "ls {host} >/dev/null && git status --porcelain | grep -q a.txt && git -C {host} diff --stat | grep -q a.txt && if [ -e {host}/chats/abc/secret.txt ]; then echo LEAKED; exit 1; fi; printf ok"
        );
        let mut wrapped = sandbox(&image, hidden).wrap_command(&probe, &work);
        let out = wrapped.output().await.unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "failed: {stderr}");
    }

    /// T-0681: a sibling-layout hidden root never reaches the container; an in-workspace one is tmpfs-masked.
    #[tokio::test]
    async fn should_hide_folders_from_docker_commands() {
        let Some(image) = image() else { return };
        let temp = tempfile::tempdir().unwrap();
        let sibling = temp.path().join("home/.octos/chat-workspaces");
        let work = temp.path().join("work");
        let chats = work.join("chats/abc");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(chats.join("secret.txt"), "chats-secret").unwrap();
        let hidden = octos_core::HiddenFolders::default();
        hidden.add(sibling.clone()).unwrap();
        hidden.add(work.join("chats")).unwrap();
        let cmd =
            "printf ok; if [ -e /workspace/chats/abc/secret.txt ]; then echo LEAKED; exit 1; fi";
        std::fs::create_dir_all(sibling.join("inner")).unwrap();
        let mut wrapped = sandbox(&image, hidden.clone()).wrap_command(cmd, &work);
        let out = wrapped.output().await.unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "docker command failed: {stderr}");
        let secret = std::fs::read_to_string(chats.join("secret.txt")).unwrap();
        assert_eq!(secret, "chats-secret");
        // A bind source inside or equal to a hidden root refuses (the bind
        // would shadow any tmpfs at its destination).
        for source in [sibling.join("inner"), sibling.clone()] {
            let mut bound = sandbox(&image, hidden.clone());
            bound.config.extra_binds = vec![format!("{}:/data", source.display())];
            let mut wrapped = bound.wrap_command("echo x", &work);
            let out = wrapped.output().await.unwrap();
            assert!(
                !out.status.success(),
                "bind of {} must refuse",
                source.display()
            );
        }
    }

    /// An owner-restorable hidden root (server-root self-heal) lets the
    /// command run; an unaccountable one stays fail-closed, as on bwrap.
    #[tokio::test]
    async fn should_heal_or_refuse_a_missing_hidden_folder() {
        let Some(image) = image() else { return };
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let missing = temp.path().join("home/.octos/chat-workspaces");
        let healed = octos_core::HiddenFolders::default();
        healed.add(missing.clone()).unwrap();
        let heal_path = missing.clone();
        healed.on_missing_folder(std::sync::Arc::new(move |_, _| {
            std::fs::create_dir_all(&heal_path).unwrap();
            Ok(true)
        }));
        let mut wrapped = sandbox(&image, healed).wrap_command("printf ran", &work);
        let out = wrapped.output().await.unwrap();
        assert!(
            out.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_dir_all(&missing).unwrap();
        let refused = octos_core::HiddenFolders::default();
        refused.add(missing).unwrap();
        refused.on_missing_folder(std::sync::Arc::new(|folder, _| {
            Err(format!("hidden root {} was renamed", folder.display()))
        }));
        let mut wrapped = sandbox(&image, refused).wrap_command("printf must-not-run", &work);
        let out = wrapped.output().await.unwrap();
        assert!(
            !out.status.success(),
            "an unaccountable hidden root must refuse"
        );
    }
}
