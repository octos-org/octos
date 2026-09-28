//! Self-update module: download, verify, backup, replace, rollback.
//!
//! Fetches release tarballs from GitHub Releases for `octos-org/octos`,
//! verifies each download against the release's `.sha256` sidecar and the
//! API-reported asset size — corruption protection only: the sidecar ships
//! from the same release, so a compromised release channel is out of scope
//! (nothing signs these artifacts yet). Backs up existing binaries, replaces
//! only the whitelisted bundle entries, and ad-hoc-signs the entries that
//! shipped without a valid code signature (macOS) so they stay executable.

use std::path::{Path, PathBuf};

use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

const GITHUB_REPO: &str = "octos-org/octos";
const ASSET_NAME: &str = "octos-bundle-aarch64-apple-darwin.tar.gz";

/// The top-level files a release bundle is allowed to install, mirroring
/// `scripts/bundle-release.sh` (the single source of truth for what ships).
/// Anything else found in the archive is refused: an update must never plant
/// unlisted executables next to the octos binary. Pinned by
/// `bundle_whitelist_matches_bundle_release_script` below.
const BUNDLE_ENTRIES: &[&str] = &[
    "octos",
    "octos-sandbox",
    "news_fetch",
    "deep-search",
    "deep_crawl",
    "send_email",
    "account_manager",
    "voice",
    "clock",
    "weather",
    "smart_home",
    "model_catalog.json",
];

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
    /// Overridden by tests (`with_skills_root`); always `None` in production
    /// builds, where the skill root derives from the user's home directory.
    skills_root: Option<PathBuf>,
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
            skills_root: None,
            http,
            github_token,
        })
    }

    /// Override the install directory. Tests only — production installs always
    /// target the running executable's own directory.
    #[cfg(test)]
    fn with_bin_dir(mut self, bin_dir: PathBuf) -> Self {
        self.bin_dir = bin_dir;
        self
    }

    /// Override the skill root `clean_skills` wipes. Tests only — the success
    /// path runs `clean_skills`, and tests must never touch a real home.
    #[cfg(test)]
    fn with_skills_root(mut self, skills_root: PathBuf) -> Self {
        self.skills_root = Some(skills_root);
        self
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
        let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
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
        let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/tags/{tag}");
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
        let tmp_dir = std::env::temp_dir().join(format!("octos-update-{}", release.tag));

        // Clean up any previous attempt
        if tmp_dir.exists() {
            std::fs::remove_dir_all(&tmp_dir)?;
        }
        std::fs::create_dir_all(&tmp_dir)?;

        let tarball_path = tmp_dir.join(ASSET_NAME);

        // 1. Stream-download the tarball
        tracing::info!(url = %release.asset_url, "downloading release tarball");
        let download = self.download_file(&release.asset_url, &tarball_path).await;
        let download = match download {
            Ok(status) if status.is_success() => Ok(()),
            Ok(status) => Err(eyre::eyre!("download HTTP error: {status}")),
            Err(e) => Err(e),
        };
        if let Err(e) = download {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(e.wrap_err("failed to download release tarball"));
        }

        // 2. Verify the download before anything is installed
        if let Err(e) = self.verify_download(release, &tarball_path).await {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(e.wrap_err("update refused before install"));
        }

        // 3. Extract tarball
        tracing::info!(path = %tarball_path.display(), "extracting tarball");
        let extract_dir = tmp_dir.join("extracted");
        let extracted = (|| -> Result<()> {
            std::fs::create_dir_all(&extract_dir)?;
            Self::extract_tarball(&tarball_path, &extract_dir)
        })();
        if let Err(e) = extracted {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(e.wrap_err("failed to extract release tarball"));
        }

        // 4. Replace binaries with backup + rollback support
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

        // 5. Clean skill dirs (bootstrap recreates them on next start)
        self.clean_skills();

        // 6. Clean up .bak files and tmp dir
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

    /// Stream-download a URL to a file path, returning the final HTTP status
    /// so callers can distinguish a clean miss from other failures.
    async fn download_file(&self, url: &str, dest: &Path) -> Result<reqwest::StatusCode> {
        let mut req = self
            .http
            .get(url)
            .header("Accept", "application/octet-stream");
        if let Some(token) = &self.github_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;

        let status = resp.status();
        if !status.is_success() {
            return Ok(status);
        }

        let mut file = tokio::fs::File::create(dest).await?;
        let mut stream = resp.bytes_stream();

        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.wrap_err("stream error")?;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;

        Ok(status)
    }

    /// Verify a downloaded tarball before anything is installed.
    ///
    /// The API-reported asset size must match exactly (when known), and the
    /// tarball must match the release's `<asset>.sha256` sidecar — the same
    /// sha256sum-format sidecar `scripts/install.sh` verifies. A checksum
    /// mismatch, an unreachable sidecar, or a sidecar that does not cover
    /// this asset all refuse; only a clean 404 (releases older than rc.12
    /// published no sidecars) skips verification with a warning. The sidecar
    /// ships over the same channel as the tarball, so this protects against
    /// a corrupted download, not a compromised release channel.
    async fn verify_download(&self, release: &ReleaseInfo, tarball: &Path) -> Result<()> {
        let downloaded = tokio::fs::metadata(tarball)
            .await
            .wrap_err("downloaded tarball is missing")?
            .len();
        if release.asset_size > 0 && downloaded != release.asset_size {
            eyre::bail!(
                "downloaded {} bytes but the release asset reports {}",
                downloaded,
                release.asset_size
            );
        }

        let sidecar_path = tarball.with_file_name(format!(
            "{}.sha256",
            tarball.file_name().unwrap_or_default().to_string_lossy()
        ));
        let sidecar_url = format!("{}.sha256", release.asset_url);
        let status = self
            .download_file(&sidecar_url, &sidecar_path)
            .await
            .wrap_err("checksum sidecar request failed")?;
        if status == reqwest::StatusCode::NOT_FOUND {
            tracing::warn!(
                "no {}.sha256 sidecar in this release — skipping checksum verification",
                ASSET_NAME
            );
            return Ok(());
        }
        if !status.is_success() {
            eyre::bail!("checksum sidecar request returned HTTP {status}");
        }

        // An unreadable (e.g. non-UTF-8) sidecar parses as not covering the
        // asset below.
        let sidecar = std::fs::read_to_string(&sidecar_path).unwrap_or_default();
        let Some(expected) = sidecar_hash_for(&sidecar, ASSET_NAME) else {
            eyre::bail!("checksum sidecar does not cover {ASSET_NAME} — refusing to install");
        };

        let actual = sha256_hex(tarball)?;
        if !actual.eq_ignore_ascii_case(&expected) {
            eyre::bail!(
                "checksum MISMATCH for {ASSET_NAME} — the download does not match the \
                 published checksum. Refusing to install."
            );
        }
        tracing::info!("checksum verified for {ASSET_NAME}");
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

            // Only install files the bundle is known to ship
            // (scripts/bundle-release.sh). A planted extra executable in the
            // archive must not land next to the octos binary.
            if !BUNDLE_ENTRIES.contains(&name.as_str()) {
                tracing::warn!(
                    entry = %name,
                    "refusing to install non-bundle entry from release archive"
                );
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

            // Sign on macOS, but never overwrite a valid incoming signature:
            // the artifact's own signature (Developer ID or ad-hoc) is its
            // Gatekeeper provenance, and a `--force -s -` re-sign would
            // replace it with a fresh ad-hoc one (#2559). Only the installed
            // copy that arrived without a valid signature gets the ad-hoc
            // sign that lets arm64 macOS execute it.
            #[cfg(target_os = "macos")]
            {
                if has_valid_code_signature(&target) {
                    tracing::debug!(binary = %name, "incoming code signature kept");
                } else {
                    // Nothing valid to preserve here (unsigned or corrupt
                    // signature): `--force` re-signs it either way, keeping
                    // today's recovery path for a corrupt one.
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
            }

            updated.push(name);
        }

        if updated.is_empty() {
            // The signature of whitelist/bundle drift (or a non-bundle
            // archive) — reporting success here would mask a no-op install.
            eyre::bail!("extracted archive contains none of the whitelisted bundle entries");
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
        let octos_dir = match &self.skills_root {
            Some(root) => Some(root.clone()),
            None => dirs::home_dir().map(|h| h.join(".octos").join("skills")),
        };

        if let Some(skills_dir) = octos_dir {
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
                    "smart-home",
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
        match (
            option_env!("OCTOS_GIT_HASH"),
            option_env!("OCTOS_BUILD_DATE"),
        ) {
            (Some(hash), Some(date)) => format!("{version} ({hash} {date})"),
            _ => version.to_string(),
        }
    }
}

/// Extract the expected sha256 for `asset_name` from a sha256sum-format
/// sidecar (`"<64-hex><blank><name>"` lines). Returns `None` when no line
/// names the asset — a present-but-non-covering sidecar, which the caller
/// treats as an anomaly and refuses (only a missing sidecar — a clean 404 —
/// skips verification with a warning).
fn sidecar_hash_for(sidecar: &str, asset_name: &str) -> Option<String> {
    // `lines()` already strips a trailing `\r`, so CRLF sidecars parse like
    // the LF originals (install.sh normalizes them for the same reason).
    sidecar.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim().trim_start_matches('*').trim();
        let valid_hash = hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit());
        (valid_hash && name == asset_name).then(|| hash.to_ascii_lowercase())
    })
}

