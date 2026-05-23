//! Initial message building and episodic memory context for the agent.

use octos_core::{Message, MessageRole, Task};
use octos_memory::Episode;
use tracing::warn;

use super::Agent;

/// Minimum hybrid similarity score required for a retrieved past episode
/// to be injected into the agent's prompt as a "Relevant Past Experience".
///
/// **Why this constant exists.** `EpisodeStore::find_relevant_hybrid` returns
/// the top-K episodes by hybrid score regardless of how relevant they
/// actually are. With an empty or sparsely populated query domain, the top
/// match can still have a near-zero score — yet the agent loop used to
/// inject it as a "Relevant Past Experience". This contaminated unrelated
/// sessions (round-2 soak NEW-06: a JWST research prompt was answered using
/// episodes from a prior Tim Cook / GPT-5.5 podcast session).
///
/// 0.35 is the codex-recommended baseline: above this, the hybrid score
/// reflects genuine BM25 keyword + cosine similarity overlap; below it the
/// match is essentially noise. Exposed as `pub const` so operators can tune
/// it without forking — exporting from the crate root is intentional so
/// downstream tools (e.g. an admin tuning helper) can reference the value.
pub const MIN_EPISODE_SIMILARITY: f32 = 0.35;

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
            let scored_result = match embedder.embed(&[query.as_str()]).await {
                Ok(vecs) => {
                    let query_emb = vecs.into_iter().next();
                    self.memory
                        .find_relevant_hybrid_scored(&query, query_emb, 6)
                        .await
                }
                Err(e) => {
                    warn!(error = %e, "embedding failed, falling back to keyword search");
                    self.memory
                        .find_relevant_hybrid_scored(&query, None, 6)
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
/// system message body. Episodes with score below
/// [`MIN_EPISODE_SIMILARITY`] are dropped to prevent cross-session
/// contamination (NEW-06). Returns `None` when no episode survives the
/// filter — callers must omit the entire system message in that case
/// instead of injecting an empty header.
///
/// `scored` is expected to be sorted by descending score (the order
/// `EpisodeStore::find_relevant_hybrid_scored` returns); the relative
/// order is preserved after filtering so the top match remains first.
fn format_relevant_experiences(scored: &[(Episode, f32)]) -> Option<String> {
    let filtered: Vec<&Episode> = scored
        .iter()
        .filter(|(_, score)| *score >= MIN_EPISODE_SIMILARITY)
        .map(|(ep, _)| ep)
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
    use octos_memory::{Episode, EpisodeOutcome};
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

    #[test]
    fn episode_injection_filters_below_threshold() {
        // 3 episodes scored at [0.5, 0.3, 0.1]; only the 0.5 episode is
        // above the 0.35 threshold so only it should appear in the
        // formatted message.
        let scored = vec![
            (make_episode("HIGH RELEVANCE rust ownership"), 0.5_f32),
            (make_episode("MID RELEVANCE python flask"), 0.3_f32),
            (make_episode("LOW RELEVANCE weather report"), 0.1_f32),
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
            "expected the 0.30 episode to be filtered (below threshold 0.35)"
        );
        assert!(
            !rendered.contains("LOW RELEVANCE weather report"),
            "expected the 0.10 episode to be filtered"
        );
    }

    #[test]
    fn episode_injection_skipped_when_all_below_threshold() {
        // All scores < MIN_EPISODE_SIMILARITY (0.35). Expect None so the
        // caller skips injecting the "Past Experiences" system message
        // entirely — no empty header allowed.
        let scored = vec![
            (make_episode("Noisy match A"), 0.34_f32),
            (make_episode("Noisy match B"), 0.20_f32),
            (make_episode("Noisy match C"), 0.05_f32),
        ];

        assert!(
            format_relevant_experiences(&scored).is_none(),
            "no episode passes the threshold; expected None so caller omits the system message"
        );
    }

    #[test]
    fn episode_injection_preserves_top_match() {
        // Top score 0.6 should appear before lower-scored entries in the
        // formatted block (the function preserves input order which is the
        // hybrid search's descending-score order).
        let scored = vec![
            (make_episode("TOP MATCH first"), 0.6_f32),
            (make_episode("RUNNER UP second"), 0.45_f32),
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
            "top match (score 0.6) should appear before runner-up (score 0.45) — got top_idx={top_idx}, runner_idx={runner_idx}"
        );
    }

    #[test]
    fn episode_injection_threshold_boundary_is_inclusive() {
        // Sanity: a score exactly at the threshold is admitted.
        let scored = vec![(make_episode("exactly at threshold"), MIN_EPISODE_SIMILARITY)];
        let rendered = format_relevant_experiences(&scored)
            .expect("score == threshold should be admitted (>=)");
        assert!(rendered.contains("exactly at threshold"));
    }
}
