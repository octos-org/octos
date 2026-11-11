//! Context ledger ordering that the turn-start coverage check depends on.
use super::*;

/// A turn that opens with a new runtime note (the workspace team roster, a
/// memory snapshot) commits its prompt after that note was recorded. The
/// agent's own list reads history, note, prompt. The ledger must read the
/// same: turn start records every row the ledger does not cover, and coverage
/// is one contiguous window or nothing, so a swapped pair records the whole
/// conversation a second time for that turn.
#[test]
fn should_not_record_history_twice_when_a_turn_opens_with_a_new_runtime_note() {
    let mut manager = ContextManager::new("local:turn-note", None);
    manager.record_persisted_message(&Message::user("Earlier question"), 0);
    manager.record_persisted_message(&Message::assistant("Earlier answer"), 1);
    manager.record_context_snapshot(
        ContextEventKind::PeerResultsReady,
        "workspace-peer-team",
        "Workspace peer team: two members",
        false,
    );
    let policy = PromptBuildPolicy::default();
    let prompt = Message::user("Next question");
    let mut handed_to_agent = manager.for_prompt(&policy).messages;
    handed_to_agent.push(prompt.clone());

    manager.record_persisted_message_merging_prompt_equivalent(&prompt, 2);

    let ledger = manager.for_prompt(&policy).messages;
    let uncovered: Vec<&str> = covered_prompt_message_indices(&handed_to_agent, &ledger)
        .into_iter()
        .zip(&handed_to_agent)
        .filter(|(covered, _)| !covered)
        .map(|(_, row)| row.content.as_str())
        .collect();
    assert!(
        uncovered.is_empty(),
        "turn start would record these rows again: {uncovered:?}"
    );
}
