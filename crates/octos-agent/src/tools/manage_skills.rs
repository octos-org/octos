//! Skill management tool for normal profile gateways.
//!
//! Allows agents to list, install, remove, and search skills directly
//! without going through the admin API.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{Tool, ToolResult};

pub struct ManageSkillsTool {
    skills_dir: PathBuf,
}

impl ManageSkillsTool {
    pub fn new(skills_dir: impl Into<PathBuf>) -> Self {
        Self {
            skills_dir: skills_dir.into(),
        }
    }
}

#[derive(Clone, Deserialize)]
struct Input {
    action: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    branch: Option<String>,
}

#[async_trait]
impl Tool for ManageSkillsTool {
    fn name(&self) -> &str {
        "manage_skills"
    }

    fn description(&self) -> &str {
        "Manage agent skills: list installed, install from GitHub (user/repo or user/repo/skill-name), update installed skills, remove by name, or search the skill registry. Install checks versions — if already installed and up to date, it reports the version. Use update to force-reinstall from the source repo."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "install", "update", "remove", "search"],
                    "description": "Action to perform"
                },
                "repo": {
                    "type": "string",
                    "description": "GitHub path user/repo or user/repo/skill-name (required for install)"
                },
                "name": {
                    "type": "string",
                    "description": "Skill name (required for remove)"
                },
                "query": {
                    "type": "string",
                    "description": "Search query (optional for search)"
                },
                "force": {
                    "type": "boolean",
                    "description": "Overwrite existing skills (for install, default false)"
                },
                "branch": {
                    "type": "string",
                    "description": "Git branch or tag (default: main)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        let skills_dir = self.skills_dir.clone();

        // Run blocking git/IO operations on a blocking thread
        let result = tokio::task::spawn_blocking(move || match input.action.as_str() {
            "list" => do_list(&skills_dir),
            "install" => do_install(&skills_dir, &input),
            "remove" => do_remove(&skills_dir, &input),
            "search" => do_search(&input),
            "update" => {
                // Update = install with force, but first check registry version
                let mut force_input = input.clone();
                force_input.force = true;
                let mut source_repo = String::new();
                if force_input.repo.is_none() {
                    if let Some(ref name) = force_input.name {
                        // Read .source to get the original repo
                        let source_file = skills_dir.join(name).join(".source");
                        if let Ok(src) = std::fs::read_to_string(&source_file) {
                            if let Ok(info) = serde_json::from_str::<serde_json::Value>(&src) {
                                let repo = info.get("repo").and_then(|v| v.as_str()).unwrap_or("");
                                source_repo = repo.to_string();
                                let subdir = info.get("subdir").and_then(|v| v.as_str());
                                force_input.repo = Some(if let Some(sub) = subdir {
                                    format!("{repo}/{sub}")
                                } else {
                                    repo.to_string()
                                });
                            }
                        }
                    }
                }
                if force_input.repo.is_none() {
                    return Ok(ToolResult {
                        output: "repo or name required for update. Use name of an installed skill (reads .source for repo).".into(),
                        success: false,
                        ..Default::default()
                    });
                }

                // Pre-clone version check: compare local vs registry
                if let Some(ref name) = input.name {
                    let local_ver = skill_version(&skills_dir.join(name));
                    if let Some(ref lv) = local_ver {
                        let registry_ver = registry_version_for(
                            if source_repo.is_empty() { force_input.repo.as_deref().unwrap_or("") } else { &source_repo },
                            Some(name),
                        );
                        if let Some(ref rv) = registry_ver {
                            if !version_newer(rv, lv) {
                                return Ok(ToolResult {
                                    output: format!("'{name}' is up to date (v{lv})"),
                                    success: true,
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }

                do_install(&skills_dir, &force_input)
            }
            other => Ok(ToolResult {
                output: format!("Unknown action: {other}. Use list, install, update, remove, or search."),
                success: false,
                ..Default::default()
            }),
        })
        .await
        .map_err(|e| eyre::eyre!("task join error: {e}"))??;

        Ok(result)
    }
}

fn do_list(skills_dir: &std::path::Path) -> Result<ToolResult> {
    // Re-use the public API from octos-cli's skills command
    // But since we can't depend on octos-cli from octos-agent, do it inline
    if !skills_dir.exists() {
        return Ok(ToolResult {
            output: "No skills installed.".into(),
            success: true,
            ..Default::default()
        });
    }

    let mut entries: Vec<_> = std::fs::read_dir(skills_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("SKILL.md").exists())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    if entries.is_empty() {
        return Ok(ToolResult {
            output: "No skills installed.".into(),
            success: true,
            ..Default::default()
        });
    }

    let mut lines = Vec::new();
    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let mut info_parts = vec![name.clone()];

        // Version from SKILL.md frontmatter
        if let Ok(content) = std::fs::read_to_string(entry.path().join("SKILL.md")) {
            if let Some(ver) = extract_fm_value(&content, "version") {
                info_parts.push(format!("v{ver}"));
            }
        }

        // Tool count from manifest.json
        if let Ok(manifest) = std::fs::read_to_string(entry.path().join("manifest.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&manifest) {
                if let Some(count) = v.get("tools").and_then(|t| t.as_array()).map(|a| a.len()) {
                    if count > 0 {
                        info_parts.push(format!("[{count} tool(s)]"));
                    }
                }
            }
        }

        lines.push(info_parts.join("  "));
    }

    Ok(ToolResult {
        output: format!(
            "Installed skills ({}):\n{}",
            entries.len(),
            lines.join("\n")
        ),
        success: true,
        ..Default::default()
    })
}

fn do_install(skills_dir: &std::path::Path, input: &Input) -> Result<ToolResult> {
    let repo = match input.repo.as_deref() {
        Some(r) => r,
        None => {
            return Ok(ToolResult {
                output: "repo is required for install (e.g. user/repo or user/repo/skill-name)"
                    .into(),
                success: false,
                ..Default::default()
            });
        }
    };
    let branch = input.branch.as_deref().unwrap_or("main");

    // Parse repo spec
    let segments: Vec<&str> = repo.trim_matches('/').split('/').collect();
    if segments.len() < 2 {
        return Ok(ToolResult {
            output: format!(
                "Invalid repo path: '{repo}'. Expected user/repo or user/repo/skill-name"
            ),
            success: false,
            ..Default::default()
        });
    }

    let clone_url = format!("https://github.com/{}/{}.git", segments[0], segments[1]);
    let subdir = if segments.len() > 2 {
        Some(segments[2..].join("/"))
    } else {
        None
    };

    // Clone to temp dir
    let tmp = tempfile::tempdir()?;
    let clone_dir = tmp.path().join(segments[1]);

    let status = std::process::Command::new("git")
        .args(["clone", "--depth", "1", "--branch", branch])
        .arg(&clone_url)
        .arg(&clone_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|_| eyre::eyre!("git not found"))?;

    if !status.success() {
        return Ok(ToolResult {
            output: format!("git clone failed for {clone_url} (branch: {branch})"),
            success: false,
            ..Default::default()
        });
    }

    std::fs::create_dir_all(skills_dir)?;

    let mut installed = Vec::new();
    let mut skipped = Vec::new();

    if let Some(ref sub) = subdir {
        // Single skill install
        let src = clone_dir.join(sub);
        if !src.is_dir() {
            return Ok(ToolResult {
                output: format!(
                    "Subdirectory '{sub}' not found in {}/{}",
                    segments[0], segments[1]
                ),
                success: false,
                ..Default::default()
            });
        }
        let name = std::path::Path::new(sub.as_str())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let dest = skills_dir.join(&name);
        if dest.exists() && !input.force {
            // Version check: compare local vs remote
            let local_ver = skill_version(&dest);
            let remote_ver = skill_version(&src);
            if let (Some(lv), Some(rv)) = (&local_ver, &remote_ver) {
                if version_newer(rv, lv) {
                    skipped.push(format!(
                        "{name} (update available: {lv} → {rv}, use force=true to update)"
                    ));
                } else {
                    skipped.push(format!("{name} (up to date: {lv})"));
                }
            } else {
                skipped.push(name);
            }
        } else {
            if dest.exists() {
                std::fs::remove_dir_all(&dest)?;
            }
            copy_dir_recursive(&src, &dest)?;
            installed.push(name);
        }
    } else {
        // Whole repo: single skill or multi-skill
        if clone_dir.join("SKILL.md").exists() {
            let dest = skills_dir.join(segments[1]);
            if dest.exists() && !input.force {
                let local_ver = skill_version(&dest);
                let remote_ver = skill_version(&clone_dir);
                if let (Some(lv), Some(rv)) = (&local_ver, &remote_ver) {
                    if version_newer(rv, lv) {
                        skipped.push(format!(
                            "{} (update available: {lv} → {rv}, use force=true to update)",
                            segments[1]
                        ));
                    } else {
                        skipped.push(format!("{} (up to date: {lv})", segments[1]));
                    }
                } else {
                    skipped.push(segments[1].to_string());
                }
            } else {
                if dest.exists() {
                    std::fs::remove_dir_all(&dest)?;
                }
                copy_dir_recursive(&clone_dir, &dest)?;
                installed.push(segments[1].to_string());
            }
        } else {
            for entry in std::fs::read_dir(&clone_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let dest = skills_dir.join(&name);
                if dest.exists() && !input.force {
                    let local_ver = skill_version(&dest);
                    let remote_ver = skill_version(&entry.path());
                    if let (Some(lv), Some(rv)) = (&local_ver, &remote_ver) {
                        if version_newer(rv, lv) {
                            skipped.push(format!(
                                "{name} (update available: {lv} → {rv}, use force=true to update)"
                            ));
                        } else {
                            skipped.push(format!("{name} (up to date: {lv})"));
                        }
                    } else {
                        skipped.push(name);
                    }
                    continue;
                }
                if dest.exists() {
                    std::fs::remove_dir_all(&dest)?;
                }
                copy_dir_recursive(&entry.path(), &dest)?;
                if entry.path().join("SKILL.md").exists() {
                    installed.push(name);
                }
            }
        }
    }

    let repo_path = format!("{}/{}", segments[0], segments[1]);

    // Post-install: npm install, binary install, source tracking
    for name in &installed {
        let dir = skills_dir.join(name);
        maybe_npm_install(&dir);
        maybe_install_binary(&dir);
        write_source_info(&dir, &repo_path, subdir.as_deref(), branch);
    }

    let mut output = String::new();
    if !installed.is_empty() {
        output.push_str(&format!("Installed: {}\n", installed.join(", ")));
    }
    if !skipped.is_empty() {
        output.push_str(&format!(
            "Skipped (already exists, use force=true): {}\n",
            skipped.join(", ")
        ));
    }
    if installed.is_empty() && skipped.is_empty() {
        output.push_str("No skills found in repository.\n");
    }

    Ok(ToolResult {
        output: output.trim().to_string(),
        success: true,
        ..Default::default()
    })
}

fn do_remove(skills_dir: &std::path::Path, input: &Input) -> Result<ToolResult> {
    let name = match input.name.as_deref() {
        Some(n) => n,
        None => {
            return Ok(ToolResult {
                output: "name is required for remove".into(),
                success: false,
                ..Default::default()
            });
        }
    };

    // Reject path traversal
    if name.contains('/')
        || name.contains('\\')
        || name == ".."
        || name == "."
        || name.contains('\0')
    {
        return Ok(ToolResult {
            output: format!("Invalid skill name: {name}"),
            success: false,
            ..Default::default()
        });
    }

    let dest = skills_dir.join(name);
    if !dest.exists() {
        return Ok(ToolResult {
            output: format!("Skill '{name}' not found"),
            success: false,
            ..Default::default()
        });
    }

    std::fs::remove_dir_all(&dest)?;
    Ok(ToolResult {
        output: format!("Removed skill '{name}'"),
        success: true,
        ..Default::default()
    })
}

fn do_search(input: &Input) -> Result<ToolResult> {
    let url = "https://raw.githubusercontent.com/octos-org/octos-hub/main/registry.json";

    let entries: Vec<serde_json::Value> = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(url)
        .send()
        .map_err(|e| eyre::eyre!("failed to fetch registry: {e}"))?
        .error_for_status()
        .map_err(|e| eyre::eyre!("registry request failed: {e}"))?
        .json()
        .map_err(|e| eyre::eyre!("invalid registry JSON: {e}"))?;

    let query_lower = input.query.as_deref().map(|q| q.to_lowercase());

    let filtered: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| {
            let Some(q) = &query_lower else {
                return true;
            };
            let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let desc = e.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let tags = e
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            name.to_lowercase().contains(q)
                || desc.to_lowercase().contains(q)
                || tags.to_lowercase().contains(q)
        })
        .collect();

    if filtered.is_empty() {
        let msg = if let Some(q) = input.query.as_deref() {
            format!("No packages matching '{q}'")
        } else {
            "Registry is empty.".into()
        };
        return Ok(ToolResult {
            output: msg,
            success: true,
            ..Default::default()
        });
    }

    let mut lines = Vec::new();
    for entry in &filtered {
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let desc = entry
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let repo = entry.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        let skills: Vec<&str> = entry
            .get("skills")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        let mut block = format!("{name}: {desc}");
        if skills.is_empty() {
            block.push_str(&format!(
                "\n  Install: manage_skills(action=\"install\", repo=\"{repo}\")"
            ));
        } else {
            block.push_str("\n  Skills (install individually):");
            for skill in &skills {
                block.push_str(&format!(
                    "\n    - {skill}: manage_skills(action=\"install\", repo=\"{repo}/{skill}\")"
                ));
            }
        }
        lines.push(block);
    }

    Ok(ToolResult {
        output: format!(
            "Available packages ({}):\n{}",
            filtered.len(),
            lines.join("\n\n")
        ),
        success: true,
        ..Default::default()
    })
}

/// Fetch the registry version for a repo (e.g. "mofa-org/mofa-skills") or skill name.
fn registry_version_for(repo: &str, skill_name: Option<&str>) -> Option<String> {
    let url = "https://raw.githubusercontent.com/octos-org/octos-hub/main/registry.json";
    let entries: Vec<serde_json::Value> = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?
        .get(url)
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .ok()?;

    entries.iter().find_map(|e| {
        let e_repo = e.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        let e_name = e.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let e_skills: Vec<&str> = e
            .get("skills")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        let matches = e_repo == repo
            || e_name == repo
            || skill_name.map_or(false, |sn| e_skills.contains(&sn));

        if matches {
            e.get("version").and_then(|v| v.as_str()).map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// Read the version from a skill directory's SKILL.md frontmatter.
fn skill_version(skill_dir: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).ok()?;
    extract_fm_value(&content, "version")
}

/// Simple semver comparison: is `a` newer than `b`?
fn version_newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> { s.split('.').filter_map(|p| p.parse().ok()).collect() };
    let va = parse(a);
    let vb = parse(b);
    for i in 0..va.len().max(vb.len()) {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false // equal
}

fn extract_fm_value(content: &str, key: &str) -> Option<String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_first = trimmed[3..].trim_start_matches(['\r', '\n']);
    let end = after_first.find("\n---")?;
    let fm_text = &after_first[..end];
    let prefix = format!("{key}:");
    fm_text.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with(&prefix) {
            Some(line[prefix.len()..].trim().to_string())
        } else {
            None
        }
    })
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".git" || name_str == "node_modules" || name_str == "target" {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn maybe_npm_install(dir: &std::path::Path) {
    if !dir.join("package.json").exists() || dir.join("node_modules").exists() {
        return;
    }
    let _ = std::process::Command::new("npm")
        .arg("install")
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Install binary for a skill that has manifest.json (tool skill).
///
/// Resolution order:
/// 1. manifest.json `binaries` field (skill author's CI/CD builds)
/// 2. Skill registry `binaries` field (registry-audited builds)
/// 3. `cargo build --release` fallback
fn maybe_install_binary(dir: &std::path::Path) {
    let has_manifest = dir.join("manifest.json").exists();
    let has_cargo = dir.join("Cargo.toml").exists();
    if !has_manifest && !has_cargo {
        return;
    }

    let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
    // Skip if executable already exists
    if dir.join(&dir_name).exists() || dir.join("main").exists() {
        return;
    }

    let key = platform_key();

    // Try 1: download from manifest.json binaries (skill repo's own CI/CD)
    if has_manifest {
        if let Ok(manifest_str) = std::fs::read_to_string(dir.join("manifest.json")) {
            if let Ok(manifest) =
                serde_json::from_str::<crate::plugins::manifest::PluginManifest>(&manifest_str)
            {
                if let Some(info) = manifest.binaries.get(&key) {
                    if let Ok(true) = download_binary(dir, &info.url, info.sha256.as_deref()) {
                        return;
                    }
                }
            }
        }
    }

    // Try 2: download from skill registry (audited builds)
    if let Some(binaries) = lookup_registry_binaries(&dir_name) {
        if let Some(info) = binaries.get(&key) {
            if let Ok(true) = download_binary(dir, &info.url, info.sha256.as_deref()) {
                return;
            }
        }
    }

    // Try 3: cargo build if Cargo.toml exists
    if !has_cargo {
        return;
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status();
    if let Ok(s) = status {
        if s.success() {
            if let Ok(cargo_toml) = std::fs::read_to_string(dir.join("Cargo.toml")) {
                for line in cargo_toml.lines() {
                    let line = line.trim();
                    if line.starts_with("name") {
                        if let Some(name) = line.split('=').nth(1) {
                            let name = name.trim().trim_matches('"');
                            let bin_path = dir.join("target").join("release").join(name);
                            if bin_path.exists() {
                                let _ = std::fs::copy(&bin_path, dir.join("main"));
                                #[cfg(unix)]
                                {
                                    use std::os::unix::fs::PermissionsExt;
                                    let _ = std::fs::set_permissions(
                                        dir.join("main"),
                                        std::fs::Permissions::from_mode(0o755),
                                    );
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn platform_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[derive(serde::Deserialize)]
struct RegistryBinaryInfo {
    url: String,
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(serde::Deserialize)]
struct RegistryEntry {
    name: String,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    binaries: std::collections::HashMap<String, RegistryBinaryInfo>,
}

fn lookup_registry_binaries(
    package_name: &str,
) -> Option<std::collections::HashMap<String, RegistryBinaryInfo>> {
    let url = "https://raw.githubusercontent.com/octos-org/octos-hub/main/registry.json";
    let entries: Vec<RegistryEntry> = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?
        .get(url)
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .ok()?;

    entries
        .into_iter()
        .find(|e| e.name == package_name || e.skills.contains(&package_name.to_string()))
        .map(|e| e.binaries)
        .filter(|b| !b.is_empty())
}

/// Download a binary from a URL, optionally verify SHA-256, and save as `main`.
///
/// Supports both raw binaries and `.tar.gz` archives (auto-detected from URL).
/// For archives, extracts the first executable file found.
fn download_binary(dir: &std::path::Path, url: &str, sha256: Option<&str>) -> Result<bool> {
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?
        .get(url)
        .send()?;

    if !response.status().is_success() {
        return Ok(false);
    }

    let bytes = response.bytes()?;

    // Verify SHA-256 if provided (hash is of the downloaded file, archive or raw)
    if let Some(expected) = sha256 {
        use sha2::{Digest, Sha256};
        let actual = format!("{:x}", Sha256::digest(&bytes));
        if actual != expected.to_lowercase() {
            return Ok(false);
        }
    }

    let dest = dir.join("main");

    if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        // Extract the first real file from the tar.gz archive.
        // Skip macOS AppleDouble resource fork files (._* prefix) which
        // appear before the actual binary in archives created on macOS.
        use std::io::Read;
        let gz = flate2::read::GzDecoder::new(&bytes[..]);
        let mut archive = tar::Archive::new(gz);
        let mut found = false;
        for entry in archive.entries()? {
            let mut entry = entry?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            // Skip AppleDouble resource fork files
            let is_apple_double = entry
                .path()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("._")))
                .unwrap_or(false);
            if is_apple_double {
                continue;
            }
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            std::fs::write(&dest, &buf)?;
            found = true;
            break;
        }
        if !found {
            return Ok(false);
        }
    } else {
        std::fs::write(&dest, &bytes)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
    }

    Ok(true)
}

/// Write .source tracking file for future updates.
fn write_source_info(dir: &std::path::Path, repo: &str, subdir: Option<&str>, branch: &str) {
    let info = serde_json::json!({
        "repo": repo,
        "subdir": subdir,
        "branch": branch,
        "installed_at": chrono::Utc::now().to_rfc3339(),
    });
    let _ = std::fs::write(
        dir.join(".source"),
        serde_json::to_string_pretty(&info).unwrap_or_default(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_metadata() {
        let tool = ManageSkillsTool::new("/tmp/skills");
        assert_eq!(tool.name(), "manage_skills");
        assert!(tool.description().contains("skill"));
        assert!(tool.tags().contains(&"gateway"));
    }

    #[test]
    fn schema_has_required_action() {
        let tool = ManageSkillsTool::new("/tmp/skills");
        let schema = tool.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"action"));
    }

    #[test]
    fn schema_action_enum() {
        let tool = ManageSkillsTool::new("/tmp/skills");
        let schema = tool.input_schema();
        let enums: Vec<&str> = schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(enums, vec!["list", "install", "update", "remove", "search"]);
    }

    #[tokio::test]
    async fn list_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(tmp.path().join("skills"));
        let result = tool
            .execute(&serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("No skills installed"));
    }

    #[tokio::test]
    async fn remove_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(tmp.path());
        let result = tool
            .execute(&serde_json::json!({"action": "remove", "name": "../../etc"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("Invalid"));
    }

    #[tokio::test]
    async fn install_missing_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(tmp.path());
        let result = tool
            .execute(&serde_json::json!({"action": "install"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("repo is required"));
    }

    #[tokio::test]
    async fn unknown_action() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(tmp.path());
        let result = tool
            .execute(&serde_json::json!({"action": "bogus"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("Unknown action"));
    }

    #[test]
    fn extract_fm_value_works() {
        let content = "---\nversion: 1.2.3\nauthor: test\n---\nBody";
        assert_eq!(extract_fm_value(content, "version"), Some("1.2.3".into()));
        assert_eq!(extract_fm_value(content, "author"), Some("test".into()));
        assert_eq!(extract_fm_value(content, "missing"), None);
    }
}
