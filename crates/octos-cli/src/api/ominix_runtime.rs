//! OMiniX runtime discovery, repair, and launchd control.

use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tar::Archive;
use tokio::process::Command;

pub(crate) const OMINIX_LABEL: &str = "io.ominix.ominix-api";
pub(crate) const DEFAULT_VOICE_MODELS: &[(&str, &str)] =
    &[("qwen3-asr-1.7b", "asr"), ("qwen3-tts", "tts")];
const DEFAULT_RUNTIME_URL: &str = "http://127.0.0.1:8081";
const DEFAULT_RELEASE_REPO: &str = "OminiX-ai/OminiX-API";
const PREFERRED_PORTS: &[u16] = &[8081, 9090, 8080];
const INSTALL_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const INSTALL_METADATA_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SERVER_CONFIG: &str = r#"{
  "allowed_models": {
    "llm": ["*"],
    "image": ["flux-8bit", "zimage"],
    "asr": ["qwen3-asr", "paraformer"],
    "tts": ["qwen3-tts", "qwen3-tts-base"],
    "vlm": ["*"]
  }
}
"#;

#[derive(Debug, Clone)]
pub(crate) struct OminixRuntimeConfig {
    home_dir: PathBuf,
    binary_path: PathBuf,
    metallib_path: PathBuf,
    models_dir: PathBuf,
    skip_launchctl: bool,
    require_metallib: bool,
    skip_platform_check: bool,
    release_repo: String,
    release_version: Option<String>,
    release_base_url: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct OminixRuntimeIssue {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub fixable: bool,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct OminixHealthProbe {
    pub healthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct OminixServiceProbe {
    pub registered: bool,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct OminixVoiceModelStatus {
    pub id: String,
    pub role: String,
    pub status: String,
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct OminixRuntimeStatus {
    pub state: String,
    pub url: String,
    pub url_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub home_dir: String,
    pub ominix_dir: String,
    pub binary_path: String,
    pub binary_installed: bool,
    pub metallib_path: String,
    pub metallib_installed: bool,
    pub models_dir: String,
    pub models_dir_exists: bool,
    pub plist_path: String,
    pub plist_exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plist_port: Option<u16>,
    pub discovery_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery_url: Option<String>,
    pub service_registered: bool,
    pub service_running: bool,
    pub launchctl_skipped: bool,
    pub health: OminixHealthProbe,
    pub voice_models_ready: bool,
    pub voice_models: Vec<OminixVoiceModelStatus>,
    pub issues: Vec<OminixRuntimeIssue>,
    pub can_repair: bool,
    pub suggested_action: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct OminixRepairResponse {
    pub ok: bool,
    pub message: String,
    pub dry_run: bool,
    pub actions: Vec<String>,
    pub status: OminixRuntimeStatus,
}

#[derive(Debug, Serialize)]
pub(crate) struct OminixServiceActionResponse {
    pub ok: bool,
    pub message: String,
    pub actions: Vec<String>,
    pub status: OminixRuntimeStatus,
}

impl OminixRuntimeConfig {
    pub(crate) fn from_env() -> Self {
        let home_dir = std::env::var_os("OCTOS_OMINIX_HOME")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/tmp"));

        let binary_path = std::env::var_os("OCTOS_OMINIX_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let local = home_dir.join(".local/bin/ominix-api");
                if local.exists() {
                    local
                } else {
                    find_in_path("ominix-api").unwrap_or(local)
                }
            });

        let metallib_path = std::env::var_os("OCTOS_OMINIX_METALLIB")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let beside_binary = binary_path
                    .parent()
                    .unwrap_or_else(|| Path::new("/"))
                    .join("mlx.metallib");
                if beside_binary.exists() {
                    beside_binary
                } else {
                    home_dir.join(".local/bin/mlx.metallib")
                }
            });

        let models_dir = std::env::var_os("OMINIX_MODELS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let lower = home_dir.join(".ominix/models");
                if lower.exists() {
                    lower
                } else {
                    home_dir.join(".OminiX/models")
                }
            });

        let skip_launchctl = std::env::var("OCTOS_OMINIX_SKIP_LAUNCHCTL")
            .map(|v| is_truthy(&v))
            .unwrap_or(false);
        let require_metallib = std::env::var("OCTOS_OMINIX_REQUIRE_METALLIB")
            .map(|v| is_truthy(&v))
            .unwrap_or(false);
        let skip_platform_check = std::env::var("OCTOS_OMINIX_SKIP_PLATFORM_CHECK")
            .map(|v| is_truthy(&v))
            .unwrap_or(false);
        let release_repo = std::env::var("OCTOS_OMINIX_RELEASE_REPO")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_RELEASE_REPO.to_string());
        let release_version = std::env::var("OCTOS_OMINIX_VERSION")
            .ok()
            .map(|v| v.trim().trim_start_matches('v').to_string())
            .filter(|v| !v.is_empty());
        let release_base_url = std::env::var("OCTOS_OMINIX_RELEASE_BASE_URL")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty());

        Self {
            home_dir,
            binary_path,
            metallib_path,
            models_dir,
            skip_launchctl,
            require_metallib,
            skip_platform_check,
            release_repo,
            release_version,
            release_base_url,
        }
    }

