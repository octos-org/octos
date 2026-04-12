//! Skills command: list, install, and remove skills.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use super::Executable;

// ── Public types for programmatic access ─────────────────────────────

/// Information about an installed skill (for programmatic use).
#[derive(Debug, Clone, Serialize)]
pub struct SkillEntry {
    pub name: String,
    pub version: Option<String>,
    pub tool_count: usize,
    pub source_repo: Option<String>,
}

/// Result of a skill installation operation.
#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub installed: Vec<String>,
    pub skipped: Vec<String>,
    pub deps_installed: Vec<String>,
}

const DEFAULT_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/octos-org/octos-hub/main/registry.json";

/// Pre-built binary info for a specific platform.
#[derive(Debug, Clone, Deserialize)]
struct BinaryInfo {
    /// Download URL.
    url: String,
    /// SHA-256 hash for integrity verification.
    #[serde(default)]
    sha256: Option<String>,
}

/// A skill package entry in the registry.
#[derive(Debug, Deserialize)]
struct RegistryEntry {
    /// Package name.
    name: String,
    /// Human-readable description.
    description: String,
    /// Source repo path (user/repo for GitHub, or full URL).
    repo: String,
    /// Package version (semver).
    #[serde(default)]
    version: Option<String>,
    /// Package author.
    #[serde(default)]
    author: Option<String>,
    /// License identifier (e.g. MIT, Apache-2.0).
    #[serde(default)]
    license: Option<String>,
    /// Individual skill names included in this package.
    #[serde(default)]
    skills: Vec<String>,
    /// External tools required (e.g. git, node).
    #[serde(default)]
    requires: Vec<String>,
    /// Whether this package provides tool executables (manifest.json).
    #[serde(default)]
    provides_tools: bool,
    /// Pre-built binaries keyed by `{os}-{arch}` (e.g. "darwin-aarch64").
    /// Managed by the registry after audit — not in the skill repo itself.
    #[serde(default)]
    binaries: std::collections::HashMap<String, BinaryInfo>,
    /// Searchable tags.
    #[serde(default)]
    tags: Vec<String>,
}

/// Source tracking info written to .source during install.
#[derive(Debug, serde::Serialize, Deserialize)]
struct SourceInfo {
    repo: String,
    #[serde(default)]
    subdir: Option<String>,
    branch: String,
    installed_at: String,
}

/// Manage agent skills.
#[derive(Debug, Args)]
pub struct SkillsCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Profile ID — install/manage skills in the profile's data directory
    /// (shared by the profile and its sub-accounts).
    #[arg(long)]
    pub profile: Option<String>,

    #[command(subcommand)]
    pub subcommand: SkillsSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SkillsSubcommand {
    /// List installed skills.
    List,
    /// Install skills from GitHub (e.g. user/repo or user/repo/skill-name).
    Install {
        /// GitHub path: user/repo (all skills) or user/repo/skill-name (single skill).
        /// Omit when using --all.
        repo: Option<String>,
        /// Install all packages from the registry.
        #[arg(long)]
        all: bool,
        /// Overwrite existing skills.
        #[arg(long)]
        force: bool,
        /// Git branch or tag to clone (default: main).
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// Remove an installed skill.
    Remove {
        /// Skill name to remove.
        name: String,
    },
    /// Search available skill packages from the registry.
    Search {
        /// Optional search query to filter results.
        query: Option<String>,
        /// Custom registry URL.
        #[arg(long)]
        registry: Option<String>,
    },
    /// Show detailed information about an installed skill.
    Info {
        /// Skill name.
        name: String,
    },
    /// Update an installed skill package from its source.
    Update {
        /// Skill name (or "all" to update everything).
        name: String,
        /// Git branch or tag (overrides stored branch).
        #[arg(long)]
        branch: Option<String>,
    },
}

impl Executable for SkillsCommand {
    fn execute(self) -> Result<()> {
        let cwd = match self.cwd {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };

        // Resolve skills directory: per-profile or global
        let skills_dir = if let Some(ref profile_id) = self.profile {
            let data_dir = super::resolve_data_dir(None)?;
            let store = crate::profiles::ProfileStore::open(&data_dir)?;
            resolve_profile_skills_dir(&store, profile_id)?
        } else {
            cwd.join(".octos").join("skills")
        };

        match self.subcommand {
            SkillsSubcommand::List => cmd_list(&skills_dir),
            SkillsSubcommand::Install {
                repo,
                all,
                force,
                branch,
            } => {
                if all {
                    cmd_install_all(&skills_dir, force, &branch)
                } else if let Some(repo) = repo {
                    cmd_install(&skills_dir, &repo, force, &branch)
                } else {
                    eyre::bail!(
                        "Provide a repo path (e.g. user/repo) or use --all to install everything from the registry"
                    )
                }
            }
            SkillsSubcommand::Remove { name } => cmd_remove(&skills_dir, &name),
            SkillsSubcommand::Search { query, registry } => {
                cmd_search(query.as_deref(), registry.as_deref())
            }
            SkillsSubcommand::Info { name } => cmd_info(&skills_dir, &name),
            SkillsSubcommand::Update { name, branch } => {
                cmd_update(&skills_dir, &name, branch.as_deref())
            }
        }
    }
}

// ── Public API for programmatic access ───────────────────────────────

/// Resolve the installed customer skills directory for exactly the requested account.
pub fn resolve_profile_skills_dir(
    store: &crate::profiles::ProfileStore,
    profile_id: &str,
) -> Result<PathBuf> {
    crate::skills_scope::resolve_account_skills_dir(store, profile_id)
}

/// List installed skills in a directory (returns structured data).
pub fn list_skills(skills_dir: &Path) -> Result<Vec<SkillEntry>> {
    if !skills_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<_> = std::fs::read_dir(skills_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("SKILL.md").exists())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut skills = Vec::new();
    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let version = std::fs::read_to_string(entry.path().join("SKILL.md"))
            .ok()
            .and_then(|c| extract_fm_value(&c, "version"));

        let tool_count = if entry.path().join("manifest.json").exists() {
            std::fs::read_to_string(entry.path().join("manifest.json"))
                .ok()
                .and_then(|m| serde_json::from_str::<serde_json::Value>(&m).ok())
                .and_then(|v| v.get("tools")?.as_array().map(|a| a.len()))
                .unwrap_or(0)
        } else {
            0
        };

        let source_repo = entry
            .path()
            .join(".source")
            .exists()
            .then(|| {
                std::fs::read_to_string(entry.path().join(".source"))
                    .ok()
                    .and_then(|s| serde_json::from_str::<SourceInfo>(&s).ok())
                    .map(|s| s.repo)
            })
            .flatten();

        skills.push(SkillEntry {
            name,
            version,
            tool_count,
            source_repo,
        });
    }
    Ok(skills)
}

