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

/// Default token budget for memory injected into the system prompt.
///
/// Mirrors codex's `memory_summary.md` injection limit. Overridable via
/// `memory.max_inject_tokens` in config; see
/// [`MemoryStore::get_injectable_context`].
pub const DEFAULT_MAX_INJECT_TOKENS: usize = 2500;

/// Persistent memory store backed by markdown files.
pub struct MemoryStore {
    memory_dir: PathBuf,
}

/// Raw markdown sections loaded from disk, before prompt formatting.
struct MemorySections {
    long_term: String,
    recent: Vec<(String, String)>,
    today: String,
}

/// Rough token estimate that stays honest for CJK-heavy content:
/// ~4 ASCII chars per token, ~1 token per non-ASCII char.
fn estimate_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii / 4 + non_ascii
}

/// Keep whole paragraphs (`\n\n`-separated) while they fit `max_tokens`.
/// Never cuts mid-entry; returns empty when not even the first paragraph fits.
fn truncate_at_paragraph(text: &str, max_tokens: usize) -> String {
    let mut kept = String::new();
    for para in text.split("\n\n") {
        let candidate_cost = estimate_tokens(para) + estimate_tokens("\n\n");
        if estimate_tokens(&kept) + candidate_cost > max_tokens {
            break;
        }
        if !kept.is_empty() {
            kept.push_str("\n\n");
        }
        kept.push_str(para);
    }
    kept
}

/// Filesystem NAME_MAX on the platforms we support.
const MAX_FILENAME_BYTES: usize = 255;

/// Sibling backup filename for `file_name` + `suffix`, bounded to fit
/// NAME_MAX: the natural `<file_name><suffix>` when it fits, else a clamped,
/// hash-disambiguated stem via `octos_core::safe_filename`. Backups are
/// internal — nothing looks them up by name — so the rename is safe.
fn bounded_backup_name(file_name: &str, suffix: &str) -> String {
    let natural = format!("{file_name}{suffix}");
    if natural.len() <= MAX_FILENAME_BYTES {
        natural
    } else {
        format!("{}{suffix}", octos_core::safe_filename(file_name))
    }
}

