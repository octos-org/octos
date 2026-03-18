//! Workspace skills loader.
//!
//! Loads skills from `.octos/skills/{name}/SKILL.md` with simple frontmatter.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};

use crate::builtin_skills::BUILTIN_SKILLS;

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
}

impl SkillsLoader {
    /// Create a new loader for the given data directory.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            skills_dirs: vec![data_dir.as_ref().join("skills")],
        }
    }

    /// Add an additional skills directory (appends `/skills` to the given dir).
    /// Skills from earlier-added directories take priority over later ones.
    pub fn add_skills_dir(&mut self, dir: impl AsRef<Path>) {
        let path = dir.as_ref().join("skills");
        // Avoid duplicates
        if !self.skills_dirs.contains(&path) {
            self.skills_dirs.push(path);
        }
    }

    /// Add a raw skills directory path (no `/skills` suffix appended).
    /// Used for layered dirs like `platform-skills/` and `bundled-app-skills/`.
    pub fn add_skills_path(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref().to_path_buf();
        if !self.skills_dirs.contains(&path) {
            self.skills_dirs.push(path);
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

        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(skills)
    }

    /// Load a specific skill's full content (without frontmatter).
    ///
    /// Checks skills directories in priority order (first added = highest priority),
    /// then falls back to built-in system skills.
    pub async fn load_skill(&self, name: &str) -> Result<Option<String>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup_skills_dir(dir: &TempDir) -> PathBuf {
        let skills = dir.path().join("skills");
        tokio::fs::create_dir_all(&skills).await.unwrap();
        skills
    }

    #[tokio::test]
    async fn test_empty_dir_has_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let loader = SkillsLoader::new(dir.path());
        let skills = loader.list_skills().await.unwrap();
        // Empty workspace dir still has built-in system skills
        assert!(skills.iter().all(|s| s.builtin));
        assert!(skills.iter().any(|s| s.name == "cron"));
        assert!(skills.iter().any(|s| s.name == "skill-store"));
        assert!(skills.iter().any(|s| s.name == "skill-creator"));
    }

    #[tokio::test]
    async fn test_list_and_load_skill() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = setup_skills_dir(&dir).await;

        let skill_dir = skills_dir.join("greet");
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        tokio::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: greet\ndescription: Say hello\nalways: false\n---\nYou are a greeter.\n",
        )
        .await
        .unwrap();

        let loader = SkillsLoader::new(dir.path());
        let skills = loader.list_skills().await.unwrap();
        let greet = skills.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(greet.description, "Say hello");
        assert!(!greet.always);
        assert!(greet.available);
        assert!(!greet.builtin);

        let content = loader.load_skill("greet").await.unwrap().unwrap();
        assert_eq!(content, "You are a greeter.\n");
    }

    #[tokio::test]
    async fn test_xml_summary() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = setup_skills_dir(&dir).await;

        let skill_dir = skills_dir.join("test-skill");
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        tokio::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test\n---\nBody.\n",
        )
        .await
        .unwrap();

        let loader = SkillsLoader::new(dir.path());
        let summary = loader.build_skills_summary().await.unwrap();
        assert!(summary.contains("<skills>"));
        assert!(summary.contains("<name>test-skill</name>"));
        assert!(summary.contains("<description>A test</description>"));
    }

    #[tokio::test]
    async fn test_always_filtering() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = setup_skills_dir(&dir).await;

        for (name, always) in &[("a", "true"), ("b", "false"), ("c", "true")] {
            let sd = skills_dir.join(name);
            tokio::fs::create_dir_all(&sd).await.unwrap();
            tokio::fs::write(
                sd.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: d\nalways: {always}\n---\nbody\n"),
            )
            .await
            .unwrap();
        }

        let loader = SkillsLoader::new(dir.path());
        let always = loader.get_always_skills().await.unwrap();
        assert!(always.contains(&"a".to_string()));
        assert!(always.contains(&"c".to_string()));
        assert!(!always.contains(&"b".to_string()));
    }

    #[test]
    fn test_frontmatter_parsing() {
        let content = "---\nname: foo\ndescription: bar\nalways: true\n---\nBody here.\n";
        let (fm, body) = split_frontmatter(content);
        let fm = fm.unwrap();
        assert_eq!(fm_value(&fm, "name").unwrap(), "foo");
        assert_eq!(fm_value(&fm, "description").unwrap(), "bar");
        assert_eq!(fm_value(&fm, "always").unwrap(), "true");
        assert_eq!(body, "Body here.\n");
    }

    #[test]
    fn test_no_frontmatter() {
        let content = "Just some text.";
        let (fm, body) = split_frontmatter(content);
        assert!(fm.is_none());
        assert_eq!(body, content);
    }

    #[test]
    fn test_version_author_parsing() {
        let content =
            "---\nname: my-skill\ndescription: Does X\nversion: 1.2.3\nauthor: alice\n---\nBody.\n";
        let path = PathBuf::from("/tmp/fake/my-skill/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        assert_eq!(info.version.as_deref(), Some("1.2.3"));
        assert_eq!(info.author.as_deref(), Some("alice"));
        assert!(!info.has_tools);
    }

    #[tokio::test]
    async fn test_has_tools_detection() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = setup_skills_dir(&dir).await;

        // Skill without tools
        let plain_dir = skills_dir.join("plain");
        tokio::fs::create_dir_all(&plain_dir).await.unwrap();
        tokio::fs::write(
            plain_dir.join("SKILL.md"),
            "---\nname: plain\ndescription: No tools\n---\nBody\n",
        )
        .await
        .unwrap();

        // Skill with tools (has manifest.json)
        let tool_dir = skills_dir.join("with-tools");
        tokio::fs::create_dir_all(&tool_dir).await.unwrap();
        tokio::fs::write(
            tool_dir.join("SKILL.md"),
            "---\nname: with-tools\ndescription: Has tools\n---\nBody\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            tool_dir.join("manifest.json"),
            r#"{"name": "with-tools", "version": "1.0", "tools": []}"#,
        )
        .await
        .unwrap();

        let loader = SkillsLoader::new(dir.path());
        let skills = loader.list_skills().await.unwrap();
        let plain = skills.iter().find(|s| s.name == "plain").unwrap();
        assert!(!plain.has_tools);
        let with_tools = skills.iter().find(|s| s.name == "with-tools").unwrap();
        assert!(with_tools.has_tools);
    }

    // --- Pure function tests for split_frontmatter ---

    #[test]
    fn test_split_frontmatter_leading_whitespace() {
        // Leading whitespace before --- should still parse
        let content = "  \n---\nname: foo\n---\nBody\n";
        let (fm, body) = split_frontmatter(content);
        let fm = fm.unwrap();
        assert_eq!(fm_value(&fm, "name").unwrap(), "foo");
        assert_eq!(body, "Body\n");
    }

    #[test]
    fn test_split_frontmatter_unclosed() {
        // Only one --- means no valid frontmatter
        let content = "---\nname: foo\nno closing fence";
        let (fm, body) = split_frontmatter(content);
        assert!(fm.is_none());
        assert_eq!(body, content);
    }

    #[test]
    fn test_split_frontmatter_empty_body() {
        let content = "---\nname: foo\n---\n";
        let (fm, body) = split_frontmatter(content);
        assert!(fm.is_some());
        assert!(body.is_empty() || body.trim().is_empty());
    }

    #[test]
    fn test_split_frontmatter_empty_frontmatter() {
        // Empty frontmatter (no lines between ---) doesn't parse because
        // the parser requires "\n---" which needs at least one line.
        let content = "---\n---\nBody only\n";
        let (fm, body) = split_frontmatter(content);
        assert!(fm.is_none());
        assert_eq!(body, content);
    }

    #[test]
    fn test_split_frontmatter_multiline_body() {
        let content = "---\nkey: val\n---\nLine 1\nLine 2\nLine 3\n";
        let (fm, body) = split_frontmatter(content);
        assert!(fm.is_some());
        assert!(body.contains("Line 1"));
        assert!(body.contains("Line 3"));
    }

    #[test]
    fn test_split_frontmatter_dashes_in_body() {
        // --- in the body (not at frontmatter position) should not interfere
        let content = "---\nname: test\n---\nSome text\n---\nMore text\n";
        let (fm, body) = split_frontmatter(content);
        let fm = fm.unwrap();
        assert_eq!(fm_value(&fm, "name").unwrap(), "test");
        // Body should contain the --- from the content
        assert!(body.contains("---"));
    }

    // --- Pure function tests for fm_value ---

    #[test]
    fn test_fm_value_missing_key() {
        let lines = vec!["name: foo".to_string(), "description: bar".to_string()];
        assert!(fm_value(&lines, "version").is_none());
    }

    #[test]
    fn test_fm_value_with_extra_whitespace() {
        let lines = vec!["  name:   spaced value  ".to_string()];
        assert_eq!(fm_value(&lines, "name").unwrap(), "spaced value");
    }

    #[test]
    fn test_fm_value_empty_value() {
        // Empty value is treated as absent (returns None)
        let lines = vec!["name:".to_string()];
        assert!(fm_value(&lines, "name").is_none());
    }

    #[test]
    fn test_fm_value_yaml_empty_markers() {
        // YAML empty markers are treated as absent
        assert!(fm_value(&["requires_bins: []".into()], "requires_bins").is_none());
        assert!(fm_value(&["requires_bins: ~".into()], "requires_bins").is_none());
        assert!(fm_value(&[r#"requires_bins: """#.into()], "requires_bins").is_none());
    }

    #[test]
    fn test_fm_value_inline_comment() {
        // Inline YAML comments are stripped
        let lines = vec!["requires_bins: []   # DOT-based pipeline".into()];
        assert!(fm_value(&lines, "requires_bins").is_none());

        let lines = vec!["model: gpt-4o # best model".into()];
        assert_eq!(fm_value(&lines, "model").unwrap(), "gpt-4o");
    }

    #[test]
    fn test_fm_value_colon_in_value() {
        let lines = vec!["description: key: value pair".to_string()];
        assert_eq!(fm_value(&lines, "description").unwrap(), "key: value pair");
    }

    #[test]
    fn test_fm_value_empty_lines() {
        let lines: Vec<String> = vec![];
        assert!(fm_value(&lines, "name").is_none());
    }

    #[test]
    fn test_fm_value_duplicate_keys_returns_first() {
        let lines = vec!["name: first".to_string(), "name: second".to_string()];
        assert_eq!(fm_value(&lines, "name").unwrap(), "first");
    }

    // --- Pure function tests for strip_frontmatter ---

    #[test]
    fn test_strip_frontmatter_with_fm() {
        let content = "---\nname: foo\n---\nBody text\n";
        assert_eq!(strip_frontmatter(content), "Body text\n");
    }

    #[test]
    fn test_strip_frontmatter_without_fm() {
        let content = "Just plain text\n";
        assert_eq!(strip_frontmatter(content), "Just plain text\n");
    }

    #[test]
    fn test_strip_frontmatter_empty() {
        assert_eq!(strip_frontmatter(""), "");
    }

    // --- Pure function tests for parse_skill ---

    #[test]
    fn test_parse_skill_minimal() {
        let content = "---\ndescription: A skill\n---\nBody\n";
        let path = PathBuf::from("/fake/my-skill/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        // Name falls back to parent directory name
        assert_eq!(info.name, "my-skill");
        assert_eq!(info.description, "A skill");
        assert!(!info.always);
        assert!(info.available);
        assert!(!info.builtin);
    }

    #[test]
    fn test_parse_skill_builtin_flag() {
        let content = "---\nname: test\ndescription: builtin\n---\nBody\n";
        let path = PathBuf::from("<builtin>/test/SKILL.md");
        let info = parse_skill(&path, content, true).unwrap();
        assert!(info.builtin);
        // builtins never have has_tools
        assert!(!info.has_tools);
    }

    #[test]
    fn test_parse_skill_no_frontmatter_returns_none() {
        let content = "Just text, no frontmatter";
        let path = PathBuf::from("/fake/skill/SKILL.md");
        assert!(parse_skill(&path, content, false).is_none());
    }

    #[test]
    fn test_parse_skill_always_true() {
        let content = "---\nname: auto\ndescription: runs always\nalways: true\n---\nBody\n";
        let path = PathBuf::from("/fake/auto/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        assert!(info.always);
    }

    #[test]
    fn test_parse_skill_always_non_true_is_false() {
        let content = "---\nname: nope\ndescription: d\nalways: yes\n---\nBody\n";
        let path = PathBuf::from("/fake/nope/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        assert!(!info.always);
    }

    #[test]
    fn test_parse_skill_requires_env_missing() {
        let content = "---\nname: envskill\ndescription: d\nrequires_env: OCTOS_NONEXISTENT_VAR_XYZ_99\n---\nB\n";
        let path = PathBuf::from("/fake/envskill/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        assert!(!info.available);
    }

    #[test]
    fn test_parse_skill_requires_env_multiple_one_missing() {
        // HOME should exist, but OCTOS_NONEXISTENT should not
        let content = "---\nname: envskill\ndescription: d\nrequires_env: HOME, OCTOS_NONEXISTENT_VAR_XYZ_99\n---\nB\n";
        let path = PathBuf::from("/fake/envskill/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        assert!(!info.available);
    }

    #[test]
    fn test_parse_skill_requires_bins_common() {
        // "ls" should exist on any Unix system
        let content = "---\nname: bintest\ndescription: d\nrequires_bins: ls\n---\nB\n";
        let path = PathBuf::from("/fake/bintest/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        assert!(info.available);
    }

    #[test]
    fn test_parse_skill_requires_bins_missing() {
        let content = "---\nname: bintest\ndescription: d\nrequires_bins: nonexistent_binary_xyz_999\n---\nB\n";
        let path = PathBuf::from("/fake/bintest/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        assert!(!info.available);
    }

    #[test]
    fn test_parse_skill_name_fallback_from_path() {
        // No name in frontmatter, should use parent dir name
        let content = "---\ndescription: fallback test\n---\nBody\n";
        let path = PathBuf::from("/data/skills/my-cool-skill/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        assert_eq!(info.name, "my-cool-skill");
    }

    #[tokio::test]
    async fn test_multi_dir_priority() {
        // Set up two directories: "global" and "profile"
        let global_dir = tempfile::tempdir().unwrap();
        let profile_dir = tempfile::tempdir().unwrap();
        let global_skills = setup_skills_dir(&global_dir).await;
        let profile_skills = setup_skills_dir(&profile_dir).await;

        // Global has skill "shared" with description "global version"
        let sd = global_skills.join("shared");
        tokio::fs::create_dir_all(&sd).await.unwrap();
        tokio::fs::write(
            sd.join("SKILL.md"),
            "---\nname: shared\ndescription: global version\n---\nGlobal body\n",
        )
        .await
        .unwrap();

        // Global has skill "global-only"
        let sd = global_skills.join("global-only");
        tokio::fs::create_dir_all(&sd).await.unwrap();
        tokio::fs::write(
            sd.join("SKILL.md"),
            "---\nname: global-only\ndescription: only in global\n---\nBody\n",
        )
        .await
        .unwrap();

        // Profile has skill "shared" with description "profile version" (overrides global)
        let sd = profile_skills.join("shared");
        tokio::fs::create_dir_all(&sd).await.unwrap();
        tokio::fs::write(
            sd.join("SKILL.md"),
            "---\nname: shared\ndescription: profile version\n---\nProfile body\n",
        )
        .await
        .unwrap();

        // Profile has skill "profile-only"
        let sd = profile_skills.join("profile-only");
        tokio::fs::create_dir_all(&sd).await.unwrap();
        tokio::fs::write(
            sd.join("SKILL.md"),
            "---\nname: profile-only\ndescription: only in profile\n---\nBody\n",
        )
        .await
        .unwrap();

        // Profile dir first (higher priority), then global
        let mut loader = SkillsLoader::new(profile_dir.path());
        loader.add_skills_dir(global_dir.path());
        let skills = loader.list_skills().await.unwrap();

        // "shared" should use profile version
        let shared = skills.iter().find(|s| s.name == "shared").unwrap();
        assert_eq!(shared.description, "profile version");

        // Both unique skills should be present
        assert!(skills.iter().any(|s| s.name == "global-only"));
        assert!(skills.iter().any(|s| s.name == "profile-only"));

        // load_skill should return profile version
        let content = loader.load_skill("shared").await.unwrap().unwrap();
        assert_eq!(content, "Profile body\n");

        // load_skill should find global-only skill
        let content = loader.load_skill("global-only").await.unwrap().unwrap();
        assert_eq!(content, "Body\n");
    }
}