    fn ominix_dir(&self) -> PathBuf {
        self.home_dir.join(".ominix")
    }

    fn alternate_ominix_dir(&self) -> PathBuf {
        self.home_dir.join(".OminiX")
    }

    fn launch_agents_dir(&self) -> PathBuf {
        self.home_dir.join("Library/LaunchAgents")
    }

    fn plist_path(&self) -> PathBuf {
        self.launch_agents_dir()
            .join(format!("{OMINIX_LABEL}.plist"))
    }

    fn discovery_path(&self) -> PathBuf {
        self.ominix_dir().join("api_url")
    }

    fn alternate_discovery_path(&self) -> PathBuf {
        self.alternate_ominix_dir().join("api_url")
    }

    fn log_path(&self) -> PathBuf {
        self.ominix_dir().join("api.log")
    }

    fn err_log_path(&self) -> PathBuf {
        self.ominix_dir().join("api.err.log")
    }

    fn release_base_url(&self) -> String {
        if let Some(url) = &self.release_base_url {
            return url.clone();
        }
        if let Some(version) = &self.release_version {
            return format!(
                "https://github.com/{}/releases/download/v{}",
                self.release_repo, version
            );
        }
        format!(
            "https://github.com/{}/releases/latest/download",
            self.release_repo
        )
    }

    fn configured_url(&self) -> (String, String) {
        if let Some(url) = std::env::var("OMINIX_API_URL")
            .ok()
            .and_then(|v| normalize_url(&v))
        {
            return (url, "env".to_string());
        }
        if let Some(url) = read_discovery_url(&self.discovery_path()) {
            return (url, "discovery".to_string());
        }
        if let Some(url) = read_discovery_url(&self.alternate_discovery_path()) {
            return (url, "legacy_discovery".to_string());
        }
        if let Some(port) = parse_plist_port(&self.plist_path()) {
            return (format!("http://127.0.0.1:{port}"), "plist".to_string());
        }
        (DEFAULT_RUNTIME_URL.to_string(), "default".to_string())
    }
}

pub(crate) fn configured_api_url() -> String {
    OminixRuntimeConfig::from_env().configured_url().0
}

pub(crate) fn models_dir() -> PathBuf {
    OminixRuntimeConfig::from_env().models_dir
}

pub(crate) async fn runtime_status(client: &reqwest::Client) -> OminixRuntimeStatus {
    let config = OminixRuntimeConfig::from_env();
    runtime_status_with_config(&config, client).await
}

pub(crate) async fn repair_runtime(
    client: &reqwest::Client,
) -> Result<OminixRepairResponse, String> {
    let config = OminixRuntimeConfig::from_env();
    repair_runtime_with_config(&config, client).await
}

pub(crate) async fn install_runtime(
    client: &reqwest::Client,
) -> Result<OminixRepairResponse, String> {
    let config = OminixRuntimeConfig::from_env();
    install_runtime_with_config(&config, client).await
}

pub(crate) async fn service_start(
    client: &reqwest::Client,
) -> Result<OminixServiceActionResponse, String> {
    let config = OminixRuntimeConfig::from_env();
    service_action_with_config(&config, client, ServiceAction::Start).await
}

pub(crate) async fn service_stop(
    client: &reqwest::Client,
) -> Result<OminixServiceActionResponse, String> {
    let config = OminixRuntimeConfig::from_env();
    service_action_with_config(&config, client, ServiceAction::Stop).await
}

pub(crate) async fn service_restart(
    client: &reqwest::Client,
) -> Result<OminixServiceActionResponse, String> {
    let config = OminixRuntimeConfig::from_env();
    service_action_with_config(&config, client, ServiceAction::Restart).await
}

