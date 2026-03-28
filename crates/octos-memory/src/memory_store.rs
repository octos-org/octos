//! Markdown-based persistent memory store.
//!
//! Stores long-term memory in `MEMORY.md`, daily notes in `YYYY-MM-DD.md`,
//! and a memory bank of entity pages in `bank/entities/` under `.octos/memory/`.
//!
//! The memory bank provides two-level retrieval:
//! - Level 1: Compact abstracts of all entities (injected into system prompt)
//! - Level 2: Full entity pages (loaded on demand via `recall_memory` tool)

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};

/// Persistent memory store backed by markdown files.
pub struct MemoryStore {
    memory_dir: PathBuf,
}

impl MemoryStore {
    /// Open (or create) the memory directory under `data_dir`.
    pub async fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let memory_dir = data_dir.as_ref().join("memory");
        tokio::fs::create_dir_all(&memory_dir)
            .await
            .wrap_err("failed to create memory directory")?;
        Ok(Self { memory_dir })
    }

    /// Read long-term memory (`MEMORY.md`). Returns empty string if missing.
    pub async fn read_long_term(&self) -> Result<String> {
        let path = self.memory_dir.join("MEMORY.md");
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).wrap_err("failed to read MEMORY.md"),
        }
    }

    /// Write long-term memory (`MEMORY.md`), replacing previous content.
    ///
    /// Infrastructure for future `write_daily_note` tool wiring -- currently
    /// has no direct callers but retained as public API for planned tool integration.
    pub async fn write_long_term(&self, content: &str) -> Result<()> {
        let path = self.memory_dir.join("MEMORY.md");
        tokio::fs::write(&path, content)
            .await
            .wrap_err("failed to write MEMORY.md")
    }

    /// Read today's daily notes. Returns empty string if missing.
    pub async fn read_today(&self) -> Result<String> {
        let path = self.today_path();
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).wrap_err("failed to read today's notes"),
        }
    }

    /// Append to today's daily notes. Creates file if new.
    /// Uses atomic append to avoid TOCTOU races (#106).
    ///
    /// Infrastructure for future `write_daily_note` tool wiring -- currently
    /// has no direct callers but retained as public API for planned tool integration.
    pub async fn append_today(&self, content: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let path = self.today_path();
        let heading = chrono::Local::now().format("%Y-%m-%d").to_string();
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .wrap_err("failed to open today's notes for append")?;
        file.write_all(format!("\n## {}\n\n{}\n", heading, content).as_bytes())
            .await
            .wrap_err("failed to append to today's notes")
    }

    /// Read recent daily notes (excluding today). Returns `(date, content)` pairs.
    pub async fn read_recent(&self, days: u32) -> Result<Vec<(String, String)>> {
        let today = chrono::Local::now().date_naive();
        let mut entries = Vec::new();

        for i in 1..=days {
            let date = today - chrono::Duration::days(i64::from(i));
            let date_str = date.format("%Y-%m-%d").to_string();
            let path = self.memory_dir.join(format!("{date_str}.md"));

            match tokio::fs::read_to_string(&path).await {
                Ok(content) => entries.push((date_str, content)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).wrap_err("failed to read recent notes"),
            }
        }

        Ok(entries)
    }

    /// Build a formatted context string for injection into the system prompt.
    pub async fn get_memory_context(&self) -> String {
        let long_term = match self.read_long_term().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to read long-term memory: {e}");
                String::new()
            }
        };
        let recent = match self.read_recent(7).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to read recent memory: {e}");
                Vec::new()
            }
        };
        let today = match self.read_today().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to read today's memory: {e}");
                String::new()
            }
        };

        let mut ctx = String::new();

        if !long_term.is_empty() {
            ctx.push_str("## Long-term Memory\n\n");
            ctx.push_str(&long_term);
            ctx.push_str("\n\n");
        }

        if !recent.is_empty() {
            ctx.push_str("## Recent Activity\n\n");
            for (date, content) in &recent {
                ctx.push_str(&format!("### {date}\n{content}\n\n"));
            }
        }

        if !today.is_empty() {
            ctx.push_str("## Today's Notes\n\n");
            ctx.push_str(&today);
            ctx.push('\n');
        }

        ctx
    }

    // --- Memory Bank ---

    /// Path to `bank/entities/` directory.
    fn bank_dir(&self) -> PathBuf {
        self.memory_dir.join("bank").join("entities")
    }

    /// Ensure the `bank/entities/` directory exists.
    pub async fn ensure_bank_dir(&self) -> Result<()> {
        tokio::fs::create_dir_all(self.bank_dir())
            .await
            .wrap_err("failed to create memory bank directory")
    }

    /// List all entity files, returning `(slug, abstract_line)` pairs sorted by name.
    pub async fn list_entities(&self) -> Result<Vec<(String, String)>> {
        let dir = self.bank_dir();
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).wrap_err("failed to read bank entities directory"),
        };

        let mut result = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md") {
                let slug = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let content = match tokio::fs::read_to_string(&path).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("failed to read entity {}: {e}", path.display());
                        String::new()
                    }
                };
                let abstract_line = extract_abstract(&content);
                result.push((slug, abstract_line));
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }

    /// Read the full content of a named entity. Returns `None` if not found.
    pub async fn read_entity(&self, name: &str) -> Result<Option<String>> {
        let safe_name = name.replace(['/', '\\', '\0', '~', '.'], "_");
        let path = self.bank_dir().join(format!("{safe_name}.md"));
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).wrap_err_with(|| format!("failed to read entity: {name}")),
        }
    }

    /// Write (create or update) an entity page. Creates bank directory if needed.
    pub async fn write_entity(&self, name: &str, content: &str) -> Result<()> {
        self.ensure_bank_dir().await?;
        let safe_name = name.replace(['/', '\\', '\0', '~', '.'], "_");
        let path = self.bank_dir().join(format!("{safe_name}.md"));
        tokio::fs::write(&path, content)
            .await
            .wrap_err_with(|| format!("failed to write entity: {name}"))
    }

    /// Build a compact bank summary for system prompt injection.
    pub async fn get_bank_summary(&self) -> String {
        let entities = match self.list_entities().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to list entities for bank summary: {e}");
                Vec::new()
            }
        };
        if entities.is_empty() {
            return String::new();
        }

        let mut summary = String::from(
            "## Memory Bank\n\
             These are facts you know about the user and their world. Treat them as ground \
             truth — use them directly when relevant (e.g. if you know the user's city, use it \
             for weather/time questions without asking). Use `recall_memory` to load full details \
             when abstracts don't have enough information.\n",
        );
        for (name, abstract_line) in &entities {
            summary.push_str(&format!("- **{name}**: {abstract_line}\n"));
        }
        summary
    }

    fn today_path(&self) -> PathBuf {
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        self.memory_dir.join(format!("{date}.md"))
    }
}

