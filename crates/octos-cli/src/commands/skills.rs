//! Skills command: list, install, and remove skills, and develop one from
//! its Rust source (`dev`, `run`, `check`).

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::{Result, WrapErr};
use octos_agent::plugins::source_runtime;
use serde::{Deserialize, Serialize};

use super::{Executable, skills_dev};

/// RFC-2 (issue #1291): validate a skill directory's `manifest.json`
/// against the strict octos JSON schema validator before we commit
/// the install to disk.
///
/// Behaviour
/// ---------
/// - If the skill has no `manifest.json`, this is a no-op (pure SKILL.md
///   skills are not affected — they don't ship tool schemas).
/// - If the manifest fails to parse OR fails strict schema validation,
///   we bail with the structured violation list so the operator sees
///   every problem at once.
/// - Operators who need to install a known-bad manifest can set
///   `OCTOS_MANIFEST_VALIDATION=lenient` (Draft 07 sanity only) or
///   `=off` (skip entirely). Default is `strict`.
pub(super) fn validate_skill_manifest(skill_dir: &Path) -> Result<()> {
    let manifest_path = skill_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Ok(());
    }
    // `PluginManifest::from_file` runs both structural and schema
    // validation, threading through `OCTOS_MANIFEST_VALIDATION` for
    // the strict-rule layer.
    octos_plugin::PluginManifest::from_file(&manifest_path)
        .map(|_| ())
        .wrap_err_with(|| {
            format!(
                "manifest at {} failed RFC-2 schema validation\n\nSet OCTOS_MANIFEST_VALIDATION=lenient to skip the strict octos rules, or =off to skip validation entirely.",
                manifest_path.display()
            )
        })
}

// ── Public types for programmatic access ─────────────────────────────

/// Information about an installed skill (for programmatic use).
#[derive(Debug, Clone, Serialize)]
pub struct SkillEntry {
    pub name: String,
    pub version: Option<String>,
    pub tool_count: usize,
    pub source_repo: Option<String>,
    /// The actual reviewed source commit this install published
    /// (UPCR-2026-R48CB467B), present when installed via
    /// `profile/skills/install_reviewed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    /// The registry review timestamp bound to that commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_at: Option<String>,
    /// The actual SHA-256 of the downloaded binary artifact, when the
    /// reviewed package carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
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

/// Resolve the registry URL: an explicit argument wins, then the
/// `OCTOS_SKILLS_REGISTRY_URL` override (private-registry operators and
/// isolated tests), then the public default. Kept in one place so the
/// search projection and the reviewed-install re-fetch can never drift.
fn resolve_registry_url(override_url: Option<&str>) -> String {
    override_url
        .map(str::to_owned)
        .or_else(|| std::env::var("OCTOS_SKILLS_REGISTRY_URL").ok())
        .unwrap_or_else(|| DEFAULT_REGISTRY_URL.to_owned())
}

/// Pre-built binary info for a specific platform.
#[derive(Debug, Clone, Deserialize)]
struct BinaryInfo {
    /// Download URL.
    url: String,
    /// SHA-256 hash; mandatory for remote downloads.
    #[serde(default)]
    sha256: Option<String>,
    /// Base64 Ed25519 signature over package/platform/digest.
    #[serde(default)]
    signature: Option<String>,
}

/// A skill package entry in the registry.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RegistryEntry {
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
    /// Reviewed upstream commit; mandatory for binary-bearing entries.
    #[serde(default)]
    source_commit: Option<String>,
    /// RFC3339 registry review time; mandatory for binary-bearing entries.
    #[serde(default)]
    reviewed_at: Option<String>,
    /// Pre-built binaries keyed by `{os}-{arch}` (e.g. "darwin-aarch64").
    /// Managed by the registry after audit — not in the skill repo itself.
    #[serde(default)]
    binaries: std::collections::HashMap<String, BinaryInfo>,
    /// Searchable tags.
    #[serde(default)]
    tags: Vec<String>,
}