async fn runtime_status_with_config(
    config: &OminixRuntimeConfig,
    client: &reqwest::Client,
) -> OminixRuntimeStatus {
    let (url, url_source) = config.configured_url();
    let health = probe_health(client, &url).await;
    let voice_models = probe_voice_models(client, &url, health.healthy).await;
    let voice_models_ready =
        !voice_models.is_empty() && voice_models.iter().all(|model| model.ready);
    let service = service_status(config).await;
    let plist_path = config.plist_path();
    let discovery_path = config.discovery_path();
    let binary_installed = config.binary_path.exists();
    let metallib_installed = config.metallib_path.exists();
    let models_dir_exists = config.models_dir.exists();
    let plist_exists = plist_path.exists();
    let plist_port = parse_plist_port(&plist_path);
    let discovery_url = read_discovery_url(&discovery_path)
        .or_else(|| read_discovery_url(&config.alternate_discovery_path()));
    let port = port_from_url(&url);

    let mut issues = Vec::new();
    if !binary_installed {
        issues.push(issue(
            "missing_binary",
            "error",
            format!(
                "OMiniX API binary not found at {}",
                config.binary_path.display()
            ),
            false,
        ));
    }
    if config.require_metallib && !metallib_installed {
        issues.push(issue(
            "missing_metallib",
            "error",
            format!(
                "MLX Metal library not found at {}",
                config.metallib_path.display()
            ),
            false,
        ));
    }
    if !models_dir_exists {
        issues.push(issue(
            "missing_models_dir",
            "warning",
            format!(
                "Model directory not found at {}",
                config.models_dir.display()
            ),
            false,
        ));
    }
    if !plist_exists {
        issues.push(issue(
            "plist_missing",
            "warning",
            "LaunchAgent plist is missing".to_string(),
            true,
        ));
    }
    if let Some(p) = port {
        if !health.healthy && !port_is_available(p) {
            issues.push(issue(
                "port_occupied",
                "error",
                format!("Port {p} is occupied but does not answer as OMiniX"),
                true,
            ));
        }
    }
    if !health.healthy {
        issues.push(issue(
            "api_unreachable",
            "error",
            format!("OMiniX API is not healthy at {url}"),
            binary_installed && (!config.require_metallib || metallib_installed),
        ));
    }
    if health.healthy && !voice_models_ready {
        for model in &voice_models {
            if !model.ready {
                issues.push(issue(
                    "missing_voice_model",
                    "warning",
                    format!(
                        "Voice model {} ({}) is not ready: {}",
                        model.id, model.role, model.status
                    ),
                    true,
                ));
            }
        }
        if voice_models.is_empty() {
            issues.push(issue(
                "model_catalog_unavailable",
                "warning",
                "OMiniX model catalog could not be checked".to_string(),
                true,
            ));
        }
    }
    if !service.registered && !config.skip_launchctl {
        issues.push(issue(
            "service_unregistered",
            "warning",
            "LaunchAgent is not registered with launchd".to_string(),
            true,
        ));
    }
    if let (Some(plist_port), Some(url_port)) = (plist_port, port) {
        if plist_port != url_port {
            issues.push(issue(
                "plist_port_mismatch",
                "warning",
                format!(
                    "LaunchAgent port {plist_port} does not match selected URL port {url_port}"
                ),
                true,
            ));
        }
    }

    let hard_blocked = issues.iter().any(|i| i.severity == "error" && !i.fixable);
    let can_repair =
        binary_installed && (!config.require_metallib || metallib_installed) && !hard_blocked;
    let state = if health.healthy && voice_models_ready {
        "healthy"
    } else if health.healthy {
        "models_missing"
    } else if can_repair {
        "repairable"
    } else {
        "missing"
    };
    let suggested_action = if health.healthy && voice_models_ready {
        "ready"
    } else if health.healthy {
        "bootstrap_voice_models"
    } else if can_repair {
        "repair"
    } else {
        "install_ominix_api_binary"
    };

    OminixRuntimeStatus {
        state: state.to_string(),
        url,
        url_source,
        port,
        home_dir: config.home_dir.display().to_string(),
        ominix_dir: config.ominix_dir().display().to_string(),
        binary_path: config.binary_path.display().to_string(),
        binary_installed,
        metallib_path: config.metallib_path.display().to_string(),
        metallib_installed,
        models_dir: config.models_dir.display().to_string(),
        models_dir_exists,
        plist_path: plist_path.display().to_string(),
        plist_exists,
        plist_port,
        discovery_path: discovery_path.display().to_string(),
        discovery_url,
        service_registered: service.registered,
        service_running: service.running,
        launchctl_skipped: config.skip_launchctl || !cfg!(target_os = "macos"),
        health,
        voice_models_ready,
        voice_models,
        issues,
        can_repair,
        suggested_action: suggested_action.to_string(),
    }
}