/// Extract an abstract from entity content.
/// Skips YAML frontmatter, takes first non-empty non-heading line, truncates to 100 chars.
fn extract_abstract(content: &str) -> String {
    let body = strip_frontmatter(content);
    let first_line = body
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with('#'));

    match first_line {
        Some(line) if line.len() > 100 => {
            // Truncate at UTF-8 boundary
            let mut end = 97;
            while end > 0 && !line.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &line[..end])
        }
        Some(line) => line.to_string(),
        None => String::new(),
    }
}

/// Strip YAML frontmatter (`---` delimited), returning only the body.
fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content;
    }
    let after_first = &trimmed[3..];
    // Skip past the first newline after opening ---
    let after_first = after_first
        .strip_prefix('\r')
        .unwrap_or(after_first)
        .strip_prefix('\n')
        .unwrap_or(after_first);
    if let Some(end) = after_first.find("\n---") {
        let body_start = end + 4; // skip "\n---"
        after_first[body_start..]
            .strip_prefix('\r')
            .unwrap_or(&after_first[body_start..])
            .strip_prefix('\n')
            .unwrap_or(&after_first[body_start..])
    } else {
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        assert_eq!(store.read_long_term().await.unwrap(), "");
        assert_eq!(store.read_today().await.unwrap(), "");
        assert_eq!(store.get_memory_context().await, "");
    }

    #[tokio::test]
    async fn test_long_term_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.write_long_term("remember this").await.unwrap();
        assert_eq!(store.read_long_term().await.unwrap(), "remember this");

        store.write_long_term("updated").await.unwrap();
        assert_eq!(store.read_long_term().await.unwrap(), "updated");
    }

    #[tokio::test]
    async fn test_append_today_creates_header() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.append_today("first note").await.unwrap();
        let content = store.read_today().await.unwrap();
        assert!(content.contains("## "));
        assert!(content.contains("first note"));
    }

    #[tokio::test]
    async fn test_append_today_appends() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.append_today("note 1").await.unwrap();
        store.append_today("note 2").await.unwrap();

        let content = store.read_today().await.unwrap();
        assert!(content.contains("note 1"));
        assert!(content.contains("note 2"));
    }

    #[tokio::test]
    async fn test_get_memory_context_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.write_long_term("I am a bot").await.unwrap();
        store.append_today("did something").await.unwrap();

        let ctx = store.get_memory_context().await;
        assert!(ctx.contains("## Long-term Memory"));
        assert!(ctx.contains("I am a bot"));
        assert!(ctx.contains("## Today's Notes"));
        assert!(ctx.contains("did something"));
    }

    #[tokio::test]
    async fn test_read_recent_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();
        let recent = store.read_recent(7).await.unwrap();
        assert!(recent.is_empty());
    }

    #[tokio::test]
    async fn test_read_recent_with_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        // Write a file for yesterday
        let yesterday = (chrono::Local::now().date_naive() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let path = dir.path().join("memory").join(format!("{yesterday}.md"));
        tokio::fs::write(&path, "# yesterday\nsome notes\n")
            .await
            .unwrap();

        let recent = store.read_recent(7).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].0, yesterday);
        assert!(recent[0].1.contains("some notes"));
    }

    #[test]
    fn test_extract_abstract_with_frontmatter() {
        let content = "---\nname: test\ntype: project\n---\n# Test\n\nA cool project for testing.\n\n## Details\nMore info.";
        assert_eq!(extract_abstract(content), "A cool project for testing.");
    }

    #[test]
    fn test_extract_abstract_no_frontmatter() {
        let content = "# My Project\n\nSimple description here.\n";
        assert_eq!(extract_abstract(content), "Simple description here.");
    }

    #[test]
    fn test_extract_abstract_truncation() {
        let long = "A".repeat(150);
        let content = format!("# Title\n\n{long}\n");
        let abs = extract_abstract(&content);
        assert!(abs.len() <= 103); // 97 + "..."
        assert!(abs.ends_with("..."));
    }

    #[test]
    fn test_extract_abstract_empty() {
        assert_eq!(extract_abstract(""), "");
        assert_eq!(extract_abstract("# Just a heading\n"), "");
    }

    #[test]
    fn test_strip_frontmatter() {
        let content = "---\nname: test\n---\nBody here.";
        assert_eq!(strip_frontmatter(content), "Body here.");
    }

    #[test]
    fn test_strip_frontmatter_no_frontmatter() {
        let content = "Just plain text.";
        assert_eq!(strip_frontmatter(content), content);
    }

    #[tokio::test]
    async fn test_list_entities_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();
        let entities = store.list_entities().await.unwrap();
        assert!(entities.is_empty());
    }

    #[tokio::test]
    async fn test_write_and_read_entity() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        let content = "---\nname: test-project\n---\n# Test\n\nA test project.\n";
        store.write_entity("test-project", content).await.unwrap();

        let read = store.read_entity("test-project").await.unwrap();
        assert_eq!(read, Some(content.to_string()));

        // Not found
        let missing = store.read_entity("nonexistent").await.unwrap();
        assert_eq!(missing, None);
    }

    #[tokio::test]
    async fn test_list_entities_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store
            .write_entity("zebra", "# Zebra\n\nA zebra entity.\n")
            .await
            .unwrap();
        store
            .write_entity("alpha", "# Alpha\n\nAn alpha entity.\n")
            .await
            .unwrap();

        let entities = store.list_entities().await.unwrap();
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0].0, "alpha");
        assert_eq!(entities[0].1, "An alpha entity.");
        assert_eq!(entities[1].0, "zebra");
        assert_eq!(entities[1].1, "A zebra entity.");
    }

    #[tokio::test]
    async fn test_get_bank_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        // Empty bank
        assert_eq!(store.get_bank_summary().await, "");

        // With entities
        store
            .write_entity("octos", "# octos\n\nRust AI agent framework.\n")
            .await
            .unwrap();

        let summary = store.get_bank_summary().await;
        assert!(summary.contains("## Memory Bank"));
        assert!(summary.contains("**octos**"));
        assert!(summary.contains("Rust AI agent framework."));
    }

    #[tokio::test]
    async fn test_get_memory_context_includes_recent() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.write_long_term("long term").await.unwrap();

        // Write yesterday's notes
        let yesterday = (chrono::Local::now().date_naive() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let path = dir.path().join("memory").join(format!("{yesterday}.md"));
        tokio::fs::write(&path, "yesterday notes").await.unwrap();

        let ctx = store.get_memory_context().await;
        assert!(ctx.contains("## Long-term Memory"));
        assert!(ctx.contains("## Recent Activity"));
        assert!(ctx.contains("yesterday notes"));
    }
}
