//! System prompt layering with AGENTS.md auto-discovery.
//!
//! Builds a layered system prompt from multiple sources:
//! 1. Base prompt (compiled-in `worker.txt` or config override)
//! 2. Project instructions (CLAUDE.md, .octos/instructions.md)
//! 3. AGENTS.md agent directory (auto-discovered)

use std::path::Path;

/// Maximum file size for discovered prompt files (64 KB).
const MAX_PROMPT_FILE_SIZE: u64 = 64 * 1024;

/// Discovery file names checked in order (first found wins per category).
const AGENTS_FILES: &[&str] = &["AGENTS.md", ".octos/agents.md", "agents.md"];
const PROJECT_INSTRUCTIONS: &[&str] = &[
    "CLAUDE.md",
    ".octos/instructions.md",
    ".claude/instructions.md",
];

/// A layered system prompt builder.
#[derive(Debug, Default)]
pub struct PromptLayerBuilder {
    base: String,
    project_instructions: Option<String>,
    agents_md: Option<String>,
    extra_layers: Vec<String>,
}

impl PromptLayerBuilder {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            ..Default::default()
        }
    }

    /// Set project instructions content directly.
    pub fn with_project_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.project_instructions = Some(instructions.into());
        self
    }

    /// Set AGENTS.md content directly.
    pub fn with_agents_md(mut self, agents: impl Into<String>) -> Self {
        self.agents_md = Some(agents.into());
        self
    }

    /// Add an extra layer (e.g., skill instructions, runtime context).
    pub fn with_extra(mut self, layer: impl Into<String>) -> Self {
        self.extra_layers.push(layer.into());
        self
    }

    /// Auto-discover and load project files from a working directory.
    /// Files larger than 64 KB are skipped to prevent memory exhaustion.
    pub fn discover(mut self, working_dir: &Path) -> Self {
        // Discover project instructions
        if self.project_instructions.is_none() {
            for name in PROJECT_INSTRUCTIONS {
                if let Some(content) = read_prompt_file(&working_dir.join(name)) {
                    self.project_instructions = Some(content);
                    break;
                }
            }
        }

        // Discover AGENTS.md
        if self.agents_md.is_none() {
            for name in AGENTS_FILES {
                if let Some(content) = read_prompt_file(&working_dir.join(name)) {
                    self.agents_md = Some(content);
                    break;
                }
            }
        }

        self
    }

    /// Build the final layered prompt.
    pub fn build(self) -> String {
        let mut parts = vec![self.base];

        if let Some(instructions) = self.project_instructions {
            parts.push(format!(
                "\n\n## Project Instructions\n\n{}",
                instructions.trim()
            ));
        }

        if let Some(agents) = self.agents_md {
            parts.push(format!("\n\n## Available Agents\n\n{}", agents.trim()));
        }

        for layer in self.extra_layers {
            parts.push(format!("\n\n{}", layer.trim()));
        }

        parts.join("")
    }
}

/// Read a prompt file if it exists, is non-empty, and within size limits.
fn read_prompt_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_PROMPT_FILE_SIZE {
        tracing::warn!(
            path = %path.display(),
            size = meta.len(),
            "skipping oversized prompt file (max {} bytes)",
            MAX_PROMPT_FILE_SIZE
        );
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    Some(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn should_build_base_only() {
        let prompt = PromptLayerBuilder::new("You are a helpful assistant.").build();
        assert_eq!(prompt, "You are a helpful assistant.");
    }

    #[test]
    fn should_layer_project_instructions() {
        let prompt = PromptLayerBuilder::new("Base prompt.")
            .with_project_instructions("Use TDD for all changes.")
            .build();
        assert!(prompt.contains("Base prompt."));
        assert!(prompt.contains("## Project Instructions"));
        assert!(prompt.contains("Use TDD for all changes."));
    }

    #[test]
    fn should_layer_agents_md() {
        let prompt = PromptLayerBuilder::new("Base.")
            .with_agents_md("## @planner\nDecomposes goals.")
            .build();
        assert!(prompt.contains("## Available Agents"));
        assert!(prompt.contains("@planner"));
    }

    #[test]
    fn should_layer_extras() {
        let prompt = PromptLayerBuilder::new("Base.")
            .with_extra("Custom skill instructions.")
            .with_extra("Runtime context info.")
            .build();
        assert!(prompt.contains("Custom skill instructions."));
        assert!(prompt.contains("Runtime context info."));
    }

    #[test]
    fn should_discover_agents_md() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "## @reviewer\nReviews code.").unwrap();

        let prompt = PromptLayerBuilder::new("Base.")
            .discover(dir.path())
            .build();
        assert!(prompt.contains("@reviewer"));
    }

    #[test]
    fn should_discover_claude_md() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "Always use Rust.").unwrap();

        let prompt = PromptLayerBuilder::new("Base.")
            .discover(dir.path())
            .build();
        assert!(prompt.contains("Always use Rust."));
    }

    #[test]
    fn should_skip_empty_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "  \n  ").unwrap();

        let prompt = PromptLayerBuilder::new("Base.")
            .discover(dir.path())
            .build();
        assert!(!prompt.contains("Available Agents"));
    }

    #[test]
    fn should_preserve_layer_order() {
        let prompt = PromptLayerBuilder::new("1. Base")
            .with_project_instructions("2. Instructions")
            .with_agents_md("3. Agents")
            .with_extra("4. Extra")
            .build();

        let base_pos = prompt.find("1. Base").unwrap();
        let instr_pos = prompt.find("2. Instructions").unwrap();
        let agents_pos = prompt.find("3. Agents").unwrap();
        let extra_pos = prompt.find("4. Extra").unwrap();

        assert!(base_pos < instr_pos);
        assert!(instr_pos < agents_pos);
        assert!(agents_pos < extra_pos);
    }
}