async fn repair_runtime_with_config(
    config: &OminixRuntimeConfig,
    client: &reqwest::Client,
) -> Result<OminixRepairResponse, String> {
    let before = runtime_status_with_config(config, client).await;
    let mut actions = Vec::new();
    if !before.binary_installed || (config.require_metallib && !before.metallib_installed) {
        return Ok(OminixRepairResponse {
            ok: false,
            message: "OMiniX API binary or MLX runtime file is missing".to_string(),
            dry_run: config.skip_launchctl || !cfg!(target_os = "macos"),
            actions,
            status: before,
        });
    }

    fs::create_dir_all(config.ominix_dir()).map_err(|e| e.to_string())?;
    actions.push(format!("ensured {}", config.ominix_dir().display()));
    fs::create_dir_all(config.launch_agents_dir()).map_err(|e| e.to_string())?;
    actions.push(format!("ensured {}", config.launch_agents_dir().display()));

    let target_port = choose_target_port(config, client, &before).await;
    let target_url = format!("http://127.0.0.1:{target_port}");
    let plist = render_plist(config, target_port);
    let plist_changed =
        write_if_changed(&config.plist_path(), &plist).map_err(|e| e.to_string())?;
    if plist_changed {
        actions.push(format!("wrote {}", config.plist_path().display()));
    } else {
        actions.push(format!("kept {}", config.plist_path().display()));
    }
    write_if_changed(&config.discovery_path(), &(target_url.clone() + "\n"))
        .map_err(|e| e.to_string())?;
    actions.push(format!("wrote {}", config.discovery_path().display()));

    let should_touch_launchd =
        !before.health.healthy || plist_changed || !before.service_registered;
    if config.skip_launchctl || !cfg!(target_os = "macos") {
        actions.push("skipped launchctl by configuration or non-macOS host".to_string());
    } else if should_touch_launchd {
        bootout(config).await;
        bootstrap(config).await?;
        kickstart().await?;
        actions.push("bootstrapped launchd service".to_string());
        wait_for_health(client, &target_url, Duration::from_secs(15)).await;
    } else {
        actions.push("launchd already healthy; no restart needed".to_string());
    }

    let status = runtime_status_with_config(config, client).await;
    let ok = status.health.healthy || config.skip_launchctl || !cfg!(target_os = "macos");
    let message = if status.health.healthy {
        format!("OMiniX API is healthy at {}", status.url)
    } else if ok {
        "OMiniX runtime files were written; launchctl was skipped".to_string()
    } else {
        format!(
            "OMiniX repair ran, but API is still not healthy at {}",
            status.url
        )
    };

    Ok(OminixRepairResponse {
        ok,
        message,
        dry_run: config.skip_launchctl || !cfg!(target_os = "macos"),
        actions,
        status,
    })
}

async fn install_runtime_with_config(
    config: &OminixRuntimeConfig,
    client: &reqwest::Client,
) -> Result<OminixRepairResponse, String> {
    let before = runtime_status_with_config(config, client).await;
    let mut actions = Vec::new();

    if !before.binary_installed {
        validate_install_platform(config)?;
        install_ominix_binary(config, client, &mut actions).await?;
    } else {
        actions.push(format!("kept existing {}", config.binary_path.display()));
    }

    fs::create_dir_all(&config.models_dir).map_err(|e| e.to_string())?;
    actions.push(format!("ensured {}", config.models_dir.display()));
    fs::create_dir_all(config.ominix_dir()).map_err(|e| e.to_string())?;
    actions.push(format!("ensured {}", config.ominix_dir().display()));

    let server_config_path = config.alternate_ominix_dir().join("server_config.json");
    let server_config_changed =
        write_if_missing(&server_config_path, DEFAULT_SERVER_CONFIG).map_err(|e| e.to_string())?;
    actions.push(format!(
        "{} {}",
        if server_config_changed {
            "wrote"
        } else {
            "kept"
        },
        server_config_path.display()
    ));

    let mut repaired = repair_runtime_with_config(config, client).await?;
    actions.append(&mut repaired.actions);
    repaired.actions = actions;
    Ok(repaired)
}

fn validate_install_platform(config: &OminixRuntimeConfig) -> Result<(), String> {
    if config.skip_platform_check {
        return Ok(());
    }
    if std::env::consts::OS != "macos" {
        return Err(format!(
            "OminiX API install requires macOS Apple Silicon; detected {}",
            std::env::consts::OS
        ));
    }
    if std::env::consts::ARCH != "aarch64" && std::env::consts::ARCH != "arm64" {
        return Err(format!(
            "OminiX API install requires Apple Silicon arm64; detected {}",
            std::env::consts::ARCH
        ));
    }
    Ok(())
}

