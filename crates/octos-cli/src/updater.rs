//! Self-update module: download, verify, backup, replace, rollback.
//!
//! Fetches release tarballs from GitHub Releases for `octos-org/octos`,
//! backs up existing binaries, replaces them, and runs `codesign` on macOS.

use std::path::{Path, PathBuf};

use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

const GITHUB_REPO: &str = "octos-org/octos";
const ASSET_NAME: &str = "octos-bundle-aarch64-apple-darwin.tar.gz";

/// Information about a GitHub release.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: String,
    pub published_at: String,
    pub asset_url: String,
    pub asset_size: u64,
}

/// Result of a successful update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateResult {
    pub old_version: String,
    pub new_version: String,
    pub binaries_updated: Vec<String>,
}

pub struct Updater {
    bin_dir: PathBuf,
    http: reqwest::Client,
    github_token: Option<String>,
}

impl Updater {
    /// Create an updater. If `github_token` is provided it's used for auth;
    /// otherwise falls back to `GITHUB_TOKEN` env var.
    pub fn new(github_token: Option<String>) -> Result<Self> {
        let exe = std::env::current_exe().wrap_err("cannot locate current executable")?;
        let bin_dir = exe
            .parent()
            .ok_or_else(|| eyre::eyre!("exe has no parent dir"))?
            .to_path_buf();

        let github_token = github_token.or_else(|| std::env::var("GITHUB_TOKEN").ok());

        let http = reqwest::Client::builder()
            .user_agent("octos-updater/1.0")
            .build()
            .wrap_err("failed to build HTTP client")?;

        Ok(Self {
            bin_dir,
            http,
            github_token,
        })
    }

    /// Build a GET request with optional GitHub token auth.
    fn github_get(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .get(url)
            .header("Accept", "application/vnd.github+json");
        if let Some(token) = &self.github_token {
            req = req.bearer_auth(token);
        }
        req
    }

    /// Check the latest release on GitHub.
    pub async fn check_latest(&self) -> Result<ReleaseInfo> {
        let url = format!(
            "https://api.github.com/repos/{}/releases/latest",
            GITHUB_REPO
        );
        let resp: serde_json::Value = self
            .github_get(&url)
            .send()
            .await
            .wrap_err("failed to fetch latest release")?
            .error_for_status()
            .wrap_err("GitHub API error")?
            .json()
            .await?;

        Self::parse_release(&resp)
    }

    /// Fetch a specific release by tag (e.g. "v0.2.0").
    pub async fn check_version(&self, tag: &str) -> Result<ReleaseInfo> {
        let url = format!(
            "https://api.github.com/repos/{}/releases/tags/{}",
            GITHUB_REPO, tag
        );
        let resp: serde_json::Value = self
            .github_get(&url)
            .send()
            .await
            .wrap_err("failed to fetch release")?
            .error_for_status()
            .wrap_err("GitHub API error (tag not found?)")?
            .json()
            .await?;

        Self::parse_release(&resp)
    }

    fn parse_release(resp: &serde_json::Value) -> Result<ReleaseInfo> {
        let tag = resp["tag_name"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("missing tag_name"))?
            .to_string();
        let version = tag.strip_prefix('v').unwrap_or(&tag).to_string();
        let published_at = resp["published_at"].as_str().unwrap_or("").to_string();

        let assets = resp["assets"]
            .as_array()
            .ok_or_else(|| eyre::eyre!("missing assets array"))?;

        let asset = assets
            .iter()
            .find(|a| a["name"].as_str() == Some(ASSET_NAME))
            .ok_or_else(|| eyre::eyre!("release {} has no asset named {}", tag, ASSET_NAME))?;

        let asset_url = asset["browser_download_url"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("missing download URL"))?
            .to_string();
        let asset_size = asset["size"].as_u64().unwrap_or(0);

        Ok(ReleaseInfo {
            tag,
            version,
            published_at,
            asset_url,
            asset_size,
        })
    }