/// Install skills from a GitHub repo or local path (blocking).
pub fn install_skill(
    skills_dir: &Path,
    repo: &str,
    force: bool,
    branch: &str,
) -> Result<InstallResult> {
    // Detect local path: starts with /, ./, ../, or ~
    let local_path = if repo.starts_with('/')
        || repo.starts_with("./")
        || repo.starts_with("../")
        || repo.starts_with('~')
    {
        let expanded = if let Some(rest) = repo.strip_prefix("~/") {
            dirs::home_dir().unwrap_or_default().join(rest)
        } else {
            PathBuf::from(repo)
        };
        let resolved = std::fs::canonicalize(&expanded)
            .wrap_err_with(|| format!("Local path not found: {}", expanded.display()))?;
        Some(resolved)
    } else {
        None
    };

    if let Some(src) = local_path {
        return install_from_local(skills_dir, &src, force);
    }

    let spec = RepoSpec::parse(repo)?;

    match install_via_git_result(skills_dir, &spec, force, branch) {
        Ok(result) => Ok(result),
        Err(e) => {
            let is_git_missing = e.to_string().contains("git not found");
            if is_git_missing && spec.subdir.is_some() {
                install_via_http(skills_dir, &spec, force, branch)?;
                let name = spec
                    .subdir
                    .as_deref()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_string();
                Ok(InstallResult {
                    installed: vec![name],
                    skipped: vec![],
                    deps_installed: vec![],
                })
            } else {
                Err(e)
            }
        }
    }
}

/// Install a skill from a local directory path.
fn install_from_local(skills_dir: &Path, src: &Path, force: bool) -> Result<InstallResult> {
    if !src.is_dir() {
        eyre::bail!("Not a directory: {}", src.display());
    }
    if !src.join("SKILL.md").exists() {
        eyre::bail!(
            "No SKILL.md found in {}. Is this a valid skill directory?",
            src.display()
        );
    }

    let name = src
        .file_name()
        .ok_or_else(|| eyre::eyre!("Cannot determine skill name from path"))?
        .to_string_lossy()
        .to_string();

    std::fs::create_dir_all(skills_dir)?;
    let dest = skills_dir.join(&name);

    if dest.exists() && !force {
        println!(
            "  {} '{}' already exists (use --force to overwrite)",
            "SKIP".yellow(),
            name
        );
        return Ok(InstallResult {
            installed: vec![],
            skipped: vec![name],
            deps_installed: vec![],
        });
    }

    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    copy_dir_recursive(src, &dest)?;

    // Build binary if needed
    maybe_install_binary(&dest)?;

    println!("  {} Installed '{}' from local path", "OK".green(), name);
    Ok(InstallResult {
        installed: vec![name],
        skipped: vec![],
        deps_installed: vec![],
    })
}

/// Remove an installed skill by name.
pub fn remove_skill(skills_dir: &Path, name: &str) -> Result<()> {
    // Reject path traversal attempts
    if name.contains('/')
        || name.contains('\\')
        || name == ".."
        || name == "."
        || name.contains('\0')
    {
        eyre::bail!("Invalid skill name: {name}");
    }
    let dest = skills_dir.join(name);
    if !dest.exists() {
        eyre::bail!("Skill '{name}' not found in {}", skills_dir.display());
    }
    std::fs::remove_dir_all(&dest)?;
    Ok(())
}

// ── CLI command handlers (print to stdout) ───────────────────────────

