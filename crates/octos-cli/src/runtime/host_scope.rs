//! Process-wide restrictions for a private stdio embedding host. These narrow
//! the runtime's extension surface; OS isolation remains the host's duty.
use std::sync::OnceLock;

static PROFILE: OnceLock<String> = OnceLock::new();

#[cfg(feature = "api")]
pub(crate) fn enable(profile: &str) -> eyre::Result<()> {
    if profile.is_empty()
        || profile.len() > 64
        || profile.starts_with('-')
        || !profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        eyre::bail!("invalid scoped profile");
    }
    PROFILE
        .set(profile.to_owned())
        .map_err(|_| eyre::eyre!("host scope already set"))
}
pub(crate) fn profile() -> Option<&'static str> {
    PROFILE.get().map(String::as_str)
}
pub(crate) fn active() -> bool {
    profile().is_some()
}

/// SearXNG is explicitly allowed by Octos to point at a private network. A
/// room member can choose the search query, so a shared host must not expose
/// that owner-configured endpoint as an arbitrary local-network query proxy.
pub(crate) fn restrict_search_provider_keys(keys: &mut std::collections::HashMap<String, String>) {
    keys.remove("searxng");
}

/// A selected profile supplies its model endpoint and credentials, not the
/// sandbox authority for a shared coding session. In particular Octos home
/// must never be added to the shell's read paths: it contains the copied
/// provider credentials. The host separately confines the entire process.
pub(crate) fn constrain_sandbox(config: &mut octos_agent::SandboxConfig) {
    // Only macOS has the strict-read implementation for this host mode.
    // Other systems must fail closed until they have equivalent isolation.
    #[cfg(target_os = "macos")]
    let read_allow_paths = [
        "/System",
        "/usr",
        "/bin",
        "/sbin",
        "/private/etc",
        "/dev",
        "/private/var/db/timezone",
    ];
    #[cfg(not(target_os = "macos"))]
    let read_allow_paths = ["/usr", "/bin", "/lib", "/lib64", "/etc", "/dev"];
    *config = octos_agent::SandboxConfig {
        fail_closed: true,
        allow_network: false,
        allow_toolchains: false,
        strict_read_paths: true,
        mode: octos_agent::SandboxMode::Macos,
        read_allow_paths: read_allow_paths.into_iter().map(str::to_owned).collect(),
        ..Default::default()
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_search_excludes_private_searx_endpoint_but_keeps_public_provider_keys() {
        let mut keys = std::collections::HashMap::from([
            ("searxng".into(), "http://127.0.0.1:9999".into()),
            ("brave".into(), "synthetic-key".into()),
        ]);
        restrict_search_provider_keys(&mut keys);
        assert!(!keys.contains_key("searxng"));
        assert_eq!(keys.get("brave").map(String::as_str), Some("synthetic-key"));
    }
    #[test]
    fn shared_host_sandbox_discards_profile_overrides_and_credential_paths() {
        let mut config = octos_agent::SandboxConfig {
            enabled: false,
            mode: octos_agent::SandboxMode::None,
            allow_network: true,
            read_allow_paths: vec!["/private/owner/profile".into()],
            repo_git_write: Some("/private/owner/repo/.git".into()),
            build_cache_slot: Some("/private/owner/cache".into()),
            allow_toolchains: true,
            ..Default::default()
        };
        constrain_sandbox(&mut config);
        assert!(
            config.enabled
                && config.fail_closed
                && config.strict_read_paths
                && !config.allow_network
                && !config.allow_toolchains
        );
        assert!(!config.read_allow_paths.is_empty());
        assert!(!config.read_allow_paths.iter().any(|p| p.contains("owner")));
        assert!(config.repo_git_write.is_none() && config.build_cache_slot.is_none());
        #[cfg(target_os = "macos")]
        assert!(matches!(config.mode, octos_agent::SandboxMode::Macos));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn shared_shell_can_read_its_workspace_but_not_profile_or_network() {
        use std::{fs, net::TcpListener};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let work = root.join("work");
        fs::create_dir(&work).unwrap();
        fs::write(work.join("visible.txt"), "workspace").unwrap();
        fs::write(root.join("secret.txt"), "synthetic-secret").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut config = octos_agent::SandboxConfig::default();
        constrain_sandbox(&mut config);
        let sandbox = octos_agent::create_sandbox(&config);
        assert!(!sandbox.is_noop());
        let command = format!(
            "cat visible.txt; if cat ../secret.txt >/dev/null 2>&1; then printf SECRET; else printf PROFILE_DENIED; fi; if /usr/bin/nc -w 1 127.0.0.1 {port} >/dev/null 2>&1; then printf NETWORK; else printf NETWORK_DENIED; fi"
        );
        let result = sandbox
            .wrap_command(&command, &work)
            .output()
            .await
            .unwrap();
        assert!(result.status.success());
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            "workspacePROFILE_DENIEDNETWORK_DENIED"
        );
    }
}