async fn install_ominix_binary(
    config: &OminixRuntimeConfig,
    client: &reqwest::Client,
    actions: &mut Vec<String>,
) -> Result<(), String> {
    let base_url = config.release_base_url();
    let checksums_url = format!("{base_url}/checksums.txt");
    let checksums = fetch_text(client, &checksums_url, INSTALL_METADATA_TIMEOUT).await?;
    actions.push(format!("downloaded {checksums_url}"));

    let (archive_name, expected_sha) = select_release_archive(&checksums)
        .ok_or_else(|| "checksums.txt did not list a darwin-aarch64 OMiniX archive".to_string())?;
    let archive_url = format!("{base_url}/{archive_name}");
    let archive_bytes = fetch_bytes(client, &archive_url, INSTALL_DOWNLOAD_TIMEOUT).await?;
    actions.push(format!("downloaded {archive_url}"));

    verify_sha256(&archive_bytes, &expected_sha)?;
    actions.push(format!("verified sha256 for {archive_name}"));

    let binary_bytes = extract_ominix_binary(&archive_bytes)?;
    write_executable_atomic(&config.binary_path, &binary_bytes).map_err(|e| e.to_string())?;
    actions.push(format!("installed {}", config.binary_path.display()));
    Ok(())
}

async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<String, String> {
    client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("failed to download {url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("failed to download {url}: {e}"))?
        .text()
        .await
        .map_err(|e| format!("failed to read {url}: {e}"))
}

async fn fetch_bytes(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let bytes = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("failed to download {url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("failed to download {url}: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("failed to read {url}: {e}"))?;
    Ok(bytes.to_vec())
}

fn select_release_archive(checksums: &str) -> Option<(String, String)> {
    checksums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let sha = parts.next()?.trim().to_ascii_lowercase();
        let name = parts.next()?.trim_start_matches('*').to_string();
        if sha.len() == 64
            && sha.chars().all(|c| c.is_ascii_hexdigit())
            && name.starts_with("ominix-api-")
            && name.ends_with("-darwin-aarch64.tar.gz")
        {
            Some((name, sha))
        } else {
            None
        }
    })
}

fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "checksum mismatch for OMiniX archive: expected {expected}, actual {actual}"
        ))
    }
}

fn extract_ominix_binary(archive_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let decoder = GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| format!("failed to read OMiniX archive: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("failed to read OMiniX archive entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("failed to read OMiniX archive path: {e}"))?;
        if entry.header().entry_type().is_file()
            && path.file_name().and_then(|v| v.to_str()) == Some("ominix-api")
        {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|e| format!("failed to extract ominix-api: {e}"))?;
            if bytes.is_empty() {
                return Err("ominix-api archive entry is empty".to_string());
            }
            return Ok(bytes);
        }
    }
    Err("OMiniX archive did not contain ominix-api".to_string())
}

enum ServiceAction {
    Start,
    Stop,
    Restart,
}

async fn service_action_with_config(
    config: &OminixRuntimeConfig,
    client: &reqwest::Client,
    action: ServiceAction,
) -> Result<OminixServiceActionResponse, String> {
    let mut actions = Vec::new();
    if config.skip_launchctl || !cfg!(target_os = "macos") {
        actions.push("skipped launchctl by configuration or non-macOS host".to_string());
        let status = runtime_status_with_config(config, client).await;
        return Ok(OminixServiceActionResponse {
            ok: true,
            message: "launchctl skipped".to_string(),
            actions,
            status,
        });
    }

    match action {
        ServiceAction::Start => {
            bootstrap(config).await?;
            kickstart().await?;
            actions.push("started launchd service".to_string());
        }
        ServiceAction::Stop => {
            bootout(config).await;
            actions.push("stopped launchd service".to_string());
        }
        ServiceAction::Restart => {
            bootout(config).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            bootstrap(config).await?;
            kickstart().await?;
            actions.push("restarted launchd service".to_string());
        }
    }

    let status = runtime_status_with_config(config, client).await;
    let ok = match action {
        ServiceAction::Stop => !status.service_running,
        ServiceAction::Start | ServiceAction::Restart => status.service_registered,
    };
    Ok(OminixServiceActionResponse {
        ok,
        message: if ok {
            "launchd action completed".to_string()
        } else {
            "launchd action completed but final status is not ready".to_string()
        },
        actions,
        status,
    })
}

