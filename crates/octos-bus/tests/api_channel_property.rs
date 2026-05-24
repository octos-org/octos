#![cfg(feature = "api")]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use eyre::{Result, ensure, eyre};
use octos_bus::{ApiChannel, Channel, SessionManager};
use octos_core::OutboundMessage;
use proptest::prelude::*;
use serde_json::Value;
use tokio::sync::Mutex;

const CHAT_ID: &str = "property-thread-binding";

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum CompletionKind {
    Assistant,
    Tool,
}

impl CompletionKind {
    fn role(self) -> &'static str {
        match self {
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    fn sort_rank(self) -> u8 {
        match self {
            Self::Assistant => 0,
            Self::Tool => 1,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
struct TurnCase {
    cmid: String,
    send_at_ms: u64,
    prompt: String,
}

#[derive(Clone, Debug, serde::Serialize)]
struct CompletionCase {
    turn_index: usize,
    kind: CompletionKind,
    complete_at_ms: u64,
    content: String,
}

#[derive(Clone, Debug, serde::Serialize)]
struct ThreadBindingScenario {
    schema: &'static str,
    turns: Vec<TurnCase>,
    completions: Vec<CompletionCase>,
}

#[derive(Clone, Debug, serde::Serialize)]
struct TranscriptRecord {
    role: &'static str,
    content: String,
    at_ms: u64,
    thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_to_client_message_id: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct ThreadBindingViolation {
    record_index: usize,
    role: String,
    expected_thread_id: String,
    actual_thread_id: String,
    response_to_client_message_id: Option<String>,
    content_preview: String,
}

impl ThreadBindingScenario {
    fn from_specs(specs: Vec<(u16, u16, Option<u16>)>) -> Self {
        let mut send_at_ms = 0_u64;
        let mut turns = Vec::with_capacity(specs.len());

        for (index, (gap_ms, _, _)) in specs.iter().enumerate() {
            if index > 0 {
                send_at_ms += u64::from(*gap_ms);
            }
            turns.push(TurnCase {
                cmid: format!("cmid-prop-{}", index + 1),
                send_at_ms,
                prompt: format!("property prompt {}", index + 1),
            });
        }

        let last_send_at_ms = turns.last().map(|turn| turn.send_at_ms).unwrap_or(0);
        let mut completions = Vec::with_capacity(specs.len() * 2);
        for (index, (_, assistant_delay_ms, maybe_tool_delay_ms)) in specs.iter().enumerate() {
            let turn = &turns[index];
            let mut complete_at_ms = turn.send_at_ms + u64::from(*assistant_delay_ms);
            if index == 0 {
                complete_at_ms = complete_at_ms.max(last_send_at_ms + 1);
            }
            completions.push(CompletionCase {
                turn_index: index,
                kind: CompletionKind::Assistant,
                complete_at_ms,
                content: format!("assistant result for {}", turn.cmid),
            });

            if let Some(tool_delay_ms) = maybe_tool_delay_ms {
                completions.push(CompletionCase {
                    turn_index: index,
                    kind: CompletionKind::Tool,
                    complete_at_ms: turn.send_at_ms + u64::from(*tool_delay_ms),
                    content: format!("tool result for {}", turn.cmid),
                });
            }
        }

        completions.sort_by_key(|completion| {
            (
                completion.complete_at_ms,
                completion.turn_index,
                completion.kind.sort_rank(),
            )
        });

        Self {
            schema: "octos.thread-binding.property.v1",
            turns,
            completions,
        }
    }

    fn origin_cmid(&self, completion: &CompletionCase) -> &str {
        &self.turns[completion.turn_index].cmid
    }

    fn latest_user_at(&self, at_ms: u64) -> &TurnCase {
        self.turns
            .iter()
            .rfind(|turn| turn.send_at_ms <= at_ms)
            .unwrap_or(&self.turns[0])
    }

    fn has_sticky_pressure(&self) -> bool {
        self.completions.iter().any(|completion| {
            self.latest_user_at(completion.complete_at_ms).cmid != self.origin_cmid(completion)
        })
    }

    fn transcript(&self, mode: BindingMode) -> Vec<TranscriptRecord> {
        let mut records = Vec::with_capacity(self.turns.len() + self.completions.len());

        for turn in &self.turns {
            records.push(TranscriptRecord {
                role: "user",
                content: turn.prompt.clone(),
                at_ms: turn.send_at_ms,
                thread_id: turn.cmid.clone(),
                client_message_id: Some(turn.cmid.clone()),
                response_to_client_message_id: None,
            });
        }

        for completion in &self.completions {
            let origin = self.origin_cmid(completion);
            let sticky = self.latest_user_at(completion.complete_at_ms).cmid.as_str();
            let thread_id = match mode {
                BindingMode::BoundAtSpawn => origin,
                BindingMode::StickyLatestUser => sticky,
            };
            records.push(TranscriptRecord {
                role: completion.kind.role(),
                content: completion.content.clone(),
                at_ms: completion.complete_at_ms,
                thread_id: thread_id.to_string(),
                client_message_id: None,
                response_to_client_message_id: Some(origin.to_string()),
            });
        }

        records.sort_by_key(|record| {
            let role_rank = if record.role == "user" { 0 } else { 1 };
            (record.at_ms, role_rank, record.content.clone())
        });
        records
    }

    fn to_promotable_fixture_json(&self) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": self.schema,
            "turns": self.turns,
            "completions": self.completions,
            "records": self.transcript(BindingMode::BoundAtSpawn),
            "sticky_latest_user_records": self.transcript(BindingMode::StickyLatestUser),
        }))
        .expect("scenario should serialize")
    }
}

#[derive(Clone, Copy)]
enum BindingMode {
    BoundAtSpawn,
    StickyLatestUser,
}

fn scenario_strategy() -> impl Strategy<Value = ThreadBindingScenario> {
    proptest::collection::vec(
        (
            0_u16..=2_500,
            0_u16..=35_000,
            prop::option::of(0_u16..=35_000),
        ),
        3..=8,
    )
    .prop_map(ThreadBindingScenario::from_specs)
}

fn check_transcript(records: &[TranscriptRecord]) -> Vec<ThreadBindingViolation> {
    let known_user_cmids: HashSet<String> = records
        .iter()
        .filter(|record| record.role == "user")
        .filter_map(|record| record.client_message_id.clone())
        .collect();
    let mut current_user_cmid = None;
    let mut violations = Vec::new();

    for (index, record) in records.iter().enumerate() {
        match record.role {
            "user" => {
                current_user_cmid = record.client_message_id.clone();
                if let Some(cmid) = &record.client_message_id {
                    if record.thread_id != *cmid {
                        violations.push(ThreadBindingViolation {
                            record_index: index,
                            role: record.role.to_string(),
                            expected_thread_id: cmid.clone(),
                            actual_thread_id: record.thread_id.clone(),
                            response_to_client_message_id: None,
                            content_preview: record.content.chars().take(80).collect(),
                        });
                    }
                }
            }
            "assistant" | "tool" => {
                let expected = record
                    .response_to_client_message_id
                    .clone()
                    .filter(|cmid| known_user_cmids.contains(cmid))
                    .or_else(|| current_user_cmid.clone());
                if let Some(expected) = expected {
                    if record.thread_id != expected {
                        violations.push(ThreadBindingViolation {
                            record_index: index,
                            role: record.role.to_string(),
                            expected_thread_id: expected,
                            actual_thread_id: record.thread_id.clone(),
                            response_to_client_message_id: record
                                .response_to_client_message_id
                                .clone(),
                            content_preview: record.content.chars().take(80).collect(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    violations
}

fn make_channel() -> Result<ApiChannel> {
    let temp = tempfile::tempdir()?;
    let sessions = Arc::new(Mutex::new(SessionManager::open(temp.path())?));
    Ok(ApiChannel::new(
        0,
        None,
        Arc::new(AtomicBool::new(false)),
        sessions,
        None,
    ))
}

fn completion_result_json(scenario: &ThreadBindingScenario, completion: &CompletionCase) -> Value {
    let cmid = scenario.origin_cmid(completion);
    serde_json::json!({
        "role": completion.kind.role(),
        "content": completion.content.clone(),
        "thread_id": cmid,
        "response_to_client_message_id": cmid,
        "timestamp": Utc.timestamp_millis_opt(completion.complete_at_ms as i64)
            .single()
            .unwrap_or_else(Utc::now)
            .to_rfc3339(),
    })
}

async fn next_watcher_event(rx: &mut tokio::sync::broadcast::Receiver<String>) -> Result<Value> {
    let payload = tokio::time::timeout(Duration::from_millis(250), rx.recv())
        .await
        .map_err(|_| eyre!("timed out waiting for watcher event"))?
        .map_err(|error| eyre!("watcher closed before event: {error}"))?;
    Ok(serde_json::from_str(&payload)?)
}

fn assert_event_bound_to_origin(
    value: &Value,
    expected_thread_id: &str,
    expected_role: &str,
    seq_by_thread: &mut HashMap<String, u64>,
) -> Result<()> {
    ensure!(
        value.get("event_type").and_then(Value::as_str) == Some("session_result"),
        "unexpected event_type in {value}"
    );
    ensure!(
        value.get("thread_id").and_then(Value::as_str) == Some(expected_thread_id),
        "envelope thread_id drifted in {value}"
    );
    ensure!(
        value.pointer("/payload/thread_id").and_then(Value::as_str) == Some(expected_thread_id),
        "payload thread_id drifted in {value}"
    );

    let next_seq = seq_by_thread
        .entry(expected_thread_id.to_string())
        .and_modify(|seq| *seq += 1)
        .or_insert(1);
    ensure!(
        value.get("event_seq").and_then(Value::as_u64) == Some(*next_seq),
        "event_seq should be per-thread and monotonic in {value}"
    );

    let message = value
        .get("message")
        .or_else(|| value.pointer("/payload/message"))
        .ok_or_else(|| eyre!("session_result event missing message: {value}"))?;
    ensure!(
        message.get("role").and_then(Value::as_str) == Some(expected_role),
        "message role drifted in {value}"
    );
    ensure!(
        message.get("thread_id").and_then(Value::as_str) == Some(expected_thread_id),
        "nested message thread_id drifted in {value}"
    );
    ensure!(
        message
            .get("response_to_client_message_id")
            .and_then(Value::as_str)
            == Some(expected_thread_id),
        "nested message response_to_client_message_id drifted in {value}"
    );

    Ok(())
}

async fn run_api_channel_scenario(scenario: &ThreadBindingScenario) -> Result<()> {
    let channel = make_channel()?;
    let mut watcher = channel.subscribe_watcher_for_tests(CHAT_ID, None).await;
    let mut seq_by_thread = HashMap::new();

    for completion in &scenario.completions {
        let cmid = scenario.origin_cmid(completion);
        let result = completion_result_json(scenario, completion);
        let msg = OutboundMessage {
            channel: "api".to_string(),
            chat_id: CHAT_ID.to_string(),
            content: completion.content.clone(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({
                "thread_id": cmid,
                "_session_result": result,
            }),
        };

        channel.send(&msg).await?;
        let event = next_watcher_event(&mut watcher).await?;
        assert_event_bound_to_origin(&event, cmid, completion.kind.role(), &mut seq_by_thread)?;
    }

    let first = scenario
        .completions
        .first()
        .ok_or_else(|| eyre!("scenario should contain at least one completion"))?;
    let unbound = OutboundMessage {
        channel: "api".to_string(),
        chat_id: CHAT_ID.to_string(),
        content: first.content.clone(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({
            "_session_result": completion_result_json(scenario, first),
        }),
    };
    let err = channel
        .send(&unbound)
        .await
        .expect_err("session_result emission without thread_id must fail closed");
    ensure!(
        err.to_string().contains("required thread_id"),
        "unexpected missing-thread error: {err}"
    );

    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    #[test]
    fn api_channel_preserves_thread_binding_under_generated_completion_orders(
        scenario in scenario_strategy()
    ) {
        prop_assert!(
            scenario.has_sticky_pressure(),
            "generator did not exercise sticky-map pressure:\n{}",
            scenario.to_promotable_fixture_json()
        );

        let baseline_violations = check_transcript(&scenario.transcript(BindingMode::BoundAtSpawn));
        prop_assert!(
            baseline_violations.is_empty(),
            "bound-at-spawn baseline violated invariant: {:?}\n{}",
            baseline_violations,
            scenario.to_promotable_fixture_json()
        );

        let sticky_violations = check_transcript(&scenario.transcript(BindingMode::StickyLatestUser));
        prop_assert!(
            !sticky_violations.is_empty(),
            "sticky latest-user model unexpectedly satisfied invariant:\n{}",
            scenario.to_promotable_fixture_json()
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");
        if let Err(error) = runtime.block_on(run_api_channel_scenario(&scenario)) {
            prop_assert!(
                false,
                "api_channel event invariant failed: {error:?}\n{}",
                scenario.to_promotable_fixture_json()
            );
        }
    }
}
