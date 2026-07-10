//! Initial message building and episodic memory context for the agent.

use octos_core::{Message, MessageRole, Task};
use octos_memory::{Episode, HybridScore};
use tracing::{info, warn};

use super::Agent;

/// Minimum per-modality similarity score required for a retrieved past
/// episode to be injected into the agent's prompt as a "Relevant Past
/// Experience".
///
/// **Why this constant exists.** `EpisodeStore::find_relevant_hybrid` returns
/// the top-K episodes by hybrid score regardless of how relevant they
/// actually are. With an empty or sparsely populated query domain, the top
/// match can still have a near-zero score — yet the agent loop used to
/// inject it as a "Relevant Past Experience". This contaminated unrelated
/// sessions (round-2 soak NEW-06: a JWST research prompt was answered using
/// episodes from a prior Tim Cook / GPT-5.5 podcast session).
///
/// **History.** 0.35 was the original codex-recommended baseline introduced
/// in PR #1192. Round-3 soak (2026-05-23) showed it was still too lax —
/// 3 of 4 minis (mini1/mini2/mini5) returned contaminated content on a
/// JWST prompt (Apple CEO / GPT-5.5 / agentic-AI-history episodes leaked
/// through). Only mini3 stayed on-topic. The cross-session noise floor
/// at 0.35 is high enough that fuzzy BM25/cosine overlaps between
/// *unrelated* topic domains still clear the gate. Tightening to 0.55
/// requires a substantially closer semantic match — keyword-perfect
/// (BM25 1.0) or genuinely on-topic vector hits still pass, but loose
/// "both mention some shared token" noise no longer does.
///
/// **Modality-aware gating.** The gate is compared against
/// [`HybridScore::best_modality`] (the max of BM25 and vector), not
/// against the configured weighted-sum `combined` score. Otherwise a
/// keyword-perfect match scoring `1.0` on BM25 would be capped at
/// `bm25_weight` (`0.3` with defaults) when an embedder is configured,
/// and the gate would always strand legitimately relevant single-modality
/// matches (older episodes without embeddings, or queries that don't
/// overlap any episode summary keywords).
///
/// Exposed as `pub const` and re-exported from the crate root so
/// operators / admin tooling can reference the threshold without
/// forking.
pub const MIN_EPISODE_SIMILARITY: f32 = 0.55;

/// Maximum number of "Relevant Past Experiences" injected into the
/// agent's prompt. The hybrid-scored search applies the
/// [`MIN_EPISODE_SIMILARITY`] floor INSIDE the index via
/// `find_relevant_hybrid_scored_filtered`, before its
/// combined-rank truncation — so this limit caps survivors only,
/// not the candidate pool. The agent-side `format_relevant_experiences`
/// also re-checks the floor as defense in depth.
const RELEVANT_EXPERIENCES_INJECT_LIMIT: usize = 6;

/// Neutral, non-misattributing log line emitted when this agent runs
/// without an embedder configured and therefore skips episodic recall.
///
/// **Why this exists (the misdiagnostic-WARN fix).** The previous
/// no-embedder branch logged at `warn!` and told the operator to
/// "investigate the `RunPipelineTool::with_embedder` wiring (NEW-06)".
/// That advice is wrong for the common case: on a host with no embedder
/// configured at all (the soak hosts), `create_embedder()` returns
/// `None` and `self.embedder` is legitimately `None` — the wiring is
/// fine, there is simply nothing to propagate. The only state knowable
/// at this call site is "this agent has no embedder" (`self.embedder`
/// is `None`); we cannot distinguish "never had one" from "had a parent
/// embedder but lost it in the worker" here, because a worker that lost
/// its embedder also just presents as `embedder == None`. Since we
/// cannot tell them apart, we must NOT assert the wiring is broken —
/// that misdirected an operator into chasing a non-bug. So the message
/// is downgraded to a neutral `debug!`, with no NEW-06 / wiring
/// attribution.
const NO_EMBEDDER_RECALL_SKIPPED_MSG: &str =
    "memory recall: no embedder configured; skipping episodic recall (no cross-task injection)";

