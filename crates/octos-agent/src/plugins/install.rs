//! Install-time and runtime-startup async wrappers around the
//! (otherwise-sync) [`PluginLoader`]. PR #1260 split this off from
//! `loader.rs` so the loader can remain sync (matching upstream's
//! hash-ledger cache) while the SPEC-VENDOR-NODE-V1 HTTP discovery
//! path stays async (catalog fetch).
//!
//! Three entry points:
//!
//! - [`activate_skill`] — used by `octos skills install` to run a
//!   skill's hardware lifecycle (preflight → init → ready_check) and
//!   then register its tools (Static binary-protocol OR HTTP-discovered).
//! - [`run_shutdown_phase`] — used by `octos skills remove` to fire
//!   the optional `shutdown` lifecycle steps before deletion.
//! - [`register_http_skills_on_startup`] — used by every agent boot
//!   path (chat / gateway / serve) AFTER the sync `PluginLoader::load_into`
//!   has registered the static skills; walks each plugin dir for
//!   `tool_discovery: { type: "http", base_url: "..." }` manifests
//!   and registers their HTTP-backed tools.
//!
//! Finding 2 (PR #1260 review): each HTTP-discovery call site
//! `?`-propagates the catalog error so a missing or broken bridge
//! turns into a load failure rather than a silent zero-tools install.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr, eyre};
use octos_plugin::{LifecycleExecutor, LifecyclePhase, Sandbox as PluginSandbox, ToolDiscovery};
use tracing::{info, warn};

use crate::sandbox::{Sandbox, SandboxConfig, create_sandbox};
use crate::tools::{Tool, ToolRegistry};

use super::http_discovery::install_http_tools_from_catalog;
use super::loader::PluginLoader;
use super::manifest::PluginManifest;

/// Result of activating a single skill from a directory.
#[derive(Debug, Default)]
pub struct SkillActivateResult {
    /// Names of all tools registered (static or HTTP-discovered).
    pub tool_names: Vec<String>,
}

/// Adapt the runtime sandbox to the plugin SDK's lifecycle trait without
/// weakening either contract: both wrap the same `tokio::process::Command`.
struct LifecycleSandboxAdapter(Box<dyn Sandbox>);

impl PluginSandbox for LifecycleSandboxAdapter {
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> tokio::process::Command {
        self.0.wrap_command(shell_command, cwd)
    }
}

/// Build the confinement lifecycle commands are REQUIRED to have. Auto with
/// no backend is not an opt-out for untrusted install content: it fails closed
/// even when the operator's general shell policy permits NoSandbox.
fn lifecycle_sandbox(skill_dir: &Path, requires_network: bool) -> Result<Box<dyn Sandbox>> {
    let config = SandboxConfig {
        mode: crate::sandbox::SandboxMode::Auto,
        fail_closed: true,
        allow_network: requires_network,
        read_allow_paths: vec![skill_dir.to_string_lossy().into_owned()],
        allow_toolchains: false,
        ..SandboxConfig::default()
    };
    let sandbox = create_sandbox(&config);
    require_confined_lifecycle_sandbox(sandbox)
}

fn require_confined_lifecycle_sandbox(sandbox: Box<dyn Sandbox>) -> Result<Box<dyn Sandbox>> {
    if let Some(refusal) = sandbox.refusal() {
        return Err(eyre!("skill lifecycle sandbox unavailable: {refusal}"));
    }
    if sandbox.is_noop() {
        return Err(eyre!(
            "skill lifecycle requires a real sandbox backend; refusing unconfined execution"
        ));
    }
    if sandbox.is_docker() {
        // Docker wrap_command filters env only on the docker CLI process; the
        // declared `hardware_lifecycle.env` is never forwarded into the
        // container. Refuse rather than silently dropping declared variables.
        return Err(eyre!(
            "Docker lifecycle execution does not forward hardware_lifecycle.env \
             into the container; refusing lifecycle on Docker backend"
        ));
    }
    Ok(sandbox)
}

fn lifecycle_executor(
    skill_dir: &Path,
    lifecycle: &octos_plugin::HardwareLifecycle,
    requires_network: bool,
) -> Result<LifecycleExecutor> {
    let sandbox = lifecycle_sandbox(skill_dir, requires_network)?;
    Ok(lifecycle_executor_with_sandbox(
        skill_dir, lifecycle, sandbox,
    ))
}

