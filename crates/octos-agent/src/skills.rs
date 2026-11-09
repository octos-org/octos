//! Workspace skills loader.
//!
//! Loads skills from `.octos/skills/{name}/SKILL.md` with simple frontmatter.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};

use crate::builtin_skills::BUILTIN_SKILLS;

/// Crate-agnostic skill selection filter.
///
/// This is the lowered form of the CLI's per-profile skill-selection layer
/// (`octos_cli::profiles::ProfileSkillsConfig`). It is intentionally free of
/// any CLI dependency so both the [`SkillsLoader`] (prompt / content injection)
/// and the plugin loader (tool specs) can consult the same selection decision.
///
/// A skill's `id` is its package identifier: the `manifest.json` `name`/`id`,
/// which equals the `SKILL.md` `name` and the skill directory name.
#[derive(Debug, Clone)]
pub enum SkillFilter {
    /// Load every discovered skill except these ids (`AllDiscovered` mode).
    AllExcept(HashSet<String>),
    /// Load only these ids (`AllowList` mode).
    Only(HashSet<String>),
}

impl SkillFilter {
    /// Whether a skill with `id` is permitted to load.
    pub fn allows(&self, id: &str) -> bool {
        match self {
            SkillFilter::AllExcept(disabled) => !disabled.contains(id),
            SkillFilter::Only(enabled) => enabled.contains(id),
        }
    }
}

/// Information about a loaded skill.
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub author: Option<String>,
    pub path: PathBuf,
    pub available: bool,
    pub always: bool,
    /// True for system skills compiled into the binary.
    pub builtin: bool,
    /// True if this skill package includes a manifest.json (provides tools).
    pub has_tools: bool,
}

/// Loads workspace skills from `.octos/skills/`.
///
/// Supports multiple skills directories (e.g. per-profile + global).
/// Earlier directories take priority over later ones for skills with the same name.
pub struct SkillsLoader {
    skills_dirs: Vec<PathBuf>,
    // Packaged defaults stay below every user / explicitly added directory.
    bundled_resources_dir: PathBuf,
    /// Optional skill-selection filter (skill layering v1). `None` ⇒ no
    /// filtering: every discovered skill is listed / loadable exactly as
    /// before this feature existed (backwards-compatible).
    filter: Option<SkillFilter>,
}

impl SkillsLoader {
    /// Instruction/resource roots for the host's read-only skill allowlist.
    /// Hosts must canonicalize these paths before attaching them to a scope.
    pub fn skill_directories(&self) -> &[PathBuf] {
        &self.skills_dirs
    }