fn cmd_list(skills_dir: &Path) -> Result<()> {
    println!("{}", "Installed Skills".cyan().bold());
    println!("{}", "=".repeat(50));
    println!();

    if !skills_dir.exists() {
        println!("  {}", "No skills installed.".dimmed());
        println!();
        println!(
            "  Install system skills: {}",
            "octos skills install octos-org/system-skills".cyan()
        );
        println!();
        return Ok(());
    }

    let mut count = 0;
    let mut entries: Vec<_> = std::fs::read_dir(skills_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("SKILL.md").exists())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let mut extras = Vec::new();

        // Show version from frontmatter if available
        if let Ok(content) = std::fs::read_to_string(entry.path().join("SKILL.md")) {
            if let Some(ver) = extract_fm_value(&content, "version") {
                extras.push(format!("v{ver}"));
            }
        }
        // Show tools indicator if manifest.json exists
        if entry.path().join("manifest.json").exists() {
            if let Ok(manifest) = std::fs::read_to_string(entry.path().join("manifest.json")) {
                let tool_count = manifest.matches("\"name\"").count().saturating_sub(1);
                if tool_count > 0 {
                    extras.push(format!("[{tool_count} tool(s)]"));
                }
            }
        }
        // Show source
        if entry.path().join(".source").exists() {
            if let Ok(source_str) = std::fs::read_to_string(entry.path().join(".source")) {
                if let Ok(source) = serde_json::from_str::<SourceInfo>(&source_str) {
                    extras.push(format!("from {}", source.repo));
                }
            }
        }

        let extras_str = if extras.is_empty() {
            String::new()
        } else {
            format!("  {}", extras.join("  "))
        };
        println!("  {} {}", name.cyan(), extras_str.dimmed());
        count += 1;
    }

    if count == 0 {
        println!("  {}", "No skills installed.".dimmed());
        println!();
        println!(
            "  Install system skills: {}",
            "octos skills install octos-org/system-skills".cyan()
        );
    }

    println!();
    Ok(())
}

fn cmd_search(query: Option<&str>, registry_url: Option<&str>) -> Result<()> {
    let entries: Vec<RegistryEntry> = if let Some(url) = registry_url {
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .wrap_err("failed to create HTTP client")?
            .get(url)
            .send()
            .wrap_err_with(|| format!("failed to fetch registry from {url}"))?
            .error_for_status()
            .wrap_err("registry request failed")?
            .json()
            .wrap_err("failed to parse registry JSON")?
    } else {
        fetch_registry()?
    };

    // Filter by query if provided (match against name, description, tags, skills).
    let query_lower = query.map(|q| q.to_lowercase());
    let filtered: Vec<&RegistryEntry> = entries
        .iter()
        .filter(|e| {
            let Some(q) = &query_lower else {
                return true;
            };
            e.name.to_lowercase().contains(q)
                || e.description.to_lowercase().contains(q)
                || e.tags.iter().any(|t| t.to_lowercase().contains(q))
                || e.skills.iter().any(|s| s.to_lowercase().contains(q))
        })
        .collect();

    println!("{}", "Available Skill Packages".cyan().bold());
    println!("{}", "=".repeat(50));
    println!();

    if filtered.is_empty() {
        if let Some(q) = query {
            println!("  No packages matching '{}'", q);
        } else {
            println!("  Registry is empty.");
        }
        println!();
        return Ok(());
    }

    for entry in &filtered {
        let version_str = entry
            .version
            .as_deref()
            .map(|v| format!(" v{v}"))
            .unwrap_or_default();
        let tools_str = if entry.provides_tools {
            "  [tools]"
        } else {
            ""
        };
        println!(
            "  {}{}{}  {}",
            entry.name.cyan().bold(),
            version_str.dimmed(),
            tools_str.dimmed(),
            entry.description
        );
        if !entry.skills.is_empty() {
            println!("  {}  {}", "Skills:".dimmed(), entry.skills.join(", "));
        }
        if !entry.requires.is_empty() {
            println!("  {} {}", "Requires:".dimmed(), entry.requires.join(", "));
        }
        if !entry.tags.is_empty() {
            println!("  {}     {}", "Tags:".dimmed(), entry.tags.join(", "));
        }
        if let Some(author) = &entry.author {
            println!("  {}   {}", "Author:".dimmed(), author);
        }
        if let Some(license) = &entry.license {
            println!("  {}  {}", "License:".dimmed(), license);
        }
        println!(
            "  {} octos skills install {}",
            "Install:".dimmed(),
            entry.repo
        );
        println!();
    }

    Ok(())
}

/// Parsed repo specification.
struct RepoSpec {
    /// GitHub user/org.
    user: String,
    /// Repository name.
    repo: String,
    /// Optional subdirectory within the repo (for single-skill install).
    subdir: Option<String>,
}

impl RepoSpec {
    fn parse(input: &str) -> Result<Self> {
        let segments: Vec<&str> = input.trim_matches('/').split('/').collect();
        match segments.len() {
            2 => Ok(Self {
                user: segments[0].to_string(),
                repo: segments[1].to_string(),
                subdir: None,
            }),
            3.. => Ok(Self {
                user: segments[0].to_string(),
                repo: segments[1].to_string(),
                subdir: Some(segments[2..].join("/")),
            }),
            _ => eyre::bail!(
                "Invalid repo path: '{input}'. Expected user/repo or user/repo/skill-name"
            ),
        }
    }

