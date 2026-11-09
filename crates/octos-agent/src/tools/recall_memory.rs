//! Recall memory tool: load full entity pages from the memory bank.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_llm::EmbeddingProvider;
use octos_memory::{MemoryStore, RecallStore, RecordKind, SearchFilter};
use serde::Deserialize;

use super::{Tool, ToolResult};

/// Tool that loads full entity pages from the memory bank.
pub struct RecallMemoryTool {
    store: Arc<MemoryStore>,
    /// Knowledge index for `query` lookups (ADR personal-memory-tiers,
    /// phase 3): finds the right page when the model does not know its name.
    recall: Option<Arc<RecallStore>>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
}

impl RecallMemoryTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self {
            store,
            recall: None,
            embedder: None,
        }
    }

    /// Enable `query`-based page lookup over the indexed bank.
    pub fn with_recall(
        mut self,
        recall: Arc<RecallStore>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        self.recall = Some(recall);
        self.embedder = embedder;
        self
    }

    /// Best-matching bank page slugs for `query` (relevance order).
    async fn matching_pages(&self, query: &str, limit: usize) -> Vec<(String, String)> {
        let Some(recall) = &self.recall else {
            return Vec::new();
        };
        let vector = match &self.embedder {
            Some(e) => e
                .embed(&[query])
                .await
                .ok()
                .and_then(|mut v| (!v.is_empty()).then(|| v.swap_remove(0))),
            None => None,
        };
        let recall = recall.clone();
        let q = query.to_string();
        let hits = tokio::task::spawn_blocking(move || {
            recall.search(
                &q,
                vector.as_deref(),
                &SearchFilter {
                    kinds: vec![RecordKind::Knowledge],
                    limit,
                    ..Default::default()
                },
            )
        })
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
        hits.into_iter()
            .filter_map(|h| {
                h.id.strip_prefix("bank:")
                    .map(|slug| (slug.to_string(), h.abstract_))
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct Input {
    /// Entity name, or "MEMORY" for the registry. Optional when `query`
    /// is given.
    #[serde(default)]
    name: Option<String>,
    /// Free-text lookup over the indexed bank: returns the best-matching
    /// page and names the runners-up.
    #[serde(default)]
    query: Option<String>,
    /// Optional 0-based page for the paged registry load (`name="MEMORY"`
    /// when the registry exceeds the tool-output limit). Ignored for bank
    /// entities.
    #[serde(default)]
    page: Option<usize>,
}

/// Normalize an entity name into a slug: trim, lowercase, spaces to hyphens.
fn to_slug(name: &str) -> String {
    name.trim().to_lowercase().replace(' ', "-")
}

/// Split the registry into byte-safe, line-aligned pages each just under
/// the tool-output limit, and render the requested page with a truthful
/// pager marker. The generic tool truncation (`truncate_head_tail`, 0.7)
/// would otherwise SILENTLY drop the middle of a large `MEMORY.md`,
/// recreating the unrecoverable-tail problem this tool exists to solve —
/// and a one-shot cap left the tail unreachable, over-promising a
/// targeted retrieval the schema never offered (codex #1608 P2). Paging
/// makes the whole registry reachable page by page.
fn render_registry_page(registry: &str, page: usize) -> String {
    // Headroom covers the pager marker appended after slicing (~75 bytes)
    // with generous slack, so every rendered page stays STRICTLY under the
    // outer truncate_head_tail limit — a too-tight margin let a full page +
    // marker + disclosure exceed it and re-trigger the silent middle-loss
    // (codex #1608 round-3 P1).
    let limit = octos_core::tool_output_limit("recall_memory");
    let budget = limit.saturating_sub(512).max(1);

    // Build page ranges at newline boundaries (always char-safe), each
    // <= budget bytes. A pathological newline-free run is cut at a char
    // boundary so slicing can never panic mid-codepoint.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    while start < registry.len() {
        let mut end = (start + budget).min(registry.len());
        while end > start && !registry.is_char_boundary(end) {
            end -= 1;
        }
        if end < registry.len() {
            if let Some(nl) = registry[start..end].rfind('\n') {
                end = start + nl + 1;
            }
        }
        if end == start {
            // Single codepoint wider than the budget: advance one char.
            end = registry[start..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| start + i)
                .unwrap_or(registry.len());
        }
        ranges.push((start, end));
        start = end;
    }

    let total = ranges.len().max(1);
    let page = page.min(total - 1);
    let (s0, e0) = ranges.get(page).copied().unwrap_or((0, registry.len()));
    let mut out = registry[s0..e0].to_string();
    if total > 1 {
        let more = if page + 1 < total {
            format!(
                " — more: call recall_memory(name=\"MEMORY\", page={})",
                page + 1
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "\n\n_[registry page {}/{}{}]_",
            page + 1,
            total,
            more
        ));
    }
    out
}

#[async_trait]
impl Tool for RecallMemoryTool {
    fn name(&self) -> &str {
        "recall_memory"
    }

    fn description(&self) -> &str {
        "Load full memory detail on demand. Pass a memory-bank entity name \
         (as shown in the Memory Bank section) for its page, or \"MEMORY\" \
         for the complete long-term registry when the injected memory is a \
         budget-truncated summary and you need an entry that isn't shown. \
         When you don't know the page name, pass `query` instead to get the \
         best-matching page."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Entity name (e.g. 'octos', 'alice'), or 'MEMORY' for the full long-term registry"
                },
                "query": {
                    "type": "string",
                    "description": "Words describing the page you need when its name is unknown; returns the best match."
                },
                "page": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-based page for a large 'MEMORY' registry load; follow the 'page=N' hint in the page marker. Ignored for entity names."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid recall_memory input")?;

        let name = match (&input.name, &input.query) {
            (Some(n), _) if !n.trim().is_empty() => n.clone(),
            (_, Some(q)) if !q.trim().is_empty() => {
                if self.recall.is_none() {
                    return Ok(ToolResult {
                        output:
                            "Query lookup is unavailable (no memory index); pass an entity name."
                                .to_string(),
                        success: false,
                        ..Default::default()
                    });
                }
                let matches = self.matching_pages(q.trim(), 4).await;
                let Some((best, _)) = matches.first().cloned() else {
                    let entities = self.store.list_entities().await.unwrap_or_default();
                    let available: Vec<_> = entities.iter().map(|(n, _)| n.as_str()).collect();
                    return Ok(ToolResult {
                        output: format!(
                            "No memory-bank page matches \"{}\". Available: {}",
                            q.trim(),
                            if available.is_empty() {
                                "(none)".to_string()
                            } else {
                                available.join(", ")
                            }
                        ),
                        success: false,
                        ..Default::default()
                    });
                };
                if let Some(content) = self.store.read_entity(&best).await? {
                    let mut output = format!("# {best}\n{content}");
                    if matches.len() > 1 {
                        output.push_str("\n\n_Other matching pages: ");
                        output.push_str(
                            &matches[1..]
                                .iter()
                                .map(|(slug, abs)| format!("{slug} ({abs})"))
                                .collect::<Vec<_>>()
                                .join("; "),
                        );
                        output.push('_');
                    }
                    return Ok(ToolResult {
                        output,
                        success: true,
                        ..Default::default()
                    });
                }
                best
            }
            _ => {
                return Ok(ToolResult {
                    output: "recall_memory needs `name` (or \"MEMORY\") or `query`.".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // Tier-2 registry load: the injected long-term memory is capped to a
        // token budget, so "MEMORY" (and aliases) returns the full MEMORY.md.
        if octos_memory::is_reserved_memory_name(&name) {
            // Prepend any PRE-UPGRADE bank entity whose name is now reserved:
            // new writes under these names are refused, but legacy files
            // would otherwise be shadowed by the alias and unreadable. Folding
            // their content into the paged text makes it reachable, size-safe
            // (the pager bounds it), and case/space-variant-aware since
            // list_entities returns the actual on-disk stems (codex #1608 P2).
            let mut full = String::new();
            if let Ok(entities) = self.store.list_entities().await {
                for (name, _) in entities
                    .iter()
                    .filter(|(n, _)| octos_memory::is_reserved_memory_name(n))
                {
                    if let Ok(Some(content)) = self.store.read_entity(name).await {
                        full.push_str(&format!(
                            "## Legacy bank entity \"{name}\" (shadowed by the registry \
                             alias; rename via save_memory to address it directly)\n{content}\n\n"
                        ));
                    }
                }
            }
            full.push_str(&self.store.read_long_term().await?);

            let output = if full.trim().is_empty() {
                "The long-term memory registry (MEMORY.md) is empty.".to_string()
            } else {
                render_registry_page(&full, input.page.unwrap_or(0))
            };
            return Ok(ToolResult {
                output,
                success: true,
                ..Default::default()
            });
        }

        let slug = to_slug(&name);

        match self.store.read_entity(&slug).await? {
            Some(content) => Ok(ToolResult {
                output: content,
                success: true,
                ..Default::default()
            }),
            None => {
                let entities = self.store.list_entities().await.unwrap_or_default();
                let available: Vec<_> = entities.iter().map(|(n, _)| n.as_str()).collect();
                Ok(ToolResult {
                    output: format!(
                        "Entity '{}' not found. Available: {}",
                        slug,
                        if available.is_empty() {
                            "(none)".to_string()
                        } else {
                            available.join(", ")
                        }
                    ),
                    success: false,
                    ..Default::default()
                })
            }
        }
    }
}