    /// Create a new loader for the given data directory.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        let data_dir = data_dir.as_ref();
        let bundled_resources_dir = data_dir.join(crate::resource_skills::DIRECTORY);
        let mut skills_dirs = vec![data_dir.join("skills")];
        match crate::resource_skills::bootstrap(data_dir) {
            Ok(_) => skills_dirs.push(bundled_resources_dir.clone()),
            Err(error) => {
                tracing::warn!(%error, "bundled resource skills could not be prepared");
            }
        }
        Self {
            skills_dirs,
            bundled_resources_dir,
            filter: None,
        }
    }

    /// Attach a skill-selection filter (builder form). Disabled skills are
    /// absent from `list_skills` (hence `build_skills_summary` /
    /// `get_always_skills`) and cannot be loaded by name.
    pub fn with_skill_filter(mut self, filter: Option<SkillFilter>) -> Self {
        self.filter = filter;
        self
    }

    /// Attach a skill-selection filter in place.
    pub fn set_skill_filter(&mut self, filter: Option<SkillFilter>) {
        self.filter = filter;
    }

    /// True when `name` is permitted by the active filter (or no filter).
    fn skill_allowed(&self, name: &str) -> bool {
        self.filter
            .as_ref()
            .is_none_or(|filter| filter.allows(name))
    }

    /// Add an additional skills directory (appends `/skills` to the given dir).
    /// Skills from earlier-added directories take priority over later ones.
    pub fn add_skills_dir(&mut self, dir: impl AsRef<Path>) {
        self.add_skills_path(dir.as_ref().join("skills"));
    }

    /// Add a raw skills directory path (no `/skills` suffix appended).
    /// Used for layered dirs like `platform-skills/` and `bundled-app-skills/`.
    pub fn add_skills_path(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref().to_path_buf();
        if !self.skills_dirs.contains(&path) {
            let defaults = self
                .skills_dirs
                .iter()
                .position(|p| p == &self.bundled_resources_dir)
                .unwrap_or(self.skills_dirs.len());
            self.skills_dirs.insert(defaults, path);
        }
    }

    /// List all skills (built-in system skills + installed workspace skills).
    ///
    /// Priority (highest first): first skills_dir, second skills_dir, ..., builtins.
    pub async fn list_skills(&self) -> Result<Vec<SkillInfo>> {
        let mut skills = Vec::new();

        // Load built-in system skills
        for (name, content) in BUILTIN_SKILLS {
            let path = PathBuf::from(format!("<builtin>/{name}/SKILL.md"));
            if let Some(info) = parse_skill(&path, content, true) {
                skills.push(info);
            }
        }

        // Load workspace skills from all directories (later dirs first so earlier
        // dirs can override them, since we use retain to remove duplicates).
        for skills_dir in self.skills_dirs.iter().rev() {
            let entries = match tokio::fs::read_dir(skills_dir).await {
                Ok(entries) => Some(entries),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    return Err(e).wrap_err_with(|| {
                        format!("failed to read skills directory: {}", skills_dir.display())
                    });
                }
            };

            if let Some(mut entries) = entries {
                while let Some(entry) = entries.next_entry().await? {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }

                    let skill_file = path.join("SKILL.md");
                    if let Ok(content) = tokio::fs::read_to_string(&skill_file).await {
                        if let Some(info) = parse_skill(&skill_file, &content, false) {
                            // Override any existing skill with the same name
                            skills.retain(|s: &SkillInfo| s.name != info.name);
                            skills.push(info);
                        }
                    }
                }
            }
        }

        // Skill layering v1: drop skills disabled by the active selection
        // filter so they are absent from the prompt summary and never treated
        // as always-on. A `None` filter retains everything (backwards-compatible).
        if self.filter.is_some() {
            skills.retain(|s: &SkillInfo| self.skill_allowed(&s.name));
        }

        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(skills)
    }

    /// Load a specific skill's full content (without frontmatter).
    ///
    /// Checks skills directories in priority order (first added = highest priority),
    /// then falls back to built-in system skills.
    pub async fn load_skill(&self, name: &str) -> Result<Option<String>> {
        // Skill layering v1: a disabled skill is not loadable by name — its
        // content must never be injected into the prompt.
        if !self.skill_allowed(name) {
            return Ok(None);
        }
        // Check workspace directories in priority order
        for skills_dir in &self.skills_dirs {
            let skill_file = skills_dir.join(name).join("SKILL.md");
            match tokio::fs::read_to_string(&skill_file).await {
                Ok(content) => return Ok(Some(strip_frontmatter(&content))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).wrap_err_with(|| format!("failed to read skill: {name}")),
            }
        }

        // Fall back to built-in system skills
        for (builtin_name, content) in BUILTIN_SKILLS {
            if *builtin_name == name {
                return Ok(Some(strip_frontmatter(content)));
            }
        }

        Ok(None)
    }

    /// Build an XML summary of all skills for the system prompt.
    pub async fn build_skills_summary(&self) -> Result<String> {
        let skills = self.list_skills().await?;
        if skills.is_empty() {
            return Ok(String::new());
        }

        let mut xml = String::from("<skills>\n");
        for s in &skills {
            let tools_attr = if s.has_tools { " tools=\"true\"" } else { "" };
            xml.push_str(&format!(
                "  <skill available=\"{}\"{}>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>\n",
                s.available, tools_attr, s.name, s.description, s.path.display()
            ));
        }
        xml.push_str("</skills>");
        Ok(xml)
    }

    /// Get names of always-on skills that meet their requirements.
    pub async fn get_always_skills(&self) -> Result<Vec<String>> {
        let skills = self.list_skills().await?;
        Ok(skills
            .into_iter()
            .filter(|s| s.always && s.available)
            .map(|s| s.name)
            .collect())
    }

    /// Load full content (minus frontmatter) for the given skill names, joined by `---`.
    pub async fn load_skills_for_context(&self, names: &[String]) -> Result<String> {
        let mut sections = Vec::new();
        for name in names {
            if let Some(content) = self.load_skill(name).await? {
                sections.push(content);
            }
        }
        Ok(sections.join("\n---\n"))
    }
}