/// Public binary-artifact proof for one platform, surfaced to API consumers
/// so a reviewed install request can carry the exact expected digest and
/// signature (UPCR-2026-R48CB467B).
#[cfg(feature = "api")]
#[derive(Debug, Clone, Serialize)]
pub struct RegistryBinaryProof {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Public skill package metadata exposed to API consumers.
#[cfg(feature = "api")]
#[derive(Debug, Clone, Serialize)]
pub struct RegistryPackage {
    pub name: String,
    pub description: String,
    pub repo: String,
    pub version: Option<String>,
    pub author: Option<String>,
    pub license: Option<String>,
    pub skills: Vec<String>,
    pub requires: Vec<String>,
    pub provides_tools: bool,
    pub tags: Vec<String>,
    /// Full 40-hex reviewed source commit (UPCR-2026-R48CB467B).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    /// RFC3339 registry review timestamp bound to `source_commit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_at: Option<String>,
    /// Per-platform (`{os}-{arch}`) downloadable artifact proofs.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub binaries: std::collections::BTreeMap<String, RegistryBinaryProof>,
}

#[cfg(feature = "api")]
impl From<RegistryEntry> for RegistryPackage {
    fn from(value: RegistryEntry) -> Self {
        Self {
            name: value.name,
            description: value.description,
            repo: value.repo,
            version: value.version,
            author: value.author,
            license: value.license,
            skills: value.skills,
            requires: value.requires,
            provides_tools: value.provides_tools,
            tags: value.tags,
            source_commit: value.source_commit,
            reviewed_at: value.reviewed_at,
            binaries: value
                .binaries
                .into_iter()
                .map(|(platform, info)| {
                    (
                        platform,
                        RegistryBinaryProof {
                            url: info.url,
                            sha256: info.sha256,
                            signature: info.signature,
                        },
                    )
                })
                .collect(),
        }
    }
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
    /// Install skills from GitHub shorthand, a full Git URL, or a local path.
    Install {
        /// Source: user/repo, user/repo/skill-name, full Git URL, or local path.
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
    /// Register a skill crate to run from its Rust source through the Relax
    /// JIT (experimental): a source edit is live on the next tool call.
    Dev {
        /// Skill crate folder (Cargo.toml, manifest.json, SKILL.md).
        path: PathBuf,
        /// Environment for the skill, written into its launcher: NAME=VALUE,
        /// or NAME to take the current value. Repeatable.
        #[arg(long = "env", value_name = "NAME[=VALUE]")]
        env: Vec<String>,
        /// Skip the first run, which compiles and caches the skill.
        #[arg(long)]
        no_warm: bool,
        /// Replace an installed skill of the same name.
        #[arg(long)]
        force: bool,
    },
    /// Call one tool of a skill from its Rust source through the Relax JIT
    /// (experimental). The call runs in the current folder.
    Run {
        /// A skill crate folder, or the name of a skill registered with `dev`.
        skill: String,
        /// Tool name.
        tool: String,
        /// JSON input. Default: stdin, or {} when stdin is a terminal.
        input: Option<String>,
    },
    /// Type- and borrow-check a skill's Rust source through the Relax JIT
    /// (experimental). The skill is not run; build scripts and procedural
    /// macros are.
    Check {
        /// A skill crate folder, or the name of a skill registered with `dev`.
        skill: String,
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
            let store = crate::profiles::ProfileStore::open_unified(&data_dir)?;
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
                        "Provide a skill source (e.g. user/repo, https://host/org/repo.git, or ./path/to/skill) or use --all to install everything from the registry"
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
            SkillsSubcommand::Dev {
                path,
                env,
                no_warm,
                force,
            } => skills_dev::cmd_dev(&skills_dir, &path, &env, !no_warm, force),
            SkillsSubcommand::Run { skill, tool, input } => {
                skills_dev::cmd_run(&skills_dir, &skill, &tool, input.as_deref())
            }
            SkillsSubcommand::Check { skill } => skills_dev::cmd_check(&skills_dir, &skill),
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

    // Dot-directories are never installed skills: they are the in-flight
    // reviewed-install staging and rollback trees (UPCR-2026-R48CB467B) and
    // platform metadata droppings.
    let mut entries: Vec<_> = std::fs::read_dir(skills_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
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

        let reviewed = read_reviewed_install_receipt(&entry.path());
        skills.push(SkillEntry {
            name,
            version,
            tool_count,
            source_repo,
            source_commit: reviewed
                .as_ref()
                .map(|receipt| receipt.source_commit.clone()),
            reviewed_at: reviewed.as_ref().map(|receipt| receipt.reviewed_at.clone()),
            artifact_sha256: reviewed.and_then(|receipt| receipt.artifact_sha256),
        });
    }
    Ok(skills)
}

/// Install skills from GitHub shorthand, a full Git URL, or a local path (blocking).
pub fn install_skill(
    skills_dir: &Path,
    repo: &str,
    force: bool,
    branch: &str,
) -> Result<InstallResult> {
    match resolve_install_source(repo)? {
        InstallSource::Local(src) => install_from_local(skills_dir, &src, force),
        InstallSource::Repo(spec) => match install_via_git_result(skills_dir, &spec, force, branch)
        {
            Ok(result) => Ok(result),
            Err(e) => {
                let is_git_missing = e.to_string().contains("git not found");
                if is_git_missing && spec.subdir.is_some() && spec.github_shorthand {
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
        },
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

    // RFC-2: reject malformed manifests BEFORE we copy anything to
    // disk. We only validate when we're actually going to copy: a
    // skip path (existing install, no `--force`) preserves whatever
    // the user already has — validating a fresh source then would
    // surface a spurious failure for an install that we never
    // intended to write.
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

    validate_skill_manifest(src)?;
    clear_for_installed_tree(&dest)?;
    copy_dir_recursive(src, &dest)?;

    // Build binary if needed
    maybe_install_binary(&dest, true)?;

    // Run hardware lifecycle phases (preflight → init → ready_check) if the
    // skill has a manifest.json. Best-effort: only skills with manifest.json
    // and a `hardware_lifecycle` section are affected. Failure is fatal here so
    // the user gets a clear error and re-install starts from a clean state.
    run_activate_lifecycle(&dest, &name, true)?;

    println!("  {} Installed '{}' from local path", "OK".green(), name);
    Ok(InstallResult {
        installed: vec![name],
        skipped: vec![],
        deps_installed: vec![],
    })
}

/// Remove an installed skill by name.
///
/// Removal is idempotent: if the skill directory is already absent the call
/// succeeds without error, matching HTTP DELETE semantics and the harness
/// compatibility contract (M4.4).
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
        // Idempotent: already removed.
        return Ok(());
    }

    // Run shutdown lifecycle phase (best-effort — failure is logged but does
    // not block removal). Only runs if the skill has manifest.json with a
    // `hardware_lifecycle.shutdown` section.
    {
        use octos_agent::plugins::run_shutdown_phase;

        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => {
                let lifecycle_allowed =
                    std::env::var("OCTOS_SKILLS_LIFECYCLE").as_deref() == Ok("1");
                rt.block_on(run_shutdown_phase(&dest, lifecycle_allowed));
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to build runtime for shutdown phase; skipping (skill removal continues)"
                );
            }
        }
    }

    std::fs::remove_dir_all(&dest)?;
    source_runtime::forget_dev_skill(&dest);
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
    // Dot-directories are never installed skills: they are the in-flight
    // reviewed-install staging and rollback trees (UPCR-2026-R48CB467B) and
    // platform metadata droppings.
    let mut entries: Vec<_> = std::fs::read_dir(skills_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
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
        if let Ok(Some(dev)) = source_runtime::read_dev_marker(&entry.path()) {
            extras.push(format!("dev, runs from {}", dev.source.display()));
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
    let entries = fetch_registry_from(registry_url)?;
    let filtered = filter_registry_entries(&entries, query);

    println!("{}", "Available Skill Packages".cyan().bold());
    println!("{}", "=".repeat(50));
    println!();

    if filtered.is_empty() {
        if let Some(q) = query {
            println!("  No packages matching '{q}'");
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
enum InstallSource {
    Local(PathBuf),
    Repo(RepoSpec),
}

pub(super) fn looks_like_local_path(input: &str) -> bool {
    input.starts_with('/')
        || input.starts_with("./")
        || input.starts_with("../")
        || input == "."
        || input == ".."
        || input.starts_with('~')
        // Windows: backslash-relative, UNC/verbatim, and drive-absolute paths.
        // `octos skills install C:\path\to\skill` must resolve as Local, not a
        // git-repo shorthand. None of these tokens match a real POSIX path, so
        // no `cfg(windows)` gate is needed; `canonicalize` still validates it.
        || input.starts_with(".\\")
        || input.starts_with("..\\")
        || input.starts_with("\\\\")
        || has_windows_drive_prefix(input)
}

/// True for a Windows drive-absolute prefix like `C:\` or `C:/`.
fn has_windows_drive_prefix(input: &str) -> bool {
    let b = input.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

fn looks_like_git_url(input: &str) -> bool {
    input.contains("://") || is_scp_like_git_url(input)
}

fn is_scp_like_git_url(input: &str) -> bool {
    if input.contains("://") || input.starts_with('/') {
        return false;
    }

    let Some((user_host, path)) = input.split_once(':') else {
        return false;
    };

    user_host.contains('@') && !path.is_empty() && !user_host.contains('/')
}

fn resolve_install_source(input: &str) -> Result<InstallSource> {
    let input = input.trim();

    if looks_like_local_path(input) {
        let expanded = if let Some(rest) = input.strip_prefix("~/") {
            dirs::home_dir().unwrap_or_default().join(rest)
        } else {
            PathBuf::from(input)
        };
        let resolved = std::fs::canonicalize(&expanded)
            .wrap_err_with(|| format!("Local path not found: {}", expanded.display()))?;
        return Ok(InstallSource::Local(resolved));
    }

    Ok(InstallSource::Repo(RepoSpec::parse(input)?))
}

fn repo_name_from_git_url(input: &str) -> Result<String> {
    let trimmed = input
        .trim()
        .trim_end_matches('/')
        .split(['#', '?'])
        .next()
        .unwrap_or_default();
    let tail = trimmed.rsplit(['/', ':']).next().ok_or_else(|| {
        eyre::eyre!(
            "Invalid Git URL: '{input}'. Accepted forms: \
                                   https://host/owner/repo, owner/repo, or a local path"
        )
    })?;
    let repo_name = tail.strip_suffix(".git").unwrap_or(tail).trim();
    if repo_name.is_empty() {
        eyre::bail!(
            "Invalid Git URL: '{input}'. Accepted forms: https://host/owner/repo, \
             owner/repo, or a local path"
        );
    }
    Ok(repo_name.to_string())
}

fn github_clone_url(user: &str, repo: &str) -> String {
    // Use SSH if the user has configured git to rewrite GitHub HTTPS to SSH,
    // or if SSH auth to github.com works (avoids credential prompts).
    let https = format!("https://github.com/{user}/{repo}.git");
    // Check if git config has an insteadOf rewrite for github HTTPS -> SSH
    if let Ok(output) = std::process::Command::new("git")
        .args(["config", "--get", "url.git@github.com:.insteadOf"])
        .output()
    {
        if output.status.success() {
            return format!("git@github.com:{user}/{repo}.git");
        }
    }
    https
}

struct RepoSpec {
    /// Canonical repo source stored in .source for future updates.
    source: String,
    /// Original user-provided source, including any subdir suffix.
    requested_source: String,
    /// Clone URL passed to git.
    clone_url: String,
    /// Repository directory name after clone.
    repo_name: String,
    /// Optional subdirectory within the repo (for single-skill install).
    subdir: Option<String>,
    /// Whether this source is GitHub shorthand and supports raw HTTP fallback.
    github_shorthand: bool,
    /// GitHub owner for shorthand sources.
    github_user: Option<String>,
}

impl RepoSpec {
    fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim().trim_end_matches('/');

        if looks_like_git_url(trimmed) {
            return Ok(Self {
                source: trimmed.to_string(),
                requested_source: trimmed.to_string(),
                clone_url: trimmed.to_string(),
                repo_name: repo_name_from_git_url(trimmed)?,
                subdir: None,
                github_shorthand: false,
                github_user: None,
            });
        }

        let segments: Vec<&str> = trimmed
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        match segments.len() {
            2 => Ok(Self {
                source: format!("{}/{}", segments[0], segments[1]),
                requested_source: trimmed.to_string(),
                clone_url: github_clone_url(segments[0], segments[1]),
                repo_name: segments[1].to_string(),
                subdir: None,
                github_shorthand: true,
                github_user: Some(segments[0].to_string()),
            }),
            3.. => Ok(Self {
                source: format!("{}/{}", segments[0], segments[1]),
                requested_source: trimmed.to_string(),
                clone_url: github_clone_url(segments[0], segments[1]),
                repo_name: segments[1].to_string(),
                subdir: Some(segments[2..].join("/")),
                github_shorthand: true,
                github_user: Some(segments[0].to_string()),
            }),
            _ => eyre::bail!(
                "Invalid skill source: '{input}'. Expected user/repo, user/repo/skill-name, full Git URL, or local path"
            ),
        }
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
    fetch_registry_from(None)
}

fn fetch_registry_from(registry_url: Option<&str>) -> Result<Vec<RegistryEntry>> {
    let url = resolve_registry_url(registry_url);
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .wrap_err("failed to create HTTP client")?
        .get(&url)
        .send()
        .wrap_err_with(|| format!("failed to fetch registry from {url}"))?
        .error_for_status()
        .wrap_err("registry request failed")?
        .json()
        .wrap_err("failed to parse registry JSON")
}

fn filter_registry_entries<'a>(
    entries: &'a [RegistryEntry],
    query: Option<&str>,
) -> Vec<&'a RegistryEntry> {
    let query_lower = query
        .map(|q| q.trim().to_lowercase())
        .filter(|q| !q.is_empty());
    entries
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
        .collect()
}

/// Search available skill packages from the registry.
#[cfg(feature = "api")]
pub fn search_registry(
    query: Option<&str>,
    registry_url: Option<&str>,
) -> Result<Vec<RegistryPackage>> {
    let entries = fetch_registry_from(registry_url)?;
    Ok(filter_registry_entries(&entries, query)
        .into_iter()
        .cloned()
        .map(RegistryPackage::from)
        .collect())
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
            println!("    - {name}: {err}");
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
    let clone_dir = tmp.path().join(&spec.repo_name);

    println!(
        "Cloning {} (branch: {})...",
        spec.clone_url.dimmed(),
        branch.cyan()
    );

    let status = std::process::Command::new("git")
        .args(["clone", "--depth", "1", "--branch", branch])
        .arg(&spec.clone_url)
        .arg(&clone_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|_| eyre::eyre!("git not found. Please install git."))?;

    if !status.success() {
        eyre::bail!(
            "git clone failed. Check the source: {}",
            spec.requested_source
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
                "Subdirectory '{subdir}' not found in {}",
                spec.requested_source
            );
        }

        let name = Path::new(subdir)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        // RFC-2 (codex follow-up, 2026-05-25): pre-flight validate
        // the target subdir AND every auto-detected shared dep that
        // we'll actually copy BEFORE we touch the user-visible
        // skills dir. Skip-only paths (already installed, no
        // `--force`) are NOT validated — a user with a working
        // skill already in place must not be blocked just because
        // the cloned repo's copy of that skill/dep has a malformed
        // manifest.
        let dest = skills_dir.join(&name);
        let will_copy_target = !dest.exists() || force;
        if will_copy_target {
            validate_skill_manifest(&src)?;
        }
        let shared_deps: Vec<String> = {
            let skill_md_path = src.join("SKILL.md");
            if skill_md_path.exists() {
                let content = std::fs::read_to_string(&skill_md_path)?;
                find_shared_deps(&content, &clone_dir, &name)
            } else {
                Vec::new()
            }
        };
        // Validate only the deps the copy loop will actually touch.
        for dep in &shared_deps {
            let dep_dest = skills_dir.join(dep);
            let will_copy = !dep_dest.exists() || force;
            if will_copy {
                validate_skill_manifest(&clone_dir.join(dep))?;
            }
        }

        // Install the target skill/dir
        if !will_copy_target {
            println!(
                "  {} '{}' already exists (use --force to overwrite)",
                "SKIP".yellow(),
                name
            );
            skipped.push(name.clone());
        } else {
            // Remote proof/build happens in the temp clone before touching
            // any existing live install.
            maybe_install_binary(&src, false)?;
            clear_for_installed_tree(&dest)?;
            copy_dir_recursive(&src, &dest)?;
            if src.join("SKILL.md").exists() {
                installed.push(name.clone());
            } else {
                deps_installed.push(name.clone());
            }
        }

        // Now copy every shared dep we already validated above.
        for dep in shared_deps {
            let dep_src = clone_dir.join(&dep);
            let dep_dest = skills_dir.join(&dep);
            if dep_dest.exists() && !force {
                println!(
                    "  {} dependency '{}' already exists (use --force to overwrite)",
                    "SKIP".yellow(),
                    dep
                );
            } else {
                maybe_install_binary(&dep_src, false)?;
                clear_for_installed_tree(&dep_dest)?;
                copy_dir_recursive(&dep_src, &dep_dest)?;
                deps_installed.push(dep);
            }
        }
    } else {
        // Whole-repo install: check if root is a single skill or multi-skill
        if clone_dir.join("SKILL.md").exists() {
            // Single-skill repo: install as repo_name/
            let dest = skills_dir.join(&spec.repo_name);
            let will_copy = !dest.exists() || force;
            // RFC-2: validate only when we'll actually copy — a
            // skip path preserves the user's existing install.
            if will_copy {
                validate_skill_manifest(&clone_dir)?;
            }
            if !will_copy {
                println!(
                    "  {} '{}' already exists (use --force to overwrite)",
                    "SKIP".yellow(),
                    spec.repo_name
                );
                skipped.push(spec.repo_name.clone());
            } else {
                maybe_install_binary(&clone_dir, false)?;
                clear_for_installed_tree(&dest)?;
                copy_dir_recursive(&clone_dir, &dest)?;
                installed.push(spec.repo_name.clone());
            }
        } else {
            // Multi-skill repo: pre-flight validate every subdir we
            // intend to copy before touching the skills dir. Skip-
            // only subdirs are NOT validated so a malformed sibling
            // in the cloned repo doesn't block users whose existing
            // installs are unrelated.
            let mut multi_copies: Vec<(PathBuf, String)> = Vec::new();
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
                let will_copy = !dest.exists() || force;
                if will_copy {
                    validate_skill_manifest(&entry.path())?;
                    multi_copies.push((entry.path(), name));
                } else {
                    println!(
                        "  {} '{}' already exists (use --force to overwrite)",
                        "SKIP".yellow(),
                        name
                    );
                    skipped.push(name);
                }
            }
            // Now actually copy the validated subdirs.
            for (src_path, name) in multi_copies {
                maybe_install_binary(&src_path, false)?;
                let dest = skills_dir.join(&name);
                clear_for_installed_tree(&dest)?;
                copy_dir_recursive(&src_path, &dest)?;

                if src_path.join("SKILL.md").exists() {
                    installed.push(name);
                } else {
                    deps_installed.push(name);
                }
            }
        }
    }

    // Run npm install only after the existing explicit environment opt-in;
    // package postinstall scripts are arbitrary third-party code.
    for name in installed.iter().chain(deps_installed.iter()) {
        let dir = skills_dir.join(name);
        let opted_in = std::env::var("OCTOS_SKILLS_NPM_INSTALL").as_deref() == Ok("1");
        maybe_npm_install(&dir, opted_in)?;
    }

    // Write .source tracking file for each installed skill
    for name in installed.iter().chain(deps_installed.iter()) {
        let dest = skills_dir.join(name);
        write_source_info(&dest, spec, branch)?;
    }

    // Run hardware lifecycle phases for each installed skill that has a
    // manifest.json. Skills without manifest.json (SKILL.md-only, shared dep
    // dirs) are silently skipped inside `run_activate_lifecycle`.
    for name in installed.iter().chain(deps_installed.iter()) {
        let dest = skills_dir.join(name);
        run_activate_lifecycle(&dest, name, false)?;
    }

    Ok(InstallResult {
        installed,
        skipped,
        deps_installed,
    })
}

/// HTTP fallback: fetch a single SKILL.md (original behavior).
fn install_via_http(skills_dir: &Path, spec: &RepoSpec, force: bool, branch: &str) -> Result<()> {
    let subdir = spec.subdir.as_deref().unwrap_or(&spec.repo_name);
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
        github_user(spec)?,
        spec.repo_name
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
            "Failed to fetch SKILL.md (HTTP {}). Check the source: {}",
            body.status(),
            spec.requested_source
        );
    }

    let content = body.text().wrap_err("failed to read response body")?;

    clear_for_installed_tree(&dest)?;
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
        repo: spec.source.clone(),
        subdir: spec.subdir.clone(),
        branch: branch.to_string(),
        installed_at: chrono::Utc::now().to_rfc3339(),
    };
    std::fs::write(dest.join(".source"), serde_json::to_string_pretty(&info)?)?;
    Ok(())
}

fn github_user(spec: &RepoSpec) -> Result<&str> {
    spec.github_user
        .as_deref()
        .ok_or_else(|| eyre::eyre!("raw HTTP fallback only supports GitHub shorthand sources"))
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

    if let Ok(Some(dev)) = source_runtime::read_dev_marker(&skill_dir) {
        println!();
        println!(
            "  {} dev skill, runs from source through the Relax JIT",
            "Runtime:".dimmed()
        );
        println!("  {}  {}", "Source:".dimmed(), dev.source.display());
        println!("  {} {}", "Package:".dimmed(), dev.package);
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

    // UPCR-2026-R48CB467B: a reviewed install is pinned to a verified object.
    // It carries no `.source` branch pointer, and `update` must never
    // silently follow a branch tip — a new object requires a fresh review.
    if skill_dir.join(REVIEWED_INSTALL_RECEIPT_FILE).exists() {
        eyre::bail!(
            "'{name}' is a pinned reviewed install; updating it requires a fresh review and \
             reinstall (profile/skills/install_reviewed), never a branch follow"
        );
    }

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
pub(crate) fn platform_key() -> String {
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
struct RegistryBinarySet {
    source_commit: String,
    reviewed_at: String,
    binaries: std::collections::HashMap<String, BinaryInfo>,
}

fn lookup_registry_binaries(
    package_name: &str,
    registry_url: Option<&str>,
) -> Result<Option<RegistryBinarySet>> {
    let url = resolve_registry_url(registry_url);
    // Transport absence keeps the existing local/source-development path;
    // a registry response that did arrive but is corrupt/stale must fail.
    let response = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
    {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    let entries: Vec<RegistryEntry> = response
        .json()
        .wrap_err("registry is corrupt or has an invalid schema")?;

    let Some(entry) = entries
        .into_iter()
        .find(|e| e.name == package_name || e.skills.contains(&package_name.to_string()))
    else {
        return Ok(None);
    };
    if entry.binaries.is_empty() {
        return Ok(None);
    }
    let (Some(source_commit), Some(reviewed_at)) = (entry.source_commit, entry.reviewed_at) else {
        eyre::bail!("registry binary entry has incomplete review provenance");
    };
    octos_plugin::integrity::validate_registry_binary_provenance(
        Some(&source_commit),
        Some(&reviewed_at),
    )?;
    Ok(Some(RegistryBinarySet {
        source_commit,
        reviewed_at,
        binaries: entry.binaries,
    }))
}

/// Verify downloaded remote bytes before any file is written. The public key
/// comes from the operator, never from the registry response itself.
fn verify_downloaded_binary(
    dir: &Path,
    bytes: &[u8],
    sha256: Option<&str>,
    signature: Option<&str>,
) -> Result<()> {
    verify_downloaded_binary_with_trust(
        dir,
        bytes,
        sha256,
        signature,
        std::env::var(octos_plugin::integrity::HUB_TRUST_PUBLIC_KEY_ENV)
            .ok()
            .as_deref(),
    )
}

fn verify_downloaded_binary_with_trust(
    dir: &Path,
    bytes: &[u8],
    sha256: Option<&str>,
    signature: Option<&str>,
    trusted_public_key: Option<&str>,
) -> Result<()> {
    let package = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| eyre::eyre!("skill install directory has no valid package name"))?;
    octos_plugin::integrity::verify_remote_binary(
        bytes,
        octos_plugin::integrity::RemoteBinaryProof {
            sha256,
            signature,
            package,
            platform: &platform_key(),
            source_commit: None,
            reviewed_at: None,
            trusted_public_key,
        },
    )
}

/// Download a pre-built binary from registry binary info.
/// Returns true only after a trusted signature and matching digest verify.
fn download_binary(dir: &Path, registry: &RegistryBinarySet) -> Result<bool> {
    let key = platform_key();
    let info = match registry.binaries.get(&key) {
        Some(info) => info,
        None => {
            println!(
                "  {} No pre-built binary for {} (available: {})",
                "WARN".yellow(),
                key,
                registry
                    .binaries
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Ok(false);
        }
    };

    if info.url.trim().is_empty() {
        eyre::bail!("registry binary entry has an empty URL");
    }
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

    // R-24: cap the download — the 120s timeout bounds time, not bytes.
    let bytes = octos_core::bounded_read::read_to_end_capped(
        response,
        octos_core::bounded_read::SKILL_DOWNLOAD_CAP_BYTES,
    )
    .wrap_err_with(|| {
        format!(
            "failed to read binary response (exceeded {} byte cap)",
            octos_core::bounded_read::SKILL_DOWNLOAD_CAP_BYTES
        )
    })?;

    let package = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| eyre::eyre!("skill install directory has no valid package name"))?;
    octos_plugin::integrity::verify_remote_binary(
        &bytes,
        octos_plugin::integrity::RemoteBinaryProof {
            sha256: info.sha256.as_deref(),
            signature: info.signature.as_deref(),
            package,
            platform: &key,
            source_commit: Some(&registry.source_commit),
            reviewed_at: Some(&registry.reviewed_at),
            trusted_public_key: std::env::var(octos_plugin::integrity::HUB_TRUST_PUBLIC_KEY_ENV)
                .ok()
                .as_deref(),
        },
    )?;
    println!("  {} Signature and hash verified", "OK".green());

    let dest = dir.join("main");

    if info.url.ends_with(".tar.gz") || info.url.ends_with(".tgz") {
        let gz = flate2::read::GzDecoder::new(&bytes[..]);
        let mut archive = tar::Archive::new(gz);
        let mut found = false;
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.header().entry_type().is_file() {
                // R-24: cap the decompressed member before writing anything.
                let buf = octos_core::bounded_read::read_to_end_capped(
                    &mut entry,
                    octos_core::bounded_read::ARCHIVE_ENTRY_CAP_BYTES,
                )
                .wrap_err("archive member exceeded the extraction byte cap")?;
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

/// Download a binary from a direct URL with mandatory signed SHA-256 proof.
/// Supports both raw binaries and `.tar.gz` archives (auto-detected from URL).
fn download_binary_from_url(
    dir: &Path,
    url: &str,
    sha256: Option<&str>,
    signature: Option<&str>,
) -> Result<bool> {
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

    // R-24: cap the download — the 120s timeout bounds time, not bytes.
    let bytes = octos_core::bounded_read::read_to_end_capped(
        response,
        octos_core::bounded_read::SKILL_DOWNLOAD_CAP_BYTES,
    )
    .wrap_err_with(|| {
        format!(
            "failed to read binary response (exceeded {} byte cap)",
            octos_core::bounded_read::SKILL_DOWNLOAD_CAP_BYTES
        )
    })?;

    verify_downloaded_binary(dir, &bytes, sha256, signature)?;
    println!("  {} Signature and hash verified", "OK".green());

    let dest = dir.join("main");

    if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        let gz = flate2::read::GzDecoder::new(&bytes[..]);
        let mut archive = tar::Archive::new(gz);
        let mut found = false;
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.header().entry_type().is_file() {
                // R-24: cap the decompressed member before writing anything.
                let buf = octos_core::bounded_read::read_to_end_capped(
                    &mut entry,
                    octos_core::bounded_read::ARCHIVE_ENTRY_CAP_BYTES,
                )
                .wrap_err("archive member exceeded the extraction byte cap")?;
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
fn maybe_install_binary(dir: &Path, trusted_local: bool) -> Result<()> {
    let has_manifest = dir.join("manifest.json").exists();
    let has_cargo = dir.join("Cargo.toml").exists();

    if !has_manifest && !has_cargo {
        return Ok(());
    }

    let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
    let bundled = if dir.join(&dir_name).exists() {
        Some(dir.join(&dir_name))
    } else {
        let main = dir.join("main");
        (main.exists() && !is_generated_lazy_cargo_wrapper(&main)).then_some(main)
    };
    if trusted_local && bundled.is_some() {
        // Explicit local development returns before manifest/registry policy,
        // exactly as the old existing-executable path did.
        return Ok(());
    }
    let manifest = if has_manifest {
        let content = std::fs::read_to_string(dir.join("manifest.json"))
            .wrap_err("failed to read skill manifest")?;
        Some(
            serde_json::from_str::<octos_agent::plugins::manifest::PluginManifest>(&content)
                .wrap_err("skill manifest or binary proof metadata is malformed")?,
        )
    } else {
        None
    };
    if let Some(executable) = bundled {
        let manifest = manifest
            .as_ref()
            .ok_or_else(|| eyre::eyre!("remote repository executable has no manifest proof"))?;
        verify_bundled_remote_binary(dir, &executable, manifest)?;
        return Ok(());
    }

    let key = platform_key();

    // Try 1: download from manifest.json binaries (skill repo's own CI/CD)
    if let Some(manifest) = &manifest {
        if let Some(info) = manifest.binaries.get(&key) {
            if info.url.trim().is_empty() {
                eyre::bail!("manifest binary entry has an empty URL");
            }
            println!("  Downloading binary for {} from manifest...", key.cyan());
            if download_binary_from_url(
                dir,
                &info.url,
                info.sha256.as_deref(),
                info.signature.as_deref(),
            )? {
                // Log installation
                install_main_to_cargo_bin(dir, &manifest.name);
                return Ok(());
            }
        }
    }

    // Try 2: registry policy belongs to remote installs only; an explicitly
    // selected local source path must not depend on a same-named hub entry.
    if !trusted_local {
        if let Some(registry) = lookup_registry_binaries(&dir_name, None)? {
            if download_binary(dir, &registry)? {
                install_main_to_cargo_bin(dir, &dir_name);
                return Ok(());
            }
        }
    }

    // Try 3: cargo build if Cargo.toml exists. Local development keeps its
    // explicit opt-in; remote source builds additionally require the existing
    // OCTOS_SKILLS_CARGO_BUILD=1 environment gate (build.rs is arbitrary code).
    if !has_cargo {
        return Ok(());
    }
    let opted_in = trusted_local || std::env::var("OCTOS_SKILLS_CARGO_BUILD").as_deref() == Ok("1");
    maybe_cargo_build_with(dir, opted_in)
}

/// Run the source-build fallback only after the source/trust gate. Keeping
/// this separate from registry/binary download makes denial directly testable.
fn maybe_cargo_build_with(dir: &Path, opted_in: bool) -> Result<()> {
    if !opted_in {
        tracing::info!(
            "skill ships Cargo.toml but OCTOS_SKILLS_CARGO_BUILD != 1; skipping cargo build"
        );
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
        dir.file_name().unwrap().to_string_lossy()
    );
    let status = std::process::Command::new("cargo")
        .args(["build", "--release"])
        // Skill builds are self-contained even when the operator's shell
        // exports a shared CARGO_TARGET_DIR; the copy step below reads this
        // exact per-skill target directory.
        .env("CARGO_TARGET_DIR", dir.join("target"))
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
                    let bin_name = if cfg!(windows) {
                        format!("{name}.exe")
                    } else {
                        name.to_string()
                    };
                    let bin_path = target_dir.join(&bin_name);
                    if bin_path.exists() {
                        std::fs::copy(
                            &bin_path,
                            dir.join(if cfg!(windows) { "main.exe" } else { "main" }),
                        )?;
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

/// Verify a repository-supplied executable while it is still in the
/// temporary clone. Local trusted paths never call this gate.
fn verify_bundled_remote_binary(
    dir: &Path,
    executable: &Path,
    manifest: &octos_agent::plugins::manifest::PluginManifest,
) -> Result<()> {
    let bytes = octos_core::bounded_read::read_to_end_capped(
        std::fs::File::open(executable).wrap_err("failed to open bundled remote executable")?,
        octos_core::bounded_read::SKILL_DOWNLOAD_CAP_BYTES,
    )
    .wrap_err("bundled remote executable exceeded the size cap")?;
    let package = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| eyre::eyre!("skill install directory has no valid package name"))?;
    octos_plugin::integrity::verify_remote_binary(
        &bytes,
        octos_plugin::integrity::RemoteBinaryProof {
            sha256: manifest.sha256.as_deref(),
            signature: manifest.signature.as_deref(),
            package,
            platform: &platform_key(),
            source_commit: None,
            reviewed_at: None,
            trusted_public_key: std::env::var(octos_plugin::integrity::HUB_TRUST_PUBLIC_KEY_ENV)
                .ok()
                .as_deref(),
        },
    )
}

fn is_generated_lazy_cargo_wrapper(path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };

    contents.contains("Skill binary is missing and cargo is not installed")
        && contents.contains("cargo build --release")
        && contents.contains("target/release/")
}

/// Recursively copy a directory, skipping .git, node_modules, and target (Rust build).
/// Clear the place an installed tree is about to take. What stood there
/// goes, and so does a dev registration of that name: the tree that lands is
/// an installed skill, whatever marker it carries.
fn clear_for_installed_tree(dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    source_runtime::forget_dev_skill(dest);
    Ok(())
}

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
fn maybe_npm_install(dir: &Path, opted_in: bool) -> Result<()> {
    if !dir.join("package.json").exists() {
        return Ok(());
    }
    if dir.join("node_modules").exists() {
        return Ok(());
    }
    if !opted_in {
        tracing::info!(
            "skill ships package.json but OCTOS_SKILLS_NPM_INSTALL != 1; skipping npm install"
        );
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

/// Run `activate_skill` lifecycle phases for a freshly-installed skill directory.
///
/// Only fires when `manifest.json` is present (i.e. skills installed via git or
/// local path, not the HTTP-only SKILL.md-only fallback). On failure the skill
/// directory is removed so re-install starts clean.
fn run_activate_lifecycle(skill_dir: &Path, name: &str, trusted_local: bool) -> Result<()> {
    // Skip if no manifest.json — HTTP-only installs have only SKILL.md.
    if !skill_dir.join("manifest.json").exists() {
        return Ok(());
    }

    use eyre::eyre;
    use octos_agent::plugins::activate_skill;
    use octos_agent::tools::ToolRegistry;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| eyre!("failed to build runtime for skill activation: {e}"))?;

    let mut registry = ToolRegistry::new();
    let lifecycle_allowed =
        trusted_local || std::env::var("OCTOS_SKILLS_LIFECYCLE").as_deref() == Ok("1");
    let activation = runtime.block_on(activate_skill(
        &mut registry,
        skill_dir,
        &[],
        lifecycle_allowed,
    ));

    if let Err(e) = activation {
        // Best-effort cleanup: remove the partially-installed skill dir so
        // re-install starts from a clean state.
        let _ = std::fs::remove_dir_all(skill_dir);
        return Err(eyre!("skill '{name}' activation failed: {e}"));
    }

    // The `registry` is intentionally discarded here — we call `activate_skill`
    // for its side effects (lifecycle phases start the dataflow). At runtime,
    // agent startup's PluginLoader scan re-registers tools via the same path.
    Ok(())
}

fn cmd_remove(skills_dir: &Path, name: &str) -> Result<()> {
    remove_skill(skills_dir, name)?;
    println!("{} Removed skill '{}'", "OK".green(), name.cyan());
    Ok(())
}

// ── Reviewed install (T-0556, UPCR-2026-R48CB467B) ─────────────────────────
//
// AppUI `profile/skills/install_reviewed`: install exactly the object the
// reviewer saw — the full source commit plus, for binary-bearing packages,
// the SHA-256 of the downloaded artifact — and report that actual object in
// the receipt. A source URL, branch, display version or description is never
// accepted as identity. Every re-check runs BEFORE any disk publish, and the
// user-visible skills directory is only ever touched by the final atomic
// rename of a fully verified staging tree. Unlike the legacy install path,
// this flow runs no npm/cargo builds and no lifecycle hooks: the reviewed
// object is the pinned tree plus the signed binary, and build products are
// by definition not that object.

/// Name of the receipt file written into a reviewed install's skill
/// directory; `list_skills` re-reads it so post-install list/read report the
/// actual source commit and artifact digest.
pub const REVIEWED_INSTALL_RECEIPT_FILE: &str = source_runtime::REVIEWED_INSTALL_RECEIPT_FILE;
#[cfg(feature = "api")]
const REVIEWED_INSTALL_RECEIPT_SCHEMA: &str = "octos.reviewed_skill_install.v1";

/// Durable record of the actual object a reviewed install published.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewedInstallReceipt {
    pub schema: String,
    pub package: String,
    pub repo: String,
    pub source_commit: String,
    pub reviewed_at: String,
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_bytes: Option<u64>,
    pub installed_at: String,
}

/// The exact artifact the reviewer approved for this platform. Both fields
/// come from the review projection; the install re-checks them against the
/// current registry entry and then against the downloaded bytes.
#[cfg(feature = "api")]
#[derive(Debug, Clone)]
pub struct ReviewedArtifactExpectation {
    pub sha256: String,
    pub signature: String,
}

/// A review-bound install request. Every field is an EXPECTED value the
/// caller reviewed; the install fails closed on any mismatch instead of
/// substituting another object.
#[cfg(feature = "api")]
#[derive(Debug, Clone)]
pub struct ReviewedInstallRequest {
    pub package: String,
    pub repo: String,
    pub source_commit: String,
    pub reviewed_at: String,
    pub platform: String,
    pub artifact: Option<ReviewedArtifactExpectation>,
    pub force: bool,
}

/// What a reviewed install published. `backup_dir` holds the replaced
/// install (rename-preserved) until the caller confirms the post-publish
/// steps succeeded via [`commit_reviewed_install_backup`] or abandons them
/// via [`rollback_reviewed_install`].
#[cfg(feature = "api")]
#[derive(Debug)]
pub struct ReviewedInstallOutcome {
    pub installed: Vec<String>,
    pub receipt: ReviewedInstallReceipt,
    pub backup_dir: Option<PathBuf>,
}

/// Typed failure for the reviewed-install flow. `kind` is stable for wire
/// mapping (`data.kind`); the message carries the human detail.
#[cfg(feature = "api")]
#[derive(Debug)]
pub struct ReviewedInstallError {
    pub kind: &'static str,
    pub message: String,
}

#[cfg(feature = "api")]
impl std::fmt::Display for ReviewedInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[cfg(feature = "api")]
impl std::error::Error for ReviewedInstallError {}

#[cfg(feature = "api")]
fn reviewed_err(kind: &'static str, message: impl Into<String>) -> eyre::Report {
    eyre::Report::new(ReviewedInstallError {
        kind,
        message: message.into(),
    })
}

#[cfg(feature = "api")]
fn is_hex_string(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Name rules shared by the registry package and the installed skill
/// directory: a single safe path component, so neither can traverse out of
/// the profile skills directory.
#[cfg(feature = "api")]
fn validate_reviewed_name(field: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && name != ".."
        && !name.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(reviewed_err(
            "reviewed_install_invalid_params",
            format!("{field} is not a safe single-component name: {name:?}"),
        ))
    }
}

/// Validate every expected-identity field BEFORE any network or disk work.
#[cfg(feature = "api")]
fn validate_reviewed_install_request(request: &ReviewedInstallRequest) -> Result<()> {
    validate_reviewed_name("package", &request.package)?;
    if request.repo.trim().is_empty() {
        return Err(reviewed_err(
            "reviewed_install_invalid_params",
            "repo is required",
        ));
    }
    if !is_hex_string(&request.source_commit, 40) {
        return Err(reviewed_err(
            "reviewed_install_invalid_params",
            "source_commit must be the full 40-hex reviewed commit",
        ));
    }
    if request.platform != platform_key() {
        return Err(reviewed_err(
            "reviewed_install_platform_unavailable",
            format!(
                "reviewed artifact platform '{}' does not match this host '{}'",
                request.platform,
                platform_key()
            ),
        ));
    }
    if let Some(artifact) = &request.artifact {
        if !is_hex_string(&artifact.sha256, 64) {
            return Err(reviewed_err(
                "reviewed_install_invalid_params",
                "artifact.sha256 must be the full 64-hex reviewed digest",
            ));
        }
        if artifact.signature.trim().is_empty() {
            return Err(reviewed_err(
                "reviewed_install_invalid_params",
                "artifact.signature is required",
            ));
        }
    }
    Ok(())
}

/// The registry entry the CURRENT registry still publishes for the reviewed
/// package, plus the artifact for this platform when the entry carries one.
#[cfg(feature = "api")]
struct ResolvedReviewedInstall {
    entry: RegistryEntry,
    binary: Option<BinaryInfo>,
}

/// Re-check the review expectations against the CURRENT registry. This is
/// the TOCTOU gate between review and install: a drifted branch, a swapped
/// registry entry, or a registry-side artifact change all fail closed here,
/// before any clone, download, or disk publish.
#[cfg(feature = "api")]
fn resolve_reviewed_install(
    entries: Vec<RegistryEntry>,
    request: &ReviewedInstallRequest,
) -> Result<ResolvedReviewedInstall> {
    let mismatch = |detail: String| reviewed_err("reviewed_install_registry_mismatch", detail);
    let entry = entries
        .into_iter()
        .find(|entry| entry.name == request.package)
        .ok_or_else(|| {
            mismatch(format!(
                "registry no longer lists the reviewed package '{}'",
                request.package
            ))
        })?;
    if entry.repo != request.repo {
        return Err(mismatch(format!(
            "registry repo '{}' no longer matches the reviewed repo '{}'",
            entry.repo, request.repo
        )));
    }
    if entry.source_commit.as_deref() != Some(request.source_commit.as_str()) {
        return Err(mismatch(format!(
            "registry source_commit {:?} no longer matches the reviewed commit {}",
            entry.source_commit, request.source_commit
        )));
    }
    if entry.reviewed_at.as_deref() != Some(request.reviewed_at.as_str()) {
        return Err(mismatch(
            "registry review timestamp no longer matches the reviewed entry".to_string(),
        ));
    }
    // Provenance freshness/shape is part of the signed semantics; a stale or
    // malformed review is a verification failure, not a warning.
    octos_plugin::integrity::validate_registry_binary_provenance(
        Some(&request.source_commit),
        Some(&request.reviewed_at),
    )
    .map_err(|error| {
        reviewed_err(
            "reviewed_install_verification_failed",
            format!("review provenance rejected: {error:#}"),
        )
    })?;
    let binary = match (&request.artifact, entry.binaries.get(&request.platform)) {
        (Some(expected), Some(info)) => {
            let registry_sha256 = info.sha256.as_deref().unwrap_or("").to_lowercase();
            if registry_sha256 != expected.sha256.to_lowercase()
                || info.signature.as_deref() != Some(expected.signature.as_str())
            {
                return Err(mismatch(
                    "registry artifact digest/signature no longer matches the review".to_string(),
                ));
            }
            if info.url.trim().is_empty() {
                return Err(mismatch(
                    "registry artifact URL is empty for the reviewed entry".to_string(),
                ));
            }
            Some(info.clone())
        }
        (Some(_), None) => {
            return Err(reviewed_err(
                "reviewed_install_platform_unavailable",
                format!(
                    "registry has no artifact for platform '{}' anymore",
                    request.platform
                ),
            ));
        }
        (None, _) if !entry.binaries.is_empty() => {
            return Err(mismatch(format!(
                "registry entry for '{}' carries binaries the review did not cover; \
                 re-review with the artifact digest and signature",
                request.package
            )));
        }
        (None, _) => None,
    };
    Ok(ResolvedReviewedInstall { entry, binary })
}

/// Check out exactly the reviewed commit into `dest`. The branch is never
/// resolved: git fetches the commit object directly (falling back to a full
/// fetch for servers that disallow sha fetch), and the final `rev-parse`
/// must equal the reviewed commit or the install fails.
#[cfg(feature = "api")]
fn checkout_source_at_commit(repo: &str, commit: &str, dest: &Path) -> Result<()> {
    let spec = RepoSpec::parse(repo).map_err(|error| {
        reviewed_err(
            "reviewed_install_invalid_params",
            format!("repo is not installable: {error:#}"),
        )
    })?;
    let run = |args: &[&str]| -> Result<std::process::Output> {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dest)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|error| {
                reviewed_err(
                    "reviewed_install_fetch_failed",
                    format!("git not available: {error}"),
                )
            })
    };
    std::fs::create_dir_all(dest).map_err(|error| {
        reviewed_err(
            "reviewed_install_fetch_failed",
            format!("failed to create the clone directory: {error}"),
        )
    })?;
    let init = run(&["init", "-q"])?;
    if !init.status.success() {
        return Err(reviewed_err(
            "reviewed_install_fetch_failed",
            format!("git init failed: {}", String::from_utf8_lossy(&init.stderr)),
        ));
    }
    let remote = run(&["remote", "add", "origin", &spec.clone_url])?;
    if !remote.status.success() {
        return Err(reviewed_err(
            "reviewed_install_fetch_failed",
            format!(
                "git remote add failed: {}",
                String::from_utf8_lossy(&remote.stderr)
            ),
        ));
    }
    // Preferred path: fetch exactly the reviewed object. The full-fetch
    // fallback is NOT a different object — the rev-parse gate below still
    // pins HEAD to the reviewed commit.
    if !run(&["fetch", "-q", "--depth", "1", "origin", commit])?
        .status
        .success()
        && !run(&["fetch", "-q", "origin"])?.status.success()
    {
        return Err(reviewed_err(
            "reviewed_install_fetch_failed",
            format!(
                "the reviewed commit {commit} is not fetchable from {}",
                spec.clone_url
            ),
        ));
    }
    let checkout = run(&["checkout", "-q", "--detach", commit])?;
    if !checkout.status.success() {
        return Err(reviewed_err(
            "reviewed_install_fetch_failed",
            format!(
                "failed to check out the reviewed commit {commit}: {}",
                String::from_utf8_lossy(&checkout.stderr)
            ),
        ));
    }
    let head = run(&["rev-parse", "HEAD"])?;
    let head = String::from_utf8_lossy(&head.stdout).trim().to_lowercase();
    if !head.is_empty() && head == commit.to_lowercase() {
        Ok(())
    } else {
        Err(reviewed_err(
            "reviewed_install_verification_failed",
            format!("checked-out HEAD {head} is not the reviewed commit {commit}"),
        ))
    }
}

/// Copy the pinned tree into staging. Symlinks and special files are
/// rejected outright: a reviewed skill is plain files and directories, and
/// refusing symlinked entries closes the escape channel by construction
/// (entry names from `read_dir` can never be `.`/`..`, so no component can
/// traverse out of the staging directory).
#[cfg(feature = "api")]
fn copy_reviewed_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|error| {
        reviewed_err(
            "reviewed_install_publish_failed",
            format!("failed to create the staging directory: {error}"),
        )
    })?;
    for entry in std::fs::read_dir(src).map_err(|error| {
        reviewed_err(
            "reviewed_install_fetch_failed",
            format!("failed to read the reviewed tree: {error}"),
        )
    })? {
        let entry = entry.map_err(|error| {
            reviewed_err(
                "reviewed_install_fetch_failed",
                format!("failed to read the reviewed tree: {error}"),
            )
        })?;
        let name = entry.file_name();
        if name == ".git" || name == "node_modules" || name == "target" {
            continue;
        }
        // `file_type` does not follow symlinks: a symlink is caught here
        // before any read of its target.
        let file_type = entry.file_type().map_err(|error| {
            reviewed_err(
                "reviewed_install_fetch_failed",
                format!("failed to stat a reviewed tree entry: {error}"),
            )
        })?;
        if file_type.is_symlink() || (!file_type.is_dir() && !file_type.is_file()) {
            return Err(reviewed_err(
                "reviewed_install_unsafe_tree",
                format!(
                    "the reviewed tree contains a non-regular entry {:?}; refusing to publish it",
                    name
                ),
            ));
        }
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        if file_type.is_dir() {
            copy_reviewed_tree(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|error| {
                reviewed_err(
                    "reviewed_install_publish_failed",
                    format!("failed to stage {}: {error}", src_path.display()),
                )
            })?;
        }
    }
    Ok(())
}

/// Fetch the artifact bytes with the shared download cap. `file://` URLs
/// read from the local filesystem (mirrors and tests); every other URL goes
/// through the same capped HTTP path as the legacy installer. A non-success
/// response is an error here — the reviewed flow has no "skip the binary"
/// fallback.
#[cfg(feature = "api")]
fn download_reviewed_artifact(url: &str) -> Result<Vec<u8>> {
    let bytes = if let Some(path) = url.strip_prefix("file://") {
        let file = std::fs::File::open(path).map_err(|error| {
            reviewed_err(
                "reviewed_install_fetch_failed",
                format!("failed to open the reviewed artifact {url}: {error}"),
            )
        })?;
        octos_core::bounded_read::read_to_end_capped(
            file,
            octos_core::bounded_read::SKILL_DOWNLOAD_CAP_BYTES,
        )
        .map_err(|error| {
            reviewed_err(
                "reviewed_install_fetch_failed",
                format!("failed to read the reviewed artifact within the byte cap: {error}"),
            )
        })?
    } else {
        let response = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .and_then(|client| client.get(url).send())
            .map_err(|error| {
                reviewed_err(
                    "reviewed_install_fetch_failed",
                    format!("failed to download the reviewed artifact from {url}: {error}"),
                )
            })?;
        if !response.status().is_success() {
            return Err(reviewed_err(
                "reviewed_install_fetch_failed",
                format!(
                    "the reviewed artifact download failed with HTTP {}",
                    response.status()
                ),
            ));
        }
        octos_core::bounded_read::read_to_end_capped(
            response,
            octos_core::bounded_read::SKILL_DOWNLOAD_CAP_BYTES,
        )
        .map_err(|error| {
            reviewed_err(
                "reviewed_install_fetch_failed",
                format!("the reviewed artifact exceeded the download byte cap: {error}"),
            )
        })?
    };
    Ok(bytes)
}

/// Extract the executable payload from the VERIFIED bytes. Archives are
/// read in memory with the shared per-member cap; member names are never
/// used as paths — the first regular file becomes the skill's `main` —
/// so archive path traversal has no target.
#[cfg(feature = "api")]
fn extract_reviewed_binary(url: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    if !(url.ends_with(".tar.gz") || url.ends_with(".tgz")) {
        return Ok(bytes.to_vec());
    }
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    let entries = archive.entries().map_err(|error| {
        reviewed_err(
            "reviewed_install_fetch_failed",
            format!("the reviewed archive is unreadable: {error}"),
        )
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|error| {
            reviewed_err(
                "reviewed_install_fetch_failed",
                format!("the reviewed archive is corrupt: {error}"),
            )
        })?;
        if entry.header().entry_type().is_file() {
            return octos_core::bounded_read::read_to_end_capped(
                &mut entry,
                octos_core::bounded_read::ARCHIVE_ENTRY_CAP_BYTES,
            )
            .map_err(|error| {
                reviewed_err(
                    "reviewed_install_fetch_failed",
                    format!("an archive member exceeded the extraction byte cap: {error}"),
                )
            });
        }
    }
    Err(reviewed_err(
        "reviewed_install_fetch_failed",
        "the reviewed archive contains no regular file",
    ))
}

/// Core of AppUI `profile/skills/install_reviewed` with the registry
/// projection injected (the handler fetches it; tests supply fixtures).
/// Verification and staging complete before the user-visible skills
/// directory is touched; publish is a rename, with the replaced install
/// preserved at `backup_dir` until the caller confirms or rolls back.
#[cfg(feature = "api")]
pub(crate) fn install_reviewed_skill_with_registry(
    skills_dir: &Path,
    request: &ReviewedInstallRequest,
    registry: Vec<RegistryEntry>,
    trusted_public_key: Option<&str>,
) -> Result<ReviewedInstallOutcome> {
    validate_reviewed_install_request(request)?;
    let resolved = resolve_reviewed_install(registry, request)?;
    let skill_name = resolved
        .entry
        .skills
        .first()
        .cloned()
        .unwrap_or_else(|| resolved.entry.name.clone());
    validate_reviewed_name("skills[0]", &skill_name)?;
    if resolved.entry.skills.len() > 1 {
        return Err(reviewed_err(
            "reviewed_install_unsupported_layout",
            "reviewed install v1 supports single-skill packages only",
        ));
    }

    std::fs::create_dir_all(skills_dir).map_err(|error| {
        reviewed_err(
            "reviewed_install_publish_failed",
            format!("failed to create the skills directory: {error}"),
        )
    })?;
    let dest = skills_dir.join(&skill_name);
    if dest.exists() && !request.force {
        return Err(reviewed_err(
            "reviewed_install_already_installed",
            format!(
                "skill '{skill_name}' is already installed; pass force to replace it with the reviewed object"
            ),
        ));
    }

    // Stage in the SAME directory so the final publish is a same-filesystem
    // rename. The dot prefix keeps the in-flight tree out of listings.
    let staging = tempfile::Builder::new()
        .prefix(".reviewed-staging-")
        .tempdir_in(skills_dir)
        .map_err(|error| {
            reviewed_err(
                "reviewed_install_publish_failed",
                format!("failed to create the staging directory: {error}"),
            )
        })?;
    let clone_dir = staging.path().join("clone");
    checkout_source_at_commit(&resolved.entry.repo, &request.source_commit, &clone_dir)?;
    // Locate the reviewed skill inside the pinned tree: the repository root
    // when it IS the skill, else the single named subdirectory.
    let payload_src = if clone_dir.join("SKILL.md").is_file() {
        clone_dir.clone()
    } else {
        let subdir = clone_dir.join(&skill_name);
        if !subdir.join("SKILL.md").is_file() {
            return Err(reviewed_err(
                "reviewed_install_unsupported_layout",
                format!(
                    "the reviewed commit does not contain skill '{skill_name}' at the root or as a subdirectory"
                ),
            ));
        }
        subdir
    };
    let staged_skill = staging.path().join(&skill_name);
    copy_reviewed_tree(&payload_src, &staged_skill)?;
    // RFC-2 schema validation runs on the staged tree, before publish.
    validate_skill_manifest(&staged_skill).map_err(|error| {
        reviewed_err(
            "reviewed_install_unsupported_layout",
            format!("the reviewed skill manifest is invalid: {error:#}"),
        )
    })?;

    let mut artifact_sha256 = None;
    let mut artifact_bytes = None;
    if let (Some(binary), Some(expected)) = (&resolved.binary, &request.artifact) {
        let bytes = download_reviewed_artifact(&binary.url)?;
        // Report the ACTUAL digest of the downloaded object; verification
        // below proves it equals the reviewed expectation.
        let actual = format!("{:x}", {
            use sha2::Digest as _;
            sha2::Sha256::digest(&bytes)
        });
        octos_plugin::integrity::verify_remote_binary(
            &bytes,
            octos_plugin::integrity::RemoteBinaryProof {
                sha256: Some(&expected.sha256),
                signature: Some(&expected.signature),
                package: &request.package,
                platform: &request.platform,
                source_commit: Some(&request.source_commit),
                reviewed_at: Some(&request.reviewed_at),
                trusted_public_key,
            },
        )
        .map_err(|error| {
            reviewed_err(
                "reviewed_install_verification_failed",
                format!("the downloaded artifact failed verification: {error:#}"),
            )
        })?;
        let payload = extract_reviewed_binary(&binary.url, &bytes)?;
        std::fs::write(staged_skill.join("main"), &payload).map_err(|error| {
            reviewed_err(
                "reviewed_install_publish_failed",
                format!("failed to stage the reviewed binary: {error}"),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                staged_skill.join("main"),
                std::fs::Permissions::from_mode(0o755),
            )
            .map_err(|error| {
                reviewed_err(
                    "reviewed_install_publish_failed",
                    format!("failed to mark the reviewed binary executable: {error}"),
                )
            })?;
        }
        artifact_sha256 = Some(actual);
        artifact_bytes = Some(bytes.len() as u64);
    }

    let receipt = ReviewedInstallReceipt {
        schema: REVIEWED_INSTALL_RECEIPT_SCHEMA.to_string(),
        package: request.package.clone(),
        repo: resolved.entry.repo.clone(),
        source_commit: request.source_commit.to_lowercase(),
        reviewed_at: request.reviewed_at.clone(),
        platform: request.platform.clone(),
        artifact_sha256,
        artifact_bytes,
        installed_at: chrono::Utc::now().to_rfc3339(),
    };
    let receipt_json = serde_json::to_string_pretty(&receipt).map_err(|error| {
        reviewed_err(
            "reviewed_install_publish_failed",
            format!("failed to encode the install receipt: {error}"),
        )
    })?;
    std::fs::write(
        staged_skill.join(REVIEWED_INSTALL_RECEIPT_FILE),
        receipt_json,
    )
    .map_err(|error| {
        reviewed_err(
            "reviewed_install_publish_failed",
            format!("failed to stage the install receipt: {error}"),
        )
    })?;

    // Publish: preserve the current install by rename, then atomically move
    // the verified staging tree into place. A failed swap restores the
    // previous install before the error propagates.
    let backup_dir = if dest.exists() {
        let backup = skills_dir.join(format!(
            ".reviewed-rollback-{skill_name}-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::rename(&dest, &backup).map_err(|error| {
            reviewed_err(
                "reviewed_install_publish_failed",
                format!("failed to preserve the existing install: {error}"),
            )
        })?;
        // A dev registration of the old folder stays with that folder. The
        // reviewed tree must not inherit it, and a rollback restores both.
        source_runtime::move_dev_registration(&dest, &backup);
        Some(backup)
    } else {
        None
    };
    // No earlier record of this name may speak for the reviewed tree.
    source_runtime::forget_dev_skill(&dest);
    if let Err(error) = std::fs::rename(&staged_skill, &dest) {
        if let Some(backup) = &backup_dir
            && std::fs::rename(backup, &dest).is_ok()
        {
            source_runtime::move_dev_registration(backup, &dest);
        }
        return Err(reviewed_err(
            "reviewed_install_publish_failed",
            format!("failed to publish the reviewed skill: {error}"),
        ));
    }
    Ok(ReviewedInstallOutcome {
        installed: vec![skill_name],
        receipt,
        backup_dir,
    })
}

/// AppUI reviewed install with the live registry: the registry fetch is
/// FAIL CLOSED here (unlike the legacy CLI lookup, a transport failure is an
/// error, never a silent unsigned fallback), and the operator-configured hub
/// trust root comes from the environment, never from the registry payload.
#[cfg(feature = "api")]
pub fn install_reviewed_skill(
    skills_dir: &Path,
    request: &ReviewedInstallRequest,
) -> Result<ReviewedInstallOutcome> {
    validate_reviewed_install_request(request)?;
    let registry = fetch_registry_from(None).map_err(|error| {
        reviewed_err(
            "reviewed_install_fetch_failed",
            format!("failed to fetch the skill registry: {error:#}"),
        )
    })?;
    let trusted_public_key = std::env::var(octos_plugin::integrity::HUB_TRUST_PUBLIC_KEY_ENV).ok();
    install_reviewed_skill_with_registry(
        skills_dir,
        request,
        registry,
        trusted_public_key.as_deref(),
    )
}

/// Undo a published reviewed install after a post-publish failure (e.g. the
/// profile runtime rebuild): remove the new tree and restore the preserved
/// previous install. Best-effort in that order so a failure never leaves
/// BOTH trees missing.
#[cfg(feature = "api")]
pub fn rollback_reviewed_install(
    skills_dir: &Path,
    skill_name: &str,
    backup_dir: Option<&Path>,
) -> Result<()> {
    let dest = skills_dir.join(skill_name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).wrap_err_with(|| {
            format!("failed to remove the failed install at {}", dest.display())
        })?;
    }
    if let Some(backup) = backup_dir {
        std::fs::rename(backup, &dest).wrap_err_with(|| {
            format!(
                "failed to restore the previous install from {}",
                backup.display()
            )
        })?;
        source_runtime::move_dev_registration(backup, &dest);
    }
    Ok(())
}

/// Confirm a published reviewed install: drop the preserved previous tree.
#[cfg(feature = "api")]
pub fn commit_reviewed_install_backup(backup_dir: Option<&Path>) -> Result<()> {
    if let Some(backup) = backup_dir {
        std::fs::remove_dir_all(backup).wrap_err_with(|| {
            format!(
                "failed to remove the replaced install at {}",
                backup.display()
            )
        })?;
        source_runtime::forget_dev_skill(backup);
    }
    Ok(())
}

/// Read the reviewed-install receipt recorded inside an installed skill.
fn read_reviewed_install_receipt(skill_dir: &Path) -> Option<ReviewedInstallReceipt> {
    let content = std::fs::read_to_string(skill_dir.join(REVIEWED_INSTALL_RECEIPT_FILE)).ok()?;
    serde_json::from_str(&content).ok()
}

// ── Reviewed-install tests (T-0556, UPCR-2026-R48CB467B) ─────────────────
//
// All fixtures are isolated tempdirs: a local git repository reached over a
// `file://` URL (no network), an in-memory registry projection, and binaries
// served from `file://` paths. No fixture executes skill code (no
// package.json / Cargo.toml / hardware lifecycle), and nothing touches a
// shared deploy.
#[cfg(test)]
#[cfg(feature = "api")]
mod reviewed_install_tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};

    fn signing_key() -> (SigningKey, String) {
        let key = SigningKey::from_bytes(&[11; 32]);
        let public = BASE64.encode(key.verifying_key().to_bytes());
        (key, public)
    }

    fn sign_artifact(
        key: &SigningKey,
        package: &str,
        platform: &str,
        bytes: &[u8],
        commit: &str,
        reviewed_at: &str,
    ) -> (String, String) {
        let digest = format!("{:x}", Sha256::digest(bytes));
        let message = octos_plugin::integrity::remote_binary_signing_message(
            package,
            platform,
            &digest,
            Some(commit),
            Some(reviewed_at),
        );
        (digest, BASE64.encode(key.sign(&message).to_bytes()))
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A one-skill git repository (SKILL.md only — no manifest, no build
    /// steps) whose HEAD commit is returned.
    fn make_skill_repo(skill: &str, marker: &str) -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "t"]);
        let dir = repo.join(skill);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {skill}\n---\n{marker}\n"),
        )
        .unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "one"]);
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        (tmp, commit)
    }

    fn file_url(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    fn registry_entry(
        package: &str,
        repo: &str,
        skill: &str,
        commit: &str,
        reviewed_at: &str,
        binaries: serde_json::Value,
    ) -> RegistryEntry {
        serde_json::from_value(serde_json::json!({
            "name": package,
            "description": "fixture",
            "repo": repo,
            "skills": [skill],
            "source_commit": commit,
            "reviewed_at": reviewed_at,
            "binaries": binaries,
        }))
        .expect("registry fixture parses")
    }

    fn reviewed_now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn request(
        package: &str,
        repo: &str,
        commit: &str,
        reviewed_at: &str,
        artifact: Option<(String, String)>,
    ) -> ReviewedInstallRequest {
        ReviewedInstallRequest {
            package: package.to_string(),
            repo: repo.to_string(),
            source_commit: commit.to_string(),
            reviewed_at: reviewed_at.to_string(),
            platform: platform_key(),
            artifact: artifact
                .map(|(sha256, signature)| ReviewedArtifactExpectation { sha256, signature }),
            force: false,
        }
    }

    fn install_err_kind(err: &eyre::Report) -> &str {
        err.downcast_ref::<ReviewedInstallError>()
            .unwrap_or_else(|| panic!("expected ReviewedInstallError, got {err:#}"))
            .kind
    }

    /// A source-only registry entry + matching request for `repo_url`.
    struct SourceFixture {
        _tmp: tempfile::TempDir,
        repo_url: String,
        commit: String,
        reviewed_at: String,
    }

    fn source_fixture(marker: &str) -> SourceFixture {
        let (tmp, commit) = make_skill_repo("demo", marker);
        SourceFixture {
            repo_url: file_url(&tmp.path().join("repo")),
            commit,
            reviewed_at: reviewed_now(),
            _tmp: tmp,
        }
    }

    impl SourceFixture {
        fn entry(&self) -> RegistryEntry {
            registry_entry(
                "demo",
                &self.repo_url,
                "demo",
                &self.commit,
                &self.reviewed_at,
                serde_json::json!({}),
            )
        }
        fn request(&self) -> ReviewedInstallRequest {
            request(
                "demo",
                &self.repo_url,
                &self.commit,
                &self.reviewed_at,
                None,
            )
        }
        fn install(
            &self,
            skills: &Path,
            req: &ReviewedInstallRequest,
        ) -> Result<ReviewedInstallOutcome> {
            install_reviewed_skill_with_registry(skills, req, vec![self.entry()], None)
        }
    }

    #[test]
    fn should_install_exactly_the_reviewed_commit_and_report_it_on_reread() {
        let fixture = source_fixture("reviewed-content");
        let req = fixture.request();
        // The reviewed branch advances AFTER the review snapshot is taken.
        let repo = fixture._tmp.path().join("repo");
        std::fs::write(
            repo.join("demo/SKILL.md"),
            "---\nname: demo\n---\ndrifted-content\n",
        )
        .unwrap();
        git(&repo, &["commit", "-qam", "two"]);
        assert_ne!(git(&repo, &["rev-parse", "HEAD"]), fixture.commit);

        let skills = tempfile::tempdir().unwrap();
        let outcome = fixture.install(skills.path(), &req).unwrap();
        assert_eq!(outcome.installed, vec!["demo".to_string()]);
        assert_eq!(outcome.receipt.source_commit, fixture.commit);
        assert_eq!(outcome.receipt.artifact_sha256, None);
        assert!(outcome.backup_dir.is_none());
        let installed = skills.path().join("demo");
        let content = std::fs::read_to_string(installed.join("SKILL.md")).unwrap();
        assert!(content.contains("reviewed-content"));
        assert!(
            !content.contains("drifted-content"),
            "branch drift must never substitute a newer object"
        );
        assert!(
            !installed.join(".source").exists(),
            "a reviewed install must not record a branch source that `update` could follow"
        );
        // The receipt and the post-install re-read report the actual object.
        let receipt: ReviewedInstallReceipt = serde_json::from_str(
            &std::fs::read_to_string(installed.join(REVIEWED_INSTALL_RECEIPT_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt.source_commit, fixture.commit);
        let listed = list_skills(skills.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].source_commit.as_deref(),
            Some(fixture.commit.as_str())
        );
    }

    #[test]
    fn should_fail_closed_when_the_registry_entry_is_swapped() {
        let fixture = source_fixture("v1");
        let mut swapped_repo = fixture.entry();
        swapped_repo.repo = "https://example.com/other/repo.git".to_string();
        let mut swapped_commit = fixture.entry();
        swapped_commit.source_commit = Some("b".repeat(40));
        let mut swapped_review = fixture.entry();
        swapped_review.reviewed_at = Some("2026-11-09T00:00:00Z".to_string());
        let mut uncovered = fixture.entry();
        uncovered.binaries.insert(
            platform_key(),
            BinaryInfo {
                url: "file:///x".to_string(),
                sha256: Some("a".repeat(64)),
                signature: Some("sig".to_string()),
            },
        );
        let stale = "2026-09-01T00:00:00Z";
        let mut stale_entry = fixture.entry();
        stale_entry.reviewed_at = Some(stale.to_string());
        let mut stale_req = fixture.request();
        stale_req.reviewed_at = stale.to_string();
        let mut foreign_req = fixture.request();
        foreign_req.platform = "plan9-mips".to_string();

        let cases = vec![
            (
                "swapped repo",
                swapped_repo,
                fixture.request(),
                "reviewed_install_registry_mismatch",
            ),
            (
                "swapped commit",
                swapped_commit,
                fixture.request(),
                "reviewed_install_registry_mismatch",
            ),
            (
                "swapped review",
                swapped_review,
                fixture.request(),
                "reviewed_install_registry_mismatch",
            ),
            (
                "uncovered artifact",
                uncovered,
                fixture.request(),
                "reviewed_install_registry_mismatch",
            ),
            (
                "stale review",
                stale_entry,
                stale_req,
                "reviewed_install_verification_failed",
            ),
            (
                "foreign platform",
                fixture.entry(),
                foreign_req,
                "reviewed_install_platform_unavailable",
            ),
        ];
        for (label, entry, req, expected_kind) in cases {
            let skills = tempfile::tempdir().unwrap();
            let err = install_reviewed_skill_with_registry(skills.path(), &req, vec![entry], None)
                .expect_err(label);
            assert_eq!(install_err_kind(&err), expected_kind, "{label}");
            assert!(!skills.path().join("demo").exists(), "{label}");
        }
    }

    #[test]
    fn should_install_signed_raw_and_archived_binaries_with_actual_digests() {
        let fixture = source_fixture("v1");
        let (key, public) = signing_key();
        let install = |served_name: &str, served_bytes: &[u8]| {
            let (sha256, signature) = sign_artifact(
                &key,
                "demo",
                &platform_key(),
                served_bytes,
                &fixture.commit,
                &fixture.reviewed_at,
            );
            let served = fixture._tmp.path().join(served_name);
            std::fs::write(&served, served_bytes).unwrap();
            let entry = registry_entry(
                "demo",
                &fixture.repo_url,
                "demo",
                &fixture.commit,
                &fixture.reviewed_at,
                serde_json::json!({ platform_key(): { "url": file_url(&served), "sha256": sha256, "signature": signature } }),
            );
            let req = request(
                "demo",
                &fixture.repo_url,
                &fixture.commit,
                &fixture.reviewed_at,
                Some((sha256.clone(), signature)),
            );
            let skills = tempfile::tempdir().unwrap();
            let outcome = install_reviewed_skill_with_registry(
                skills.path(),
                &req,
                vec![entry],
                Some(&public),
            )
            .unwrap();
            assert_eq!(
                outcome.receipt.artifact_sha256.as_deref(),
                Some(sha256.as_str())
            );
            let listed = list_skills(skills.path()).unwrap();
            assert_eq!(listed[0].artifact_sha256.as_deref(), Some(sha256.as_str()));
            assert_eq!(
                listed[0].source_commit.as_deref(),
                Some(fixture.commit.as_str())
            );
            skills
        };

        let skills = install("artifact.bin", b"binary-bytes");
        let main = skills.path().join("demo/main");
        assert_eq!(std::fs::read(&main).unwrap(), b"binary-bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&main).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }

        // Nested archive member names never become install paths: only the
        // reviewed payload is extracted to `main`, with its own actual digest.
        let payload = b"archived-binary";
        let archive_bytes = {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "deeply/nested/evil", &payload[..])
                .unwrap();
            let encoder = builder.into_inner().unwrap();
            encoder.finish().unwrap()
        };
        let archive_skills = install("artifact.tar.gz", &archive_bytes);
        assert_eq!(
            std::fs::read(archive_skills.path().join("demo/main")).unwrap(),
            payload
        );
        assert!(!archive_skills.path().join("demo/deeply").exists());
    }

    #[test]
    fn should_fail_closed_on_artifact_substitution() {
        let fixture = source_fixture("v1");
        let (key, public) = signing_key();
        let (sha256, signature) = sign_artifact(
            &key,
            "demo",
            &platform_key(),
            b"reviewed-binary",
            &fixture.commit,
            &fixture.reviewed_at,
        );
        let served = fixture._tmp.path().join("served.bin");
        let entry = |sha256: &str, signature: &str| {
            registry_entry(
                "demo",
                &fixture.repo_url,
                "demo",
                &fixture.commit,
                &fixture.reviewed_at,
                serde_json::json!({ platform_key(): { "url": file_url(&served), "sha256": sha256, "signature": signature } }),
            )
        };
        let install = |skills: &Path, sha256: &str, signature: &str| {
            let req = request(
                "demo",
                &fixture.repo_url,
                &fixture.commit,
                &fixture.reviewed_at,
                Some((sha256.to_string(), signature.to_string())),
            );
            install_reviewed_skill_with_registry(
                skills,
                &req,
                vec![entry(sha256, signature)],
                Some(&public),
            )
        };
        // The server serves DIFFERENT bytes at the reviewed URL (digest swap).
        std::fs::write(&served, b"swapped-binary").unwrap();
        let skills = tempfile::tempdir().unwrap();
        let err = install(skills.path(), &sha256, &signature)
            .expect_err("a digest swap must fail closed");
        assert_eq!(
            install_err_kind(&err),
            "reviewed_install_verification_failed"
        );
        assert!(!skills.path().join("demo").exists());
        // The served bytes match the digest, but a rogue key signed them.
        std::fs::write(&served, b"reviewed-binary").unwrap();
        let rogue = SigningKey::from_bytes(&[99; 32]);
        let (_, rogue_signature) = sign_artifact(
            &rogue,
            "demo",
            &platform_key(),
            b"reviewed-binary",
            &fixture.commit,
            &fixture.reviewed_at,
        );
        let skills = tempfile::tempdir().unwrap();
        let err = install(skills.path(), &sha256, &rogue_signature)
            .expect_err("a foreign signature must fail closed");
        assert_eq!(
            install_err_kind(&err),
            "reviewed_install_verification_failed"
        );
        assert!(!skills.path().join("demo").exists());
    }

    #[cfg(unix)]
    #[test]
    fn should_refuse_a_skill_tree_containing_symlinks() {
        let (tmp, _commit) = make_skill_repo("demo", "v1");
        let repo = tmp.path().join("repo");
        std::os::unix::fs::symlink("/etc", repo.join("demo/escape")).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qam", "symlink"]);
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        let repo_url = file_url(&repo);
        let reviewed_at = reviewed_now();
        let skills = tempfile::tempdir().unwrap();
        let entry = registry_entry(
            "demo",
            &repo_url,
            "demo",
            &commit,
            &reviewed_at,
            serde_json::json!({}),
        );
        let req = request("demo", &repo_url, &commit, &reviewed_at, None);
        let err = install_reviewed_skill_with_registry(skills.path(), &req, vec![entry], None)
            .expect_err("a symlink in the reviewed tree must fail the install");
        assert_eq!(install_err_kind(&err), "reviewed_install_unsafe_tree");
        assert!(!skills.path().join("demo").exists());
    }

    #[test]
    fn should_protect_the_existing_install_on_refusal_or_failure() {
        let fixture = source_fixture("v1");
        let skills = tempfile::tempdir().unwrap();
        let old = skills.path().join("demo");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("SKILL.md"), "old-install").unwrap();
        // Without force, replacing an existing install is refused outright.
        let err = fixture
            .install(skills.path(), &fixture.request())
            .expect_err("an existing install without force must be refused");
        assert_eq!(install_err_kind(&err), "reviewed_install_already_installed");
        // A forced reinstall whose artifact fails verification leaves the old
        // install untouched (staging never reaches the live directory).
        let (key, public) = signing_key();
        let (sha256, signature) = sign_artifact(
            &key,
            "demo",
            &platform_key(),
            b"reviewed",
            &fixture.commit,
            &fixture.reviewed_at,
        );
        let served = fixture._tmp.path().join("served.bin");
        std::fs::write(&served, b"swapped").unwrap();
        let entry = registry_entry(
            "demo",
            &fixture.repo_url,
            "demo",
            &fixture.commit,
            &fixture.reviewed_at,
            serde_json::json!({ platform_key(): { "url": file_url(&served), "sha256": sha256, "signature": signature } }),
        );
        let mut req = request(
            "demo",
            &fixture.repo_url,
            &fixture.commit,
            &fixture.reviewed_at,
            Some((sha256, signature)),
        );
        req.force = true;
        let _ =
            install_reviewed_skill_with_registry(skills.path(), &req, vec![entry], Some(&public))
                .expect_err("digest mismatch must fail");
        assert_eq!(
            std::fs::read_to_string(old.join("SKILL.md")).unwrap(),
            "old-install"
        );
    }

    #[test]
    fn should_roll_back_a_published_install_and_restore_the_previous_one() {
        let fixture = source_fixture("v1");
        let skills = tempfile::tempdir().unwrap();
        fixture.install(skills.path(), &fixture.request()).unwrap();
        // Forced reinstall publishes a new object; a later failure rolls back.
        let mut forced = fixture.request();
        forced.force = true;
        let outcome = fixture.install(skills.path(), &forced).unwrap();
        let backup = outcome.backup_dir.expect("replacing keeps a backup");
        std::fs::write(skills.path().join("demo/marker"), "new").unwrap();
        rollback_reviewed_install(skills.path(), "demo", Some(&backup)).unwrap();
        assert!(!skills.path().join("demo/marker").exists());
        assert!(skills.path().join("demo/SKILL.md").exists());
        // Commit path: the backup is consumed.
        let outcome = fixture.install(skills.path(), &forced).unwrap();
        let backup = outcome.backup_dir.unwrap();
        commit_reviewed_install_backup(Some(&backup)).unwrap();
        assert!(!backup.exists());

        remove_skill(skills.path(), "demo").unwrap();
        assert!(!skills.path().join("demo").exists());
        // Reinstall is a new review-bound request, never replay of the forced one.
        let fresh = fixture.request();
        assert!(!fresh.force);
        fixture.install(skills.path(), &fresh).unwrap();
        assert!(skills.path().join("demo/SKILL.md").exists());
    }

    /// A dev registration belongs to the folder it was made for. A tree that
    /// an install puts in that place is an installed skill, even when it
    /// brings the dev skill's own marker along. A rollback brings the dev
    /// skill back together with its registration.
    #[test]
    fn should_not_let_a_dev_registration_outlive_the_skill_it_was_made_for() {
        let fixture = source_fixture("v1");
        let root = tempfile::tempdir().unwrap();
        let (skills, source) = (root.path().join("skills"), root.path().join("src/demo"));
        std::fs::create_dir_all(&source).unwrap();
        let manifest = r#"{"name":"demo","version":"1.0.0","tools":[]}"#;
        std::fs::write(source.join("manifest.json"), manifest).unwrap();
        std::fs::write(source.join("SKILL.md"), "live source").unwrap();
        let dev: source_runtime::DevSkill = serde_json::from_value(serde_json::json!({
            "source": source, "package": "demo", "workspace": source,
            "relax": root.path().join("relax"), "cargo_bin_dir": null, "env": {},
        }))
        .unwrap();
        let dest = source_runtime::register_dev_skill(&skills, &dev, false).unwrap();
        let marker_file = dest.join(source_runtime::DEV_MARKER_FILE);
        let marker = std::fs::read(&marker_file).unwrap();
        let record = skills.join(".octos-dev/demo");
        let is_dev = || source_runtime::is_dev_skill(&dest);
        assert!(is_dev() && record.exists());

        // A reviewed replacement: the record leaves with the old folder.
        let mut forced = fixture.request();
        forced.force = true;
        let backup = fixture.install(&skills, &forced).unwrap().backup_dir;
        let backup = backup.expect("replacing keeps a backup");
        std::fs::write(&marker_file, &marker).unwrap();
        assert!(!record.exists() && !is_dev());
        // A record that is still there does not speak for a reviewed tree.
        let kept = skills.join(".octos-dev").join(backup.file_name().unwrap());
        std::fs::copy(&kept, &record).unwrap();
        assert!(!is_dev());
        // A rollback restores the dev skill with its registration.
        rollback_reviewed_install(&skills, "demo", Some(&backup)).unwrap();
        assert!(is_dev() && !kept.exists());
        // A commit ends it: no record is left for either name.
        let backup = fixture.install(&skills, &forced).unwrap().backup_dir;
        commit_reviewed_install_backup(backup.as_deref()).unwrap();
        let records = std::fs::read_dir(skills.join(".octos-dev")).unwrap();
        assert_eq!(records.count(), 0);

        // A plain install, over a dev skill and over the record of one whose
        // folder was deleted by hand.
        let tree = root.path().join("pkg/demo");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("SKILL.md"), "installed").unwrap();
        std::fs::write(tree.join(source_runtime::DEV_MARKER_FILE), &marker).unwrap();
        for folder_deleted in [false, true] {
            source_runtime::register_dev_skill(&skills, &dev, true).unwrap();
            assert!(is_dev());
            if folder_deleted {
                std::fs::remove_dir_all(&dest).unwrap();
            }
            install_from_local(&skills, &tree, true).unwrap();
            assert!(marker_file.exists() && !is_dev(), "{folder_deleted}");
        }
    }

    #[test]
    fn should_reject_traversal_and_unknown_packages() {
        let skills = tempfile::tempdir().unwrap();
        let commit = "a".repeat(40);
        let reviewed_at = reviewed_now();
        for package in ["../x", "a/b", "a\\b", "", "."] {
            let req = request(
                package,
                "https://example.com/o/r.git",
                &commit,
                &reviewed_at,
                None,
            );
            let err = install_reviewed_skill_with_registry(skills.path(), &req, Vec::new(), None)
                .expect_err("unsafe package names must be rejected");
            assert_eq!(install_err_kind(&err), "reviewed_install_invalid_params");
        }
        let req = request(
            "absent",
            "https://example.com/o/r.git",
            &commit,
            &reviewed_at,
            None,
        );
        let err = install_reviewed_skill_with_registry(skills.path(), &req, Vec::new(), None)
            .expect_err("an unknown package must fail closed");
        assert_eq!(install_err_kind(&err), "reviewed_install_registry_mismatch");
    }

    #[test]
    fn should_project_review_provenance_into_the_search_result() {
        let commit = "a".repeat(40);
        let reviewed_at = reviewed_now();
        let entry = registry_entry(
            "demo",
            "octos-org/demo",
            "demo",
            &commit,
            &reviewed_at,
            serde_json::json!({ platform_key(): { "url": "https://x/bin", "sha256": "b".repeat(64), "signature": "c2ln" } }),
        );
        let value = serde_json::to_value(RegistryPackage::from(entry)).unwrap();
        assert_eq!(value["source_commit"], serde_json::json!(commit));
        assert_eq!(value["reviewed_at"], serde_json::json!(reviewed_at));
        assert_eq!(
            value["binaries"][platform_key()]["sha256"],
            serde_json::json!("b".repeat(64))
        );
        assert_eq!(
            value["binaries"][platform_key()]["signature"],
            serde_json::json!("c2ln")
        );
    }
}

