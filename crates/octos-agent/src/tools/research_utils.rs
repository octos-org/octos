//! Shared utilities for research tools (synthesis map-reduce logic).
//!
//! Used by `SynthesizeResearchTool` for map-reduce over crawled sources.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use octos_core::{Message, MessageRole, TokenUsage};
use octos_llm::{ChatConfig, LlmProvider};
use tracing::{info, warn};

/// Maximum chars per LLM batch (~80K chars ≈ ~20K tokens).
pub const BATCH_CHAR_LIMIT: usize = 80_000;
/// Maximum total chars to process (safety cap).
pub const TOTAL_CHAR_LIMIT: usize = 500_000;
/// Maximum number of source files to read.
pub const MAX_FILES: usize = 50;

/// Read all source .md files from a research directory.
///
/// Skips `_`-prefixed files (e.g. `_search_results.md`, `_report.md`).
/// Returns `(filename, content)` pairs sorted by filename.
pub async fn read_sources(dir: &Path) -> Result<Vec<(String, String)>> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .wrap_err_with(|| format!("cannot read directory: {}", dir.display()))?;

    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Skip non-markdown, index files, and report files
        if !name.ends_with(".md") {
            continue;
        }
        if name.starts_with('_') {
            continue;
        }

        if files.len() >= MAX_FILES {
            warn!(max = MAX_FILES, "reached file limit, skipping remaining");
            break;
        }

        match tokio::fs::read_to_string(&path).await {
            Ok(content) if !content.is_empty() => {
                files.push((name, content));
            }
            Ok(_) => {} // skip empty files
            Err(e) => {
                warn!(file = %name, error = %e, "failed to read source file");
            }
        }
    }

    // Sort by filename for deterministic ordering
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Partition files into batches that fit within the char limit.
pub fn partition_batches(files: &[(String, String)]) -> Vec<Vec<usize>> {
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current_batch: Vec<usize> = Vec::new();
    let mut current_size: usize = 0;

    for (i, (_name, content)) in files.iter().enumerate() {
        let size = content.len();
        if !current_batch.is_empty() && current_size + size > BATCH_CHAR_LIMIT {
            batches.push(std::mem::take(&mut current_batch));
            current_size = 0;
        }
        current_batch.push(i);
        current_size += size;
    }

    if !current_batch.is_empty() {
        batches.push(current_batch);
    }

    batches
}

/// Map phase: extract key findings from a batch of source files.
pub async fn extract_findings(
    llm: &dyn LlmProvider,
    query: &str,
    focus: Option<&str>,
    files: &[(String, String)],
    batch_indices: &[usize],
    batch_num: usize,
    total_batches: usize,
) -> Result<(String, TokenUsage)> {
    let mut sources = String::new();
    for &i in batch_indices {
        let (name, content) = &files[i];
        sources.push_str(&format!("### Source: {name}\n\n{content}\n\n---\n\n"));
    }

    let focus_instruction = match focus {
        Some(f) => format!("\n\nFocus particularly on: {f}"),
        None => String::new(),
    };

    let prompt = format!(
        "You are analyzing research sources (batch {batch_num}/{total_batches}).\n\n\
         Original research question: {query}{focus_instruction}\n\n\
         Extract ALL key findings from these sources. Rules:\n\
         - Keep ALL specific numbers, percentages, dates, names, and quotes\n\
         - Keep ALL source URLs and citations\n\
         - Organize findings by topic/theme\n\
         - Include contradictions or differing perspectives\n\
         - Be comprehensive — do not summarize away important details\n\n\
         Sources:\n\n{sources}"
    );

    let messages = vec![Message {
        role: MessageRole::User,
        content: prompt,
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        timestamp: chrono::Utc::now(),
    }];

    let config = ChatConfig {
        max_tokens: Some(8192),
        temperature: Some(0.0),
        ..Default::default()
    };

    let response = llm.chat(&messages, &[], &config).await?;
    let usage = TokenUsage {
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        ..Default::default()
    };
    Ok((response.content.unwrap_or_default(), usage))
}