impl Agent {
    /// Embedding-backed episodic recall for `query`, gated by the shared
    /// contamination guard: hybrid scored+filtered search at the
    /// [`MIN_EPISODE_SIMILARITY`] floor when an embedder is present, and a
    /// NO-OP (`None`) otherwise — BM25-only same-workspace recall can't
    /// tell on-task from cross-task and would leak unrelated memory.
    /// Returns the "Relevant Past Experiences" system message to inject, or
    /// `None`. Shared by the task loop ([`Self::build_initial_messages`])
    /// and conversational session-start recall (#1587). `cwd` is used only
    /// for the no-embedder diagnostic log.
    pub(super) async fn recall_relevant_episodes(
        &self,
        query: &str,
        cwd: &std::path::Path,
        include_conversations: bool,
    ) -> Option<Message> {
        let Some(ref embedder) = self.embedder else {
            tracing::debug!(
                caller = "agent_memory_no_embedder",
                agent = %self.id,
                cwd = %cwd.display(),
                query_len = query.len(),
                "{NO_EMBEDDER_RECALL_SKIPPED_MSG}"
            );
            return None;
        };
        // Source isolation (codex #1618 P2): a TASK's generic query (e.g.
        // the fixed "code review") can BM25-admit an unrelated CONVERSATION
        // summary past the 0.55 floor. Task recall excludes conversation
        // episodes (the store guarantees none leak in); conversation recall
        // keeps both.
        let exclude_conversation = !include_conversations;
        let scored_result = match embedder.embed(&[query]).await {
            Ok(vecs) => {
                let query_emb = vecs.into_iter().next();
                self.memory
                    .find_relevant_hybrid_scored_filtered(
                        query,
                        query_emb,
                        RELEVANT_EXPERIENCES_INJECT_LIMIT,
                        Some(MIN_EPISODE_SIMILARITY),
                        exclude_conversation,
                    )
                    .await
            }
            Err(e) => {
                warn!(error = %e, "embedding failed, falling back to keyword search");
                self.memory
                    .find_relevant_hybrid_scored_filtered(
                        query,
                        None,
                        RELEVANT_EXPERIENCES_INJECT_LIMIT,
                        Some(MIN_EPISODE_SIMILARITY),
                        exclude_conversation,
                    )
                    .await
            }
        };
        let scored = scored_result.ok()?;
        // NEW-06 diagnostic: one structured line naming the admitted
        // episodes + per-modality scores, so operators can confirm the gate
        // without inspecting the model-visible prompt.
        let admitted: Vec<(String, f32, f32, f32)> = scored
            .iter()
            .filter(|(_, s)| s.best_modality() >= MIN_EPISODE_SIMILARITY)
            .map(|(ep, s)| (ep.id.clone(), s.bm25, s.vector, s.combined))
            .collect();
        info!(
            caller = "agent_memory_hybrid",
            agent = %self.id,
            query_len = query.len(),
            candidates = scored.len(),
            admitted = admitted.len(),
            threshold = MIN_EPISODE_SIMILARITY,
            episodes = ?admitted,
            "memory recall: hybrid scored + filtered path"
        );
        let content = format_relevant_experiences(&scored)?;
        Some(Message {
            role: MessageRole::System,
            content,
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        })
    }

    /// Persist a conversation's compaction summary as a searchable episode
    /// (#1587 write side): store it and — fire-and-forget so the turn isn't
    /// blocked — embed it, so a future conversation's session-start recall
    /// ([`Self::recall_relevant_episodes`]) can surface it. No-op unless
    /// episodes are enabled AND an embedder is present: an unembedded
    /// episode is invisible to every (embedder-gated) recall path, so
    /// storing it would only bloat the index.
    pub(super) async fn save_conversation_episode(&self, summary: String) {
        if !self.config.save_episodes || self.embedder.is_none() {
            return;
        }
        let summary_truncated = octos_core::truncated_utf8(&summary, 500, "...");
        if summary_truncated.trim().is_empty() {
            return;
        }
        let cwd = self
            .tools
            .workspace_root()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));

        let mut episode = octos_memory::Episode::new(
            octos_core::TaskId::new(),
            self.id.clone(),
            cwd,
            summary_truncated.clone(),
            octos_memory::EpisodeOutcome::Success,
        );
        episode.source = octos_memory::EpisodeSource::Conversation;
        let ep_id = episode.id.clone();
        // Save ONCE per session, at the first compaction. Preflight
        // compaction fires on iteration 1 of EVERY turn once the
        // conversation is large, so saving on each would grow per-turn
        // (codex #1618 round-3). Supersede-via-delete was rejected: the
        // hybrid index only TOMBSTONES on delete (never reclaims), so it
        // would churn the index per compaction (round-4). One episode
        // captures the conversation's first substantial summary — enough
        // for "was there a conversation about X"; durable facts are handled
        // separately by the memory-refresh pipeline. The swap makes
        // concurrent saves collapse to exactly one.
        if self
            .conversation_episode_saved
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        if let Err(e) = self.memory.store(episode).await {
            warn!(error = %e, "failed to save conversation episode");
            // Allow a retry on the next compaction — the save didn't land.
            self.conversation_episode_saved
                .store(false, std::sync::atomic::Ordering::Release);
            return;
        }
        info!(
            episode_id = %ep_id,
            summary_len = summary_truncated.len(),
            "saved conversation episode (#1587 write side)"
        );

        // Fire-and-forget embed (mirrors the task-loop pattern): compaction
        // already ran mid-turn, so don't add an embed round-trip to the
        // turn's latency.
        if let Some(ref embedder) = self.embedder {
            let embedder = embedder.clone();
            let memory = self.memory.clone();
            tokio::spawn(async move {
                match embedder.embed(&[summary_truncated.as_str()]).await {
                    Ok(vecs) => {
                        if let Some(vec) = vecs.into_iter().next() {
                            if let Err(e) = memory.store_embedding(&ep_id, vec).await {
                                warn!(error = %e, "failed to store conversation-episode embedding");
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, episode_id = %ep_id, "failed to embed conversation episode");
                    }
                }
            });
        }
    }

    pub(super) async fn build_initial_messages(&self, task: &Task) -> Vec<Message> {
        let mut messages = vec![Message {
            role: MessageRole::System,
            content: super::execution::compose_system_prompt(self),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }];

        // Add working memory from context
        messages.extend(task.context.working_memory.clone());

        // Query episodic memory for relevant past experiences
        let query = match &task.kind {
            octos_core::TaskKind::Plan { goal } => goal.clone(),
            octos_core::TaskKind::Code { instruction, .. } => instruction.clone(),
            octos_core::TaskKind::Review { .. } => "code review".to_string(),
            octos_core::TaskKind::Test { command } => command.clone(),
            octos_core::TaskKind::Custom { name, .. } => name.clone(),
        };

        // Episodic recall (embedder-gated contamination guard lives in the
        // shared helper). WITHOUT an embedder it is a no-op: BM25-only
        // keyword overlap within a single shared workspace can't
        // discriminate on-task from cross-task and would leak stale memory.
        if let Some(msg) = self
            .recall_relevant_episodes(&query, &task.context.working_dir, false)
            .await
        {
            messages.push(msg);
        }

        // Add the task as user message
        let task_content = match &task.kind {
            octos_core::TaskKind::Plan { goal } => format!("Plan how to accomplish: {goal}"),
            octos_core::TaskKind::Code { instruction, files } => {
                let files_str = files
                    .iter()
                    .map(|f| f.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Code task: {instruction}\nFiles in scope: {files_str}")
            }
            octos_core::TaskKind::Review { diff } => format!("Review this diff:\n{diff}"),
            octos_core::TaskKind::Test { command } => format!("Run test: {command}"),
            octos_core::TaskKind::Custom { name, params } => {
                format!("Custom task '{name}': {params}")
            }
        };

        messages.push(Message {
            role: MessageRole::User,
            content: task_content,
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });

        messages
    }
}

