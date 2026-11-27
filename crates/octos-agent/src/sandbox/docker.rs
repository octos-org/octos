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

/// Strip a legitimate Windows drive-letter prefix (`C:\…` or `C:/…`): its
/// colon is the drive's, not the mount spec's `source:dest` separator. Bare
/// `C:foo` is a drive-RELATIVE path (per-drive cwd), not an absolute one, so
/// it is not stripped and stays refused like every other colon path. UNC
/// paths (`\\server\share`) carry no drive colon and pass through whole.
fn strip_windows_drive(source: &str) -> &str {
    let bytes = source.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return &source[2..];
    }
    source
}

/// True when `source` cannot be a Docker mount source: control characters
/// always, and any `:` left after a legitimate Windows drive prefix (it would
/// split the `-v source:dest` spec — T-0696). This is NOT "allow all colons":
/// a second colon, a bare `C:foo`, or any Unix colon still fails closed.
fn mount_source_has_injection(source: &str) -> bool {
    source.contains(['\0', '\n', '\r']) || strip_windows_drive(source).contains(':')
}

/// Split an extra bind's `source[:dest]`, honouring a Windows drive colon in
/// the source (`C:\foo:/bar` splits after `C:\foo`, not after `C`).
fn split_bind_spec(bind: &str) -> (&str, Option<&str>) {
    let skipped = bind.len() - strip_windows_drive(bind).len();
    match bind[skipped..].find(':') {
        Some(at) => (&bind[..skipped + at], Some(&bind[skipped + at + 1..])),
        None => (bind, None),
    }
}

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

        // Workspace mount -- validate path to prevent volume mount injection
        // via ':' (a Windows drive-letter colon is not the separator).
        let cwd_str = cwd.to_string_lossy();
        if mount_source_has_injection(&cwd_str) {
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

        match self.config.mount_mode {
            MountMode::ReadWrite => {
                cmd.arg("-v").arg(format!("{cwd_str}:/workspace"));
                cmd.arg("-w").arg("/workspace");
                hidden_mounts.push((cwd.to_path_buf(), "/workspace".to_owned()));
            }
            MountMode::ReadOnly => {
                cmd.arg("-v").arg(format!("{cwd_str}:/workspace:ro"));
                cmd.arg("-w").arg("/workspace");
                hidden_mounts.push((cwd.to_path_buf(), "/workspace".to_owned()));
            }
            MountMode::None => {
                cmd.arg("-w").arg("/tmp");
            }
        }

        // Extra bind mounts (validated against dangerous sources)
        for bind in &self.config.extra_binds {
            let (source, dest) = split_bind_spec(bind);
            if is_blocked_bind_source(source) {
                tracing::warn!(
                    bind = %bind,
                    "skipping dangerous bind mount source"
                );
                continue;
            }
            let dest = dest.filter(|dest| !dest.is_empty()).unwrap_or(source);
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
            if mount_source_has_injection(&source) || is_blocked_bind_source(&source) {
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

#[cfg(test)]
mod validation_tests {
    //! T-0696: mount-source validation is host-agnostic string work — these
    //! run on every platform, including the Windows CI runner.
    use super::*;

    #[test]
    fn should_allow_legitimate_windows_drive_and_unc_sources() {
        for ok in [
            "C:\\work",
            "C:/work",
            "c:\\temp",
            "D:\\",
            "E:/a b",
            "\\server\\share\\dir",
            "/data/work",
            "/",
        ] {
            assert!(!mount_source_has_injection(ok), "{ok}");
        }
    }

    #[test]
    fn should_refuse_colons_beyond_the_drive_and_control_characters() {
        for bad in [
            "C:\\work:evil",
            "C:/a:b",
            "C:work",
            "C:",
            "1:\\x",
            "\\server\\share:evil",
            "/data:evil",
            ":C",
            "C:\\a\nb",
            "C:\\a\rb",
            "/data\0x",
        ] {
            assert!(mount_source_has_injection(bad), "{bad}");
        }
    }

    #[test]
    fn should_split_bind_specs_after_a_windows_drive() {
        let cases: [(&str, (&str, Option<&str>)); 5] = [
            ("C:\\foo:/bar", ("C:\\foo", Some("/bar"))),
            ("C:\\foo", ("C:\\foo", None)),
            ("/a:/b", ("/a", Some("/b"))),
            ("/a", ("/a", None)),
            ("C:\\foo:ro", ("C:\\foo", Some("ro"))),
        ];
        for (bind, expected) in cases {
            assert_eq!(split_bind_spec(bind), expected, "{bind}");
        }
    }
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
