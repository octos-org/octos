//! Memory command: operator surface for the memory-refresh pipeline.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::{Result, WrapErr};

use super::Executable;
use crate::config::Config;

/// Inspect and drive the memory-refresh pipeline.
#[derive(Debug, Args)]
pub struct MemoryCommand {
    #[command(subcommand)]
    action: MemoryAction,
}

#[derive(Debug, Subcommand)]
enum MemoryAction {
    /// Run one extraction pass now (works even when the background sweep
    /// is disabled in config; refuses when a running service holds the
    /// profile lock).
    Refresh {
        /// Data directory (defaults to the resolved profile data dir).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Show sweep state: lock holder, staging backlog, daily budgets.
    Status {
        /// Data directory (defaults to the resolved profile data dir).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Regenerate missing or wrong-width episode embeddings.
    ///
    /// Changing the embedding model changes the vector WIDTH, and stored
    /// vectors cannot be converted — only regenerated. Until they are, those
    /// episodes fall back to BM25-only recall, which is invisible at query
    /// time (search is just quietly worse). This is the repair.
    ///
    /// Requires the profile's store lock: stop `octos serve` first.
    Reindex {
        /// Report what would be re-embedded, then exit without writing.
        #[arg(long)]
        dry_run: bool,
        /// Data directory (defaults to the resolved profile data dir).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Record a host-authored remember request (full consolidation
    /// authority — no model in the loop).
    Remember {
        /// What to remember, verbatim.
        text: Vec<String>,
        /// Data directory (defaults to the resolved profile data dir).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Record a host-authored forget request. Free text starts a
    /// confirm flow; `--id ^m…` deletes that exact entry on the next
    /// consolidation.
    Forget {
        /// What to forget (free text), unless --id is given.
        text: Vec<String>,
        /// Exact MEMORY.md entry id (e.g. ^m4k2abq) to hard-delete.
        #[arg(long)]
        id: Option<String>,
        /// Sensitive data: candidates are interim-archived immediately and
        /// scrubbed everywhere on confirmation.
        #[arg(long)]
        sensitive: bool,
        /// Data directory (defaults to the resolved profile data dir).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

impl Executable for MemoryCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .wrap_err("failed to create tokio runtime")?
            .block_on(self.run_async())
    }
}

impl MemoryCommand {
    async fn run_async(self) -> Result<()> {
        match self.action {
            MemoryAction::Refresh { data_dir } => run_refresh(data_dir).await,
            MemoryAction::Status { data_dir } => run_status(data_dir).await,
            MemoryAction::Reindex { dry_run, data_dir } => run_reindex(dry_run, data_dir).await,
            MemoryAction::Remember { text, data_dir } => {
                write_host_note(data_dir, octos_memory::NoteKind::UserRequest, text, false).await
            }
            MemoryAction::Forget {
                text,
                id,
                sensitive,
                data_dir,
            } => {
                let content = match id {
                    Some(id) => {
                        let id = id.trim();
                        let id = if id.starts_with("^m") {
                            id.to_string()
                        } else {
                            format!("^m{}", id.trim_start_matches('m'))
                        };
                        // Fail fast on malformed ids: the consolidator only
                        // recognizes ^m + 6 chars of [a-z2-7]; anything else
                        // would silently degrade into an unmatchable
                        // free-text pending note.
                        let suffix = &id[2..];
                        if suffix.len() != 6
                            || !suffix.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7'))
                        {
                            eyre::bail!(
                                "invalid entry id '{id}': expected ^m followed by 6 chars of \
                                 [a-z2-7] (as shown in your Long-term Memory)"
                            );
                        }
                        format!("id:{id}")
                    }
                    None => text.join(" "),
                };
                write_host_note(
                    data_dir,
                    octos_memory::NoteKind::Forget,
                    vec![content],
                    sensitive,
                )
                .await
            }
        }
    }
}

/// Episodes embedded per provider call. The in-process llama.cpp backend
/// amortises its fixed per-forward-pass cost across a batch (~6x at 16), and
/// remote providers accept batched input too, so this is the difference between
/// a reindex taking seconds and taking minutes.
const REINDEX_BATCH: usize = 16;

async fn run_reindex(dry_run: bool, data_dir: Option<PathBuf>) -> Result<()> {
    let (data_dir, config) = resolve(data_dir).await?;

    // Resolve the embedder FIRST: it decides the index width, and without one
    // there is nothing to regenerate. Failing here costs nothing.
    let Some(embedder) = crate::commands::chat::create_embedder(&config) else {
        eyre::bail!(
            "no embedding provider configured — set `embedding` in your config.\n\
             Without one, episodes are BM25-only by design and there is nothing to reindex."
        );
    };
    let dimension = embedder.dimension();

    // Strict open: this needs the writer lock. `octos serve` holds it in the
    // normal fleet, so say that plainly instead of surfacing redb's
    // "Database already open" — the operator's next action is to stop serve.
    let store = octos_memory::EpisodeStore::open_with_dimension(&data_dir, dimension)
        .await
        .wrap_err_with(|| {
            format!(
                "failed to open the episode store at {} — if `octos serve` (or a gateway) is \
                 running it holds the writer lock; stop it and retry",
                data_dir.display()
            )
        })?;

    let before = store.vector_coverage();
    let pending = store.episodes_needing_vectors().await?;

    println!("{}", "Episode vector coverage".bold());
    println!("  data dir      {}", data_dir.display());
    println!("  index width   {dimension}");
    println!(
        "  vectorized    {} / {} ({:.0}%)",
        before.vectorized,
        before.total,
        before.ratio() * 100.0
    );
    if before.has_dimension_mismatch() {
        println!(
            "  {}  {} stored vector(s) have the wrong width for this model",
            "mismatch".yellow(),
            before.dimension_mismatches
        );
    }

    if pending.is_empty() {
        println!("\n{} nothing to reindex.", "✓".green());
        return Ok(());
    }
    println!("\n  {} episode(s) need embedding", pending.len());

    if dry_run {
        for (id, summary) in pending.iter().take(10) {
            let head: String = summary.chars().take(72).collect();
            println!("    {id}  {head}");
        }
        if pending.len() > 10 {
            println!("    … and {} more", pending.len() - 10);
        }
        println!("\n{} dry run — nothing written.", "•".dimmed());
        return Ok(());
    }

    let mut embedded = 0usize;
    let mut failed = 0usize;
    for chunk in pending.chunks(REINDEX_BATCH) {
        let texts: Vec<&str> = chunk.iter().map(|(_, summary)| summary.as_str()).collect();
        // One failed batch must not abandon the rest: a provider hiccup or a
        // single pathological summary would otherwise leave the store half
        // repaired with no indication of where it stopped.
        let vectors = match embedder.embed(&texts).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  {} batch failed: {e}", "!".yellow());
                failed += chunk.len();
                continue;
            }
        };
        if vectors.len() != chunk.len() {
            eprintln!(
                "  {} provider returned {} vector(s) for {} input(s); skipping batch",
                "!".yellow(),
                vectors.len(),
                chunk.len()
            );
            failed += chunk.len();
            continue;
        }
        for ((ep_id, _), vector) in chunk.iter().zip(vectors) {
            match store.store_embedding(ep_id, vector).await {
                Ok(()) => embedded += 1,
                Err(e) => {
                    eprintln!("  {} {ep_id}: {e}", "!".yellow());
                    failed += 1;
                }
            }
        }
        println!("  {} / {} embedded", embedded, pending.len());
    }

    let after = store.vector_coverage();
    println!(
        "\n{} {embedded} embedded, {failed} failed. Coverage {:.0}% → {:.0}% ({} / {}).",
        if failed == 0 {
            "✓".green()
        } else {
            "partial".yellow()
        },
        before.ratio() * 100.0,
        after.ratio() * 100.0,
        after.vectorized,
        after.total
    );
    if failed > 0 {
        println!("  Re-run to retry the {failed} that failed.");
    }
    Ok(())
}

async fn write_host_note(
    data_dir: Option<PathBuf>,
    kind: octos_memory::NoteKind,
    text: Vec<String>,
    sensitive: bool,
) -> Result<()> {
    let content = text.join(" ").trim().to_string();
    if content.is_empty() {
        eyre::bail!("empty request — provide the text to remember/forget");
    }
    let (data_dir, _config) = resolve(data_dir).await?;
    let memory_store = Arc::new(
        octos_memory::MemoryStore::open(&data_dir)
            .await
            .wrap_err("failed to open memory store")?,
    );
    let note = octos_memory::StagingNote {
        origin: octos_memory::NoteOrigin::Host,
        kind,
        content,
        session_key: None,
        sensitive,
        replaces_id: None,
    };
    let sensitive = note.sensitive;
    let path = memory_store.write_staging_note(&note).await?;
    if sensitive {
        // No content-derived path echo for sensitive requests.
        println!("{} recorded", "host note".green().bold());
    } else {
        println!(
            "{} recorded at {}",
            "host note".green().bold(),
            path.display()
        );
    }
    println!("It applies on the next consolidation pass (or run `octos memory refresh` now).");
    Ok(())
}

async fn resolve(data_dir: Option<PathBuf>) -> Result<(PathBuf, Config)> {
    let ctx = super::resolve_command_context(data_dir)?;
    let cwd = std::env::current_dir().wrap_err("failed to get current directory")?;
    let config = Config::load_with_context(&cwd, &ctx)?;
    Ok((ctx.data_dir, config))
}

async fn run_refresh(data_dir: Option<PathBuf>) -> Result<()> {
    let (data_dir, config) = resolve(data_dir).await?;
    let memory_store = Arc::new(
        octos_memory::MemoryStore::open(&data_dir)
            .await
            .wrap_err("failed to open memory store")?,
    );

    // Manual runs work with the flag off (this is the operator/testing
    // path); the background service still requires `enabled`.
    let (llm, provider_name, _router, _strong) =
        crate::commands::gateway::profile_factory::build_llm_stack(&config, false)?;
    let refresh_cfg = config.memory.as_ref().and_then(|m| m.refresh.as_ref());
    let provider = crate::memory_refresh::resolve_refresh_provider(
        &config,
        llm.clone(),
        refresh_cfg.and_then(|r| r.extract_model.as_deref()),
    );
    let consolidate_provider = crate::memory_refresh::resolve_refresh_provider(
        &config,
        llm,
        refresh_cfg.and_then(|r| r.consolidate_model.as_deref()),
    );
    let knobs = crate::config::MemoryRefreshConfig::knobs(config.memory.as_ref());

    println!(
        "{} one extraction pass ({} via {provider_name})",
        "octos memory refresh".cyan().bold(),
        provider.model_id()
    );
    let report = crate::memory_refresh::run_once(
        &data_dir,
        &memory_store,
        provider.as_ref(),
        consolidate_provider,
        &knobs,
    )
    .await?;
    println!(
        "candidates: {}  extracted: {}  budget-limited: {}",
        report.candidates, report.extracted, report.skipped_budget
    );
    println!(
        "pending extractions: {}  pending notes: {}",
        memory_store.count_staging_extractions().await,
        memory_store.count_staging_notes().await
    );
    Ok(())
}

async fn run_status(data_dir: Option<PathBuf>) -> Result<()> {
    let (data_dir, _config) = resolve(data_dir).await?;
    let memory_store = Arc::new(
        octos_memory::MemoryStore::open(&data_dir)
            .await
            .wrap_err("failed to open memory store")?,
    );
    println!("{}", "octos memory status".cyan().bold());
    println!(
        "{}",
        crate::memory_refresh::refresh_status(&data_dir, &memory_store).await
    );
    Ok(())
}