/// Format scored hybrid-search results into the "Relevant Past Experiences"
/// system message body. Episodes whose best per-modality score falls below
/// [`MIN_EPISODE_SIMILARITY`] are dropped to prevent cross-session
/// contamination (NEW-06). Returns `None` when no episode survives the
/// filter — callers must omit the entire system message in that case
/// instead of injecting an empty header.
///
/// Gating uses [`HybridScore::best_modality`] (max of BM25 and vector)
/// rather than the configured weighted-sum `combined` score, so a
/// keyword-perfect older episode without a stored embedding still
/// passes — see the `MIN_EPISODE_SIMILARITY` docs for the rationale.
///
/// `scored` is expected to be sorted by descending combined score (the
/// order `EpisodeStore::find_relevant_hybrid_scored_filtered` returns).
/// The agent calls `_filtered` with the floor pushed into the index so
/// the dead-band crowd-out cannot occur; the filter below is
/// defense-in-depth so this helper stays correct even if a future
/// caller forgets to push the floor down (it would just receive an
/// already-filtered set). Relative order is preserved.
fn format_relevant_experiences(scored: &[(Episode, HybridScore)]) -> Option<String> {
    let filtered: Vec<&Episode> = scored
        .iter()
        .filter(|(_, score)| score.best_modality() >= MIN_EPISODE_SIMILARITY)
        .map(|(ep, _)| ep)
        .take(RELEVANT_EXPERIENCES_INJECT_LIMIT)
        .collect();
    if filtered.is_empty() {
        return None;
    }
    // Episodic summaries are HISTORICAL records — spawn/pipeline workers
    // receive them with no other memory context, so the staleness
    // etiquette must ride along here too (#1589 codex P2: the similarity
    // floor checks relevance, not freshness).
    Some(format!(
        "{}\n{}",
        render_relevant_experiences_iter(filtered.into_iter()),
        crate::memory_segment::MEMORY_USE_GUIDANCE
    ))
}