/// Reduce phase: merge partial findings into a final synthesis.
pub async fn merge_findings(
    llm: &dyn LlmProvider,
    query: &str,
    focus: Option<&str>,
    partials: &[String],
    source_count: usize,
) -> Result<(String, TokenUsage)> {
    let mut sections = String::new();
    for (i, partial) in partials.iter().enumerate() {
        sections.push_str(&format!(
            "## Partial Analysis {}\n\n{}\n\n---\n\n",
            i + 1,
            partial
        ));
    }

    let focus_instruction = match focus {
        Some(f) => format!("\n\nFocus particularly on: {f}"),
        None => String::new(),
    };

    let prompt = format!(
        "Synthesize these {count} partial analyses into ONE comprehensive research report.\n\n\
         Original question: {query}{focus_instruction}\n\n\
         Rules:\n\
         - Remove duplicates and redundancies across partial analyses\n\
         - Organize logically with clear section headers (use ## and ###)\n\
         - Keep ALL specific numbers, percentages, dates, names, and direct quotes\n\
         - Use markdown tables where data comparison is appropriate\n\
         - Include a ## Sources section at the end listing all URLs referenced\n\
         - Note any contradictions or areas of disagreement between sources\n\
         - Write in the same language as the original question\n\n\
         Analyzed {source_count} source pages total.\n\n\
         {sections}\n\n\
         Write the complete synthesized report.",
        count = partials.len(),
    );

    let messages = vec![Message {
        role: MessageRole::User,
        content: prompt,
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        timestamp: chrono::Utc::now(),
    }];

    let config = ChatConfig {
        max_tokens: Some(8192),
        temperature: Some(0.0),
        ..Default::default()
    };

    let response = llm.chat(&messages, &[], &config).await?;
    let usage = TokenUsage {
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        ..Default::default()
    };
    Ok((response.content.unwrap_or_default(), usage))
}

/// Resolve research directory from whatever the LLM provides.
///
/// All candidates are resolved relative to `data_dir` — never uses
/// `std::env::current_dir()` (which may differ from the logical working
/// directory in multi-profile gateway mode).  The resolved path is
/// validated to stay within `data_dir` to prevent cross-profile reads.
pub fn resolve_research_dir(data_dir: &Path, input: &str) -> Option<PathBuf> {
    let stripped = input.trim().trim_start_matches("./");

    // Extract the slug (last path component) for fuzzy matching
    let slug = std::path::Path::new(stripped)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(stripped);

    // Candidate directories to try, in priority order.
    // All resolved relative to data_dir (the profile's isolated directory).
    let candidates: Vec<PathBuf> = vec![
        // 1. Relative to data_dir (e.g. "research/topic-slug" or "skill-output/...")
        data_dir.join(stripped),
        // 2. Just slug under data_dir/research/
        data_dir.join("research").join(slug),
        // 3. Exact absolute path (validated below)
        PathBuf::from(input),
    ];

    let canonical_base = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());

    for candidate in candidates {
        if candidate.is_dir() {
            // Validate the resolved path stays within data_dir
            let canonical = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
            if !canonical.starts_with(&canonical_base) {
                warn!(
                    input = %input,
                    resolved = %canonical.display(),
                    data_dir = %data_dir.display(),
                    "research directory resolved outside data_dir, rejecting"
                );
                continue;
            }
            info!(resolved = %candidate.display(), input = %input, "resolved research directory");
            return Some(candidate);
        }
    }

    warn!(input = %input, "could not resolve research directory");
    None
}

/// Extract the URL from a source file's YAML frontmatter.
///
/// Parses the `---\nurl: <url>\n---` header used by deep_search output files.
pub fn extract_url_from_frontmatter(content: &str) -> Option<String> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }

    // Find closing ---
    let rest = &content[3..];
    let end = rest.find("---")?;
    let frontmatter = &rest[..end];

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(url) = line.strip_prefix("url:") {
            let url = url.trim();
            if !url.is_empty() {
                return Some(url.to_string());
            }
        }
    }

    None
}

