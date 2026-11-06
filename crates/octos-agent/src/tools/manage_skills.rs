//! Skill management tool for normal profile gateways.
//!
//! Allows agents to list, install, remove, and search skills directly
//! without going through the admin API.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;

use super::{
    ConcurrencyClass, TOOL_APPROVAL_CTX, TOOL_CTX, Tool, ToolApprovalDecision, ToolApprovalRequest,
    ToolResult,
};

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

    // Mutating actions block on the interactive approval bridge; a human may
    // legitimately take longer than any dispatch timeout (same contract as
    // the other approval-gated tools).
    fn blocks_on_human_input(&self) -> bool {
        true
    }

    // Approval-gated tools are Exclusive: nothing else runs while the human
    // decides, and the mutation itself is serialized against every other
    // tool call (mirrors shell/edit_file/write_file).
    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
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

        // #2321: install/update/remove are mutating, supply-chain-facing
        // actions — installs clone arbitrary repositories and their
        // lifecycle commands run on this machine at install time — so they
        // block on an interactive approval. list/search stay ungated. No
        // approval bridge (headless path) ⇒ fail closed rather than run
        // unapproved.
        if matches!(input.action.as_str(), "install" | "update" | "remove") {
            let requester = TOOL_APPROVAL_CTX.try_with(Clone::clone).ok();
            let Some(requester) = requester else {
                tracing::warn!(
                    action = %input.action,
                    "skill mutation requires approval — denied (no interactive approval available)"
                );
                return Ok(ToolResult {
                    output: format!(
                        "Skill {} requires interactive approval and was denied: no interactive approval channel is available. The user can run `octos skills {}` themselves instead.",
                        input.action, input.action
                    ),
                    success: false,
                    ..Default::default()
                });
            };
            let tool_id = TOOL_CTX
                .try_with(|inner| inner.tool_id.clone())
                .unwrap_or_default();
            let decision = requester
                .request_approval(ToolApprovalRequest {
                    tool_id,
                    tool_name: self.name().to_owned(),
                    title: format!("Approve skill {}", input.action),
                    body: match input.action.as_str() {
                        "remove" => format!(
                            "Remove skill '{}' from the installed skills directory.",
                            input.name.as_deref().unwrap_or_default()
                        ),
                        _ => format!(
                            "{} skill from '{}': installs third-party code whose lifecycle commands run on this machine at install time.",
                            input.action,
                            input.repo.as_deref().unwrap_or_default()
                        ),
                    },
                    command: None,
                    cwd: None,
                    once_only: false,
                    host_tool: None,
                })
                .await;
            if matches!(decision, ToolApprovalDecision::Deny) {
                tracing::warn!(action = %input.action, "skill mutation denied by interactive approval");
                return Ok(ToolResult {
                    output: format!(
                        "Skill {} was denied by interactive approval. Do not retry without the user asking again.",
                        input.action
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        }

        let skills_dir = self.skills_dir.clone();

        // R-16: the approval wait above is exempt from dispatch timeouts (a
        // human may take arbitrarily long), but the MUTATION is not. Bound
        // the blocking phase and share an abort flag with the subprocess
        // runner so a timed-out install kills its current child instead of
        // leaving it writing into the skills directory.
        let abort = Arc::new(AtomicBool::new(false));
        let abort_for_thread = Arc::clone(&abort);
        let mutation = tokio::task::spawn_blocking(move || match input.action.as_str() {
            "list" => do_list(&skills_dir),
            "install" => do_install(&skills_dir, &input, &abort_for_thread),
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
                            if source_repo.is_empty() {
                                force_input.repo.as_deref().unwrap_or("")
                            } else {
                                &source_repo
                            },
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

                do_install(&skills_dir, &force_input, &abort_for_thread)
            }
            other => Ok(ToolResult {
                output: format!(
                    "Unknown action: {other}. Use list, install, update, remove, or search."
                ),
                success: false,
                ..Default::default()
            }),
        });

        // R-16: bound the whole mutation (clone + npm/cargo + copies) and
        // signal the shared abort flag so the subprocess runner kills its
        // current child instead of leaving it running after the tool failed.
        let result = match tokio::time::timeout(SKILL_MUTATION_TIMEOUT, mutation).await {
            Ok(joined) => joined.map_err(|e| eyre::eyre!("task join error: {e}"))??,
            Err(_elapsed) => {
                abort.store(true, Ordering::Relaxed);
                return Ok(ToolResult {
                    output: format!(
                        "Skill operation timed out after {}s; the running subprocess was killed.",
                        SKILL_MUTATION_TIMEOUT.as_secs()
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

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

#[derive(Debug)]
struct RepoSpec {
    owner: String,
    repository: String,
    subdir: Option<String>,
    install_name: String,
}

fn parse_repo_spec(repo: &str) -> Result<RepoSpec, String> {
    let segments: Vec<&str> = repo.split('/').collect();
    if segments.len() < 2
        || segments.iter().any(|segment| {
            segment.is_empty() || matches!(*segment, "." | "..") || segment.contains('\\')
        })
    {
        return Err(format!(
            "Invalid repo path: '{repo}'. Expected user/repo or user/repo/skill-name with non-empty segments other than '.' and '..' and without backslashes"
        ));
    }
    let subdir = if segments.len() > 2 {
        Some(segments[2..].join("/"))
    } else {
        None
    };
    let install_name = match subdir.as_deref() {
        Some(sub) => sub
            .rsplit_once('/')
            .map_or(sub, |(_, name)| name)
            .to_string(),
        None => segments[1].to_string(),
    };
    Ok(RepoSpec {
        owner: segments[0].to_string(),
        repository: segments[1].to_string(),
        subdir,
        install_name,
    })
}

/// Bound on any one skill-install subprocess (git clone / npm install /
/// cargo build). R-16: a bare `.status()` wait has no deadline, so a stuck
/// child parks the install forever and nothing can cancel it once the
/// blocking thread starts. Five minutes is generous for a shallow clone or a
/// small skill build while guaranteeing a stuck child is killed.
const SKILL_SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(300);

/// Bound on the whole mutating skill operation (clone + npm/cargo + copies).
/// Three bounded subprocesses could each run to the per-subprocess deadline,
/// so the total cap is 3x plus headroom for local IO.
const SKILL_MUTATION_TIMEOUT: Duration = Duration::from_secs(1000);

/// Run a child process with a hard deadline and a cooperative abort flag:
/// past the deadline, or once `abort` is set (tool timeout), the child is
/// killed and reaped instead of being left running — a cancelled install must
/// not keep writing into the skills directory (R-16).
fn run_skill_command(
    cmd: &mut std::process::Command,
    timeout: Duration,
    abort: &AtomicBool,
) -> Result<std::process::ExitStatus, String> {
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {:?}: {e}", cmd.get_program()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if abort.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{:?} cancelled by install abort",
                        cmd.get_program()
                    ));
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{:?} timed out after {}s",
                        cmd.get_program(),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed to wait on {:?}: {e}", cmd.get_program()));
            }
        }
    }
}

fn do_install(
    skills_dir: &std::path::Path,
    input: &Input,
    abort: &AtomicBool,
) -> Result<ToolResult> {
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

    let spec = match parse_repo_spec(repo) {
        Ok(spec) => spec,
        Err(output) => {
            return Ok(ToolResult {
                output,
                success: false,
                ..Default::default()
            });
        }
    };
    let clone_url = format!("https://github.com/{}/{}.git", spec.owner, spec.repository);
    let subdir = spec.subdir.clone();

    // Clone to temp dir
    let tmp = tempfile::tempdir()?;
    let clone_dir = tmp.path().join(&spec.repository);

    let mut git_cmd = std::process::Command::new("git");
    git_cmd
        .args(["clone", "--depth", "1", "--branch", branch])
        .arg(&clone_url)
        .arg(&clone_dir)
        .stdout(std::process::Stdio::null())
        // The piped stderr was never read; a dead pipe can wedge the writer,
        // so the bounded runner below discards it instead.
        .stderr(std::process::Stdio::null());
    let status = run_skill_command(&mut git_cmd, SKILL_SUBPROCESS_TIMEOUT, abort).map_err(|e| {
        if e.starts_with("failed to spawn") {
            eyre::eyre!("git not found")
        } else {
            eyre::eyre!("git clone failed to run to completion: {e}")
        }
    })?;

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
        // Containment: the subdir must resolve inside the clone — `..`
        // segments would escape it and copy arbitrary local directories
        // into the skills dir (#2680).
        let src_canonical = src.canonicalize()?;
        let clone_canonical = clone_dir.canonicalize()?;
        if !src_canonical.starts_with(&clone_canonical) {
            return Ok(ToolResult {
                output: format!("Subdirectory '{sub}' escapes the cloned repository"),
                success: false,
                ..Default::default()
            });
        }
        let src = src_canonical;
        if !src.is_dir() {
            return Ok(ToolResult {
                output: format!(
                    "Subdirectory '{sub}' not found in {}/{}",
                    spec.owner, spec.repository
                ),
                success: false,
                ..Default::default()
            });
        }
        let name = spec.install_name.clone();
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
            // Verify/build in the temporary clone before touching any prior
            // live installation; proof failure leaves the old skill intact.
            maybe_install_binary(&src, abort)?;
            if dest.exists() {
                std::fs::remove_dir_all(&dest)?;
            }
            copy_dir_recursive(&src, &dest)?;
            installed.push(name);
        }
    } else {
        // Whole repo: single skill or multi-skill
        if clone_dir.join("SKILL.md").exists() {
            let dest = skills_dir.join(&spec.repository);
            if dest.exists() && !input.force {
                let local_ver = skill_version(&dest);
                let remote_ver = skill_version(&clone_dir);
                if let (Some(lv), Some(rv)) = (&local_ver, &remote_ver) {
                    if version_newer(rv, lv) {
                        skipped.push(format!(
                            "{} (update available: {lv} → {rv}, use force=true to update)",
                            spec.repository
                        ));
                    } else {
                        skipped.push(format!("{} (up to date: {lv})", spec.repository));
                    }
                } else {
                    skipped.push(spec.repository.clone());
                }
            } else {
                maybe_install_binary(&clone_dir, abort)?;
                if dest.exists() {
                    std::fs::remove_dir_all(&dest)?;
                }
                copy_dir_recursive(&clone_dir, &dest)?;
                installed.push(spec.repository.clone());
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
                maybe_install_binary(&entry.path(), abort)?;
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

    let repo_path = format!("{}/{}", spec.owner, spec.repository);

    // Post-install: npm install, binary install, source tracking
    for name in &installed {
        let dir = skills_dir.join(name);
        maybe_npm_install(&dir, abort);
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

        let matches =
            e_repo == repo || e_name == repo || skill_name.is_some_and(|sn| e_skills.contains(&sn));

        if matches {
            e.get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
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
        // Refuse symlinked entries: a crafted repo can point one at any
        // local directory or file outside the clone (#2680).
        if std::fs::symlink_metadata(&src_path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(true)
        {
            tracing::info!("skill install: skipping symlink {name_str}");
            continue;
        }
        let dst_path = dst.join(&name);
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn maybe_npm_install(dir: &std::path::Path, abort: &AtomicBool) {
    let opted_in = std::env::var("OCTOS_SKILLS_NPM_INSTALL").as_deref() == Ok("1");
    maybe_npm_install_with(dir, opted_in, abort);
}

fn maybe_npm_install_with(dir: &std::path::Path, opted_in: bool, abort: &AtomicBool) {
    if !dir.join("package.json").exists() || dir.join("node_modules").exists() {
        return;
    }
    // Installing dependencies executes arbitrary postinstall scripts —
    // an operator opt-in (the #2665 env-pattern; #2680).
    if !opted_in {
        tracing::info!(
            "skill ships package.json but OCTOS_SKILLS_NPM_INSTALL != 1; skipping npm install"
        );
        return;
    }
    let _ = run_skill_command(
        std::process::Command::new("npm")
            .arg("install")
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
        SKILL_SUBPROCESS_TIMEOUT,
        abort,
    );
}

/// Install binary for a skill that has manifest.json (tool skill).
///
/// Resolution order:
/// 1. manifest.json `binaries` field (skill author's CI/CD builds)
/// 2. Skill registry `binaries` field (registry-audited builds)
/// 3. `cargo build --release` fallback
fn maybe_install_binary(dir: &std::path::Path, abort: &AtomicBool) -> Result<()> {
    let has_manifest = dir.join("manifest.json").exists();
    let has_cargo = dir.join("Cargo.toml").exists();
    if !has_manifest && !has_cargo {
        return Ok(());
    }

    let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
    // This function runs in the temporary clone BEFORE anything is copied to
    // the live skills dir. A repository-supplied executable is therefore not
    // trusted merely because it exists: it needs the manifest's signed hash.
    let manifest = if has_manifest {
        let content = std::fs::read_to_string(dir.join("manifest.json"))
            .wrap_err("failed to read remote skill manifest")?;
        Some(
            serde_json::from_str::<crate::plugins::manifest::PluginManifest>(&content)
                .wrap_err("remote skill manifest or binary proof metadata is malformed")?,
        )
    } else {
        None
    };
    let bundled = if dir.join(&dir_name).exists() {
        Some(dir.join(&dir_name))
    } else {
        let main = dir.join("main");
        (main.exists() && !is_generated_lazy_cargo_wrapper(&main)).then_some(main)
    };
    if let Some(executable) = bundled {
        let manifest = manifest
            .as_ref()
            .ok_or_else(|| eyre::eyre!("remote repository executable has no manifest proof"))?;
        verify_bundled_remote_binary(dir, &executable, manifest)?;
        return Ok(());
    }

    let key = platform_key();

    // Try 1: download from manifest.json binaries (skill repo's own CI/CD).
    // A declared but unverifiable download is an error: never fall through to
    // source execution or a launcher as if the package had been verified.
    if let Some(manifest) = &manifest {
        if let Some(info) = manifest.binaries.get(&key) {
            if info.url.trim().is_empty() {
                eyre::bail!("manifest binary entry has an empty URL");
            }
            if download_binary(
                dir,
                &info.url,
                info.sha256.as_deref(),
                info.signature.as_deref(),
                None,
                None,
            )? {
                return Ok(());
            }
        }
    }

    // For script-based skills, create a runnable wrapper before any network lookup.
    if !has_cargo && let Ok(true) = crate::plugins::loader::ensure_plugin_executable(dir) {
        return Ok(());
    }

    // Try 2: download from skill registry (audited builds). Corrupt/stale
    // registry metadata is an error, not an invisible absence of binaries.
    if let Some(registry) = lookup_registry_binaries(&dir_name)? {
        if let Some(info) = registry.binaries.get(&key) {
            if info.url.trim().is_empty() {
                eyre::bail!("registry binary entry has an empty URL");
            }
            if download_binary(
                dir,
                &info.url,
                info.sha256.as_deref(),
                info.signature.as_deref(),
                Some(&registry.source_commit),
                Some(&registry.reviewed_at),
            )? {
                return Ok(());
            }
        }
    }

    // Try 3: cargo build if Cargo.toml exists. Operator opt-in (#2665
    // env-pattern, mirrors OCTOS_SKILLS_NPM_INSTALL): building from source
    // executes the repository's build.rs — arbitrary code from whatever was
    // cloned (#2321). Without the opt-in the skill falls through to the
    // lazy launcher.
    if !has_cargo {
        let _ = crate::plugins::loader::ensure_plugin_executable(dir);
        return Ok(());
    }
    let cargo_opted_in = std::env::var("OCTOS_SKILLS_CARGO_BUILD").as_deref() == Ok("1");
    if !maybe_cargo_build_with(dir, cargo_opted_in, abort) {
        // Final fallback: create a lazy launcher so the loader has an
        // executable.
        let _ = crate::plugins::loader::ensure_plugin_executable(dir);
    }
    Ok(())
}

/// Verify a repository-supplied executable in the temporary clone. The
/// manifest's top-level sha256/signature pair binds these exact bytes; the
/// source tree is never copied to the live skills dir before this succeeds.
fn verify_bundled_remote_binary(
    dir: &std::path::Path,
    executable: &std::path::Path,
    manifest: &crate::plugins::manifest::PluginManifest,
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
        RemoteBinaryAttestation {
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

/// Attempt the source-build fallback for a Cargo-backed skill. Returns
/// `true` only when a binary was actually installed to `dir/main` — a build
/// that succeeds without producing a located binary must fall through to the
/// lazy launcher, exactly like a failed build. Gated behind the operator
/// opt-in: building from source executes the cloned repository's `build.rs`
/// (#2321), so without `OCTOS_SKILLS_CARGO_BUILD=1` this logs and declines
/// without spawning.
fn maybe_cargo_build_with(dir: &std::path::Path, opted_in: bool, abort: &AtomicBool) -> bool {
    if !opted_in {
        tracing::info!(
            "skill ships Cargo.toml but OCTOS_SKILLS_CARGO_BUILD != 1; skipping cargo build"
        );
        return false;
    }
    let status = run_skill_command(
        std::process::Command::new("cargo")
            .args(["build", "--release"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            // Piped but never read: discard instead of leaving a dead pipe.
            .stderr(std::process::Stdio::null()),
        SKILL_SUBPROCESS_TIMEOUT,
        abort,
    );
    let Ok(s) = status else {
        return false;
    };
    if !s.success() {
        return false;
    }
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
                        // Only a real install counts: if the name heuristic
                        // fails to locate the produced binary, the caller
                        // must still get the lazy launcher, not a silent
                        // no-executable skill.
                        return true;
                    }
                }
            }
        }
    }
    tracing::warn!(
        "cargo build succeeded but no binary was located under target/release; \
         falling back to the lazy launcher"
    );
    false
}

#[cfg(test)]
fn has_installed_skill_executable(dir: &std::path::Path, dir_name: &str) -> bool {
    if dir.join(dir_name).exists() {
        return true;
    }

    let main = dir.join("main");
    main.exists() && !is_generated_lazy_cargo_wrapper(&main)
}

fn is_generated_lazy_cargo_wrapper(path: &std::path::Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };

    contents.contains("Skill binary is missing and cargo is not installed")
        && contents.contains("cargo build --release")
        && contents.contains("target/release/")
}

fn platform_key() -> String {
    // Rust reports macOS as "macos", while skill manifests/registry
    // conventionally publish Apple binaries under "darwin-*".
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

#[derive(serde::Deserialize)]
struct RegistryBinaryInfo {
    url: String,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    signature: Option<String>,
}

#[derive(serde::Deserialize)]
struct RegistryEntry {
    name: String,
    #[serde(default)]
    skills: Vec<String>,
    /// Upstream commit and review time are mandatory when binaries exist.
    #[serde(default)]
    source_commit: Option<String>,
    #[serde(default)]
    reviewed_at: Option<String>,
    #[serde(default)]
    binaries: std::collections::HashMap<String, RegistryBinaryInfo>,
}

struct RegistryBinarySet {
    source_commit: String,
    reviewed_at: String,
    binaries: std::collections::HashMap<String, RegistryBinaryInfo>,
}

fn lookup_registry_binaries(package_name: &str) -> Result<Option<RegistryBinarySet>> {
    let url = "https://raw.githubusercontent.com/octos-org/octos-hub/main/registry.json";
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
        .wrap_err("skill registry is corrupt or has an invalid schema")?;

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

/// Download a binary only after verifying its signed SHA-256 proof.
/// Supports both raw binaries and `.tar.gz` archives (auto-detected from URL).
fn download_binary(
    dir: &std::path::Path,
    url: &str,
    sha256: Option<&str>,
    signature: Option<&str>,
    source_commit: Option<&str>,
    reviewed_at: Option<&str>,
) -> Result<bool> {
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?
        .get(url)
        .send()?;

    if !response.status().is_success() {
        return Ok(false);
    }

    // R-24: the 120s timeout bounds TIME, not bytes — stream the body under a
    // hard byte cap so a hostile endpoint cannot exhaust memory mid-download.
    let bytes = octos_core::bounded_read::read_to_end_capped(
        response,
        octos_core::bounded_read::SKILL_DOWNLOAD_CAP_BYTES,
    )
    .wrap_err_with(|| {
        format!(
            "skill binary download exceeded {} bytes",
            octos_core::bounded_read::SKILL_DOWNLOAD_CAP_BYTES
        )
    })?;
    let package = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| eyre::eyre!("skill install directory has no valid package name"))?;
    install_attested_bytes_into_dir(
        dir,
        url,
        &bytes,
        RemoteBinaryAttestation {
            sha256,
            signature,
            package,
            platform: &platform_key(),
            source_commit,
            reviewed_at,
            trusted_public_key: std::env::var(octos_plugin::integrity::HUB_TRUST_PUBLIC_KEY_ENV)
                .ok()
                .as_deref(),
        },
    )
}

/// Proof and identity context for one remote binary install.
#[doc(hidden)]
pub type RemoteBinaryAttestation<'a> = octos_plugin::integrity::RemoteBinaryProof<'a>;

/// Install in-memory remote bytes into `dir` (writes `<dir>/main`) only after
/// their Ed25519 signature and SHA-256 both verify against an out-of-band
/// trust root. The URL is consulted only for archive detection.
///
/// `Ok(false)` is reserved for an archive with no real files. Missing/bad
/// proof, mismatched bytes and I/O failures are errors and never fall through
/// to source execution. The digest covers the exact archive/raw bytes.
#[doc(hidden)]
pub fn install_attested_bytes_into_dir(
    dir: &std::path::Path,
    url: &str,
    bytes: &[u8],
    attestation: RemoteBinaryAttestation<'_>,
) -> Result<bool> {
    octos_plugin::integrity::verify_remote_binary(bytes, attestation)?;

    let dest = dir.join("main");

    if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        // Extract the first real file from the tar.gz archive.
        // Skip macOS AppleDouble resource fork files (._* prefix) which
        // appear before the actual binary in archives created on macOS.
        let gz = flate2::read::GzDecoder::new(bytes);
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
            // R-24: cap the decompressed member (zip-bomb guard) BEFORE
            // writing — a capped read that overflows fails loudly and nothing
            // partial lands on disk.
            let buf = octos_core::bounded_read::read_to_end_capped(
                &mut entry,
                octos_core::bounded_read::ARCHIVE_ENTRY_CAP_BYTES,
            )
            .wrap_err("archive member exceeded the extraction byte cap")?;
            std::fs::write(&dest, &buf)?;
            found = true;
            break;
        }
        if !found {
            return Ok(false);
        }
    } else {
        std::fs::write(&dest, bytes)?;
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
    use super::run_skill_command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// R-16: a stuck install subprocess must be killed past its deadline
    /// instead of parking the install forever.
    #[cfg(unix)]
    #[test]
    fn skill_command_kills_child_past_deadline() {
        let abort = AtomicBool::new(false);
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");

        let start = Instant::now();
        let error = run_skill_command(&mut cmd, Duration::from_millis(200), &abort)
            .expect_err("a 30s sleep must not survive a 200ms deadline");

        assert!(error.contains("timed out"), "unexpected error: {error}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "deadline must kill the child promptly, took {:?}",
            start.elapsed()
        );
    }

    /// R-16: setting the abort flag (tool timeout / cancel) kills the running
    /// child instead of leaving it writing into the skills directory.
    #[cfg(unix)]
    #[test]
    fn skill_command_kills_child_when_aborted() {
        let abort = AtomicBool::new(false);
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");

        let flag = &abort;
        let trigger = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                flag.store(true, Ordering::Relaxed);
            });
            run_skill_command(&mut cmd, Duration::from_secs(60), flag)
        });

        let error = trigger.expect_err("an aborted install must not wait for the child");
        assert!(error.contains("cancelled"), "unexpected error: {error}");
    }

    #[cfg(unix)]
    #[test]
    fn skill_command_returns_child_exit_status() {
        let abort = AtomicBool::new(false);
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "exit 3"]);

        let status = run_skill_command(&mut cmd, Duration::from_secs(5), &abort)
            .expect("fast command must succeed");

        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn copy_dir_recursive_skips_symlinked_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        std::fs::write(secret.path().join("secret.txt"), "outside").unwrap();
        let src = tmp.path().join("skill");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("ok.txt"), "inside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(secret.path(), src.join("sub/escape")).unwrap();
        let dst = tmp.path().join("installed");
        copy_dir_recursive(&src, &dst).unwrap();
        assert!(dst.join("ok.txt").exists());
        assert!(
            !dst.join("sub/escape").exists(),
            "symlinked entries must not be followed into the install (#2680)"
        );
        assert!(
            !std::fs::read_to_string(dst.join("sub/escape/secret.txt"))
                .map(|c| c.contains("outside"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn maybe_install_binary_cargo_fallback_is_gated_behind_the_operator_opt_in() {
        // #2321: a source build executes the cloned repo's build.rs — the
        // fallback must be an explicit operator opt-in, mirroring the npm
        // gate. Without OCTOS_SKILLS_CARGO_BUILD=1 no cargo build runs: a
        // broken Cargo.toml would otherwise fail loudly via cargo; here the
        // gate returns before spawning, so the malformed manifest stays
        // silently unattempted and no target/ dir appears.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skill-src");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "not a valid manifest @@@").unwrap();
        std::fs::write(dir.join("manifest.json"), r#"{"binaries": {}}"#).unwrap();
        maybe_cargo_build_with(&dir, false, &AtomicBool::new(false));
        assert!(
            !dir.join("target").exists(),
            "cargo must not be spawned without the opt-in"
        );
        assert!(!dir.join("main").exists());
    }

    #[test]
    fn cargo_build_that_produces_no_located_binary_falls_back_to_the_launcher() {
        // Review finding on this PR: a successful `cargo build` that the
        // name-scan heuristic cannot map to a produced binary must return
        // `false` (so the caller installs the lazy launcher), not `true`.
        // Pinned with a real opt-in build of a workspace-shaped skill: the
        // root manifest has no `name` line at all, the member crate builds
        // to target/release/<member>, and no scan line matches it.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skill-src");
        std::fs::create_dir_all(dir.join("member/src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"member\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("member/Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("member/src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("manifest.json"), r#"{"binaries": {}}"#).unwrap();
        assert!(
            !maybe_cargo_build_with(&dir, true, &AtomicBool::new(false)),
            "no binary was copied to dir/main, so the fallback must run"
        );
        assert!(
            !dir.join("main").exists(),
            "the copy path must not have fired for the renamed binary"
        );
    }

    #[test]
    fn maybe_npm_install_is_gated_behind_the_operator_opt_in() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        // The opt-in must be explicit: without it the gate returns before
        // spawning npm (a postinstall would run arbitrary code otherwise).
        maybe_npm_install_with(tmp.path(), false, &AtomicBool::new(false));
        // Without the opt-in the node_modules dir is not created and no npm
        // process was spawned (the gate returns before the Command).
        assert!(!tmp.path().join("node_modules").exists());
    }
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

    use crate::tools::ToolApprovalRequester;

    struct FixedDecisionRequester {
        decision: ToolApprovalDecision,
    }

    #[async_trait]
    impl ToolApprovalRequester for FixedDecisionRequester {
        async fn request_approval(&self, _request: ToolApprovalRequest) -> ToolApprovalDecision {
            // ToolApprovalDecision is Copy — returning the field directly
            // avoids the clippy::clone_on_copy hit under -D warnings.
            self.decision
        }
    }

    #[tokio::test]
    async fn remove_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(tmp.path());
        // Mutating actions pass the confirmation gate first; traversal
        // validation still runs inside the gated path.
        let requester = std::sync::Arc::new(FixedDecisionRequester {
            decision: ToolApprovalDecision::Approve,
        });
        let result = TOOL_APPROVAL_CTX
            .scope(requester, async {
                tool.execute(&serde_json::json!({"action": "remove", "name": "../../etc"}))
                    .await
                    .unwrap()
            })
            .await;
        assert!(!result.success);
        assert!(result.output.contains("Invalid"));
    }

    #[tokio::test]
    async fn install_missing_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(tmp.path());
        let requester = std::sync::Arc::new(FixedDecisionRequester {
            decision: ToolApprovalDecision::Approve,
        });
        let result = TOOL_APPROVAL_CTX
            .scope(requester, async {
                tool.execute(&serde_json::json!({"action": "install"}))
                    .await
                    .unwrap()
            })
            .await;
        assert!(!result.success);
        assert!(result.output.contains("repo is required"));
    }

    #[tokio::test]
    async fn install_rejects_invalid_repo_spec_before_clone() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(tmp.path());
        let requester = std::sync::Arc::new(FixedDecisionRequester {
            decision: ToolApprovalDecision::Approve,
        });
        let result = TOOL_APPROVAL_CTX
            .scope(requester, async {
                tool.execute(&serde_json::json!({
                    "action": "install",
                    "repo": "owner/repo/."
                }))
                .await
                .unwrap()
            })
            .await;

        assert!(!result.success);
        assert!(result.output.contains("owner/repo/."));
        assert!(result.output.contains("segment"));
    }

    #[test]
    fn repo_spec_rejects_missing_normal_final_segment() {
        let error = parse_repo_spec("owner/repo/.").unwrap_err();

        assert!(error.contains("owner/repo/."), "unexpected error: {error}");
        assert!(error.contains("segment"), "unexpected error: {error}");
    }

    #[test]
    fn repo_spec_rejects_invalid_segment_counts_and_components() {
        for repo in [
            "owner/repo/",
            "owner/repo//skill",
            "owner/repo/..",
            r"owner/repo/foo\bar",
            r"owner/repo/foo\..",
            "owner",
        ] {
            let error = parse_repo_spec(repo).unwrap_err();
            assert!(error.contains(repo), "unexpected error: {error}");
            assert!(error.contains("segment"), "unexpected error: {error}");
        }
    }

    #[test]
    fn repo_spec_preserves_valid_install_names_and_subdirectories() {
        let whole = parse_repo_spec("owner/repo").unwrap();
        assert_eq!(whole.owner, "owner");
        assert_eq!(whole.repository, "repo");
        assert_eq!(whole.subdir, None);
        assert_eq!(whole.install_name, "repo");

        let nested = parse_repo_spec("owner/repo/tools/工具").unwrap();
        assert_eq!(nested.subdir.as_deref(), Some("tools/工具"));
        assert_eq!(nested.install_name, "工具");
    }

    #[tokio::test]
    async fn install_without_approval_bridge_denies_fail_closed() {
        // #2321: no TOOL_APPROVAL_CTX scoped (headless path) ⇒ a mutating
        // action must NOT run.
        let tmp = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(tmp.path());
        let result = tool
            .execute(&serde_json::json!({
                "action": "install",
                "repo": "evil/repo"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("requires interactive approval"));
    }

    #[tokio::test]
    async fn install_denied_by_interactive_approval_does_not_run() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(tmp.path());
        let requester = std::sync::Arc::new(FixedDecisionRequester {
            decision: ToolApprovalDecision::Deny,
        });
        let result = TOOL_APPROVAL_CTX
            .scope(requester, async {
                tool.execute(&serde_json::json!({
                    "action": "install",
                    "repo": "evil/repo"
                }))
                .await
                .unwrap()
            })
            .await;
        assert!(!result.success);
        assert!(result.output.contains("denied by interactive approval"));
    }

    #[tokio::test]
    async fn list_and_search_stay_ungated() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(tmp.path());
        // No bridge scoped — read-only actions must not require one.
        let listed = tool
            .execute(&serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(listed.success);
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

    #[test]
    fn platform_key_matches_manifest_convention() {
        let key = platform_key();
        #[cfg(target_os = "macos")]
        assert_eq!(key, format!("darwin-{}", std::env::consts::ARCH));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            key,
            format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
        );
    }

    #[test]
    fn remote_bundled_executable_requires_signed_manifest_proof() {
        use sha2::Digest as _;
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("demo-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("main"), b"remote executable").unwrap();
        let digest = format!("{:x}", sha2::Sha256::digest(b"remote executable"));
        std::fs::write(
            skill_dir.join("manifest.json"),
            format!(r#"{{"name":"demo-skill","tools":[],"sha256":"{digest}"}}"#),
        )
        .unwrap();
        let _ = maybe_install_binary(&skill_dir, &AtomicBool::new(false))
            .expect_err("a copied repository executable must not bypass the proof gate");
    }

    #[test]
    fn malformed_remote_binary_proof_is_an_error_not_a_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("demo-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("manifest.json"),
            r#"{"name":"demo-skill","tools":[],"binaries":{"linux-x86_64":{"url":"https://example.invalid/x","sha256":123}}}"#,
        )
        .unwrap();
        let _ = maybe_install_binary(&skill_dir, &AtomicBool::new(false))
            .expect_err("wrong-typed proof metadata must be rejected");
    }

    #[test]
    fn empty_declared_binary_url_is_an_error_not_a_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("demo-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("manifest.json"),
            format!(
                r#"{{"name":"demo-skill","tools":[],"binaries":{{"{}":{{"url":"  ","sha256":"{}","signature":"eA=="}}}}}}"#,
                platform_key(),
                "0".repeat(64),
            ),
        )
        .unwrap();
        let _ = maybe_install_binary(&skill_dir, &AtomicBool::new(false))
            .expect_err("an empty declared URL is corrupt metadata");
    }

    #[cfg(unix)]
    #[test]
    fn maybe_install_binary_generates_mofa_publish_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("mofa-publish");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("manifest.json"),
            r#"{
  "name":"mofa-publish",
  "version":"0.1.0",
  "tools":[{"name":"mofa_publish","description":"deploy"}]
}"#,
        )
        .unwrap();
        // mofa-publish now ships as a Cargo-based Rust skill with an explicit
        // [[bin]] name, so it gets the generic lazy-cargo wrapper.
        std::fs::write(
            skill_dir.join("Cargo.toml"),
            r#"[package]
name = "mofa-publish-crate"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "mofa-publish"
path = "src/main.rs"
"#,
        )
        .unwrap();

        maybe_install_binary(&skill_dir, &AtomicBool::new(false)).unwrap();
        let wrapper = std::fs::read_to_string(skill_dir.join("main")).unwrap();
        assert!(wrapper.contains("cargo build --release"));
        assert!(wrapper.contains("target/release/mofa-publish"));
    }

    #[cfg(unix)]
    #[test]
    fn maybe_install_binary_generates_mofa_site_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("mofa-site");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("manifest.json"),
            r#"{
  "name":"mofa-site",
  "version":"0.1.0",
  "tools":[{"name":"mofa_site","description":"site"}]
}"#,
        )
        .unwrap();
        // mofa-site now ships as a Cargo-based Rust skill with an explicit
        // [[bin]] name, so it gets the generic lazy-cargo wrapper.
        std::fs::write(
            skill_dir.join("Cargo.toml"),
            r#"[package]
name = "mofa-site-crate"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "mofa-site"
path = "src/main.rs"
"#,
        )
        .unwrap();

        maybe_install_binary(&skill_dir, &AtomicBool::new(false)).unwrap();
        let wrapper = std::fs::read_to_string(skill_dir.join("main")).unwrap();
        assert!(wrapper.contains("cargo build --release"));
        assert!(wrapper.contains("target/release/mofa-site"));
    }

    #[cfg(unix)]
    #[test]
    fn maybe_install_binary_generates_lazy_cargo_wrapper_for_mofa_podcast() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("mofa-podcast");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("manifest.json"),
            r#"{
  "name":"mofa-podcast",
  "version":"0.4.5",
  "tools":[{"name":"podcast_generate","description":"podcast"}]
}"#,
        )
        .unwrap();
        std::fs::write(
            skill_dir.join("Cargo.toml"),
            r#"[package]
name = "mofa-podcast"
version = "0.4.5"
edition = "2021"
"#,
        )
        .unwrap();

        maybe_install_binary(&skill_dir, &AtomicBool::new(false)).unwrap();
        let wrapper = std::fs::read_to_string(skill_dir.join("main")).unwrap();
        assert!(wrapper.contains("cargo build --release"));
        assert!(wrapper.contains("target/release/mofa-podcast"));
    }

    #[cfg(unix)]
    #[test]
    fn generated_lazy_cargo_wrapper_does_not_block_binary_install() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("mofa-fm");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("main"),
            r#"#!/usr/bin/env bash
set -euo pipefail
BIN="$SCRIPT_DIR/target/release/mofa-fm"
if [[ ! -x "$BIN" ]]; then
  if ! command -v cargo >/dev/null 2>&1; then
    printf '{"output":"Skill binary is missing and cargo is not installed. Run: cargo build --release in mofa-fm","success":false}\n'
    exit 0
  fi
  cargo build --release
fi
"#,
        )
        .unwrap();

        assert!(!has_installed_skill_executable(&skill_dir, "mofa-fm"));
    }

    #[cfg(unix)]
    #[test]
    fn real_main_executable_blocks_binary_reinstall() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("mofa-fm");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("main"), "#!/usr/bin/env bash\necho ok\n").unwrap();

        assert!(has_installed_skill_executable(&skill_dir, "mofa-fm"));
    }
}