/// Atomically replace `path` with `content`: same-dir temp file, fsync,
/// rename over the target, then fsync the directory (Unix). When `backup`
/// is given and the target exists, the previous content is first copied to
/// the backup path so one prior version stays recoverable after a bad
/// rewrite (crash mid-write leaves the original untouched).
async fn write_atomic_with_backup(
    path: PathBuf,
    content: String,
    backup: Option<PathBuf>,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;

        let parent = path
            .parent()
            .ok_or_else(|| eyre::eyre!("path has no parent: {}", path.display()))?
            .to_path_buf();

        if let Some(backup_path) = backup {
            match std::fs::copy(&path, &backup_path) {
                Ok(_) => {}
                // First write: nothing to back up.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(eyre::Report::new(e).wrap_err("failed to write memory backup"));
                }
            }
        }

        // Short fixed-prefix temp name: the target basename must NOT be
        // embedded, or near-NAME_MAX entity slugs would push the temp name
        // over the filesystem limit and fail writes that used to succeed.
        let tmp_path = parent.join(format!(".mem.{}.tmp", uuid::Uuid::now_v7().simple()));
        let write_result = (|| -> Result<()> {
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            // Preserve a tightened mode on the file being replaced: rename
            // swaps the inode, so without this a 0600 memory file would
            // silently revert to the default 0666 & umask.
            #[cfg(unix)]
            if let Ok(meta) = std::fs::metadata(&path) {
                let _ = std::fs::set_permissions(&tmp_path, meta.permissions());
            }
            std::fs::rename(&tmp_path, &path)?;
            #[cfg(unix)]
            if let Ok(dir) = std::fs::File::open(&parent) {
                let _ = dir.sync_all();
            }
            Ok(())
        })();
        if write_result.is_err() {
            // If create/write/rename failed the tmp file may exist and must not leak.
            let _ = std::fs::remove_file(&tmp_path);
        }
        write_result
    })
    .await
    .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
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
    /// Atomic (temp + fsync + rename) and keeps the previous version in
    /// `MEMORY.md.bak` so a bad rewrite is recoverable. Output slot for the
    /// memory-refresh consolidator (design PR-4); currently has no runtime
    /// callers but retained as public API for that integration.
    pub async fn write_long_term(&self, content: &str) -> Result<()> {
        let path = self.memory_dir.join("MEMORY.md");
        let backup = self.memory_dir.join("MEMORY.md.bak");
        write_atomic_with_backup(path, content.to_string(), Some(backup))
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
            .wrap_err("failed to append to today's notes")?;
        file.flush().await.wrap_err("failed to flush today's notes")
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

    /// Load the raw markdown sections, downgrading read errors to warnings.
    async fn load_sections(&self) -> MemorySections {
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
        MemorySections {
            long_term,
            recent,
            today,
        }
    }

    /// Render sections in the canonical prompt order.
    fn render_context(sections: &MemorySections) -> String {
        let mut ctx = String::new();

        if !sections.long_term.is_empty() {
            ctx.push_str("## Long-term Memory\n\n");
            ctx.push_str(&sections.long_term);
            ctx.push_str("\n\n");
        }

        if !sections.recent.is_empty() {
            ctx.push_str("## Recent Activity\n\n");
            for (date, content) in &sections.recent {
                ctx.push_str(&format!("### {date}\n{content}\n\n"));
            }
        }

        if !sections.today.is_empty() {
            ctx.push_str("## Today's Notes\n\n");
            ctx.push_str(&sections.today);
            ctx.push('\n');
        }

        ctx
    }

    /// Build a formatted context string for injection into the system prompt.
    ///
    /// Unbounded — prefer [`Self::get_injectable_context`], which also
    /// includes the bank summary and enforces a token budget.
    pub async fn get_memory_context(&self) -> String {
        Self::render_context(&self.load_sections().await)
    }

    /// Build the complete, token-capped memory block for system prompt
    /// injection: long-term memory + recent/today notes + bank summary.
    ///
    /// The budget is spent in priority order — `MEMORY.md` first (truncated
    /// at a paragraph boundary if it alone exceeds the budget), then today's
    /// notes, then the bank summary, then older daily notes newest-first —
    /// but the output keeps the canonical section order. Anything dropped is
    /// disclosed in a trailing marker so the model knows memory is partial.
    pub async fn get_injectable_context(&self, max_tokens: usize) -> String {
        let mut sections = self.load_sections().await;
        let mut bank = self.get_bank_summary().await;

        let mut budget = max_tokens;
        let mut omitted: Vec<String> = Vec::new();

        // Priority 1: long-term memory (paragraph-truncated when needed).
        if !sections.long_term.is_empty() {
            let cost = estimate_tokens(&sections.long_term);
            if cost <= budget {
                budget -= cost;
            } else {
                let kept = truncate_at_paragraph(&sections.long_term, budget);
                budget = 0;
                if kept.is_empty() {
                    sections.long_term = String::new();
                    omitted.push("long-term memory".to_string());
                } else {
                    sections.long_term = format!(
                        "{kept}\n\n_[long-term memory truncated to fit the context budget — full MEMORY.md on disk]_"
                    );
                }
            }
        }

        // Priority 2: today's notes (all-or-nothing).
        if !sections.today.is_empty() {
            let cost = estimate_tokens(&sections.today);
            if cost <= budget {
                budget -= cost;
            } else {
                sections.today = String::new();
                omitted.push("today's notes".to_string());
            }
        }

        // Priority 3: bank summary (all-or-nothing).
        if !bank.is_empty() {
            let cost = estimate_tokens(&bank);
            if cost <= budget {
                budget -= cost;
            } else {
                bank = String::new();
                omitted.push("memory bank summary".to_string());
            }
        }

        // Priority 4: older daily notes, newest first (read_recent returns
        // yesterday-first already).
        let mut kept_recent = Vec::new();
        let mut dropped_days = 0usize;
        for (date, content) in sections.recent.drain(..) {
            let cost = estimate_tokens(&content);
            if dropped_days == 0 && cost <= budget {
                budget -= cost;
                kept_recent.push((date, content));
            } else {
                dropped_days += 1;
            }
        }
        if dropped_days > 0 {
            omitted.push(format!("{dropped_days} older daily note(s)"));
        }
        sections.recent = kept_recent;

        let mut ctx = Self::render_context(&sections);
        if !bank.is_empty() {
            if !ctx.is_empty() && !ctx.ends_with('\n') {
                ctx.push('\n');
            }
            if !ctx.is_empty() {
                ctx.push('\n');
            }
            ctx.push_str(&bank);
        }
        if !omitted.is_empty() {
            ctx.push_str(&format!(
                "\n_[memory budget: omitted {}; full files under the memory directory]_\n",
                omitted.join(", ")
            ));
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
    ///
    /// Atomic (temp + fsync + rename) and keeps the previous version in
    /// `<slug>.md.prev`, so a careless `save_memory` full-overwrite is
    /// recoverable (`save_memory`'s "merge, don't discard" is prompt-level
    /// guidance only — this is the mechanical safety net).
    pub async fn write_entity(&self, name: &str, content: &str) -> Result<()> {
        self.ensure_bank_dir().await?;
        let safe_name = name.replace(['/', '\\', '\0', '~', '.'], "_");
        let file_name = format!("{safe_name}.md");
        let path = self.bank_dir().join(&file_name);
        let backup = self
            .bank_dir()
            .join(bounded_backup_name(&file_name, ".prev"));
        write_atomic_with_backup(path, content.to_string(), Some(backup))
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
             Curated notes about the user and their world from past sessions. Use them \
             directly when relevant (e.g. if you know the user's city, use it for weather/time \
             questions without asking), but they may be stale — when the current conversation \
             contradicts a note, trust the conversation, and verify time-sensitive facts before \
             acting on them. Use `recall_memory` to load full details when abstracts don't have \
             enough information.\n",
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
///
/// Both fences must be bare `---` lines: `---abc` is not an opener, `----`
/// is a horizontal rule rather than a closing fence, and an empty
/// frontmatter block (`---\n---\n`) strips cleanly.
fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    let Some(after_opener) = trimmed.strip_prefix("---") else {
        return content;
    };
    let Some(rest) = after_opener
        .strip_prefix("\r\n")
        .or_else(|| after_opener.strip_prefix('\n'))
    else {
        return content;
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return &rest[offset + line.len()..];
        }
        offset += line.len();
    }
    content
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

        let content = read_all_daily_notes(dir.path()).await;
        assert!(content.contains("note 1"));
        assert!(content.contains("note 2"));
    }

    async fn read_all_daily_notes(data_dir: &Path) -> String {
        let mut entries = tokio::fs::read_dir(data_dir.join("memory")).await.unwrap();
        let mut content = String::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let path = entry.path();
            if path.file_name().and_then(|name| name.to_str()) == Some("MEMORY.md") {
                continue;
            }
            content.push_str(&tokio::fs::read_to_string(path).await.unwrap());
        }
        content
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

    #[test]
    fn test_strip_frontmatter_empty_frontmatter() {
        assert_eq!(strip_frontmatter("---\n---\nBody here."), "Body here.");
    }

    #[test]
    fn test_strip_frontmatter_requires_bare_opener_line() {
        // "---abc" is a plain text line, not a frontmatter fence.
        let content = "---abc\nnot frontmatter\n---\nBody";
        assert_eq!(strip_frontmatter(content), content);
    }

    #[test]
    fn test_strip_frontmatter_ignores_longer_dash_runs() {
        // A "----" line is a horizontal rule / typo, not a closing fence.
        let content = "---\nname: x\n----\nBody";
        assert_eq!(strip_frontmatter(content), content);
    }

    #[test]
    fn test_strip_frontmatter_crlf() {
        assert_eq!(strip_frontmatter("---\r\nname: x\r\n---\r\nBody"), "Body");
    }

    #[test]
    fn test_strip_frontmatter_closing_fence_at_eof() {
        assert_eq!(strip_frontmatter("---\nname: x\n---"), "");
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

    // --- PR-1 foundations: atomic writes + backups ---

    #[tokio::test]
    async fn should_create_backup_when_rewriting_long_term() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.write_long_term("version one").await.unwrap();
        store.write_long_term("version two").await.unwrap();

        assert_eq!(store.read_long_term().await.unwrap(), "version two");
        let bak = tokio::fs::read_to_string(dir.path().join("memory").join("MEMORY.md.bak"))
            .await
            .unwrap();
        assert_eq!(bak, "version one");
    }

    #[tokio::test]
    async fn should_not_create_backup_when_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.write_long_term("first").await.unwrap();

        let bak_exists = tokio::fs::try_exists(dir.path().join("memory").join("MEMORY.md.bak"))
            .await
            .unwrap();
        assert!(!bak_exists);
    }

    #[tokio::test]
    async fn should_leave_no_temp_files_when_writes_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.write_long_term("a").await.unwrap();
        store.write_long_term("b").await.unwrap();
        store
            .write_entity("thing", "# Thing\n\ncontent\n")
            .await
            .unwrap();
        store
            .write_entity("thing", "# Thing\n\nupdated\n")
            .await
            .unwrap();

        for sub in [
            dir.path().join("memory"),
            dir.path().join("memory/bank/entities"),
        ] {
            let mut entries = tokio::fs::read_dir(&sub).await.unwrap();
            while let Some(entry) = entries.next_entry().await.unwrap() {
                let name = entry.file_name().to_string_lossy().to_string();
                assert!(!name.contains(".tmp."), "leaked temp file: {name}");
            }
        }
    }

    #[tokio::test]
    async fn should_keep_prev_revision_when_overwriting_entity() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.write_entity("proj", "old details").await.unwrap();
        store.write_entity("proj", "new details").await.unwrap();

        assert_eq!(
            store.read_entity("proj").await.unwrap(),
            Some("new details".to_string())
        );
        let prev = tokio::fs::read_to_string(dir.path().join("memory/bank/entities/proj.md.prev"))
            .await
            .unwrap();
        assert_eq!(prev, "old details");

        // The .prev revision must not surface as a bank entity.
        let entities = store.list_entities().await.unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].0, "proj");
    }

    #[tokio::test]
    async fn should_write_and_backup_entity_when_name_is_near_filename_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        // 250-char slug + ".md" = 253 bytes: a valid target filename, but one
        // where embedding it in temp/backup names would exceed NAME_MAX.
        let name = "a".repeat(250);
        store.write_entity(&name, "v1").await.unwrap();
        store.write_entity(&name, "v2").await.unwrap();

        assert_eq!(
            store.read_entity(&name).await.unwrap(),
            Some("v2".to_string())
        );

        let mut entries = tokio::fs::read_dir(dir.path().join("memory/bank/entities"))
            .await
            .unwrap();
        let mut prev_contents = None;
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            assert!(file_name.len() <= 255, "over-limit filename: {file_name}");
            if file_name.ends_with(".prev") {
                prev_contents = Some(tokio::fs::read_to_string(entry.path()).await.unwrap());
            }
        }
        assert_eq!(prev_contents.as_deref(), Some("v1"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn should_preserve_tightened_mode_when_rewriting_long_term() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();
        let path = dir.path().join("memory/MEMORY.md");

        store.write_long_term("private v1").await.unwrap();
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .await
            .unwrap();

        store.write_long_term("private v2").await.unwrap();

        let mode = tokio::fs::metadata(&path)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "rename must not loosen a tightened mode");
    }

    // --- PR-1 foundations: budgeted injectable context ---

    #[tokio::test]
    async fn should_include_all_sections_in_canonical_order_when_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.write_long_term("long term facts").await.unwrap();
        store.append_today("today note").await.unwrap();
        store
            .write_entity("octos", "# octos\n\nagent framework\n")
            .await
            .unwrap();
        let yesterday = (chrono::Local::now().date_naive() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        tokio::fs::write(
            dir.path().join("memory").join(format!("{yesterday}.md")),
            "yesterday note",
        )
        .await
        .unwrap();

        let ctx = store.get_injectable_context(10_000).await;
        let lt = ctx.find("## Long-term Memory").expect("long-term present");
        let recent = ctx.find("## Recent Activity").expect("recent present");
        let today = ctx.find("## Today's Notes").expect("today present");
        let bank = ctx.find("## Memory Bank").expect("bank present");
        assert!(lt < recent && recent < today && today < bank);
        assert!(!ctx.contains("memory budget: omitted"));
    }

    #[tokio::test]
    async fn should_drop_oldest_daily_notes_first_when_over_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.write_long_term("tiny").await.unwrap();
        let today = chrono::Local::now().date_naive();
        for (days_ago, tag) in [(1, "NEWEST"), (2, "OLDEST")] {
            let date = (today - chrono::Duration::days(days_ago)).format("%Y-%m-%d");
            tokio::fs::write(
                dir.path().join("memory").join(format!("{date}.md")),
                format!("{tag} {}", "x".repeat(4000)),
            )
            .await
            .unwrap();
        }

        // ~1000 tokens per day-note; budget fits exactly one.
        let ctx = store.get_injectable_context(1_200).await;
        assert!(ctx.contains("NEWEST"));
        assert!(!ctx.contains("OLDEST"));
        assert!(ctx.contains("1 older daily note(s)"));
    }

    #[tokio::test]
    async fn should_truncate_long_term_at_paragraph_when_alone_over_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        let big_para = "y".repeat(2000);
        store
            .write_long_term(&format!("first entry\n\n{big_para}"))
            .await
            .unwrap();

        let ctx = store.get_injectable_context(30).await;
        assert!(ctx.contains("first entry"));
        assert!(!ctx.contains(&big_para));
        assert!(ctx.contains("long-term memory truncated"));
    }

    #[tokio::test]
    async fn should_prefer_today_and_bank_over_recent_when_budget_tight() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();

        store.write_long_term(&"l".repeat(400)).await.unwrap();
        store.append_today("small today note").await.unwrap();
        store
            .write_entity("octos", "# octos\n\nagent framework\n")
            .await
            .unwrap();
        let yesterday = (chrono::Local::now().date_naive() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        tokio::fs::write(
            dir.path().join("memory").join(format!("{yesterday}.md")),
            "z".repeat(4000),
        )
        .await
        .unwrap();

        let ctx = store.get_injectable_context(500).await;
        assert!(ctx.contains("## Today's Notes"));
        assert!(ctx.contains("## Memory Bank"));
        assert!(!ctx.contains("## Recent Activity"));
        assert!(ctx.contains("1 older daily note(s)"));
    }

    #[tokio::test]
    async fn should_return_empty_when_no_memory_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();
        assert_eq!(store.get_injectable_context(2500).await, "");
    }

    #[tokio::test]
    async fn should_not_claim_ground_truth_when_bank_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).await.unwrap();
        store
            .write_entity("octos", "# octos\n\nframework\n")
            .await
            .unwrap();

        let summary = store.get_bank_summary().await;
        assert!(!summary.contains("ground truth"));
        assert!(summary.contains("may be stale"));
    }

    // --- PR-1 foundations: token estimation ---

    #[test]
    fn should_estimate_ascii_at_quarter_char_rate() {
        assert_eq!(estimate_tokens(&"a".repeat(400)), 100);
    }

    #[test]
    fn should_estimate_cjk_at_one_token_per_char() {
        let text = "记".repeat(100);
        assert_eq!(estimate_tokens(&text), 100);
    }

    #[test]
    fn should_keep_whole_paragraphs_when_truncating() {
        let text = "aaaa\n\nbbbb\n\ncccc";
        // Each paragraph ≈1 token; a budget of 2 keeps exactly two.
        let kept = truncate_at_paragraph(text, 2);
        assert_eq!(kept, "aaaa\n\nbbbb");
    }

    #[test]
    fn should_return_empty_when_first_paragraph_exceeds_budget() {
        assert_eq!(truncate_at_paragraph(&"x".repeat(4000), 10), "");
    }
}