/// Slugify a string for use as a directory name (matches deep_search binary convention).
pub fn slugify(s: &str) -> String {
    let mut slug = String::with_capacity(s.len());
    for ch in s.chars().take(80) {
        if ch.is_alphanumeric() || ch > '\x7f' {
            // Keep CJK and other unicode chars as-is for readability
            slug.push(ch);
        } else if (ch == ' ' || ch == '-' || ch == '_') && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

/// Truncate source files to fit within the total char limit.
pub fn truncate_to_limit(files: Vec<(String, String)>) -> Vec<(String, String)> {
    let total_chars: usize = files.iter().map(|(_, c)| c.len()).sum();
    if total_chars <= TOTAL_CHAR_LIMIT {
        return files;
    }

    warn!(
        total_chars,
        limit = TOTAL_CHAR_LIMIT,
        "total content exceeds limit, truncating files"
    );

    let mut acc = 0usize;
    files
        .into_iter()
        .take_while(|(_, c)| {
            acc += c.len();
            acc <= TOTAL_CHAR_LIMIT
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_batches_single() {
        let files = vec![
            ("a.md".into(), "x".repeat(1000)),
            ("b.md".into(), "y".repeat(1000)),
        ];
        let batches = partition_batches(&files);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0, 1]);
    }

    #[test]
    fn test_partition_batches_multiple() {
        let files: Vec<(String, String)> = (0..5)
            .map(|i| (format!("{i}.md"), "x".repeat(30_000)))
            .collect();
        let batches = partition_batches(&files);
        // 5 files × 30K = 150K, should split into ~2 batches (80K limit)
        assert!(batches.len() >= 2);
        // All indices should be covered
        let all: Vec<usize> = batches.iter().flatten().copied().collect();
        assert_eq!(all, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_partition_batches_empty() {
        let files: Vec<(String, String)> = vec![];
        let batches = partition_batches(&files);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_partition_single_large_file() {
        let files = vec![("big.md".into(), "x".repeat(100_000))];
        let batches = partition_batches(&files);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0]);
    }

    #[test]
    fn test_extract_url_from_frontmatter() {
        let content = "---\nurl: https://example.com/page\n---\n\nBody text here";
        assert_eq!(
            extract_url_from_frontmatter(content),
            Some("https://example.com/page".to_string())
        );
    }

    #[test]
    fn test_extract_url_no_frontmatter() {
        let content = "Just some plain text without frontmatter";
        assert_eq!(extract_url_from_frontmatter(content), None);
    }

    #[test]
    fn test_extract_url_empty_url() {
        let content = "---\nurl:\n---\n\nBody";
        assert_eq!(extract_url_from_frontmatter(content), None);
    }

    #[test]
    fn test_slugify() {
        assert_eq!(
            slugify("AI agent frameworks 2025"),
            "AI-agent-frameworks-2025"
        );
        assert_eq!(slugify("hello--world"), "hello-world");
        assert_eq!(slugify("  spaces  "), "spaces");
    }

    #[test]
    fn test_slugify_cjk() {
        assert_eq!(slugify("AI智能体框架对比"), "AI智能体框架对比");
    }

    #[test]
    fn test_truncate_to_limit() {
        let files = vec![
            ("a.md".into(), "x".repeat(100)),
            ("b.md".into(), "y".repeat(100)),
        ];
        // Under limit — should pass through unchanged
        let result = truncate_to_limit(files.clone());
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_resolve_research_dir_inside_data_dir() {
        let data_dir = tempfile::tempdir().unwrap();
        let research = data_dir.path().join("research").join("ai-agents");
        std::fs::create_dir_all(&research).unwrap();

        // Slug-based resolution
        let result = resolve_research_dir(data_dir.path(), "ai-agents");
        assert_eq!(result.unwrap(), research);

        // Relative path resolution
        let result = resolve_research_dir(data_dir.path(), "research/ai-agents");
        assert_eq!(result.unwrap(), research);
    }

    #[test]
    fn test_resolve_research_dir_rejects_outside_path() {
        let data_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_research = outside.path().join("research").join("secret");
        std::fs::create_dir_all(&outside_research).unwrap();

        // Absolute path outside data_dir — should be rejected
        let result = resolve_research_dir(data_dir.path(), &outside_research.to_string_lossy());
        assert!(result.is_none(), "should reject path outside data_dir");
    }

    #[test]
    fn test_resolve_research_dir_no_cwd_fallback() {
        // Ensure we don't resolve against OS cwd
        let data_dir = tempfile::tempdir().unwrap();
        // Even if a research dir exists under OS cwd, we should NOT find it
        // because we only search under data_dir.
        let result = resolve_research_dir(data_dir.path(), "nonexistent-slug");
        assert!(result.is_none());
    }
}