    /// Download and install a release. Returns the update result on success.
    pub async fn update(&self, release: &ReleaseInfo) -> Result<UpdateResult> {
        let old_version = env!("CARGO_PKG_VERSION").to_string();
        let tmp_dir = std::env::temp_dir().join(format!("octos-update-{}", &release.tag));

        // Clean up any previous attempt
        if tmp_dir.exists() {
            std::fs::remove_dir_all(&tmp_dir)?;
        }
        std::fs::create_dir_all(&tmp_dir)?;

        let tarball_path = tmp_dir.join(ASSET_NAME);

        // 1. Stream-download the tarball
        tracing::info!(url = %release.asset_url, "downloading release tarball");
        self.download_file(&release.asset_url, &tarball_path)
            .await
            .wrap_err("failed to download release tarball")?;

        // 2. Extract tarball
        tracing::info!(path = %tarball_path.display(), "extracting tarball");
        let extract_dir = tmp_dir.join("extracted");
        std::fs::create_dir_all(&extract_dir)?;
        Self::extract_tarball(&tarball_path, &extract_dir)?;

        // 3. Replace binaries with backup + rollback support
        let mut updated = Vec::new();
        let mut backed_up = Vec::new();

        let result = self.replace_binaries(&extract_dir, &mut updated, &mut backed_up);
        if let Err(e) = result {
            // Rollback: restore all backed up files
            tracing::error!(error = %e, "update failed, rolling back");
            self.rollback(&backed_up);
            // Clean up tmp
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(e.wrap_err("update failed, rolled back"));
        }

        // 4. Clean skill dirs (bootstrap recreates them on next start)
        self.clean_skills();

        // 5. Clean up .bak files and tmp dir
        for name in &backed_up {
            let bak = self.bin_dir.join(format!("{name}.bak"));
            let _ = std::fs::remove_file(bak);
        }
        let _ = std::fs::remove_dir_all(&tmp_dir);

        Ok(UpdateResult {
            old_version,
            new_version: release.version.clone(),
            binaries_updated: updated,
        })
    }

    /// Stream-download a URL to a file path.
    async fn download_file(&self, url: &str, dest: &Path) -> Result<()> {
        let mut req = self
            .http
            .get(url)
            .header("Accept", "application/octet-stream");
        if let Some(token) = &self.github_token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await?
            .error_for_status()
            .wrap_err("download HTTP error")?;

        let mut file = tokio::fs::File::create(dest).await?;
        let mut stream = resp.bytes_stream();

        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.wrap_err("stream error")?;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;

        Ok(())
    }

    /// Extract a .tar.gz to a directory.
    fn extract_tarball(tarball: &Path, dest: &Path) -> Result<()> {
        let file = std::fs::File::open(tarball)?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(dest)?;
        Ok(())
    }

    /// Replace binaries in bin_dir with files from extract_dir.
    /// Tracks updated and backed-up names for rollback.
    fn replace_binaries(
        &self,
        extract_dir: &Path,
        updated: &mut Vec<String>,
        backed_up: &mut Vec<String>,
    ) -> Result<()> {
        let entries =
            std::fs::read_dir(extract_dir).wrap_err("failed to read extracted directory")?;

        for entry in entries {
            let entry = entry?;
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy().to_string();

            // Skip non-files
            if !entry.file_type()?.is_file() {
                continue;
            }

            let target = self.bin_dir.join(&name);
            let backup = self.bin_dir.join(format!("{name}.bak"));

            // Backup existing binary if it exists
            if target.exists() {
                std::fs::rename(&target, &backup)
                    .wrap_err_with(|| format!("failed to backup {name}"))?;
                backed_up.push(name.clone());
            }

            // Copy new binary
            std::fs::copy(entry.path(), &target)
                .wrap_err_with(|| format!("failed to copy {name}"))?;

            // Make executable
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
            }

            // Codesign on macOS
            #[cfg(target_os = "macos")]
            {
                let status = std::process::Command::new("codesign")
                    .args(["--force", "-s", "-"])
                    .arg(&target)
                    .status();
                match status {
                    Ok(s) if s.success() => {
                        tracing::debug!(binary = %name, "codesigned");
                    }
                    Ok(s) => {
                        tracing::warn!(binary = %name, code = ?s.code(), "codesign failed");
                    }
                    Err(e) => {
                        tracing::warn!(binary = %name, error = %e, "codesign command failed");
                    }
                }
            }

            updated.push(name);
        }