async fn choose_target_port(
    config: &OminixRuntimeConfig,
    client: &reqwest::Client,
    before: &OminixRuntimeStatus,
) -> u16 {
    if before.health.healthy {
        if let Some(port) = before.port {
            return port;
        }
    }
    for candidate in [
        before.port,
        parse_plist_port(&config.plist_path()),
        Some(8081),
        Some(9090),
        Some(8080),
    ]
    .into_iter()
    .flatten()
    {
        let url = format!("http://127.0.0.1:{candidate}");
        if probe_health(client, &url).await.healthy || port_is_available(candidate) {
            return candidate;
        }
    }
    for port in 8082..=8099 {
        if port_is_available(port) {
            return port;
        }
    }
    *PREFERRED_PORTS.first().unwrap_or(&8081)
}

async fn probe_health(client: &reqwest::Client, base_url: &str) -> OminixHealthProbe {
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    match client
        .get(&url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let detail = resp.json::<serde_json::Value>().await.ok();
            OminixHealthProbe {
                healthy: status.is_success(),
                http_status: Some(status.as_u16()),
                error: None,
                detail,
            }
        }
        Err(err) => OminixHealthProbe {
            healthy: false,
            http_status: None,
            error: Some(err.to_string()),
            detail: None,
        },
    }
}

async fn probe_voice_models(
    client: &reqwest::Client,
    base_url: &str,
    health_ready: bool,
) -> Vec<OminixVoiceModelStatus> {
    if !health_ready {
        return DEFAULT_VOICE_MODELS
            .iter()
            .map(|(id, role)| OminixVoiceModelStatus {
                id: (*id).to_string(),
                role: (*role).to_string(),
                status: "unchecked".to_string(),
                ready: false,
                name: None,
                size: None,
            })
            .collect();
    }

    let url = format!("{}/v1/models/catalog", base_url.trim_end_matches('/'));
    let catalog = match client
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp.json::<serde_json::Value>().await.ok(),
        _ => None,
    };

    DEFAULT_VOICE_MODELS
        .iter()
        .map(|(id, role)| voice_model_status_from_catalog(id, role, catalog.as_ref()))
        .collect()
}

fn voice_model_status_from_catalog(
    id: &str,
    role: &str,
    catalog: Option<&serde_json::Value>,
) -> OminixVoiceModelStatus {
    let model = catalog.and_then(catalog_models_array).and_then(|models| {
        models
            .iter()
            .find(|model| model.get("id").and_then(|v| v.as_str()) == Some(id))
    });
    let status = model
        .and_then(|model| model.get("status").and_then(|v| v.as_str()))
        .unwrap_or("unknown")
        .to_string();
    let ready = is_ready_model_status(&status);
    OminixVoiceModelStatus {
        id: id.to_string(),
        role: role.to_string(),
        status,
        ready,
        name: model
            .and_then(|model| model.get("name").and_then(|v| v.as_str()))
            .map(|v| v.to_string()),
        size: model
            .and_then(|model| {
                model
                    .get("storage")
                    .and_then(|storage| storage.get("total_size_display"))
                    .and_then(|v| v.as_str())
            })
            .map(|v| v.to_string()),
    }
}

pub(crate) fn is_ready_model_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "ready" | "downloaded" | "installed"
    )
}

fn catalog_models_array(value: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    value
        .as_array()
        .or_else(|| value.get("models").and_then(|models| models.as_array()))
}

async fn service_status(config: &OminixRuntimeConfig) -> OminixServiceProbe {
    if config.skip_launchctl || !cfg!(target_os = "macos") {
        return OminixServiceProbe {
            registered: false,
            running: false,
            detail: Some("launchctl skipped".to_string()),
        };
    }
    let Ok(target) = launchctl_target().await else {
        return OminixServiceProbe {
            registered: false,
            running: false,
            detail: Some("could not resolve launchctl target".to_string()),
        };
    };
    let output = Command::new("launchctl")
        .args(["print", &format!("{target}/{OMINIX_LABEL}")])
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let running = stdout.contains("state = running") || stdout.contains("\npid = ");
            OminixServiceProbe {
                registered: true,
                running,
                detail: Some(first_nonempty_line(&stdout).unwrap_or_else(|| "registered".into())),
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            OminixServiceProbe {
                registered: false,
                running: false,
                detail: if stderr.is_empty() {
                    Some("not registered".to_string())
                } else {
                    Some(stderr)
                },
            }
        }
        Err(err) => OminixServiceProbe {
            registered: false,
            running: false,
            detail: Some(err.to_string()),
        },
    }
}

async fn bootstrap(config: &OminixRuntimeConfig) -> Result<(), String> {
    let target = launchctl_target().await?;
    let plist = config.plist_path();
    let output = Command::new("launchctl")
        .args(["bootstrap", &target, &plist.display().to_string()])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("already bootstrapped") || stderr.contains("service already loaded") {
            Ok(())
        } else {
            Err(format!("launchctl bootstrap failed: {stderr}"))
        }
    }
}