    fn clone_url(&self) -> String {
        // Use SSH if the user has configured git to rewrite GitHub HTTPS to SSH,
        // or if SSH auth to github.com works (avoids credential prompts).
        let https = format!("https://github.com/{}/{}.git", self.user, self.repo);
        // Check if git config has an insteadOf rewrite for github HTTPS -> SSH
        if let Ok(output) = std::process::Command::new("git")
            .args(["config", "--get", "url.git@github.com:.insteadOf"])
            .output()
        {
            if output.status.success() {
                return format!("git@github.com:{}/{}.git", self.user, self.repo);
            }
        }
        https
    }
}

fn cmd_install(skills_dir: &Path, repo: &str, force: bool, branch: &str) -> Result<()> {
    let result = install_skill(skills_dir, repo, force, branch)?;

    // Print summary
    println!();
    if !result.installed.is_empty() {
        println!(
            "{} Installed {} skill(s): {}",
            "OK".green(),
            result.installed.len(),
            result.installed.join(", ").cyan()
        );
    }
    if !result.deps_installed.is_empty() {
        println!(
            "{} Installed {} shared dep(s): {}",
            "OK".green(),
            result.deps_installed.len(),
            result.deps_installed.join(", ").dimmed()
        );
    }
    if !result.skipped.is_empty() {
        println!(
            "{} Skipped {} existing: {}",
            "SKIP".yellow(),
            result.skipped.len(),
            result.skipped.join(", ")
        );
    }
    if result.installed.is_empty() && result.deps_installed.is_empty() && result.skipped.is_empty()
    {
        println!("{} No skills found in repository", "WARN".yellow());
    }
    Ok(())
}

fn fetch_registry() -> Result<Vec<RegistryEntry>> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .wrap_err("failed to create HTTP client")?
        .get(DEFAULT_REGISTRY_URL)
        .send()
        .wrap_err_with(|| format!("failed to fetch registry from {DEFAULT_REGISTRY_URL}"))?
        .error_for_status()
        .wrap_err("registry request failed")?
        .json()
        .wrap_err("failed to parse registry JSON")
}

fn cmd_install_all(skills_dir: &Path, force: bool, branch: &str) -> Result<()> {
    println!("{} Fetching skill registry...", "INFO".cyan());
    let entries = fetch_registry()?;

    if entries.is_empty() {
        println!("{} Registry is empty — nothing to install", "WARN".yellow());
        return Ok(());
    }

    println!(
        "{} Found {} package(s) in registry\n",
        "OK".green(),
        entries.len()
    );

    let mut total_installed: Vec<String> = Vec::new();
    let mut total_skipped: Vec<String> = Vec::new();
    let mut total_failed: Vec<(String, String)> = Vec::new();

    for entry in &entries {
        println!("  {} {}...", "Installing".dimmed(), entry.repo.cyan());
        match install_skill(skills_dir, &entry.repo, force, branch) {
            Ok(result) => {
                total_installed.extend(result.installed);
                total_installed.extend(result.deps_installed);
                total_skipped.extend(result.skipped);
            }
            Err(e) => {
                println!("    {} {}: {}", "FAIL".red(), entry.name, e);
                total_failed.push((entry.name.clone(), e.to_string()));
            }
        }
    }

    // Summary
    println!();
    println!("{}", "Summary".cyan().bold());
    println!("{}", "=".repeat(40));
    if !total_installed.is_empty() {
        println!(
            "  {} Installed {} skill(s): {}",
            "OK".green(),
            total_installed.len(),
            total_installed.join(", ").cyan()
        );
    }
    if !total_skipped.is_empty() {
        println!(
            "  {} Skipped {} existing: {}",
            "SKIP".yellow(),
            total_skipped.len(),
            total_skipped.join(", ")
        );
    }
    if !total_failed.is_empty() {
        println!(
            "  {} Failed {} package(s):",
            "FAIL".red(),
            total_failed.len()
        );
        for (name, err) in &total_failed {
            println!("    - {}: {}", name, err);
        }
    }
    if total_installed.is_empty() && total_skipped.is_empty() && total_failed.is_empty() {
        println!("  No skills found in any registry package");
    }
    println!();

    Ok(())
}

