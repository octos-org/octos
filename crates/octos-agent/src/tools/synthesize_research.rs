//! Synthesize research tool: reads deep_search source files and produces a
//! comprehensive analysis via map-reduce LLM calls.
//!
//! After `deep_search` saves source files to disk (up to 20K chars each),
//! this tool reads them all, batches into context-window-sized chunks,
//! extracts key findings per batch, then merges into a final synthesis.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use octos_core::TokenUsage;
use octos_llm::LlmProvider;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tracing::info;

use super::research_utils;
use super::{Tool, ToolResult};

pub struct SynthesizeResearchTool {
    llm: Arc<dyn LlmProvider>,
    data_dir: PathBuf,
}

impl SynthesizeResearchTool {
    pub fn new(llm: Arc<dyn LlmProvider>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            llm,
            data_dir: data_dir.into(),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    research_dir: String,
    query: String,
    #[serde(default)]
    focus: Option<String>,
}

#[async_trait]
impl Tool for SynthesizeResearchTool {
    fn name(&self) -> &str {
        "synthesize_research"
    }

    fn description(&self) -> &str {
        "Read all source files from a deep_search research directory and produce a comprehensive \
         synthesis using map-reduce analysis. This reads the FULL content of every saved source \
         page (up to 20K chars each) — much more thorough than the truncated previews returned \
         by deep_search. Use this after deep_search completes to get a detailed, data-rich report."
    }

    fn tags(&self) -> &[&str] {
        &["web"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "research_dir": {
                    "type": "string",
                    "description": "Path to the research directory from deep_search output (e.g. './research/topic-name' or 'research/topic-name')"
                },
                "query": {
                    "type": "string",
                    "description": "The original research question (provides context for synthesis)"
                },
                "focus": {
                    "type": "string",
                    "description": "Optional: specific aspect to focus the synthesis on"
                }
            },
            "required": ["research_dir", "query"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid synthesize_research input")?;

        // Resolve research directory transparently.
        let dir = match research_utils::resolve_research_dir(&self.data_dir, &input.research_dir) {
            Some(d) => d,
            None => {
                return Ok(ToolResult {
                    output: "Research directory not found. Run deep_search first.".into(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        info!(
            dir = %dir.display(),
            query = %input.query,
            focus = ?input.focus,
            "starting research synthesis"
        );

        // Step 1: Read all source files
        let files = research_utils::read_sources(&dir).await?;

        if files.is_empty() {
            return Ok(ToolResult {
                output: format!(
                    "No source files found in {}. The research directory may be empty.",
                    dir.display()
                ),
                success: false,
                ..Default::default()
            });
        }

        let total_chars: usize = files.iter().map(|(_, c)| c.len()).sum();
        info!(file_count = files.len(), total_chars, "read source files");

        // Truncate if over total limit
        let files = research_utils::truncate_to_limit(files);
        let source_count = files.len();
        let mut total_tokens = TokenUsage::default();

        // Step 2: Partition into batches
        let batches = research_utils::partition_batches(&files);
        info!(
            batches = batches.len(),
            source_count, "partitioned into batches"
        );

        if batches.len() == 1 {
            // Single batch: direct synthesis (no map phase needed)
            info!("single batch — direct synthesis");
            let (synthesis, usage) = research_utils::extract_findings(
                self.llm.as_ref(),
                &input.query,
                input.focus.as_deref(),
                &files,
                &batches[0],
                1,
                1,
            )
            .await?;

            total_tokens.input_tokens += usage.input_tokens;
            total_tokens.output_tokens += usage.output_tokens;

            return Ok(ToolResult {
                output: format!(
                    "{synthesis}\n\n---\n_Synthesized from {source_count} source files._"
                ),
                success: true,
                tokens_used: Some(total_tokens),
                ..Default::default()
            });
        }

        // Step 3: Map phase — extract findings from each batch
        let total_batches = batches.len();
        let mut partials = Vec::with_capacity(total_batches);

        for (i, batch) in batches.iter().enumerate() {
            info!(
                batch = i + 1,
                total = total_batches,
                files_in_batch = batch.len(),
                "extracting findings from batch"
            );

            match research_utils::extract_findings(
                self.llm.as_ref(),
                &input.query,
                input.focus.as_deref(),
                &files,
                batch,
                i + 1,
                total_batches,
            )
            .await
            {
                Ok((findings, usage)) => {
                    total_tokens.input_tokens += usage.input_tokens;
                    total_tokens.output_tokens += usage.output_tokens;
                    if !findings.is_empty() {
                        partials.push(findings);
                    }
                }
                Err(e) => {
                    tracing::warn!(batch = i + 1, error = %e, "batch extraction failed");
                }
            }
        }

        if partials.is_empty() {
            return Ok(ToolResult {
                output: "All batch extractions failed. Could not synthesize research.".into(),
                success: false,
                tokens_used: Some(total_tokens),
                ..Default::default()
            });
        }

        // Step 4: Reduce phase — merge partials
        info!(partial_count = partials.len(), "merging partial analyses");

        let (synthesis, merge_usage) = research_utils::merge_findings(
            self.llm.as_ref(),
            &input.query,
            input.focus.as_deref(),
            &partials,
            source_count,
        )
        .await?;

        total_tokens.input_tokens += merge_usage.input_tokens;
        total_tokens.output_tokens += merge_usage.output_tokens;

        Ok(ToolResult {
            output: format!(
                "{synthesis}\n\n---\n_Synthesized from {source_count} source files \
                 across {total_batches} batches._"
            ),
            success: true,
            tokens_used: Some(total_tokens),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::research_utils;

    #[test]
    fn test_partition_batches_single() {
        let files = vec![
            ("a.md".into(), "x".repeat(1000)),
            ("b.md".into(), "y".repeat(1000)),
        ];
        let batches = research_utils::partition_batches(&files);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0, 1]);
    }

    #[test]
    fn test_partition_batches_multiple() {
        let files: Vec<(String, String)> = (0..5)
            .map(|i| (format!("{i}.md"), "x".repeat(30_000)))
            .collect();
        let batches = research_utils::partition_batches(&files);
        assert!(batches.len() >= 2);
        let all: Vec<usize> = batches.iter().flatten().copied().collect();
        assert_eq!(all, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_partition_batches_empty() {
        let files: Vec<(String, String)> = vec![];
        let batches = research_utils::partition_batches(&files);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_partition_single_large_file() {
        let files = vec![("big.md".into(), "x".repeat(100_000))];
        let batches = research_utils::partition_batches(&files);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0]);
    }
}