fn lifecycle_executor_with_sandbox(
    skill_dir: &Path,
    lifecycle: &octos_plugin::HardwareLifecycle,
    sandbox: Box<dyn Sandbox>,
) -> LifecycleExecutor {
    LifecycleExecutor::new(
        Box::new(LifecycleSandboxAdapter(sandbox)),
        skill_dir.to_path_buf(),
    )
    .with_env_allowlist(lifecycle.env.clone())
}

/// Run hardware lifecycle phases (preflight → init → ready_check) and then
/// register the skill's tools (static binary-protocol or HTTP-discovered)
/// into `registry`.
///
/// This is an **async** entry point designed for use by the CLI install
/// path and by tests. It does NOT copy files — the skill directory must
/// already exist on disk.
///
/// Returns an error (and registers **no** tools) if any critical lifecycle
/// phase fails or if HTTP tool discovery returns a non-2xx response.
pub async fn activate_skill(
    registry: &mut ToolRegistry,
    skill_dir: &Path,
    extra_env: &[(String, String)],
    lifecycle_allowed: bool,
) -> Result<SkillActivateResult> {
    let manifest_path = skill_dir.join("manifest.json");
    if !manifest_path.exists() {
        // No manifest → nothing to activate. Skills without manifest.json
        // are app-level docs only and have no tools or lifecycle.
        return Ok(SkillActivateResult { tool_names: vec![] });
    }
    let content = std::fs::read_to_string(&manifest_path).map_err(|e| {
        eyre!(
            "could not read manifest.json in {}: {e}",
            skill_dir.display()
        )
    })?;
    let manifest: PluginManifest = serde_json::from_str(&content)
        .map_err(|e| eyre!("invalid manifest.json in {}: {e}", skill_dir.display()))?;

    let skill_name = manifest.name.clone();

    // 1) Run hardware_lifecycle phases only after the operator/source trust
    // gate. When allowed, commands are confined to the skill directory, get
    // network only when the manifest declares it, and inherit declared env.
    if let Some(lifecycle) = manifest.hardware_lifecycle.as_ref() {
        if lifecycle_allowed {
            let executor = lifecycle_executor(skill_dir, lifecycle, manifest.requires_network)?;
            for (phase, steps) in [
                (LifecyclePhase::Preflight, &lifecycle.preflight),
                (LifecyclePhase::Init, &lifecycle.init),
                (LifecyclePhase::ReadyCheck, &lifecycle.ready_check),
            ] {
                if steps.is_empty() {
                    continue;
                }
                let result = executor.run_phase(phase, steps).await;
                if !result.success {
                    return Err(eyre!(
                        "skill {skill_name} {} failed: {}",
                        phase.as_str(),
                        result.error.unwrap_or_default()
                    ));
                }
                info!(
                    skill = %skill_name,
                    phase = phase.as_str(),
                    "lifecycle phase completed"
                );
            }
        } else {
            info!(
                skill = %skill_name,
                "skill lifecycle is not explicitly allowed; skipping lifecycle commands"
            );
        }
    }

    // 2) Tool discovery (static binary-protocol is the existing path;
    //    Http triggers GET <base_url>/tools registration).
    let mut tool_names = Vec::new();
    match &manifest.tool_discovery {
        ToolDiscovery::Static => {
            // Important #2 fix from PR #1260 review: load ONLY this skill,
            // not its parent directory. The previous `load_into(&[parent])`
            // scanned every sibling skill folder as a side-effect, leaking
            // their tools into the registry on install.
            //
            // `extras` (MCP servers, hooks, prompts) are intentionally
            // NOT resolved here — `activate_skill`'s contract is tool
            // registration only; extras flow through
            // `PluginLoader::load_into_with_options` at runtime startup.
            //
            // Error handling: hard-fail on every `load_plugin` error EXCEPT
            // "no executable found", which is a legitimate "tools declared
            // in the manifest but binary deferred" case (matches upstream's
            // RFC-2 valid-manifest install flow: copy files now, register
            // tools later at agent boot). Integrity errors (sha256 mismatch,
            // oversized binary, `require_signed` rejection) MUST propagate
            // so install rolls back instead of silently parking a broken
            // skill on disk. Reviewer post-merge feedback (Critical A).
            match PluginLoader::load_plugin(skill_dir, extra_env) {
                Ok((tools, _extras)) => {
                    for tool in tools {
                        let name = tool.name().to_string();
                        // Never over a compiled-in tool (see the loader).
                        if registry.is_builtin_name(&name) {
                            warn!(
                                tool = %name,
                                "plugin tool name collides with a built-in tool, skipping"
                            );
                            continue;
                        }
                        registry.mark_as_plugin(&name);
                        tool_names.push(name);
                        registry.register(tool);
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("no executable found") {
                        // Manifest declares `tools: [...]` but ships no
                        // binary — typical for RFC-2 docs-first skills.
                        // Tool registration will happen at agent boot if
                        // a binary appears later; otherwise the skill
                        // legitimately has no static tools.
                        info!(
                            skill = %skill_name,
                            "skill has no executable; static tool registration deferred to agent boot"
                        );
                    } else {
                        return Err(e).wrap_err_with(|| {
                            format!(
                                "skill {skill_name} static load_plugin failed; install must roll back"
                            )
                        });
                    }
                }
            }
        }
        ToolDiscovery::Http { base_url } => {
            // Finding 2 (PR #1260 review): hard-fail on HTTP discovery
            // error rather than silently registering zero tools.
            let names = install_http_tools_from_catalog(registry, base_url, Some(&manifest))
                .await
                .wrap_err_with(|| {
                    format!("skill {skill_name} HTTP tool discovery from {base_url} failed")
                })?;
            info!(
                skill = %skill_name,
                base_url = %base_url,
                count = names.len(),
                "HTTP tool discovery completed"
            );
            tool_names.extend(names);
        }
    }

    Ok(SkillActivateResult { tool_names })
}

/// Run the `shutdown` lifecycle phase for a skill before uninstall.
///
/// Best-effort: failures are logged but do not abort the uninstall (since
/// the alternative is leaving the skill directory on disk, which is worse).
/// No-op when the manifest has no `hardware_lifecycle` block.
pub async fn run_shutdown_phase(skill_dir: &Path, lifecycle_allowed: bool) {
    let manifest_path = skill_dir.join("manifest.json");
    if !manifest_path.exists() {
        return;
    }
    let content = match std::fs::read_to_string(&manifest_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(skill_dir = %skill_dir.display(), error = %e, "shutdown: could not read manifest");
            return;
        }
    };
    let manifest: PluginManifest = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            warn!(skill_dir = %skill_dir.display(), error = %e, "shutdown: invalid manifest");
            return;
        }
    };

    if let Some(lifecycle) = manifest.hardware_lifecycle.as_ref() {
        if lifecycle.shutdown.is_empty() {
            return;
        }
        if !lifecycle_allowed {
            info!(
                skill = %manifest.name,
                "skill shutdown lifecycle is not explicitly allowed; skipping commands"
            );
            return;
        }
        let executor = match lifecycle_executor(skill_dir, lifecycle, manifest.requires_network) {
            Ok(executor) => executor,
            Err(e) => {
                warn!(
                    skill = %manifest.name,
                    error = %e,
                    "shutdown sandbox unavailable (best-effort, continuing removal)"
                );
                return;
            }
        };
        let result = executor
            .run_phase(LifecyclePhase::Shutdown, &lifecycle.shutdown)
            .await;
        if !result.success {
            warn!(
                skill = %manifest.name,
                error = ?result.error,
                "shutdown phase failed (best-effort, continuing removal)"
            );
        } else {
            info!(skill = %manifest.name, "shutdown phase completed");
        }
    }
}

