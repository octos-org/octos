//! Pipeline file discovery — finds .dot pipeline files from standard locations.

use std::path::{Path, PathBuf};

use eyre::Result;

/// Information about a discoverable pipeline.
pub struct PipelineInfo {
    pub name: String,
    pub path: PathBuf,
}

/// Discovers pipeline files from standard locations.
pub struct PipelineDiscovery {
    search_paths: Vec<PathBuf>,
}

impl PipelineDiscovery {
    pub fn new(data_dir: &Path, working_dir: &Path) -> Self {
        Self {
            search_paths: vec![
                // Project-level pipelines
                working_dir.join(".crew").join("pipelines"),
                // User-level pipelines
                data_dir.join("pipelines"),
                // Installed skills (each skill dir may contain .dot files)
                data_dir.join("skills"),
            ],
        }
    }

    /// Add additional search paths (e.g. global crew_home/skills/).
    pub fn add_search_path(&mut self, path: PathBuf) {
        if !self.search_paths.contains(&path) {
            self.search_paths.push(path);
        }
    }

    /// List all discoverable pipelines.
    pub fn list_available(&self) -> Vec<PipelineInfo> {
        let mut pipelines = Vec::new();

        for dir in &self.search_paths {
            // Direct .dot files in the directory
            scan_dot_files(dir, &mut pipelines);

            // Also scan one level deeper (skills/<name>/*.dot)
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let sub = entry.path();
                    if sub.is_dir() {
                        scan_dot_files(&sub, &mut pipelines);
                    }
                }
            }
        }

        pipelines
    }

    /// Resolve a pipeline name, path, or inline DOT content to its DOT string.
    pub async fn resolve(&self, name_or_path: &str) -> Result<String> {
        // 0. Check if it's inline DOT content (starts with "digraph")
        let trimmed = name_or_path.trim();
        if trimmed.starts_with("digraph ") || trimmed.starts_with("digraph{") {
            return Ok(name_or_path.to_string());
        }

        // 1. Check if it's a direct file path
        let as_path = PathBuf::from(name_or_path);
        if as_path.exists() && as_path.extension().is_some_and(|e| e == "dot") {
            return tokio::fs::read_to_string(&as_path)
                .await
                .map_err(|e| eyre::eyre!("failed to read pipeline file: {e}"));
        }

        // 2. Check if it's a relative path like "mofa-research/deep_research.dot"
        for dir in &self.search_paths {
            let candidate = dir.join(name_or_path);
            if candidate.exists() {
                return tokio::fs::read_to_string(&candidate)
                    .await
                    .map_err(|e| eyre::eyre!("failed to read pipeline file: {e}"));
            }
            // Try with .dot extension
            let with_ext = dir.join(format!("{name_or_path}.dot"));
            if with_ext.exists() {
                return tokio::fs::read_to_string(&with_ext)
                    .await
                    .map_err(|e| eyre::eyre!("failed to read pipeline file: {e}"));
            }
        }

        // 3. Search by bare name across all paths (including nested skill dirs)
        let all = self.list_available();
        for info in &all {
            if info.name == name_or_path {
                return tokio::fs::read_to_string(&info.path)
                    .await
                    .map_err(|e| eyre::eyre!("failed to read pipeline file: {e}"));
            }
        }

        let available: Vec<_> = all.iter().map(|p| p.name.as_str()).collect();
        eyre::bail!(
            "pipeline '{}' not found. Available: {}",
            name_or_path,
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        )
    }
}

fn scan_dot_files(dir: &Path, pipelines: &mut Vec<PipelineInfo>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "dot") {
                let name = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if !pipelines.iter().any(|p| p.name == name) {
                    pipelines.push(PipelineInfo { name, path });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_resolve_inline_dot() {
        let discovery = PipelineDiscovery::new(Path::new("/tmp"), Path::new("/tmp"));
        let dot = "digraph test { a [prompt=\"hello\"] }";
        let result = discovery.resolve(dot).await.unwrap();
        assert_eq!(result, dot);
    }

    #[tokio::test]
    async fn should_resolve_inline_dot_with_whitespace() {
        let discovery = PipelineDiscovery::new(Path::new("/tmp"), Path::new("/tmp"));
        let dot = "  digraph research {\n  search -> analyze\n}";
        let result = discovery.resolve(dot).await.unwrap();
        assert_eq!(result, dot);
    }
}