fn install_via_git_result(
    skills_dir: &Path,
    spec: &RepoSpec,
    force: bool,
    branch: &str,
) -> Result<InstallResult> {
    // Clone to a temp directory
    let tmp = tempfile::tempdir().wrap_err("failed to create temp directory")?;
    let clone_dir = tmp.path().join(&spec.repo);

    println!(
        "Cloning {} (branch: {})...",
        spec.clone_url().dimmed(),
        branch.cyan()
    );

    let status = std::process::Command::new("git")
        .args(["clone", "--depth", "1", "--branch", branch])
        .arg(spec.clone_url())
        .arg(&clone_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|_| eyre::eyre!("git not found. Please install git."))?;

    if !status.success() {
        eyre::bail!(
            "git clone failed. Check the repo path: {}/{}",
            spec.user,
            spec.repo
        );
    }

    std::fs::create_dir_all(skills_dir)?;

    let mut installed = Vec::new();
    let mut skipped = Vec::new();
    let mut deps_installed = Vec::new();

    if let Some(subdir) = &spec.subdir {
        // Targeted install: just one subdirectory + shared deps
        let src = clone_dir.join(subdir);
        if !src.is_dir() {
            eyre::bail!(
                "Subdirectory '{subdir}' not found in {}/{}",
                spec.user,
                spec.repo
            );
        }

        let name = Path::new(subdir)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        // Install the target skill/dir
        let dest = skills_dir.join(&name);
        if dest.exists() && !force {
            println!(
                "  {} '{}' already exists (use --force to overwrite)",
                "SKIP".yellow(),
                name
            );
            skipped.push(name.clone());
        } else {
            if dest.exists() {
                std::fs::remove_dir_all(&dest)?;
            }
            copy_dir_recursive(&src, &dest)?;
            if src.join("SKILL.md").exists() {
                installed.push(name.clone());
            } else {
                deps_installed.push(name.clone());
            }
        }

        // Auto-detect shared dependencies referenced in SKILL.md
        let skill_md_path = src.join("SKILL.md");
        if skill_md_path.exists() {
            let content = std::fs::read_to_string(&skill_md_path)?;
            let shared = find_shared_deps(&content, &clone_dir, &name);
            for dep in shared {
                let dep_src = clone_dir.join(&dep);
                let dep_dest = skills_dir.join(&dep);
                if dep_dest.exists() && !force {
                    println!(
                        "  {} dependency '{}' already exists (use --force to overwrite)",
                        "SKIP".yellow(),
                        dep
                    );
                } else {
                    if dep_dest.exists() {
                        std::fs::remove_dir_all(&dep_dest)?;
                    }
                    copy_dir_recursive(&dep_src, &dep_dest)?;
                    deps_installed.push(dep);
                }
            }
        }
    } else {
        // Whole-repo install: check if root is a single skill or multi-skill
        if clone_dir.join("SKILL.md").exists() {
            // Single-skill repo: install as repo_name/
            let dest = skills_dir.join(&spec.repo);
            if dest.exists() && !force {
                println!(
                    "  {} '{}' already exists (use --force to overwrite)",
                    "SKIP".yellow(),
                    spec.repo
                );
                skipped.push(spec.repo.clone());
            } else {
                if dest.exists() {
                    std::fs::remove_dir_all(&dest)?;
                }
                copy_dir_recursive(&clone_dir, &dest)?;
                installed.push(spec.repo.clone());
            }
        } else {
            // Multi-skill repo: copy all top-level directories
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
                if dest.exists() && !force {
                    println!(
                        "  {} '{}' already exists (use --force to overwrite)",
                        "SKIP".yellow(),
                        name
                    );
                    skipped.push(name);
                    continue;
                }
                if dest.exists() {
                    std::fs::remove_dir_all(&dest)?;
                }
                copy_dir_recursive(&entry.path(), &dest)?;

                if entry.path().join("SKILL.md").exists() {
                    installed.push(name);
                } else {
                    deps_installed.push(name);
                }
            }
        }
    }

    // Run npm install in any installed dirs that have package.json
    // and run cargo build for Rust crate tools
    for name in installed.iter().chain(deps_installed.iter()) {
        let dir = skills_dir.join(name);
        maybe_npm_install(&dir)?;
        maybe_install_binary(&dir)?;
    }

    // Write .source tracking file for each installed skill
    for name in installed.iter().chain(deps_installed.iter()) {
        let dest = skills_dir.join(name);
        write_source_info(&dest, spec, branch)?;
    }

    Ok(InstallResult {
        installed,
        skipped,
        deps_installed,
    })
}

/// HTTP fallback: fetch a single SKILL.md (original behavior).
fn install_via_http(skills_dir: &Path, spec: &RepoSpec, force: bool, branch: &str) -> Result<()> {
    let subdir = spec.subdir.as_deref().unwrap_or(&spec.repo);
    let name = subdir.rsplit('/').next().unwrap();

    let dest = skills_dir.join(name);
    if dest.exists() && !force {
        eyre::bail!(
            "Skill '{name}' already exists at {} (use --force to overwrite)",
            dest.display()
        );
    }

    let url = format!(
        "https://raw.githubusercontent.com/{}/{}/{branch}/{subdir}/SKILL.md",
        spec.user, spec.repo
    );
    println!("Fetching {}...", url.dimmed());

    let body = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .wrap_err("failed to create HTTP client")?
        .get(&url)
        .send()
        .wrap_err_with(|| format!("failed to fetch {url}"))?;

    if !body.status().is_success() {
        eyre::bail!(
            "Failed to fetch SKILL.md (HTTP {}). Check the repo path: {}",
            body.status(),
            spec.subdir.as_deref().unwrap_or("")
        );
    }

    let content = body.text().wrap_err("failed to read response body")?;

    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::create_dir_all(&dest)?;
    std::fs::write(dest.join("SKILL.md"), &content)?;
    write_source_info(&dest, spec, branch)?;

    println!(
        "{} Installed skill '{}' to {}",
        "OK".green(),
        name.cyan(),
        dest.display()
    );
    Ok(())
}

/// Write source tracking info so we can update later.
fn write_source_info(dest: &Path, spec: &RepoSpec, branch: &str) -> Result<()> {
    let info = SourceInfo {
        repo: format!("{}/{}", spec.user, spec.repo),
        subdir: spec.subdir.clone(),
        branch: branch.to_string(),
        installed_at: chrono::Utc::now().to_rfc3339(),
    };
    std::fs::write(dest.join(".source"), serde_json::to_string_pretty(&info)?)?;
    Ok(())
}