        Ok(())
    }

    /// Rollback: restore .bak files.
    fn rollback(&self, backed_up: &[String]) {
        for name in backed_up {
            let target = self.bin_dir.join(name);
            let backup = self.bin_dir.join(format!("{name}.bak"));
            if backup.exists() {
                if let Err(e) = std::fs::rename(&backup, &target) {
                    tracing::error!(binary = %name, error = %e, "rollback failed");
                }
            }
        }
    }

    /// Clean skill dirs so bootstrap recreates them on next start.
    fn clean_skills(&self) {
        let crew_dir = dirs::home_dir().map(|h| h.join(".octos").join("skills"));

        if let Some(skills_dir) = crew_dir {
            if skills_dir.exists() {
                let skills = [
                    "news",
                    "deep-search",
                    "deep-crawl",
                    "send-email",
                    "account-manager",
                    "voice",
                    "clock",
                    "weather",
                ];
                for skill in &skills {
                    let dir = skills_dir.join(skill);
                    if dir.exists() {
                        if let Err(e) = std::fs::remove_dir_all(&dir) {
                            tracing::warn!(skill = %skill, error = %e, "failed to clean skill dir");
                        }
                    }
                }
            }
        }
    }

    /// Get the current version string.
    pub fn current_version() -> String {
        let version = env!("CARGO_PKG_VERSION");
        match (option_env!("OCTOS_GIT_HASH"), option_env!("OCTOS_BUILD_DATE")) {
            (Some(hash), Some(date)) => format!("{version} ({hash} {date})"),
            _ => version.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_release_json() -> serde_json::Value {
        json!({
            "tag_name": "v0.3.1",
            "published_at": "2026-03-01T12:00:00Z",
            "assets": [
                {
                    "name": ASSET_NAME,
                    "browser_download_url": "https://github.com/octos-org/octos/releases/download/v0.3.1/octos-bundle-aarch64-apple-darwin.tar.gz",
                    "size": 12345678
                }
            ]
        })
    }

    #[test]
    fn parse_release_valid() {
        let resp = valid_release_json();
        let info = Updater::parse_release(&resp).expect("should parse valid release");
        assert_eq!(info.tag, "v0.3.1");
        assert_eq!(info.version, "0.3.1");
        assert_eq!(info.published_at, "2026-03-01T12:00:00Z");
        assert!(info.asset_url.contains("octos-bundle-aarch64-apple-darwin"));
        assert_eq!(info.asset_size, 12345678);
    }

    #[test]
    fn parse_release_missing_tag_name() {
        let resp = json!({
            "published_at": "2026-03-01T12:00:00Z",
            "assets": []
        });
        let err = Updater::parse_release(&resp).unwrap_err();
        assert!(err.to_string().contains("missing tag_name"));
    }

    #[test]
    fn parse_release_missing_assets() {
        let resp = json!({
            "tag_name": "v0.3.1",
            "published_at": "2026-03-01T12:00:00Z"
        });
        let err = Updater::parse_release(&resp).unwrap_err();
        assert!(err.to_string().contains("missing assets array"));
    }

    #[test]
    fn parse_release_no_matching_asset() {
        let resp = json!({
            "tag_name": "v0.3.1",
            "published_at": "2026-03-01T12:00:00Z",
            "assets": [
                {
                    "name": "some-other-asset.tar.gz",
                    "browser_download_url": "https://example.com/other.tar.gz",
                    "size": 100
                }
            ]
        });
        let err = Updater::parse_release(&resp).unwrap_err();
        assert!(err.to_string().contains("no asset named"));
    }

    #[test]
    fn parse_release_strips_v_prefix() {
        let resp = valid_release_json();
        let info = Updater::parse_release(&resp).unwrap();
        assert_eq!(info.version, "0.3.1");

        // Without v prefix
        let mut resp_no_v = valid_release_json();
        resp_no_v["tag_name"] = json!("0.4.0");
        let info = Updater::parse_release(&resp_no_v).unwrap();
        assert_eq!(info.tag, "0.4.0");
        assert_eq!(info.version, "0.4.0");
    }

    #[test]
    fn release_info_serde_roundtrip() {
        let info = ReleaseInfo {
            tag: "v1.0.0".into(),
            version: "1.0.0".into(),
            published_at: "2026-01-01T00:00:00Z".into(),
            asset_url: "https://example.com/asset.tar.gz".into(),
            asset_size: 999,
        };
        let serialized = serde_json::to_string(&info).unwrap();
        let deserialized: ReleaseInfo = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.tag, info.tag);
        assert_eq!(deserialized.version, info.version);
        assert_eq!(deserialized.published_at, info.published_at);
        assert_eq!(deserialized.asset_url, info.asset_url);
        assert_eq!(deserialized.asset_size, info.asset_size);
    }

    #[test]
    fn update_result_serde_roundtrip() {
        let result = UpdateResult {
            old_version: "0.2.0".into(),
            new_version: "0.3.0".into(),
            binaries_updated: vec!["octos".into(), "octos-gateway".into()],
        };
        let serialized = serde_json::to_string(&result).unwrap();
        let deserialized: UpdateResult = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.old_version, result.old_version);
        assert_eq!(deserialized.new_version, result.new_version);
        assert_eq!(deserialized.binaries_updated, result.binaries_updated);
    }

    #[test]
    fn current_version_non_empty() {
        let version = Updater::current_version();
        assert!(!version.is_empty());
    }
}