/// Walk `plugin_dirs` for skills declaring `tool_discovery: { type: "http", ... }`
/// and register their HTTP-backed tools into `registry`.
///
/// Called from the agent boot path (chat / gateway / serve) AFTER the
/// sync [`PluginLoader::load_into`] has registered the static skills.
/// Finding 2 (PR #1260 review): the first HTTP discovery failure hard-
/// fails the whole boot — an installed HTTP-backed skill with a missing
/// bridge must not register zero tools and pretend to succeed.
pub async fn register_http_skills_on_startup(
    registry: &mut ToolRegistry,
    plugin_dirs: &[PathBuf],
) -> Result<()> {
    for dir in plugin_dirs {
        if !dir.exists() {
            continue;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(plugin_dir = %dir.display(), error = %e, "could not read plugin dir for HTTP discovery");
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("manifest.json");
            if !manifest_path.exists() {
                continue;
            }
            let content = match std::fs::read_to_string(&manifest_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let manifest: PluginManifest = match serde_json::from_str(&content) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if let ToolDiscovery::Http { base_url } = &manifest.tool_discovery {
                let names = install_http_tools_from_catalog(registry, base_url, Some(&manifest))
                    .await
                    .wrap_err_with(|| {
                        format!(
                            "HTTP tool discovery failed for skill at {} (bridge {})",
                            path.display(),
                            base_url
                        )
                    })?;
                info!(
                    plugin_dir = %path.display(),
                    base_url = %base_url,
                    count = names.len(),
                    "registered HTTP-discovered tools at startup",
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod execution_gate_tests {
    use super::*;

    fn lifecycle_fixture(command: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::json!({
                "name": "untrusted-probe",
                "version": "0.1.0",
                "tools": [],
                "hardware_lifecycle": {
                    "init": [{"label": "init", "command": command}],
                    "shutdown": [{"label": "shutdown", "command": command}]
                }
            })
            .to_string(),
        )
        .unwrap();
        let marker = dir.join("executed.marker");
        (tmp, marker)
    }

    #[tokio::test]
    async fn init_and_shutdown_do_not_execute_without_explicit_allowance() {
        let (tmp, marker) = lifecycle_fixture("echo executed > executed.marker");
        let dir = tmp.path().join("skill");
        let mut registry = ToolRegistry::new();
        activate_skill(&mut registry, &dir, &[], false)
            .await
            .expect("denied lifecycle still permits non-executable tool registration");
        assert!(!marker.exists(), "init command ran without allowance");
        run_shutdown_phase(&dir, false).await;
        assert!(!marker.exists(), "shutdown command ran without allowance");
    }

    #[test]
    fn no_sandbox_backend_is_rejected_instead_of_silently_running_unconfined() {
        let error = match require_confined_lifecycle_sandbox(Box::new(crate::sandbox::NoSandbox)) {
            Ok(_) => panic!("NoSandbox must not satisfy the lifecycle execution gate"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("real sandbox backend"));
    }

    #[test]
    fn docker_backend_is_rejected_so_declared_lifecycle_env_is_not_dropped() {
        let sandbox = crate::sandbox::DockerSandbox {
            config: crate::sandbox::DockerConfig::default(),
            allow_network: false,
        };
        let error = match require_confined_lifecycle_sandbox(Box::new(sandbox)) {
            Ok(_) => panic!("Docker must not satisfy the lifecycle execution gate"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("hardware_lifecycle.env"));
    }

    #[test]
    fn lifecycle_sandbox_maps_requires_network_to_backend_allow_network() {
        // Production mapping: the same Auto/fail_closed policy must set the
        // backend network flag from the manifest, not from a hand-built backend.
        // Where bwrap/sandbox-exec are absent (e.g. CI containers without
        // unprivileged userns), Auto resolves to Docker and the production
        // gate refuses — assert that refusal, and on hosts with a native
        // backend assert the wrapped command reflects the network flag.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skill");
        std::fs::create_dir_all(&dir).unwrap();

        #[cfg(target_os = "linux")]
        {
            match lifecycle_sandbox(&dir, false) {
                Ok(sandbox) => {
                    let cmd = sandbox.wrap_command("true", &dir);
                    let args: Vec<_> = cmd
                        .as_std()
                        .get_args()
                        .map(|a| a.to_string_lossy().to_string())
                        .collect();
                    assert!(
                        args.windows(2).any(|w| w[0] == "--unshare-net"),
                        "offline bwrap must unshare net"
                    );
                }
                Err(e) => {
                    // Docker-only host: production must refuse lifecycle.
                    assert!(
                        e.to_string().contains("hardware_lifecycle.env"),
                        "unexpected refusal on network=false: {e}"
                    );
                }
            }
            match lifecycle_sandbox(&dir, true) {
                Ok(sandbox) => {
                    let cmd = sandbox.wrap_command("true", &dir);
                    let args: Vec<_> = cmd
                        .as_std()
                        .get_args()
                        .map(|a| a.to_string_lossy().to_string())
                        .collect();
                    assert!(
                        !args.windows(2).any(|w| w[0] == "--unshare-net"),
                        "online bwrap must not unshare net"
                    );
                }
                Err(e) => {
                    assert!(
                        e.to_string().contains("hardware_lifecycle.env"),
                        "unexpected refusal on network=true: {e}"
                    );
                }
            }
        }

        #[cfg(target_os = "macos")]
        {
            fn profile_of(sandbox: &dyn Sandbox, dir: &Path) -> String {
                let cmd = sandbox.wrap_command("true", dir);
                let args: Vec<_> = cmd
                    .as_std()
                    .get_args()
                    .map(|a| a.to_string_lossy().to_string())
                    .collect();
                // sandbox-exec -p <profile> sh -c true
                args.windows(2)
                    .find(|w| w[0] == "-p")
                    .map(|w| w[1].clone())
                    .unwrap_or_default()
            }
            match lifecycle_sandbox(&dir, false) {
                Ok(sandbox) => {
                    let profile = profile_of(sandbox.as_ref(), &dir);
                    assert!(
                        profile.contains("(deny network*)"),
                        "offline macOS profile must deny network: {profile}"
                    );
                }
                Err(e) => assert!(
                    e.to_string().contains("hardware_lifecycle.env"),
                    "unexpected refusal on network=false: {e}"
                ),
            }
            match lifecycle_sandbox(&dir, true) {
                Ok(sandbox) => {
                    let profile = profile_of(sandbox.as_ref(), &dir);
                    assert!(
                        profile.contains("(allow network*)"),
                        "online macOS profile must allow network: {profile}"
                    );
                }
                Err(e) => assert!(
                    e.to_string().contains("hardware_lifecycle.env"),
                    "unexpected refusal on network=true: {e}"
                ),
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            // Windows/Other resolve to AppContainer or Docker; both must either
            // honour the policy or refuse with the Docker env reason.
            for requires_network in [false, true] {
                match lifecycle_sandbox(&dir, requires_network) {
                    Ok(_) => {}
                    Err(e) => assert!(
                        e.to_string().contains("hardware_lifecycle.env"),
                        "unexpected refusal on network={requires_network}: {e}"
                    ),
                }
            }
        }
    }

    #[tokio::test]
    async fn allowed_lifecycle_failure_is_the_command_exit_not_sandbox_unavailable() {
        // Prove the rollback path observes a REAL command failure (exit 7)
        // after entering the sandbox, not a sandbox-unavailable early exit.
        // Use an injectable bwrap backend when the host supports it; otherwise
        // use a recording sandbox that really runs the command so we still
        // observe the exit-7 phase failure rather than an early refusal.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::json!({
                "name": "rollback-probe",
                "version": "0.1.0",
                "tools": [],
                "hardware_lifecycle": {
                    "init": [{"label": "fail", "command": "exit 7"}]
                }
            })
            .to_string(),
        )
        .unwrap();

        let manifest = serde_json::from_str::<PluginManifest>(
            &std::fs::read_to_string(dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        let lifecycle = manifest.hardware_lifecycle.clone().unwrap();

        // Try the production bwrap backend first (Linux with userns).
        #[cfg(target_os = "linux")]
        let sandbox: Box<dyn Sandbox> = {
            let probe_dir = tempfile::tempdir().unwrap();
            let probe_dir_path = probe_dir.path().join("probe");
            std::fs::create_dir_all(&probe_dir_path).unwrap();
            let probe = std::process::Command::new("bwrap")
                .args([
                    "--ro-bind",
                    "/usr",
                    "/usr",
                    "--ro-bind",
                    "/lib",
                    "/lib",
                    "--ro-bind",
                    "/lib64",
                    "/lib64",
                    "--ro-bind",
                    "/bin",
                    "/bin",
                    "--ro-bind",
                    "/sbin",
                    "/sbin",
                    "--ro-bind",
                    "/etc",
                    "/etc",
                    "--tmpfs",
                    "/tmp",
                    "--bind",
                    probe_dir_path.to_str().unwrap(),
                    probe_dir_path.to_str().unwrap(),
                    "--dev",
                    "/dev",
                    "--proc",
                    "/proc",
                    "--unshare-pid",
                    "--die-with-parent",
                    "--chdir",
                    probe_dir_path.to_str().unwrap(),
                    "--",
                    "sh",
                    "-c",
                    "true",
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if probe.map(|s| s.success()).unwrap_or(false) {
                Box::new(crate::sandbox::BwrapSandbox {
                    allow_network: false,
                    workspace_write: true,
                    repo_git_write: None,
                    read_only_view: None,
                })
            } else {
                Box::new(RecordingSandbox::default())
            }
        };
        #[cfg(not(target_os = "linux"))]
        let sandbox: Box<dyn Sandbox> = Box::new(RecordingSandbox::default());

        let executor = lifecycle_executor_with_sandbox(&dir, &lifecycle, sandbox);
        let phase = executor
            .run_phase(LifecyclePhase::Init, &lifecycle.init)
            .await;
        assert!(!phase.success, "exit 7 lifecycle must fail the phase");
        let error = phase.error.unwrap_or_default();
        assert!(
            error.contains("exited with"),
            "expected real command exit, got: {error}"
        );
        assert!(
            !error.contains("sandbox unavailable"),
            "must not be a sandbox-unavailable early exit: {error}"
        );
    }

    /// A sandbox that records the wrap and then really runs the command
    /// unconfined. Used only where no native backend is available so the
    /// test still observes a real command exit instead of an early refusal.
    #[derive(Default)]
    struct RecordingSandbox {
        wrapped: std::sync::Mutex<Vec<String>>,
    }

    impl Sandbox for RecordingSandbox {
        fn wrap_command(&self, shell_command: &str, cwd: &Path) -> tokio::process::Command {
            self.wrapped.lock().unwrap().push(shell_command.to_string());
            crate::sandbox::NoSandbox.wrap_command(shell_command, cwd)
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn allowed_macos_lifecycle_confines_cwd_and_env_then_fails_real_exit() {
        if std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg("(version 1)(allow default)")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| !status.success())
            .unwrap_or(true)
        {
            eprintln!("sandbox-exec unavailable; skipping real backend lifecycle boundary test");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::json!({
                "name": "macos-probe",
                "version": "0.1.0",
                "tools": [],
                "requires_network": false,
                "hardware_lifecycle": {
                    "env": ["OCTOS_LIFECYCLE_ALLOWED"],
                    "init": [
                        {"label": "inside", "command": "pwd && printf 'allowed=%s\\n' \"$OCTOS_LIFECYCLE_ALLOWED\" && printf 'secret=%s\\n' \"$OCTOS_LIFECYCLE_SECRET\" && touch init-executed.marker"},
                        {"label": "outside", "command": "touch /etc/lifecycle-outside-write.marker"}
                    ],
                    "shutdown": [{"label": "outside-shutdown", "command": "touch /etc/lifecycle-shutdown-write.marker"}]
                }
            })
            .to_string(),
        )
        .unwrap();
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("OCTOS_LIFECYCLE_ALLOWED", "present");
            std::env::set_var("OCTOS_LIFECYCLE_SECRET", "must-not-leak");
        }
        let _env_guard = EnvVarGuard::new(&["OCTOS_LIFECYCLE_ALLOWED", "OCTOS_LIFECYCLE_SECRET"]);

        let manifest = serde_json::from_str::<PluginManifest>(
            &std::fs::read_to_string(dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        let sandbox = crate::sandbox::MacosSandbox {
            allow_network: false,
            read_allow_paths: vec![dir.to_string_lossy().into_owned()],
            workspace_write: true,
            repo_git_write: None,
            build_cache_slot: None,
            write_allow_globs: None,
            toolchain_write_grants: crate::sandbox::ToolchainWriteGrants::default(),
            read_only_view: None,
        };
        let executor = LifecycleExecutor::new(
            Box::new(LifecycleSandboxAdapter(Box::new(sandbox))),
            dir.clone(),
        )
        .with_env_allowlist(manifest.hardware_lifecycle.as_ref().unwrap().env.clone());

        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                executor
                    .run_phase(
                        LifecyclePhase::Init,
                        &manifest.hardware_lifecycle.as_ref().unwrap().init,
                    )
                    .await
            });
        assert!(
            !result.success,
            "writing outside the skill cwd must fail the confined lifecycle"
        );
        assert!(
            dir.join("init-executed.marker").exists(),
            "the cwd-confined init command did not really execute"
        );
        assert!(!std::path::Path::new("/etc/lifecycle-outside-write.marker").exists());
        assert!(!std::path::Path::new("/etc/lifecycle-shutdown-write.marker").exists());
    }

    #[cfg(target_os = "macos")]
    struct EnvVarGuard {
        names: Vec<String>,
    }

    #[cfg(target_os = "macos")]
    impl EnvVarGuard {
        fn new(names: &[&str]) -> Self {
            Self {
                names: names.iter().map(|name| name.to_string()).collect(),
            }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for name in &self.names {
                #[allow(unsafe_code)]
                unsafe {
                    std::env::remove_var(name);
                }
            }
        }
    }
}