async fn bootout(config: &OminixRuntimeConfig) {
    if let Ok(target) = launchctl_target().await {
        let _ = Command::new("launchctl")
            .args([
                "bootout",
                &target,
                &config.plist_path().display().to_string(),
            ])
            .output()
            .await;
    }
}

async fn kickstart() -> Result<(), String> {
    let target = launchctl_target().await?;
    let service = format!("{target}/{OMINIX_LABEL}");
    let output = Command::new("launchctl")
        .args(["kickstart", "-k", &service])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("launchctl kickstart failed: {stderr}"))
    }
}

async fn launchctl_target() -> Result<String, String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err("id -u failed".to_string());
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() {
        Err("id -u returned empty uid".to_string())
    } else {
        Ok(format!("gui/{uid}"))
    }
}

async fn wait_for_health(client: &reqwest::Client, url: &str, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if probe_health(client, url).await.healthy {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn render_plist(config: &OminixRuntimeConfig, port: u16) -> String {
    let binary = xml_escape(&config.binary_path.display().to_string());
    let workdir = xml_escape(
        &config
            .binary_path
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .display()
            .to_string(),
    );
    let home = xml_escape(&config.home_dir.display().to_string());
    let models_dir = xml_escape(&config.models_dir.display().to_string());
    let asr_model = xml_escape(
        &config
            .models_dir
            .join("qwen3-asr-1.7b")
            .display()
            .to_string(),
    );
    let out = xml_escape(&config.log_path().display().to_string());
    let err = xml_escape(&config.err_log_path().display().to_string());

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{OMINIX_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>--port</string>
    <string>{port}</string>
    <string>--asr-model</string>
    <string>{asr_model}</string>
    <string>--models-dir</string>
    <string>{models_dir}</string>
  </array>
  <key>WorkingDirectory</key>
  <string>{workdir}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>{home}</string>
    <key>OMINIX_MODELS_DIR</key>
    <string>{models_dir}</string>
    <key>PATH</key>
    <string>{workdir}:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>RUST_LOG</key>
    <string>ominix_api=info,qwen3_tts_mlx=info</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>StandardOutPath</key>
  <string>{out}</string>
  <key>StandardErrorPath</key>
  <string>{err}</string>
</dict>
</plist>
"#
    )
}

fn write_if_changed(path: &Path, content: &str) -> std::io::Result<bool> {
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == content {
            return Ok(false);
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(true)
}

fn write_if_missing(path: &Path, content: &str) -> std::io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(true)
}

fn write_executable_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("ominix-api");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_path = parent.join(format!(".{file_name}.tmp-{}-{nonce}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755))?;
    }
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

fn port_is_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn parse_plist_port(path: &Path) -> Option<u16> {
    let content = fs::read_to_string(path).ok()?;
    let values = plist_string_values(&content);
    values
        .windows(2)
        .find(|pair| pair[0] == "--port")
        .and_then(|pair| pair[1].parse().ok())
}

fn plist_string_values(content: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("<string>") {
        rest = &rest[start + "<string>".len()..];
        let Some(end) = rest.find("</string>") else {
            break;
        };
        values.push(xml_unescape(&rest[..end]));
        rest = &rest[end + "</string>".len()..];
    }
    values
}

fn read_discovery_url(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| normalize_url(&s))
}

fn normalize_url(input: &str) -> Option<String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn port_from_url(url: &str) -> Option<u16> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    let port = host_port.rsplit_once(':')?.1;
    port.parse().ok()
}

fn find_in_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn issue(code: &str, severity: &str, message: String, fixable: bool) -> OminixRuntimeIssue {
    OminixRuntimeIssue {
        code: code.to_string(),
        severity: severity.to_string(),
        message,
        fixable,
    }
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}

fn first_nonempty_line(input: &str) -> Option<String> {
    input
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::net::TcpListener as StdTcpListener;
    use std::thread;
    use tar::{Builder, Header};

    fn test_config(root: &Path) -> OminixRuntimeConfig {
        let bin_dir = root.join(".local/bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let binary_path = bin_dir.join("ominix-api");
        let metallib_path = bin_dir.join("mlx.metallib");
        fs::write(&binary_path, b"fake").unwrap();
        fs::write(&metallib_path, b"fake").unwrap();
        let models_dir = root.join(".OminiX/models");
        fs::create_dir_all(models_dir.join("qwen3-asr-1.7b")).unwrap();
        OminixRuntimeConfig {
            home_dir: root.to_path_buf(),
            binary_path,
            metallib_path,
            models_dir,
            skip_launchctl: true,
            require_metallib: false,
            skip_platform_check: true,
            release_repo: DEFAULT_RELEASE_REPO.to_string(),
            release_version: None,
            release_base_url: None,
        }
    }

    #[test]
    fn parses_port_from_launch_agent_plist() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let plist = render_plist(&config, 8087);
        fs::create_dir_all(config.launch_agents_dir()).unwrap();
        fs::write(config.plist_path(), plist).unwrap();
        assert_eq!(parse_plist_port(&config.plist_path()), Some(8087));
    }

    #[tokio::test]
    async fn repair_writes_discovery_and_plist_in_isolated_home() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let client = reqwest::Client::new();
        let response = repair_runtime_with_config(&config, &client)
            .await
            .expect("repair");
        assert!(response.ok);
        assert!(response.dry_run);
        assert!(config.plist_path().exists());
        let discovery = fs::read_to_string(config.discovery_path()).unwrap();
        assert!(discovery.starts_with("http://127.0.0.1:"));
    }

    #[tokio::test]
    async fn status_reports_missing_binary_as_not_repairable() {
        let dir = tempfile::tempdir().unwrap();
        let config = OminixRuntimeConfig {
            home_dir: dir.path().to_path_buf(),
            binary_path: dir.path().join(".local/bin/ominix-api"),
            metallib_path: dir.path().join(".local/bin/mlx.metallib"),
            models_dir: dir.path().join(".OminiX/models"),
            skip_launchctl: true,
            require_metallib: false,
            skip_platform_check: true,
            release_repo: DEFAULT_RELEASE_REPO.to_string(),
            release_version: None,
            release_base_url: None,
        };
        let client = reqwest::Client::new();
        let status = runtime_status_with_config(&config, &client).await;
        assert!(!status.can_repair);
        assert!(
            status
                .issues
                .iter()
                .any(|issue| issue.code == "missing_binary")
        );
    }

    #[tokio::test]
    async fn repair_avoids_occupied_non_ominix_port() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        fs::create_dir_all(config.ominix_dir()).unwrap();
        fs::write(config.discovery_path(), "http://127.0.0.1:8080\n").unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 8080)).ok();
        if listener.is_none() {
            return;
        }
        let client = reqwest::Client::new();
        let response = repair_runtime_with_config(&config, &client)
            .await
            .expect("repair");
        assert!(response.ok);
        assert_ne!(response.status.port, Some(8080));
    }

    #[tokio::test]
    async fn install_downloads_archive_and_writes_runtime_files() {
        let dir = tempfile::tempdir().unwrap();
        let archive = fixture_archive(b"#!/bin/sh\necho ominix\n");
        let release_base_url = spawn_release_fixture(archive);
        let config = OminixRuntimeConfig {
            home_dir: dir.path().to_path_buf(),
            binary_path: dir.path().join(".local/bin/ominix-api"),
            metallib_path: dir.path().join(".local/bin/mlx.metallib"),
            models_dir: dir.path().join(".OminiX/models"),
            skip_launchctl: true,
            require_metallib: false,
            skip_platform_check: true,
            release_repo: DEFAULT_RELEASE_REPO.to_string(),
            release_version: None,
            release_base_url: Some(release_base_url),
        };
        let client = reqwest::Client::new();
        let response = install_runtime_with_config(&config, &client)
            .await
            .expect("install");

        assert!(response.ok);
        assert!(response.dry_run);
        assert!(config.binary_path.exists());
        assert!(config.plist_path().exists());
        assert!(config.discovery_path().exists());
        assert!(
            config
                .alternate_ominix_dir()
                .join("server_config.json")
                .exists()
        );
        let installed = fs::read(&config.binary_path).unwrap();
        assert_eq!(installed, b"#!/bin/sh\necho ominix\n");
        assert!(
            response
                .actions
                .iter()
                .any(|action| action.contains("verified sha256"))
        );
    }

    fn fixture_archive(binary: &[u8]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        let mut header = Header::new_gnu();
        header.set_path("ominix-api").unwrap();
        header.set_size(binary.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, binary).unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    fn spawn_release_fixture(archive: Vec<u8>) -> String {
        let listener = StdTcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let sha = format!("{:x}", Sha256::digest(&archive));
        let checksums = format!("{sha}  ominix-api-9.9.9-darwin-aarch64.tar.gz\n");
        thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut request = [0_u8; 1024];
                let n = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..n]);
                let (content_type, body): (&str, Vec<u8>) = if request.contains("/checksums.txt") {
                    ("text/plain", checksums.as_bytes().to_vec())
                } else {
                    ("application/gzip", archive.clone())
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        format!("http://{addr}")
    }
}