/// Extract a frontmatter value from raw SKILL.md content (simple helper for display).
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

fn cmd_info(skills_dir: &Path, name: &str) -> Result<()> {
    let skill_dir = skills_dir.join(name);
    let skill_file = skill_dir.join("SKILL.md");

    if !skill_file.exists() {
        eyre::bail!(
            "Skill '{name}' not found. Install it with: octos skills install <repo>/{name}"
        );
    }

    let content = std::fs::read_to_string(&skill_file)?;

    println!("{}", "Skill Package Info".cyan().bold());
    println!("{}", "=".repeat(50));
    println!();

    // Frontmatter fields
    println!("  {}    {}", "Name:".dimmed(), name.cyan());
    if let Some(desc) = extract_fm_value(&content, "description") {
        println!("  {}    {}", "Desc:".dimmed(), desc);
    }
    if let Some(ver) = extract_fm_value(&content, "version") {
        println!("  {} {}", "Version:".dimmed(), ver);
    }
    if let Some(author) = extract_fm_value(&content, "author") {
        println!("  {}  {}", "Author:".dimmed(), author);
    }
    if let Some(always) = extract_fm_value(&content, "always") {
        println!("  {}  {}", "Always:".dimmed(), always);
    }
    if let Some(bins) = extract_fm_value(&content, "requires_bins") {
        println!("  {}    {}", "Bins:".dimmed(), bins);
    }
    if let Some(envs) = extract_fm_value(&content, "requires_env") {
        println!("  {}     {}", "Env:".dimmed(), envs);
    }

    // Tools (manifest.json)
    let manifest_path = skill_dir.join("manifest.json");
    if manifest_path.exists() {
        let manifest_str = std::fs::read_to_string(&manifest_path)?;
        if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&manifest_str) {
            if let Some(tools) = manifest.get("tools").and_then(|t| t.as_array()) {
                println!();
                println!("  {} ({} tool(s))", "Tools:".cyan(), tools.len());
                for tool in tools {
                    let tname = tool.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    let tdesc = tool
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("");
                    println!("    - {}  {}", tname.cyan(), tdesc.dimmed());
                }
            }
        }
    }

    // Source info
    let source_path = skill_dir.join(".source");
    if source_path.exists() {
        if let Ok(source_str) = std::fs::read_to_string(&source_path) {
            if let Ok(source) = serde_json::from_str::<SourceInfo>(&source_str) {
                println!();
                println!("  {}  {}", "Source:".dimmed(), source.repo);
                println!("  {}  {}", "Branch:".dimmed(), source.branch);
                println!("  {} {}", "Installed:".dimmed(), source.installed_at);
            }
        }
    }

    // Node.js deps
    if skill_dir.join("package.json").exists() {
        println!();
        println!("  {} Node.js (package.json present)", "Runtime:".dimmed());
    }
    // Rust crate
    if skill_dir.join("Cargo.toml").exists() {
        println!();
        println!("  {} Rust crate (Cargo.toml present)", "Runtime:".dimmed());
    }

    println!();
    Ok(())
}

fn cmd_update(skills_dir: &Path, name: &str, branch_override: Option<&str>) -> Result<()> {
    if name == "all" {
        // Update all skills that have .source files
        if !skills_dir.exists() {
            println!("{} No skills directory found", "WARN".yellow());
            return Ok(());
        }
        let mut updated = 0;
        for entry in std::fs::read_dir(skills_dir)? {
            let entry = entry?;
            let skill_name = entry.file_name().to_string_lossy().to_string();
            if entry.path().join(".source").exists() {
                println!("Updating {}...", skill_name.cyan());
                match update_single(skills_dir, &skill_name, branch_override) {
                    Ok(()) => updated += 1,
                    Err(e) => println!("  {} {}: {}", "FAIL".red(), skill_name, e),
                }
            }
        }
        println!();
        println!("{} Updated {} skill(s)", "OK".green(), updated);
        return Ok(());
    }

    update_single(skills_dir, name, branch_override)
}

fn update_single(skills_dir: &Path, name: &str, branch_override: Option<&str>) -> Result<()> {
    let skill_dir = skills_dir.join(name);
    let source_path = skill_dir.join(".source");

    if !source_path.exists() {
        eyre::bail!("No source info for '{name}'. Was it installed with `octos skills install`?");
    }

    let source: SourceInfo = serde_json::from_str(&std::fs::read_to_string(&source_path)?)?;

    // Pre-clone version check: compare local version against registry
    let local_ver = if skill_dir.join("SKILL.md").exists() {
        let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).ok();
        content.and_then(|c| extract_fm_value(&c, "version"))
    } else {
        None
    };

    if let Some(ref lv) = local_ver {
        // Try fetching registry to compare versions before cloning
        if let Ok(entries) = fetch_registry() {
            let registry_ver = entries
                .iter()
                .find(|e| {
                    e.repo == source.repo || e.skills.contains(&name.to_string()) || e.name == name
                })
                .and_then(|e| e.version.as_ref());

            if let Some(rv) = registry_ver {
                if !version_newer(rv, lv) {
                    println!("  {} '{}' is up to date (v{})", "OK".green(), name, lv);
                    return Ok(());
                }
                println!(
                    "  {} '{}' update available: v{} → v{}",
                    "INFO".cyan(),
                    name,
                    lv,
                    rv
                );
            }
        }
        // If registry fetch fails or entry not found, fall through to clone
    }

    let branch = branch_override.unwrap_or(&source.branch);
    let repo = if let Some(subdir) = &source.subdir {
        format!("{}/{}", source.repo, subdir)
    } else {
        source.repo.clone()
    };

    cmd_install(skills_dir, &repo, true, branch)
}