/// True when `path` carries a valid code signature (`codesign --verify`).
/// Ad-hoc signatures count: the goal is to keep whatever signature the
/// release artifact shipped with, since re-signing with `--force -s -`
/// replaces a Developer ID / notarized signature with an ad-hoc one and
/// destroys its Gatekeeper provenance. Unsigned data files (the model
/// catalog) fail this too, and take the re-sign path — `codesign` signs
/// them as generic format entries, same as it always has.
#[cfg(target_os = "macos")]
fn has_valid_code_signature(path: &Path) -> bool {
    use std::process::{Command, Stdio};

    Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// SHA-256 of a file, lower-case hex.
fn sha256_hex(path: &Path) -> Result<String> {
    use sha2::Digest;
    let file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut std::io::BufReader::new(file), &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
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

    #[test]
    fn bundle_whitelist_matches_bundle_release_script() {
        // scripts/bundle-release.sh is the single source of truth for what
        // ships in a release; the installer whitelist must stay in lockstep.
        let script = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/bundle-release.sh"
        ));
        let block = script
            .split("BINARIES=(")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .expect("BINARIES=( ... ) block in bundle-release.sh");
        let mut expected: Vec<&str> = block
            .lines()
            .map(|l| l.trim().trim_end_matches('"').trim_start_matches('"'))
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        expected.push("model_catalog.json");
        expected.sort_unstable();

        let mut actual: Vec<&str> = BUNDLE_ENTRIES.to_vec();
        actual.sort_unstable();
        assert_eq!(
            actual, expected,
            "BUNDLE_ENTRIES drifted from bundle-release.sh"
        );
    }

    #[test]
    fn sidecar_hash_reads_standard_line() {
        let sidecar = "abc1234567890abc1234567890abc1234567890abc1234567890abc123456789  octos-bundle-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            sidecar_hash_for(sidecar, ASSET_NAME).as_deref(),
            Some("abc1234567890abc1234567890abc1234567890abc1234567890abc123456789")
        );
    }

    #[test]
    fn sidecar_hash_tolerates_crlf_binary_mode_and_uppercase() {
        let sidecar = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789 *octos-bundle-aarch64-apple-darwin.tar.gz\r\n";
        assert_eq!(
            sidecar_hash_for(sidecar, ASSET_NAME).as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
    }

    #[test]
    fn sidecar_hash_picks_the_requested_asset_from_a_sums_file() {
        let sidecar = "1111111111111111111111111111111111111111111111111111111111111111  other.tar.gz\n\
                       2222222222222222222222222222222222222222222222222222222222222222  octos-bundle-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            sidecar_hash_for(sidecar, ASSET_NAME).as_deref(),
            Some("2".repeat(64).as_str())
        );
    }

    #[test]
    fn sidecar_hash_rejects_malformed_and_unnamed_lines() {
        // Not a hash line / wrong length / names another asset / empty.
        assert_eq!(sidecar_hash_for("octos-bundle.tar.gz\n", ASSET_NAME), None);
        assert_eq!(
            sidecar_hash_for(
                "abc123  octos-bundle-aarch64-apple-darwin.tar.gz\n",
                ASSET_NAME
            ),
            None
        );
        assert_eq!(
            sidecar_hash_for(
                "2222222222222222222222222222222222222222222222222222222222222222  other.tar.gz\n",
                ASSET_NAME
            ),
            None
        );
        assert_eq!(sidecar_hash_for("", ASSET_NAME), None);
    }

    #[test]
    fn replace_binaries_installs_only_whitelisted_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        let extract_dir = tmp.path().join("extracted");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&extract_dir).unwrap();

        let entry = |dir: &Path, name: &str, contents: &str| {
            let p = dir.join(name);
            std::fs::write(p, contents).unwrap();
        };
        entry(&extract_dir, "clock", "new-clock");
        entry(&extract_dir, "evil.sh", "do-not-install");
        // An existing binary gets backed up so rollback can restore it.
        entry(&bin_dir, "clock", "old-clock");

        let updater = Updater::new(None).unwrap().with_bin_dir(bin_dir.clone());
        let mut updated = Vec::new();
        let mut backed_up = Vec::new();
        updater
            .replace_binaries(&extract_dir, &mut updated, &mut backed_up)
            .unwrap();

        assert_eq!(updated, vec!["clock".to_string()]);
        assert_eq!(backed_up, vec!["clock".to_string()]);
        assert_eq!(
            std::fs::read_to_string(bin_dir.join("clock")).unwrap(),
            "new-clock"
        );
        assert_eq!(
            std::fs::read_to_string(bin_dir.join("clock.bak")).unwrap(),
            "old-clock"
        );
        assert!(
            !bin_dir.join("evil.sh").exists(),
            "non-bundle entry must not install"
        );

        updater.rollback(&backed_up);
        assert_eq!(
            std::fs::read_to_string(bin_dir.join("clock")).unwrap(),
            "old-clock"
        );
        assert!(!bin_dir.join("clock.bak").exists());
    }

    /// Serve canned `path -> (status, body)` responses over 127.0.0.1 for the
    /// updater's plain GETs — the same HTTP code path as GitHub Releases
    /// without leaving the process.
    async fn spawn_fixture_server(
        routes: std::collections::HashMap<String, (u16, Vec<u8>)>,
    ) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let mut request = Vec::new();
                    loop {
                        let Ok(n) = sock.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..n]);
                        if request.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let line = String::from_utf8_lossy(&request);
                    let path = line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let (status, body) = routes
                        .get(&path)
                        .cloned()
                        .unwrap_or_else(|| (404, b"not found".to_vec()));
                    let reason = if status == 200 { "OK" } else { "Not Found" };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });
        addr
    }

    fn tar_gz_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(encoder);
        for (name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, name, *contents).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn release_for(url: &str, size: u64, tag: &str) -> ReleaseInfo {
        ReleaseInfo {
            tag: tag.into(),
            version: "0.0.0".into(),
            published_at: String::new(),
            asset_url: url.into(),
            asset_size: size,
        }
    }

    /// A pipeline updater whose install dir and skill root both live in the
    /// sandbox — `update()` wipes skill dirs on success, so the real home
    /// must never be reachable from a test.
    fn sandbox_updater(
        tmp: &tempfile::TempDir,
    ) -> (std::path::PathBuf, std::path::PathBuf, Updater) {
        let bin_dir = tmp.path().join("bin");
        let skills_root = tmp.path().join("skills");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&skills_root).unwrap();
        let updater = Updater::new(None)
            .unwrap()
            .with_bin_dir(bin_dir.clone())
            .with_skills_root(skills_root.clone());
        (bin_dir, skills_root, updater)
    }

    async fn fixture_tarball(tarball: &[u8], sidecar: Option<String>) -> std::net::SocketAddr {
        let mut routes = std::collections::HashMap::new();
        routes.insert("/bundle.tar.gz".to_string(), (200, tarball.to_vec()));
        if let Some(sidecar) = sidecar {
            routes.insert(
                "/bundle.tar.gz.sha256".to_string(),
                (200, sidecar.into_bytes()),
            );
        }
        spawn_fixture_server(routes).await
    }

    fn sidecar_for(tarball: &[u8]) -> String {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(tarball);
        format!("{:x}  {ASSET_NAME}\n", h.finalize())
    }

    #[tokio::test]
    async fn update_installs_verified_bundle_and_skips_unlisted_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin_dir, skills_root, updater) = sandbox_updater(&tmp);
        // A pre-existing skill dir that a successful update must clean.
        std::fs::create_dir_all(skills_root.join("news")).unwrap();

        let tarball = tar_gz_bytes(&[
            ("clock", "new-clock".as_bytes()),
            ("evil.sh", "planted".as_bytes()),
        ]);
        let addr = fixture_tarball(&tarball, Some(sidecar_for(&tarball))).await;

        let result = updater
            .update(&release_for(
                &format!("http://{addr}/bundle.tar.gz"),
                tarball.len() as u64,
                "v0.0.0-whitelist",
            ))
            .await
            .expect("verified bundle installs");

        assert_eq!(result.binaries_updated, vec!["clock".to_string()]);
        assert_eq!(
            std::fs::read_to_string(bin_dir.join("clock")).unwrap(),
            "new-clock"
        );
        assert!(
            !bin_dir.join("evil.sh").exists(),
            "planted entry must not install"
        );
        assert!(
            !skills_root.join("news").exists(),
            "successful update cleans the (sandboxed) skill dirs"
        );
        assert!(
            !std::env::temp_dir()
                .join("octos-update-v0.0.0-whitelist")
                .exists(),
            "tmp dir cleaned after success"
        );
    }

    #[tokio::test]
    async fn update_refuses_checksum_mismatch_before_installing() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin_dir, _skills, updater) = sandbox_updater(&tmp);

        let tarball = tar_gz_bytes(&[("clock", "tampered".as_bytes())]);
        let wrong = format!("{}  {ASSET_NAME}\n", "0".repeat(64));
        let addr = fixture_tarball(&tarball, Some(wrong)).await;

        let err = updater
            .update(&release_for(
                &format!("http://{addr}/bundle.tar.gz"),
                tarball.len() as u64,
                "v0.0.0-mismatch",
            ))
            .await
            .expect_err("mismatched checksum must refuse to install");

        assert!(
            format!("{err:#}").to_lowercase().contains("mismatch"),
            "error should name the checksum failure, got: {err:#}"
        );
        assert!(
            bin_dir.read_dir().unwrap().next().is_none(),
            "nothing may be installed when verification fails"
        );
    }

    #[tokio::test]
    async fn update_refuses_size_mismatch_before_installing() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin_dir, _skills, updater) = sandbox_updater(&tmp);

        let tarball = tar_gz_bytes(&[("clock", "truncated".as_bytes())]);
        let addr = fixture_tarball(&tarball, None).await;

        // The API-reported size no longer matches what the (proxied) download
        // actually delivered.
        let err = updater
            .update(&release_for(
                &format!("http://{addr}/bundle.tar.gz"),
                999_999,
                "v0.0.0-size",
            ))
            .await
            .expect_err("size mismatch must refuse to install");
        assert!(format!("{err:#}").contains("bytes"), "got: {err:#}");
        assert!(bin_dir.read_dir().unwrap().next().is_none());
    }

    #[tokio::test]
    async fn update_proceeds_without_sidecar_for_pre_rc12_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin_dir, _skills, updater) = sandbox_updater(&tmp);

        let tarball = tar_gz_bytes(&[("clock", "legacy".as_bytes())]);
        // No /bundle.tar.gz.sha256 route — the fixture server 404s it, the
        // signature of a pre-rc.12 release.
        let addr = fixture_tarball(&tarball, None).await;

        let result = updater
            .update(&release_for(
                &format!("http://{addr}/bundle.tar.gz"),
                tarball.len() as u64,
                "v0.0.0-nosidecar",
            ))
            .await
            .expect("missing sidecar warns but proceeds, like install.sh");

        assert_eq!(result.binaries_updated, vec!["clock".to_string()]);
        assert_eq!(
            std::fs::read_to_string(bin_dir.join("clock")).unwrap(),
            "legacy"
        );
    }

    #[tokio::test]
    async fn update_refuses_sidecar_that_does_not_cover_the_asset() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin_dir, _skills, updater) = sandbox_updater(&tmp);

        let tarball = tar_gz_bytes(&[("clock", "sneaky".as_bytes())]);
        // The sidecar exists but names some other asset — exactly the case
        // where refusing is cheap and skipping is dangerous.
        let other = format!("{}  some-other-asset.tar.gz\n", "9".repeat(64));
        let addr = fixture_tarball(&tarball, Some(other)).await;

        let err = updater
            .update(&release_for(
                &format!("http://{addr}/bundle.tar.gz"),
                tarball.len() as u64,
                "v0.0.0-wrongname",
            ))
            .await
            .expect_err("sidecar without a line for this asset must refuse");
        assert!(
            format!("{err:#}").contains("does not cover"),
            "got: {err:#}"
        );
        assert!(bin_dir.read_dir().unwrap().next().is_none());
    }

    #[tokio::test]
    async fn update_refuses_when_the_sidecar_cannot_be_fetched() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin_dir, _skills, updater) = sandbox_updater(&tmp);

        let tarball = tar_gz_bytes(&[("clock", "unverified".as_bytes())]);
        let mut routes = std::collections::HashMap::new();
        routes.insert("/bundle.tar.gz".to_string(), (200, tarball.clone()));
        routes.insert("/bundle.tar.gz.sha256".to_string(), (500, b"boom".to_vec()));
        let addr = spawn_fixture_server(routes).await;

        // Anything short of a clean 404 (legacy releases) must refuse: an
        // adversary who can strip the sidecar request must not strip the
        // verification with it.
        let err = updater
            .update(&release_for(
                &format!("http://{addr}/bundle.tar.gz"),
                tarball.len() as u64,
                "v0.0.0-sidecar5xx",
            ))
            .await
            .expect_err("failing sidecar fetch must refuse");
        assert!(format!("{err:#}").contains("sidecar"), "got: {err:#}");
        assert!(bin_dir.read_dir().unwrap().next().is_none());
    }

    #[tokio::test]
    async fn update_refuses_an_archive_with_no_whitelisted_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin_dir, _skills, updater) = sandbox_updater(&tmp);

        // Only non-bundle entries — the signature of whitelist/bundle drift.
        let tarball = tar_gz_bytes(&[
            ("evil.sh", "planted".as_bytes()),
            ("readme.txt", "hi".as_bytes()),
        ]);
        let addr = fixture_tarball(&tarball, Some(sidecar_for(&tarball))).await;

        let err = updater
            .update(&release_for(
                &format!("http://{addr}/bundle.tar.gz"),
                tarball.len() as u64,
                "v0.0.0-empty",
            ))
            .await
            .expect_err("zero whitelisted entries must not report success");
        assert!(
            format!("{err:#}").contains("none of the whitelisted"),
            "got: {err:#}"
        );
        assert!(bin_dir.read_dir().unwrap().next().is_none());
    }

    /// Copy the running test binary and sign it ad-hoc with a custom
    /// identifier and the hardened-runtime flag — the closest local stand-in
    /// for a future signed release artifact (#2559): both are properties a
    /// `--force -s -` re-sign is known to destroy.
    #[cfg(target_os = "macos")]
    fn signed_binary_fixture(dest: &Path) {
        std::fs::copy(std::env::current_exe().unwrap(), dest).unwrap();
        let ok = std::process::Command::new("codesign")
            .args([
                "--force",
                "-s",
                "-",
                "--identifier",
                "octos-updater-test",
                "-o",
                "runtime",
            ])
            .arg(dest)
            .status()
            .unwrap();
        assert!(ok.success(), "codesign fixture setup failed");
    }

    /// `(verifies, hardened-runtime flag kept, Identifier line)` of a path.
    /// The runtime flag is read from the `flags=0x…` bitfield (0x10000 =
    /// CS_RUNTIME) rather than matched as a substring of the flag list.
    #[cfg(target_os = "macos")]
    fn signature_of(path: &Path) -> (bool, bool, String) {
        let verifies = std::process::Command::new("codesign")
            .args(["--verify", "--strict"])
            .arg(path)
            .status()
            .unwrap()
            .success();
        let output = std::process::Command::new("codesign")
            .arg("-dvv")
            .arg(path)
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        let info = String::from_utf8_lossy(&output.stderr);
        let runtime = info
            .split("flags=0x")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_hexdigit()).next())
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .map(|bits| bits & 0x10000 != 0)
            .unwrap_or(false);
        let identifier = info
            .lines()
            .find(|l| l.starts_with("Identifier="))
            .unwrap_or_default()
            .to_string();
        (verifies, runtime, identifier)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn has_valid_code_signature_accepts_signed_and_rejects_unsigned_binaries() {
        let tmp = tempfile::tempdir().unwrap();
        let signed = tmp.path().join("signed");
        signed_binary_fixture(&signed);
        assert!(
            has_valid_code_signature(&signed),
            "a freshly signed binary must verify"
        );

        let unsigned = tmp.path().join("unsigned");
        std::fs::copy(&signed, &unsigned).unwrap();
        let ok = std::process::Command::new("codesign")
            .arg("--remove-signature")
            .arg(&unsigned)
            .status()
            .unwrap();
        assert!(ok.success(), "codesign --remove-signature failed");
        assert!(
            !has_valid_code_signature(&unsigned),
            "a stripped binary must not verify"
        );
    }

    /// The provenance contract: a signed artifact keeps its signature through
    /// an update (identifier and runtime flag intact — both would be replaced
    /// by a `--force -s -` re-sign), while an unsigned one still gets the
    /// ad-hoc signature that lets arm64 macOS execute it.
    #[cfg(target_os = "macos")]
    #[test]
    fn replace_binaries_keeps_incoming_signatures_and_signs_only_unsigned_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        let extract_dir = tmp.path().join("extracted");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&extract_dir).unwrap();

        signed_binary_fixture(&extract_dir.join("clock"));
        std::fs::copy(std::env::current_exe().unwrap(), extract_dir.join("voice")).unwrap();
        let ok = std::process::Command::new("codesign")
            .arg("--remove-signature")
            .arg(extract_dir.join("voice"))
            .status()
            .unwrap();
        assert!(ok.success(), "codesign --remove-signature failed");

        let updater = Updater::new(None).unwrap().with_bin_dir(bin_dir.clone());
        let mut updated = Vec::new();
        let mut backed_up = Vec::new();
        updater
            .replace_binaries(&extract_dir, &mut updated, &mut backed_up)
            .unwrap();
        assert_eq!(updated.len(), 2);

        let (verifies, runtime, identifier) = signature_of(&bin_dir.join("clock"));
        assert!(verifies, "installed binary must still verify");
        assert!(runtime, "hardened-runtime flag must survive the install");
        assert_eq!(
            identifier, "Identifier=octos-updater-test",
            "re-signing would replace the identifier with a file-name-derived hash"
        );

        let (verifies, _, _) = signature_of(&bin_dir.join("voice"));
        assert!(
            verifies,
            "an unsigned artifact must leave the install with the ad-hoc signature"
        );
    }

    /// End-to-end: a full `update()` install — download, sidecar verify,
    /// extract, backup, replace — keeps the artifact's own signature instead
    /// of re-signing over it (#2559).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn update_preserves_incoming_signatures_through_the_full_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin_dir, _skills, updater) = sandbox_updater(&tmp);

        let signed = tmp.path().join("signed-clock");
        signed_binary_fixture(&signed);
        let signed_bytes = std::fs::read(&signed).unwrap();

        let tarball = tar_gz_bytes(&[("clock", signed_bytes.as_slice())]);
        let addr = fixture_tarball(&tarball, Some(sidecar_for(&tarball))).await;

        updater
            .update(&release_for(
                &format!("http://{addr}/bundle.tar.gz"),
                tarball.len() as u64,
                "v0.0.0-signed",
            ))
            .await
            .expect("verified signed bundle installs");

        let (verifies, runtime, identifier) = signature_of(&bin_dir.join("clock"));
        assert!(verifies, "installed binary must still verify");
        assert!(runtime, "hardened-runtime flag must survive the update");
        assert_eq!(
            identifier, "Identifier=octos-updater-test",
            "re-signing would replace the identifier with a file-name-derived hash"
        );
    }

    /// The recovery path: a corrupt signature fails verify and is re-signed
    /// into a valid one (the behavior the old unconditional `--force -s -`
    /// provided), and unsigned data files like the model catalog take the
    /// same path — `codesign` signs them as generic format entries.
    #[cfg(target_os = "macos")]
    #[test]
    fn replace_binaries_resigns_corrupt_signatures_and_unsigned_data_files() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        let extract_dir = tmp.path().join("extracted");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&extract_dir).unwrap();

        // A signed Mach-O with one flipped byte inside the first content
        // page (away from the header and the trailing signature blob): the
        // page hash no longer matches, so verify must fail — and the re-sign
        // path must still produce a valid binary.
        signed_binary_fixture(&extract_dir.join("clock"));
        let mut corrupted = std::fs::read(extract_dir.join("clock")).unwrap();
        corrupted[4096] ^= 0xff;
        std::fs::write(extract_dir.join("clock"), &corrupted).unwrap();
        assert!(
            !has_valid_code_signature(&extract_dir.join("clock")),
            "fixture must fail verify before the re-sign path runs"
        );

        std::fs::write(extract_dir.join("model_catalog.json"), "{}").unwrap();

        let updater = Updater::new(None).unwrap().with_bin_dir(bin_dir.clone());
        let mut updated = Vec::new();
        let mut backed_up = Vec::new();
        updater
            .replace_binaries(&extract_dir, &mut updated, &mut backed_up)
            .unwrap();
        assert_eq!(updated.len(), 2);

        let (verifies, _, _) = signature_of(&bin_dir.join("clock"));
        assert!(
            verifies,
            "a corrupt signature must leave the install re-signed and valid"
        );
        let (verifies, _, _) = signature_of(&bin_dir.join("model_catalog.json"));
        assert!(
            verifies,
            "an unsigned data file must be signed as a generic entry"
        );
    }

    #[tokio::test]
    async fn update_cleans_the_tmp_dir_when_the_download_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin_dir, _skills, updater) = sandbox_updater(&tmp);

        let mut routes = std::collections::HashMap::new();
        routes.insert("/bundle.tar.gz".to_string(), (500, b"boom".to_vec()));
        let addr = spawn_fixture_server(routes).await;

        let err = updater
            .update(&release_for(
                &format!("http://{addr}/bundle.tar.gz"),
                0,
                "v0.0.0-dlfail",
            ))
            .await
            .expect_err("a failed download must refuse to install");
        assert!(
            format!("{err:#}").contains("500"),
            "error should name the HTTP failure, got: {err:#}"
        );
        assert!(bin_dir.read_dir().unwrap().next().is_none());
        assert!(
            !std::env::temp_dir()
                .join("octos-update-v0.0.0-dlfail")
                .exists(),
            "tmp dir cleaned after download failure"
        );
    }
}