/// Parse skill frontmatter and check requirements.
fn parse_skill(path: &Path, content: &str, builtin: bool) -> Option<SkillInfo> {
    let (fm, _) = split_frontmatter(content);
    let fm = fm?;

    let name = fm_value(&fm, "name").unwrap_or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    });

    let description = fm_value(&fm, "description").unwrap_or_default();
    let always = fm_value(&fm, "always")
        .map(|v| v == "true")
        .unwrap_or(false);

    let bins_ok = fm_value(&fm, "requires_bins")
        .map(|v| {
            v.split(',')
                .map(|b| b.trim())
                .filter(|b| !b.is_empty())
                .all(which_exists)
        })
        .unwrap_or(true);

    let env_ok = fm_value(&fm, "requires_env")
        .map(|v| {
            v.split(',')
                .map(|e| e.trim())
                .filter(|e| !e.is_empty())
                .all(|var| std::env::var(var).is_ok())
        })
        .unwrap_or(true);

    let version = fm_value(&fm, "version");
    let author = fm_value(&fm, "author");
    let has_tools = !builtin
        && path
            .parent()
            .map(|p| p.join("manifest.json").exists())
            .unwrap_or(false);

    Some(SkillInfo {
        name,
        description,
        version,
        author,
        path: path.to_path_buf(),
        available: bins_ok && env_ok,
        always,
        builtin,
        has_tools,
    })
}

/// Split content into (Option<frontmatter_lines>, body).
fn split_frontmatter(content: &str) -> (Option<Vec<String>>, &str) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, content);
    }

    // Find second ---
    let after_first = &trimmed[3..].trim_start_matches(['\r', '\n']);
    if let Some(end) = after_first.find("\n---") {
        let fm_text = &after_first[..end];
        let lines: Vec<String> = fm_text.lines().map(|l| l.to_string()).collect();
        let body_start = end + 4; // skip \n---
        let body = after_first[body_start..].trim_start_matches(['\r', '\n']);
        (Some(lines), body)
    } else {
        (None, content)
    }
}

/// Extract a key from frontmatter lines (simple `key: value` format).
/// Returns `None` for missing keys and YAML empty values (`[]`, `""`, `~`).
fn fm_value(lines: &[String], key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    lines.iter().find_map(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with(&prefix) {
            let mut val = trimmed[prefix.len()..].trim();
            // Strip YAML inline comments (e.g. `[] # comment`)
            if let Some(hash_pos) = val.find('#') {
                val = val[..hash_pos].trim();
            }
            // Treat YAML empty markers as absent
            if val.is_empty() || val == "[]" || val == "\"\"" || val == "~" {
                None
            } else {
                Some(val.to_string())
            }
        } else {
            None
        }
    })
}

/// Strip frontmatter from content, returning only the body.
fn strip_frontmatter(content: &str) -> String {
    let (_, body) = split_frontmatter(content);
    body.to_string()
}

/// Check if a binary exists on PATH.
fn which_exists(bin: &str) -> bool {
    #[cfg(windows)]
    let prog = "where";
    #[cfg(not(windows))]
    let prog = "which";

    std::process::Command::new(prog)
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
