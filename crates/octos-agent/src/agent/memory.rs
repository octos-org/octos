//! Initial message building and episodic memory context for the agent.

use octos_core::{Message, MessageRole, Task};
use octos_memory::{Episode, HybridScore};
use tracing::warn;

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

impl Agent {
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

        // Hybrid (embedding-aware) path returns scored matches so we can
        // filter out below-threshold noise that would otherwise contaminate
        // unrelated sessions. The cwd-scoped `find_relevant` fallback is
        // already filtered by working directory and keyword overlap, so it
        // doesn't need the score gate.
        if let Some(ref embedder) = self.embedder {
            // Push the modality-aware similarity floor down into the
            // index via `_filtered`. The floor is applied to every
            // candidate that contributed in either modality BEFORE the
            // combined-rank truncation to `limit`, so a high-
            // `best_modality` low-`combined` candidate (a keyword-perfect
            // older episode) survives even when many sub-threshold
            // vector-only hits would otherwise crowd it out. Codex P2
            // round 2 flagged that a fixed agent-side over-fetch wasn't
            // sufficient for larger memory stores; pushing the floor
            // into the index makes the BM25-only recall guarantee hold
            // regardless of store size.
            let scored_result = match embedder.embed(&[query.as_str()]).await {
                Ok(vecs) => {
                    let query_emb = vecs.into_iter().next();
                    self.memory
                        .find_relevant_hybrid_scored_filtered(
                            &query,
                            query_emb,
                            RELEVANT_EXPERIENCES_INJECT_LIMIT,
                            Some(MIN_EPISODE_SIMILARITY),
                        )
                        .await
                }
                Err(e) => {
                    warn!(error = %e, "embedding failed, falling back to keyword search");
                    self.memory
                        .find_relevant_hybrid_scored_filtered(
                            &query,
                            None,
                            RELEVANT_EXPERIENCES_INJECT_LIMIT,
                            Some(MIN_EPISODE_SIMILARITY),
                        )
                        .await
                }
            };

            if let Ok(scored) = scored_result {
                if let Some(content) = format_relevant_experiences(&scored) {
                    messages.push(Message {
                        role: MessageRole::System,
                        content,
                        media: vec![],
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                        client_message_id: None,
                        thread_id: None,
                        timestamp: chrono::Utc::now(),
                    });
                }
            }
        } else if let Ok(episodes) = self
            .memory
            .find_relevant(&task.context.working_dir, &query, 3)
            .await
        {
            // CWD-scoped fallback path. No scoring infrastructure here;
            // the cwd filter + keyword match already constrain results.
            // Inject only when there's something to inject (no empty header).
            if !episodes.is_empty() {
                let content = render_relevant_experiences(&episodes);
                messages.push(Message {
                    role: MessageRole::System,
                    content,
                    media: vec![],
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    client_message_id: None,
                    thread_id: None,
                    timestamp: chrono::Utc::now(),
                });
            }
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
    Some(render_relevant_experiences_iter(filtered.into_iter()))
}

/// Render a slice of episodes (no scores) into the "Relevant Past
/// Experiences" system message body. Used by the cwd-scoped fallback path
/// where scores aren't available; the cwd filter constrains noise instead.
fn render_relevant_experiences(episodes: &[Episode]) -> String {
    render_relevant_experiences_iter(episodes.iter())
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
}