#[cfg(test)]
mod execution_gate_tests {
    use super::*;

    #[test]
    fn npm_source_scripts_do_not_run_without_the_operator_opt_in() {
        let tmp = tempfile::tempdir().unwrap();
        // Invalid JSON makes an accidental npm spawn fail loudly; denial must
        // return before npm can inspect it or create any install artifacts.
        std::fs::write(tmp.path().join("package.json"), "not json").unwrap();
        maybe_npm_install(tmp.path(), false).expect("denied npm is a deliberate skip");
        assert!(!tmp.path().join("node_modules").exists());
        assert!(!tmp.path().join("package-lock.json").exists());
    }

    #[test]
    fn cargo_source_build_is_denied_without_spawning_or_mutating_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("Cargo.toml");
        std::fs::write(&manifest, "not a valid cargo manifest").unwrap();
        maybe_cargo_build_with(tmp.path(), false).expect("denied cargo is a deliberate skip");
        assert_eq!(
            std::fs::read_to_string(manifest).unwrap(),
            "not a valid cargo manifest"
        );
        assert!(!tmp.path().join("target").exists());
        assert!(!tmp.path().join("main").exists());
    }

    #[test]
    fn explicitly_allowed_cargo_source_build_still_produces_the_local_binary() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"gate_probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        maybe_cargo_build_with(tmp.path(), true).expect("explicit local source build works");
        assert!(
            tmp.path()
                .join(if cfg!(windows) { "main.exe" } else { "main" })
                .exists()
        );
    }

    #[test]
    fn allowed_lifecycle_failure_rolls_back_the_partial_install() {
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
        let error = run_activate_lifecycle(&dir, "rollback-probe", true)
            .expect_err("failed allowed lifecycle must fail activation");
        assert!(error.to_string().contains("activation failed"));
        assert!(!dir.exists(), "partial install was not rolled back");
    }
}