fn render_relevant_experiences_iter<'a, I>(iter: I) -> String
where
    I: Iterator<Item = &'a Episode>,
{
    let mut context_str = String::from("## Relevant Past Experiences\n\n");
    for ep in iter {
        context_str.push_str(&format!(
            "### {} ({})\n{}\n",
            ep.task_id,
            match ep.outcome {
                octos_memory::EpisodeOutcome::Success => "succeeded",
                octos_memory::EpisodeOutcome::Failure => "failed",
                octos_memory::EpisodeOutcome::Blocked => "blocked",
                octos_memory::EpisodeOutcome::Cancelled => "cancelled",
            },
            ep.summary
        ));
        if !ep.key_decisions.is_empty() {
            context_str.push_str("Key decisions:\n");
            for decision in &ep.key_decisions {
                context_str.push_str(&format!("- {decision}\n"));
            }
        }
        context_str.push('\n');
    }
    context_str
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::{AgentId, TaskId};
    use octos_memory::{Episode, EpisodeOutcome, HybridScore};
    use std::path::PathBuf;

    fn make_episode(summary: &str) -> Episode {
        Episode::new(
            TaskId::new(),
            AgentId::new("test-agent"),
            PathBuf::from("/proj"),
            summary.into(),
            EpisodeOutcome::Success,
        )
    }

    /// Construct a HybridScore where the BM25 channel carries the test's
    /// chosen similarity value (vector left at zero, combined unused for
    /// gating). The agent gate compares against `best_modality()` so this
    /// mirrors the "older episode with no embedding" case.
    fn score_bm25(s: f32) -> HybridScore {
        HybridScore {
            combined: s,
            bm25: s,
            vector: 0.0,
        }
    }

    #[test]
    fn episode_injection_filters_below_threshold() {
        // 3 episodes scored at [0.7, 0.5, 0.1]; only the 0.7 episode is
        // above the 0.55 threshold so only it should appear in the
        // formatted message.
        let scored = vec![
            (
                make_episode("HIGH RELEVANCE rust ownership"),
                score_bm25(0.7),
            ),
            (make_episode("MID RELEVANCE python flask"), score_bm25(0.5)),
            (
                make_episode("LOW RELEVANCE weather report"),
                score_bm25(0.1),
            ),
        ];

        let rendered =
            format_relevant_experiences(&scored).expect("at least one episode above threshold");
        assert!(rendered.contains("## Relevant Past Experiences"));
        // #1589: episodic summaries are historical — the staleness
        // etiquette must ride with them (workers get no other memory
        // context).
        assert!(
            rendered.contains("## Memory Use"),
            "episodic recall message must carry the use-guidance"
        );
        assert!(
            rendered.contains("HIGH RELEVANCE rust ownership"),
            "expected the above-threshold episode to be present"
        );
        assert!(
            !rendered.contains("MID RELEVANCE python flask"),
            "expected the 0.50 episode to be filtered (below threshold 0.55)"
        );
        assert!(
            !rendered.contains("LOW RELEVANCE weather report"),
            "expected the 0.10 episode to be filtered"
        );
    }

    #[test]
    fn episode_injection_skipped_when_all_below_threshold() {
        // All scores < MIN_EPISODE_SIMILARITY (0.55). Expect None so the
        // caller skips injecting the "Past Experiences" system message
        // entirely — no empty header allowed.
        let scored = vec![
            (make_episode("Noisy match A"), score_bm25(0.54)),
            (make_episode("Noisy match B"), score_bm25(0.40)),
            (make_episode("Noisy match C"), score_bm25(0.05)),
        ];

        assert!(
            format_relevant_experiences(&scored).is_none(),
            "no episode passes the threshold; expected None so caller omits the system message"
        );
    }

    #[test]
    fn episode_injection_preserves_top_match() {
        // Top score 0.8 should appear before lower-scored entries in the
        // formatted block (the function preserves input order which is the
        // hybrid search's descending-score order).
        let scored = vec![
            (make_episode("TOP MATCH first"), score_bm25(0.8)),
            (make_episode("RUNNER UP second"), score_bm25(0.6)),
        ];

        let rendered = format_relevant_experiences(&scored).expect("matches exist above threshold");
        let top_idx = rendered
            .find("TOP MATCH first")
            .expect("top match should be present");
        let runner_idx = rendered
            .find("RUNNER UP second")
            .expect("runner up should be present");
        assert!(
            top_idx < runner_idx,
            "top match (score 0.8) should appear before runner-up (score 0.6) — got top_idx={top_idx}, runner_idx={runner_idx}"
        );
    }

    #[test]
    fn episode_injection_filters_round_3_soak_contamination_scenario() {
        // Regression for round-3 fleet soak (NEW-06 round 2): on mini5 a
        // JWST research prompt rendered final content about "Apple CEO
        // Succession / Google-Anthropic / OpenAI GPT-5.5" because a prior
        // tech-news podcast episode survived the (then) 0.35 gate with a
        // weak cross-domain hybrid score of ~0.4. Such a score reflects
        // shared incidental tokens ("the", "research", "report") between
        // wholly unrelated topics, not genuine semantic overlap. The 0.55
        // gate keeps that loose noise out while still admitting close
        // matches (>=0.55 means substantial keyword overlap or strong
        // cosine similarity, not just shared boilerplate vocabulary).
        let scored = vec![(
            make_episode(
                "Tech news podcast: Apple CEO Tim Cook succession, John Ternus, GPT-5.5 launch",
            ),
            HybridScore {
                // Cross-domain noise score that USED to pass at 0.35
                // (PR #1192 baseline) — round-3 soak proved this is
                // still contamination, not signal.
                combined: 0.40,
                bm25: 0.40,
                vector: 0.40,
            },
        )];

        assert!(
            format_relevant_experiences(&scored).is_none(),
            "cross-domain Apple/GPT episode at hybrid score 0.40 must be filtered when query is \
             'James Webb telescope research' — 0.40 cleared the old 0.35 gate but is noise per \
             round-3 soak evidence"
        );

        // Sanity check the other direction: a genuinely on-topic match at
        // 0.7 still passes the tightened gate.
        let on_topic = vec![(
            make_episode("Deep research: James Webb Space Telescope observations 2024"),
            score_bm25(0.7),
        )];
        let rendered = format_relevant_experiences(&on_topic)
            .expect("on-topic JWST episode at 0.70 must still pass the tightened 0.55 gate");
        assert!(rendered.contains("James Webb Space Telescope"));
    }

    #[test]
    fn episode_injection_threshold_boundary_is_inclusive() {
        // Sanity: a score exactly at the threshold is admitted.
        let scored = vec![(
            make_episode("exactly at threshold"),
            score_bm25(MIN_EPISODE_SIMILARITY),
        )];
        let rendered = format_relevant_experiences(&scored)
            .expect("score == threshold should be admitted (>=)");
        assert!(rendered.contains("exactly at threshold"));
    }

    #[test]
    fn episode_injection_admits_bm25_only_match_with_weak_combined_score() {
        // Regression for codex P2 (PR #1192): when an embedder is
        // configured, the combined weighted-sum score for a BM25-only
        // match maxes out at `bm25_weight` (0.3 with defaults) — well
        // below the agent's similarity gate (now 0.55). The agent gate
        // uses `best_modality()` so the match still passes on its raw
        // BM25 score (1.0), preserving older-episode recall.
        let scored = vec![(
            make_episode("keyword-perfect older episode"),
            HybridScore {
                combined: 0.30, // weighted-sum (bm25_weight=0.3 * bm25=1.0)
                bm25: 1.0,
                vector: 0.0,
            },
        )];

        let rendered = format_relevant_experiences(&scored).expect(
            "keyword-perfect single-modality match must survive the gate even though combined (0.30) < threshold (0.55)",
        );
        assert!(rendered.contains("keyword-perfect older episode"));
    }

    #[test]
    fn episode_injection_dead_band_resolved_by_overfetch() {
        // Regression for codex P2 round-2 (this PR, agent-side defense
        // in depth): with the 0.55 gate, a small fetch limit would
        // create a dead band — six sub-threshold vector-only hits at
        // combined=0.378 each rank ABOVE one keyword-perfect BM25-only
        // episode at combined=0.30, so a limit-6 fetch returned only
        // the six vector hits, all of which then failed the gate.
        // Result: zero injected episodes, even though a perfect BM25
        // match existed.
        //
        // The contamination-safe fix lives in the index:
        // `find_relevant_hybrid_scored_filtered` applies the
        // `best_modality()` floor to every candidate (not just the
        // top-`limit`-by-combined) BEFORE truncation, so the BM25
        // winner survives regardless of store size.
        //
        // This unit test exercises the agent-side defense-in-depth
        // filter inside `format_relevant_experiences`: if a stray
        // caller ever passes through an unfiltered combined-sorted
        // list (e.g. via the older `find_relevant_hybrid_scored` entry
        // point), the agent still drops sub-threshold entries and
        // preserves the BM25 winner.
        let mut scored = Vec::new();
        for i in 0..6 {
            scored.push((
                make_episode(&format!("VECTOR NOISE {i}")),
                HybridScore {
                    // combined ~0.378 = vector_weight 0.7 * vector 0.54
                    combined: 0.378,
                    bm25: 0.0,
                    vector: 0.54, // sub-threshold (< 0.55)
                },
            ));
        }
        // The BM25-perfect episode appears LAST in combined-sorted order
        // (combined 0.30 < 0.378), but over-fetch ensures it is in the
        // candidate set.
        scored.push((
            make_episode("BM25 PERFECT older episode"),
            HybridScore {
                combined: 0.30,
                bm25: 1.0,
                vector: 0.0,
            },
        ));

        let rendered = format_relevant_experiences(&scored).expect(
            "the BM25-perfect episode must survive over-fetch + threshold filtering, not be dropped by the limit-6 truncate before the gate",
        );
        assert!(
            rendered.contains("BM25 PERFECT older episode"),
            "the BM25-only winner must reach the injected message"
        );
        for i in 0..6 {
            assert!(
                !rendered.contains(&format!("VECTOR NOISE {i}")),
                "sub-threshold vector hit {i} must be filtered out"
            );
        }
    }

    #[test]
    fn episode_injection_truncates_after_filtering_to_inject_limit() {
        // Beyond the dead-band fix, the formatter must still cap the
        // injected set at RELEVANT_EXPERIENCES_INJECT_LIMIT episodes
        // (currently 6) so over-fetched candidates don't bloat the LLM
        // prompt. Supply 10 above-threshold matches and assert only the
        // first 6 (highest combined rank) make it into the output.
        let mut scored = Vec::new();
        for i in 0..10 {
            scored.push((
                make_episode(&format!("RANK {i:02}")),
                // All clear the 0.55 gate; combined decreases with i so
                // input order is the descending-combined-rank order
                // we'd see from `find_relevant_hybrid_scored`.
                score_bm25(0.9 - (i as f32) * 0.01),
            ));
        }
        let rendered = format_relevant_experiences(&scored)
            .expect("matches above threshold should produce output");

        // First 6 ranks must appear.
        for i in 0..RELEVANT_EXPERIENCES_INJECT_LIMIT {
            let needle = format!("RANK {i:02}");
            assert!(
                rendered.contains(&needle),
                "expected top-rank '{needle}' in injected output"
            );
        }
        // Ranks 6..10 must be truncated.
        for i in RELEVANT_EXPERIENCES_INJECT_LIMIT..10 {
            let needle = format!("RANK {i:02}");
            assert!(
                !rendered.contains(&needle),
                "expected rank '{needle}' to be truncated past the inject limit ({RELEVANT_EXPERIENCES_INJECT_LIMIT})"
            );
        }
    }

    // ---- Fix #1: the no-embedder log must not misattribute to wiring ----

    /// The neutral no-embedder log line must NOT tell the operator to
    /// investigate the `RunPipelineTool::with_embedder` wiring (the prior
    /// misdiagnostic WARN). The only state knowable on that path is
    /// "this agent has no embedder", which is the expected condition on
    /// an unconfigured host — not evidence of a wiring bug — so the
    /// message must carry no NEW-06 / `with_embedder` blame.
    #[test]
    fn no_embedder_log_message_does_not_misattribute_to_wiring() {
        let msg = NO_EMBEDDER_RECALL_SKIPPED_MSG.to_ascii_lowercase();
        assert!(
            !msg.contains("with_embedder"),
            "no-embedder log must not blame RunPipelineTool::with_embedder wiring: {NO_EMBEDDER_RECALL_SKIPPED_MSG}"
        );
        assert!(
            !msg.contains("new-06"),
            "no-embedder log must not reference the NEW-06 wiring investigation: {NO_EMBEDDER_RECALL_SKIPPED_MSG}"
        );
        assert!(
            !msg.contains("propagate") && !msg.contains("investigate"),
            "no-embedder log must not direct the operator to investigate embedder propagation: {NO_EMBEDDER_RECALL_SKIPPED_MSG}"
        );
        // And it should still say *something* useful about why recall was skipped.
        assert!(
            msg.contains("embedder") && msg.contains("skip"),
            "no-embedder log should still explain that recall was skipped because no embedder is configured: {NO_EMBEDDER_RECALL_SKIPPED_MSG}"
        );
    }

    // ---- Fix #3 (option a): no-embedder recall injects nothing ----

    /// A stub provider that immediately ends the turn — enough to let us
    /// drive `build_initial_messages` through `Agent`.
    struct EndTurnProvider;

    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for EndTurnProvider {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            Ok(octos_llm::ChatResponse {
                content: Some(String::new()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }
        fn model_id(&self) -> &str {
            "test-model"
        }
        fn provider_name(&self) -> &str {
            "test"
        }
    }

    /// Contamination guard (the "Jingkang artifact" bug). On a host with
    /// NO embedder configured, a prior *unrelated* episode that merely
    /// shares vocabulary with the current task — and lives in the SAME
    /// workspace (so the old cwd filter would have admitted it) — must
    /// NOT be injected into the fresh task's prompt.
    ///
    /// Pre-fix: `build_initial_messages` fell into the no-embedder branch,
    /// ran a BM25-only same-cwd recall, and injected the shared-vocab
    /// episode as a "Relevant Past Experience" — exactly how a stale
    /// "Jingkang Incident" research doc leaked into a code-review run.
    /// Post-fix (option a): the no-embedder branch skips recall entirely,
    /// so no "Relevant Past Experiences" message is produced.
    /// Embedder that returns one fixed unit vector for every text, so a
    /// stored episode carrying the SAME vector matches at cosine 1.0 —
    /// deterministic vector-modality admission independent of BM25.
    struct ConstEmbedder;

    #[async_trait::async_trait]
    impl octos_llm::EmbeddingProvider for ConstEmbedder {
        async fn embed(&self, texts: &[&str]) -> eyre::Result<Vec<Vec<f32>>> {
            Ok(vec![Self::vector(); texts.len()])
        }
        fn dimension(&self) -> usize {
            octos_memory::EPISODIC_INDEX_DIMENSION
        }
    }
    impl ConstEmbedder {
        fn vector() -> Vec<f32> {
            let mut v = vec![0.0_f32; octos_memory::EPISODIC_INDEX_DIMENSION];
            v[0] = 1.0;
            v
        }
    }

    #[tokio::test]
    async fn recall_returns_experiences_when_embedder_present_and_match() {
        use crate::{Agent, tools::ToolRegistry};
        use octos_memory::EpisodeStore;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let memory = Arc::new(EpisodeStore::open(workspace.join("memory")).await.unwrap());

        let ep = Episode::new(
            TaskId::new(),
            AgentId::new("prior"),
            workspace.clone(),
            "Tuned the websocket gateway keepalive from 60s to 25s".to_string(),
            EpisodeOutcome::Success,
        );
        let ep_id = ep.id.clone();
        memory.store(ep).await.unwrap();
        // Store the SAME vector the embedder returns → cosine 1.0 match.
        memory
            .store_embedding(&ep_id, ConstEmbedder::vector())
            .await
            .unwrap();

        let provider: Arc<dyn octos_llm::LlmProvider> = Arc::new(EndTurnProvider);
        let tools = ToolRegistry::with_builtins(&workspace);
        let agent = Agent::new(AgentId::new("chat"), provider, tools, memory)
            .with_embedder(Arc::new(ConstEmbedder));

        let msg = agent
            .recall_relevant_episodes("clients keep dropping", &workspace, true)
            .await
            .expect("embedder present + vector match must recall");
        assert!(msg.content.contains("Relevant Past Experiences"));
        assert!(msg.content.contains("keepalive"));
    }

    #[tokio::test]
    async fn save_conversation_episode_stores_recallable_episode() {
        use crate::{Agent, tools::ToolRegistry};
        use octos_memory::EpisodeStore;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let memory = Arc::new(EpisodeStore::open(workspace.join("memory")).await.unwrap());

        let provider: Arc<dyn octos_llm::LlmProvider> = Arc::new(EndTurnProvider);
        let tools = ToolRegistry::with_builtins(&workspace);
        let agent = Agent::new(AgentId::new("chat"), provider, tools, memory.clone())
            .with_embedder(Arc::new(ConstEmbedder));

        agent
            .save_conversation_episode(
                "We diagnosed the websocket gateway keepalive dropping clients".to_string(),
            )
            .await;

        // The store write is awaited (the embed is fire-and-forget); the
        // episode is BM25-findable in its cwd immediately.
        let hits = memory
            .find_relevant(&workspace, "websocket gateway keepalive", 5)
            .await
            .unwrap();
        assert!(
            hits.iter().any(|e| e.summary.contains("keepalive")),
            "conversation episode must be stored + recallable"
        );
    }

    #[tokio::test]
    async fn task_recall_excludes_conversation_episodes() {
        use crate::{Agent, tools::ToolRegistry};
        use octos_memory::EpisodeStore;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let memory = Arc::new(EpisodeStore::open(workspace.join("memory")).await.unwrap());

        let mut conv = Episode::new(
            TaskId::new(),
            AgentId::new("chat"),
            workspace.clone(),
            "Chatted about a code review of the parser".to_string(),
            EpisodeOutcome::Success,
        );
        conv.source = octos_memory::EpisodeSource::Conversation;
        let conv_id = conv.id.clone();
        memory.store(conv).await.unwrap();
        memory
            .store_embedding(&conv_id, ConstEmbedder::vector())
            .await
            .unwrap();

        let provider: Arc<dyn octos_llm::LlmProvider> = Arc::new(EndTurnProvider);
        let tools = ToolRegistry::with_builtins(&workspace);
        let agent = Agent::new(AgentId::new("worker"), provider, tools, memory)
            .with_embedder(Arc::new(ConstEmbedder));

        // Task path (include_conversations = false): must NOT surface it.
        let task_recall = agent
            .recall_relevant_episodes("code review", &workspace, false)
            .await;
        assert!(
            task_recall
                .as_ref()
                .map(|m| !m.content.contains("code review of the parser"))
                .unwrap_or(true),
            "task recall must exclude conversation episodes"
        );
        // Conversation path (true): the same episode IS recallable.
        let conv_recall = agent
            .recall_relevant_episodes("code review", &workspace, true)
            .await
            .expect("conversation recall includes conversation episodes");
        assert!(conv_recall.content.contains("code review of the parser"));
    }

    #[tokio::test]
    async fn save_conversation_episode_saves_once_per_session() {
        use crate::{Agent, tools::ToolRegistry};
        use octos_memory::EpisodeStore;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let memory = Arc::new(EpisodeStore::open(workspace.join("memory")).await.unwrap());
        let provider: Arc<dyn octos_llm::LlmProvider> = Arc::new(EndTurnProvider);
        let tools = ToolRegistry::with_builtins(&workspace);
        let agent = Agent::new(AgentId::new("chat"), provider, tools, memory.clone())
            .with_embedder(Arc::new(ConstEmbedder));

        // Save-once per session: the first compaction's summary is kept;
        // later compactions are no-ops. Exactly one conversation episode.
        agent
            .save_conversation_episode("first chunk about widgets".to_string())
            .await;
        agent
            .save_conversation_episode("second chunk about gadgets".to_string())
            .await;

        let all = memory.find_relevant(&workspace, "chunk", 10).await.unwrap();
        let conv: Vec<_> = all
            .iter()
            .filter(|e| e.summary.contains("chunk about"))
            .collect();
        assert_eq!(
            conv.len(),
            1,
            "one conversation episode per session, got {}",
            conv.len()
        );
        assert!(
            conv[0].summary.contains("widgets"),
            "the first compaction's summary is the one kept"
        );
    }

    #[tokio::test]
    async fn save_conversation_episode_is_noop_without_embedder() {
        use crate::{Agent, tools::ToolRegistry};
        use octos_memory::EpisodeStore;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let memory = Arc::new(EpisodeStore::open(workspace.join("memory")).await.unwrap());
        let provider: Arc<dyn octos_llm::LlmProvider> = Arc::new(EndTurnProvider);
        let tools = ToolRegistry::with_builtins(&workspace);
        // No embedder: an unembedded conversation episode is invisible to
        // recall, so we must not store it at all.
        let agent = Agent::new(AgentId::new("chat"), provider, tools, memory.clone());

        agent
            .save_conversation_episode("some conversation summary".to_string())
            .await;

        let hits = memory
            .find_relevant(&workspace, "conversation summary", 5)
            .await
            .unwrap();
        assert!(hits.is_empty(), "no episode without an embedder");
    }

    #[tokio::test]
    async fn recall_is_noop_without_embedder() {
        use crate::{Agent, tools::ToolRegistry};
        use octos_memory::EpisodeStore;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let memory = Arc::new(EpisodeStore::open(workspace.join("memory")).await.unwrap());
        memory
            .store(Episode::new(
                TaskId::new(),
                AgentId::new("prior"),
                workspace.clone(),
                "Some prior episode about gateways".to_string(),
                EpisodeOutcome::Success,
            ))
            .await
            .unwrap();

        let provider: Arc<dyn octos_llm::LlmProvider> = Arc::new(EndTurnProvider);
        let tools = ToolRegistry::with_builtins(&workspace);
        // Agent::new defaults embedder = None.
        let agent = Agent::new(AgentId::new("chat"), provider, tools, memory);

        assert!(
            agent
                .recall_relevant_episodes("gateways", &workspace, true)
                .await
                .is_none(),
            "no-embedder recall must be a no-op"
        );
    }

    #[tokio::test]
    async fn no_embedder_path_does_not_inject_shared_vocab_episode() {
        use crate::{Agent, tools::ToolRegistry};
        use octos_core::{TaskContext, TaskKind};
        use octos_memory::EpisodeStore;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_path_buf();

        let memory = Arc::new(EpisodeStore::open(workspace.join("memory")).await.unwrap());

        // A prior, unrelated episode that shares vocabulary ("history",
        // "incident", "report") with a code-review-style query, stored in
        // the SAME workspace so a cwd-scoped BM25 recall WOULD have matched
        // it. This stands in for the stale "Jingkang Incident" history doc.
        let contaminating = Episode::new(
            TaskId::new(),
            AgentId::new("prior-task"),
            workspace.clone(),
            "Researched the Jingkang Incident history report and synthesized a timeline"
                .to_string(),
            EpisodeOutcome::Success,
        );
        memory.store(contaminating).await.unwrap();

        // Sanity: the store CAN find it by shared vocabulary in this cwd
        // (proving the contamination vector is real and the guard, not a
        // cwd/keyword miss, is what closes it).
        let hits = memory
            .find_relevant(&workspace, "review the incident history report", 5)
            .await
            .unwrap();
        assert!(
            hits.iter().any(|e| e.summary.contains("Jingkang")),
            "precondition: the shared-vocab episode is recallable by BM25 in this cwd"
        );

        // Agent::new defaults embedder=None — the no-embedder path.
        let provider: Arc<dyn octos_llm::LlmProvider> = Arc::new(EndTurnProvider);
        let tools = ToolRegistry::with_builtins(&workspace);
        let agent = Agent::new(AgentId::new("reviewer"), provider, tools, memory);

        let task = octos_core::Task::new(
            TaskKind::Custom {
                name: "review the incident history report".to_string(),
                params: serde_json::Value::Null,
            },
            TaskContext {
                working_dir: workspace.clone(),
                ..Default::default()
            },
        );

        let messages = agent.build_initial_messages(&task).await;

        // No message may carry the stale episode or a "Relevant Past
        // Experiences" header — recall is skipped entirely on this path.
        for m in &messages {
            assert!(
                !m.content.contains("Jingkang"),
                "no-embedder path injected the stale cross-task episode: {:?}",
                m.content
            );
            assert!(
                !m.content.contains("Relevant Past Experiences"),
                "no-embedder path must not inject any past-experience block: {:?}",
                m.content
            );
        }
    }
}