/// Get the current platform key for binary downloads (e.g. "darwin-aarch64").
fn platform_key() -> String {
    // Rust's std::env::consts::OS returns "macos" but the convention
    // in manifest.json and the registry is "darwin".
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = std::env::consts::ARCH; // aarch64, x86_64
    format!("{os}-{arch}")
}

/// Fetch the registry and find binary info for a package name.
fn lookup_registry_binaries(
    package_name: &str,
    registry_url: Option<&str>,
) -> Option<std::collections::HashMap<String, BinaryInfo>> {
    let url = registry_url.unwrap_or(DEFAULT_REGISTRY_URL);
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

/// Download a pre-built binary from registry binary info.
/// Returns true if a binary was downloaded and verified successfully.
fn download_binary(
    dir: &Path,
    binaries: &std::collections::HashMap<String, BinaryInfo>,
) -> Result<bool> {
    let key = platform_key();
    let info = match binaries.get(&key) {
        Some(info) => info,
        None => {
            println!(
                "  {} No pre-built binary for {} (available: {})",
                "WARN".yellow(),
                key,
                binaries.keys().cloned().collect::<Vec<_>>().join(", ")
            );
            return Ok(false);
        }
    };

    println!("  Downloading binary for {}...", key.cyan());

    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .wrap_err("failed to create HTTP client")?
        .get(&info.url)
        .send()
        .wrap_err_with(|| format!("failed to download binary from {}", info.url))?;

    if !response.status().is_success() {
        println!(
            "  {} Download failed (HTTP {})",
            "WARN".yellow(),
            response.status()
        );
        return Ok(false);
    }

    let bytes = response
        .bytes()
        .wrap_err("failed to read binary response")?;

    // Verify SHA-256 if provided by registry
    if let Some(expected_hash) = &info.sha256 {
        use sha2::{Digest, Sha256};
        let actual_hash = format!("{:x}", Sha256::digest(&bytes));
        if actual_hash != expected_hash.to_lowercase() {
            println!(
                "  {} Binary integrity check failed (hash mismatch)",
                "FAIL".red()
            );
            return Ok(false);
        }
        println!("  {} Hash verified", "OK".green());
    }

    let dest = dir.join("main");

    if info.url.ends_with(".tar.gz") || info.url.ends_with(".tgz") {
        use std::io::Read;
        let gz = flate2::read::GzDecoder::new(&bytes[..]);
        let mut archive = tar::Archive::new(gz);
        let mut found = false;
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.header().entry_type().is_file() {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                std::fs::write(&dest, &buf)?;
                found = true;
                break;
            }
        }
        if !found {
            println!("  {} No file found in archive", "WARN".yellow());
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

    println!(
        "  {} Downloaded binary ({} bytes)",
        "OK".green(),
        bytes.len()
    );
    Ok(true)
}

/// Download a binary from a direct URL with optional SHA-256 verification.
///
/// Supports both raw binaries and `.tar.gz` archives (auto-detected from URL).
fn download_binary_from_url(dir: &Path, url: &str, sha256: Option<&str>) -> Result<bool> {
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .wrap_err("failed to create HTTP client")?
        .get(url)
        .send()
        .wrap_err_with(|| format!("failed to download binary from {url}"))?;

    if !response.status().is_success() {
        println!(
            "  {} Download failed (HTTP {})",
            "WARN".yellow(),
            response.status()
        );
        return Ok(false);
    }

    let bytes = response
        .bytes()
        .wrap_err("failed to read binary response")?;

    if let Some(expected) = sha256 {
        use sha2::{Digest, Sha256};
        let actual = format!("{:x}", Sha256::digest(&bytes));
        if actual != expected.to_lowercase() {
            println!(
                "  {} Binary integrity check failed (hash mismatch)",
                "FAIL".red()
            );
            return Ok(false);
        }
        println!("  {} Hash verified", "OK".green());
    }

    let dest = dir.join("main");

    if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        use std::io::Read;
        let gz = flate2::read::GzDecoder::new(&bytes[..]);
        let mut archive = tar::Archive::new(gz);
        let mut found = false;
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.header().entry_type().is_file() {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                std::fs::write(&dest, &buf)?;
                found = true;
                break;
            }
        }
        if !found {
            println!("  {} No file found in archive", "WARN".yellow());
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

    println!(
        "  {} Downloaded binary ({} bytes)",
        "OK".green(),
        bytes.len()
    );
    Ok(true)
}

/// Install binary for skill package.
///
/// Resolution order:
/// 1. Download from manifest.json `binaries` field (skill author's CI/CD)
/// 2. Download from skill registry `binaries` field (registry-audited)
/// 3. `cargo build --release` as fallback if Cargo.toml exists
fn maybe_install_binary(dir: &Path) -> Result<()> {
    let has_manifest = dir.join("manifest.json").exists();
    let has_cargo = dir.join("Cargo.toml").exists();

    if !has_manifest && !has_cargo {
        return Ok(());
    }

    // Skip if executable already exists
    let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
    if dir.join(&dir_name).exists() || dir.join("main").exists() {
        return Ok(());
    }

    let key = platform_key();

    // Try 1: download from manifest.json binaries (skill repo's own CI/CD)
    if has_manifest {
        if let Ok(manifest_str) = std::fs::read_to_string(dir.join("manifest.json")) {
            if let Ok(manifest) = serde_json::from_str::<
                octos_agent::plugins::manifest::PluginManifest,
            >(&manifest_str)
            {
                if let Some(info) = manifest.binaries.get(&key) {
                    println!("  Downloading binary for {} from manifest...", key.cyan());
                    if download_binary_from_url(dir, &info.url, info.sha256.as_deref())? {
                        // Log installation
                        install_main_to_cargo_bin(dir, &manifest.name);
                        return Ok(());
                    }
                }
            }
        }
    }

    // Try 2: look up pre-built binary from registry
    if let Some(binaries) = lookup_registry_binaries(&dir_name, None) {
        if download_binary(dir, &binaries)? {
            install_main_to_cargo_bin(dir, &dir_name);
            return Ok(());
        }
    }

    // Try 3: cargo build if Cargo.toml exists
    if !has_cargo {
        return Ok(());
    }

    // Ensure the skill crate is not absorbed into a parent workspace
    let cargo_toml_path = dir.join("Cargo.toml");
    if let Ok(content) = std::fs::read_to_string(&cargo_toml_path) {
        if !content.contains("[workspace]") {
            let patched = format!("{}\n[workspace]\n", content.trim_end());
            let _ = std::fs::write(&cargo_toml_path, patched);
        }
    }

    println!(
        "  Running {} in {}...",
        "cargo build --release".cyan(),
        dir_name
    );
    let status = std::process::Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|_| {
            eyre::eyre!(
                "cargo not found. Install Rust or ask the skill author for pre-built binaries."
            )
        })?;
    if !status.success() {
        eyre::bail!("cargo build failed in {}", dir.display());
    }

    // Copy the built binary to 'main' for PluginLoader to find,
    // Copy built binary to skill dir as `main` for plugin loader discovery.
    let target_dir = dir.join("target").join("release");
    if let Ok(cargo_toml) = std::fs::read_to_string(dir.join("Cargo.toml")) {
        for line in cargo_toml.lines() {
            let line = line.trim();
            if line.starts_with("name") {
                if let Some(name) = line.split('=').nth(1) {
                    let name = name.trim().trim_matches('"');
                    let bin_path = target_dir.join(name);
                    if bin_path.exists() {
                        std::fs::copy(&bin_path, dir.join("main"))?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            std::fs::set_permissions(
                                dir.join("main"),
                                std::fs::Permissions::from_mode(0o755),
                            )?;
                        }
                        // Log installation
                        install_main_to_cargo_bin(dir, name);
                        println!("  {} Built and installed binary '{}'", "OK".green(), name);
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Recursively copy a directory, skipping .git, node_modules, and target (Rust build).
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
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

/// Scan SKILL.md for references to sibling directories (shared deps).
/// Looks for patterns like `~/.octos/skills/XXX/` where XXX is a sibling dir in the clone.
fn find_shared_deps(skill_md: &str, clone_dir: &Path, self_name: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let re = regex::Regex::new(r"~/.octos/skills/([a-zA-Z0-9_-]+)/").unwrap();
    for cap in re.captures_iter(skill_md) {
        let dep_name = cap[1].to_string();
        if dep_name == self_name {
            continue;
        }
        // Only include if this dir actually exists in the cloned repo
        if clone_dir.join(&dep_name).is_dir() && !deps.contains(&dep_name) {
            deps.push(dep_name);
        }
    }
    deps
}

/// Run `npm install` if `package.json` exists but `node_modules/` does not.
fn maybe_npm_install(dir: &Path) -> Result<()> {
    if !dir.join("package.json").exists() {
        return Ok(());
    }
    if dir.join("node_modules").exists() {
        return Ok(());
    }
    println!(
        "  Running {} in {}...",
        "npm install".cyan(),
        dir.file_name().unwrap().to_string_lossy()
    );
    let status = std::process::Command::new("npm")
        .arg("install")
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|_| {
            eyre::eyre!("npm not found. Please install Node.js to set up dependencies.")
        })?;
    if !status.success() {
        eyre::bail!("npm install failed in {}", dir.display());
    }
    Ok(())
}

/// Log that the binary was installed in the skill directory.
/// Previously this copied to ~/.cargo/bin/ which is global and shared
/// across profiles. Skills should be self-contained in their directory.
fn install_main_to_cargo_bin(dir: &Path, name: &str) {
    let main_path = dir.join("main");
    if !main_path.exists() {
        return;
    }
    println!(
        "  {} Installed '{}' to {}",
        "OK".green(),
        name,
        dir.display()
    );
}

fn cmd_remove(skills_dir: &Path, name: &str) -> Result<()> {
    remove_skill(skills_dir, name)?;
    println!("{} Removed skill '{}'", "OK".green(), name.cyan());
    Ok(())
}
