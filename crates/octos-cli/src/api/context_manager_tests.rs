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

/// One user request driving a long tool loop (G2: a PPT task reached 529K
/// input tokens against a 262K window). Everything is pinned behind the
/// newest user turn, so compaction cannot summarize it; the outgoing
/// projection must still fit by degrading OLDER rounds, without touching the
/// ledger or the newest rounds.
#[test]
fn context_pressure_degrades_older_rounds_of_a_single_request_tool_loop() {
    let mut manager = ContextManager::new("local:pinned-tail", None);
    manager.record_message(&Message::user("Research HBM and build the deck"));
    for i in 0..8 {
        let mut call = Message::assistant("");
        call.reasoning_content = Some(format!("plan {i} ").repeat(300));
        call.tool_calls = Some(vec![octos_core::ToolCall {
            id: format!("call_{i}"),
            name: "write_file".into(),
            arguments: serde_json::json!({ "path": format!("s{i}.py"), "content": "x = 1\n".repeat(2000) }),
            metadata: None,
        }]);
        manager.record_message(&call);
        let body = format!("HEAD-{i} {} TAIL-{i}", "lorem ipsum ".repeat(1000));
        manager.record_message(&Message {
            role: MessageRole::Tool,
            tool_call_id: Some(format!("call_{i}")),
            ..Message::user(body)
        });
    }
    let raw_policy = PromptBuildPolicy {
        include_reasoning: true,
        ..PromptBuildPolicy::default()
    };
    let view = |messages: &[Message]| -> Vec<serde_json::Value> {
        messages
            .iter()
            .map(|m| {
                let calls = m.tool_calls.iter().flatten();
                let calls = calls.map(|c| (&c.id, &c.arguments)).collect::<Vec<_>>();
                serde_json::json!([m.content, m.reasoning_content, m.tool_call_id, calls])
            })
            .collect()
    };
    let raw = manager.for_prompt(&raw_policy).messages;
    let budget = estimate_messages_tokens(&raw) / 2;
    let policy = PromptBuildPolicy {
        context_pressure_budget: Some(budget),
        ..raw_policy.clone()
    };
    let frame = manager.for_prompt(&policy);
    assert!(estimate_messages_tokens(&frame.messages) <= budget);
    assert!(!frame.report.truncated_item_ids.is_empty());
    assert_eq!(frame.messages.len(), raw.len());
    let (cut_view, raw_view) = (view(&frame.messages), view(&raw));
    assert_eq!(cut_view[raw.len() - 6..], raw_view[raw.len() - 6..]);
    for (cut, full) in frame.messages.iter().zip(&raw) {
        assert_eq!(
            (cut.role, &cut.tool_call_id),
            (full.role, &full.tool_call_id)
        );
        if cut.role == MessageRole::Tool {
            assert!(cut.content.starts_with("HEAD-") && cut.content.contains("TAIL-"));
        }
        for call in cut.tool_calls.iter().flatten() {
            assert!(call.arguments["path"].as_str().unwrap().ends_with(".py"));
        }
    }
    // The ledger is untouched: an unbudgeted projection is still raw, and a
    // budget the prompt already fits changes nothing.
    assert_eq!(view(&manager.for_prompt(&raw_policy).messages), raw_view);
    let roomy = PromptBuildPolicy {
        context_pressure_budget: Some(budget * 4),
        ..raw_policy
    };
    assert_eq!(view(&manager.for_prompt(&roomy).messages), raw_view);
}
