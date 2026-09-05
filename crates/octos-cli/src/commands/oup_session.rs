//! Frontend-side OUP session lifecycle shared by chat and ACP.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eyre::{Result, WrapErr};
use octos_core::SessionKey;
use octos_core::ui_protocol::*;
use serde_json::json;
use tokio::sync::Mutex;

use super::oup_client::OupClient;

#[async_trait::async_trait]
pub(crate) trait OupFrontend: Send + Sync {
    /// Render a typed event and, for a blocking approval/question, return the
    /// corresponding OUP response. Never execute tools in the frontend.
    async fn event(&self, event: UiNotification) -> Result<Option<UiCommand>>;
}

#[derive(Debug)]
pub(crate) struct OupTurnResult {
    pub text: String,
    pub model: Option<String>,
    pub usage: EnvelopeTokenUsage,
    pub interrupted: bool,
}

/// A failed terminal remains an error, but must not discard the actual
/// current-turn answer or usage before an ephemeral frontend closes its store.
#[derive(Debug)]
pub(crate) struct OupTurnFailure {
    pub terminal_error: Option<TurnTerminalError>,
    pub partial: OupTurnResult,
}

impl std::fmt::Display for OupTurnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            self.terminal_error
                .as_ref()
                .map(|error| error.message.as_str())
                .unwrap_or("OUP turn failed"),
        )
    }
}

impl std::error::Error for OupTurnFailure {}

fn authoritative_partial_answer(
    error: Option<&TurnTerminalError>,
    canonical_answers: &HashMap<String, String>,
) -> String {
    error
        .and_then(|error| error.data.as_ref())
        .and_then(|data| data.get("partial_result"))
        .and_then(|value| serde_json::from_value::<TurnErrorPartialResult>(value.clone()).ok())
        .and_then(|partial| partial.session_result)
        .and_then(|result| canonical_answers.get(&result.message_id).cloned())
        .unwrap_or_default()
}

pub(crate) struct OupSession {
    pub client: OupClient,
    pub session_id: SessionKey,
    turn_gate: Mutex<()>,
    events: Mutex<tokio::sync::broadcast::Receiver<serde_json::Value>>,
    active_turn: std::sync::Mutex<Option<TurnId>>,
}

struct ActiveTurn<'a>(&'a std::sync::Mutex<Option<TurnId>>);
impl Drop for ActiveTurn<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = None;
    }
}

impl OupSession {
    pub(crate) async fn open(
        state: Arc<crate::api::AppState>,
        session_id: SessionKey,
        cwd: &Path,
        permissions: octos_agent::EffectivePermissions,
    ) -> Result<Self> {
        Self::open_with_questions(state, session_id, cwd, permissions, true).await
    }

    pub(crate) async fn open_with_questions(
        state: Arc<crate::api::AppState>,
        session_id: SessionKey,
        cwd: &Path,
        permissions: octos_agent::EffectivePermissions,
        questions: bool,
    ) -> Result<Self> {
        let client = OupClient::connect(state).await?;
        let mut supported_features = vec![
            UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2,
            UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
            UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1,
            UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1,
            UI_PROTOCOL_FEATURE_USER_QUESTION_V1,
            UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1,
            UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1,
            UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1,
        ];
        if !questions {
            supported_features.retain(|feature| *feature != UI_PROTOCOL_FEATURE_USER_QUESTION_V1);
        }
        client
            .request(
                "client_hello",
                json!({
                    "client": "octos-local-frontend", "supported_features": supported_features,
                }),
            )
            .await?;
        // Permission selection uses the same solo gate and narrowing checks
        // as every other OUP client. Set it before open can build a runtime.
        let mode = match permissions.permission_profile {
            octos_agent::PermissionProfile::ReadOnly => "read_only",
            octos_agent::PermissionProfile::WorkspaceWrite => "workspace_write",
            octos_agent::PermissionProfile::DangerFullAccess => "danger_full_access",
        };
        client.request(methods::PERMISSION_PROFILE_SET, json!({
            "session_id": session_id,
            "update": {
                "mode": mode,
                "network": match permissions.network {
                    octos_agent::NetworkPolicy::Allowed => Some("allow"),
                    octos_agent::NetworkPolicy::Inherit => None,
                },
                "approval_policy": if permissions.approval_policy == octos_agent::ApprovalPolicy::Never {
                    "never"
                } else { "ask" },
            },
        })).await?;
        let events = client.subscribe();
        let opened: SessionOpenResult = serde_json::from_value(
            client
                .request(
                    methods::SESSION_OPEN,
                    json!({
                        "session_id": session_id,
                        "profile_id": session_id.profile_id(),
                        "cwd": cwd.to_string_lossy(),
                    }),
                )
                .await?,
        )?;
        Ok(Self {
            client,
            session_id: opened.opened.session_id,
            turn_gate: Mutex::new(()),
            events: Mutex::new(events),
            active_turn: std::sync::Mutex::new(None),
        })
    }

    pub(crate) async fn hydrate(&self) -> Result<SessionHydrateResult> {
        serde_json::from_value(
            self.client
                .request(
                    methods::SESSION_HYDRATE,
                    json!({
                        "session_id": self.session_id,
                        "include": ["messages", "threads", "context"],
                    }),
                )
                .await?,
        )
        .wrap_err("decode OUP session hydration")
    }

    pub(crate) async fn turn(
        &self,
        input: &str,
        effort: Option<ReasoningEffortLevel>,
        cancelled: &AtomicBool,
        frontend: &dyn OupFrontend,
    ) -> Result<OupTurnResult> {
        let _turn = self
            .turn_gate
            .try_lock()
            .map_err(|_| eyre::eyre!("session already has an active turn"))?;
        let turn_id = TurnId(uuid::Uuid::now_v7());
        let result = async {
        let mut events = self.events.lock().await;
        self.client.request(methods::TURN_START, json!({
            "session_id": self.session_id,
            "turn_id": turn_id,
            "input": [InputItem::Text { text: input.to_owned() }],
            "reasoning_effort": effort,
        })).await?;
        let _background = self.client.allow_background_work();
        *self.active_turn.lock().unwrap() = Some(turn_id.clone());
        let _active = ActiveTurn(&self.active_turn);
        let mut interrupt_sent = false;
        self.maybe_interrupt(&turn_id, cancelled, &mut interrupt_sent).await?;
        let mut cancel_tick = tokio::time::interval(Duration::from_millis(50));
        cancel_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut seen = HashSet::new();
        let mut final_text = String::new();
        let mut canonical_answers = HashMap::new();
        let mut answer_invalidated_by_tool = false;
        let mut model = None;
        loop {
            let frame = tokio::select! {
                frame = events.recv() => frame.wrap_err("OUP event stream lost; reopen the session to recover")?,
                _ = cancel_tick.tick() => {
                    self.maybe_interrupt(&turn_id, cancelled, &mut interrupt_sent).await?;
                    continue;
                }
            };
            if frame["method"] == "local/connection_closed" {
                eyre::bail!("OUP connection ended before a terminal event");
            }
            let notification: RpcNotification<serde_json::Value> = serde_json::from_value(frame)?;
            let event = match UiNotification::from_rpc_notification(notification) {
                Ok(event) => event,
                // Raw diagnostics outside the typed OUP event registry are
                // not lifecycle signals. The adapter must not infer Done.
                Err(_) => continue,
            };
            if event.session_id() != &self.session_id {
                continue;
            }
            if let UiNotification::ProgressUpdated(progress) = &event {
                if let Some(answering_model) = progress.metadata.token_cost.as_ref()
                    .filter(|_| progress.turn_id.as_ref() == Some(&turn_id))
                    .and_then(|cost| cost.model.as_ref()) {
                    model = Some(answering_model.clone());
                }
            }
            if let UiNotification::EnvelopeV2(envelope) = &event
                && envelope.envelope.turn_id == turn_id.0.to_string() {
                if !seen.insert((envelope.envelope.thread_id.clone(), envelope.envelope.seq)) {
                    continue;
                }
                match &envelope.envelope.payload {
                    PayloadV2::AssistantPersisted { text, meta, .. } => {
                        canonical_answers.insert(meta.message_id.clone(), text.clone());
                        final_text.clone_from(text);
                        answer_invalidated_by_tool = false;
                    }
                    PayloadV2::ToolStart { .. } => {
                        final_text.clear();
                        answer_invalidated_by_tool = true;
                    }
                    PayloadV2::TurnTerminal { outcome, error, token_usage } => {
                        if !matches!(outcome, TurnTerminalOutcome::Completed | TurnTerminalOutcome::Interrupted) {
                            // Batched preambles may arrive after ToolStart. Neither
                            // latest text nor an absent legacy pointer proves a final.
                            final_text = authoritative_partial_answer(error.as_ref(), &canonical_answers);
                            return Err(OupTurnFailure {
                                terminal_error: error.clone(),
                                partial: OupTurnResult {
                                    text: final_text,
                                    model,
                                    usage: token_usage.clone().unwrap_or_default(),
                                    interrupted: false,
                                },
                            }.into());
                        }
                        let interrupted = *outcome == TurnTerminalOutcome::Interrupted;
                        if !interrupted && answer_invalidated_by_tool {
                            eyre::bail!("OUP turn completed after tool activity without a final assistant answer");
                        }
                        if !interrupted && final_text.trim().is_empty() {
                            // Hydrated legacy rows lack typed turn identity.
                            // Do not borrow a prior/background answer or return
                            // success for text the frontend has never rendered.
                            eyre::bail!("OUP turn completed without a final assistant answer; reopen the session to inspect canonical history");
                        }
                        return Ok(OupTurnResult {
                            text: final_text,
                            model,
                            usage: token_usage.clone().unwrap_or_default(),
                            interrupted,
                        });
                    }
                    _ => {}
                }
            }
            // A blocking UI prompt must remain cancellable. Polling here is
            // only cancellation delivery, never a model execution deadline.
            let reply = frontend.event(event);
            tokio::pin!(reply);
            loop {
                tokio::select! {
                    reply = &mut reply => {
                        if let Some(command) = reply? {
                            let request = command.into_rpc_request("frontend-reply")?;
                            self.client.request(&request.method, request.params).await?;
                        }
                        break;
                    }
                    _ = cancel_tick.tick() => {
                        if self.maybe_interrupt(&turn_id, cancelled, &mut interrupt_sent).await? {
                            break;
                        }
                    }
                }
            }
        }
        }.await;
        if result.is_err() {
            // A failed renderer or lagged receiver must not strand a live
            // turn. Address only our own turn, never a newer continuation.
            let _ = self
                .client
                .request(
                    methods::TURN_INTERRUPT,
                    json!({
                        "session_id": self.session_id, "turn_id": turn_id,
                    }),
                )
                .await;
        }
        result
    }

    async fn maybe_interrupt(
        &self,
        turn_id: &TurnId,
        cancelled: &AtomicBool,
        sent: &mut bool,
    ) -> Result<bool> {
        if !*sent && cancelled.load(Ordering::Acquire) {
            self.client
                .request(
                    methods::TURN_INTERRUPT,
                    json!({
                        "session_id": self.session_id, "turn_id": turn_id,
                    }),
                )
                .await?;
            *sent = true;
        }
        Ok(*sent)
    }

    /// Consume notifications between foreground turns, using the same cursor
    /// as turn(). Background completions and staged peers remain visible while
    /// a CLI waits for the next line. This future is cancellation-safe while
    /// waiting for an event; it never owns model execution.
    pub(crate) async fn listen(&self, frontend: &dyn OupFrontend) -> Result<()> {
        let mut events = self.events.lock().await;
        let _background = self.client.allow_background_work();
        loop {
            let frame = events.recv().await.wrap_err("OUP event stream lost")?;
            if frame["method"] == "local/connection_closed" {
                eyre::bail!("OUP connection closed");
            }
            let rpc = serde_json::from_value(frame)?;
            let Ok(event) = UiNotification::from_rpc_notification(rpc) else {
                continue;
            };
            if event.session_id() == &self.session_id
                && let Some(command) = frontend.event(event).await?
            {
                let request = command.into_rpc_request("frontend-reply")?;
                self.client.request(&request.method, request.params).await?;
            }
        }
    }

    pub(crate) async fn close(&self) -> Result<()> {
        self.client.close().await
    }

    pub(crate) async fn interrupt(&self) -> Result<()> {
        let turn_id = self.active_turn.lock().unwrap().clone();
        let Some(turn_id) = turn_id else {
            return Ok(());
        };
        self.client
            .request(
                methods::TURN_INTERRUPT,
                json!({
                    "session_id": self.session_id,
                    "turn_id": turn_id,
                }),
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingModel {
        inputs: std::sync::Mutex<Vec<Vec<octos_core::Message>>>,
    }

    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for RecordingModel {
        async fn chat(
            &self,
            messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            let mut inputs = self.inputs.lock().unwrap();
            inputs.push(messages.to_vec());
            Ok(octos_llm::ChatResponse {
                content: Some(format!("canonical-answer-{}", inputs.len())),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }
        fn provider_name(&self) -> &str {
            "local"
        }
        fn model_id(&self) -> &str {
            "oup-mock"
        }
    }

    struct Frontend;

    struct TerminalModel {
        stop: octos_llm::StopReason,
        reasoning_only: bool,
        recover: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for TerminalModel {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            let recovered = self.recover && attempt > 0;
            Ok(octos_llm::ChatResponse {
                content: if self.reasoning_only && !recovered {
                    None
                } else {
                    Some(
                        if recovered {
                            "Recovered final answer"
                        } else {
                            "First I need to inspect the image and then I will"
                        }
                        .into(),
                    )
                },
                reasoning_content: self
                    .reasoning_only
                    .then(|| "Need to inspect the image.".into()),
                tool_calls: vec![],
                stop_reason: if recovered {
                    octos_llm::StopReason::EndTurn
                } else {
                    self.stop
                },
                usage: octos_llm::TokenUsage {
                    input_tokens: 12,
                    output_tokens: 7,
                    ..Default::default()
                },
                provider_index: None,
            })
        }
        async fn chat_stream(
            &self,
            messages: &[octos_core::Message],
            tools: &[octos_llm::ToolSpec],
            config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatStream> {
            use octos_llm::StreamEvent;
            let response = self.chat(messages, tools, config).await?;
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::ReasoningDelta(response.reasoning_content.unwrap_or_default()),
                StreamEvent::TextDelta(response.content.unwrap_or_default()),
                StreamEvent::Usage(response.usage),
                StreamEvent::Done(response.stop_reason),
            ])))
        }
        fn provider_name(&self) -> &str {
            "local"
        }
        fn model_id(&self) -> &str {
            "terminal-integrity"
        }
    }

    async fn terminal_integrity_case(
        reasoning_only: bool,
        recover: bool,
        stop: octos_llm::StopReason,
    ) {
        use crate::autonomy::agent_orchestrator::{
            AgentOrchestrator, GoalSetRequest, default_agent_orchestrator,
        };
        use crate::commands::acp::{SessionAgentFactory, TestAgentFactory};
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let model = Arc::new(TerminalModel {
            stop,
            reasoning_only,
            recover,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let factory = TestAgentFactory::new(
            model.clone(),
            data.path().to_owned(),
            workspace.path().to_owned(),
        );
        let state = factory.oup_state().await.unwrap();
        let key = SessionKey::with_profile(
            octos_core::MAIN_PROFILE_ID,
            "acp",
            &uuid::Uuid::now_v7().to_string(),
        );
        let session = OupSession::open(
            state.clone(),
            key.clone(),
            workspace.path(),
            octos_agent::EffectivePermissions::workspace_write(),
        )
        .await
        .unwrap();
        let mut events = session.client.subscribe();
        if !reasoning_only {
            default_agent_orchestrator()
                .set_goal(GoalSetRequest {
                    session_id: session.session_id.clone(),
                    profile_id: octos_core::MAIN_PROFILE_ID.into(),
                    objective: "Complete the requested inspection".into(),
                    status: Some("active".into()),
                    token_budget: Some(50_000),
                    transition_actor: None,
                })
                .unwrap();
        }
        let result = tokio::time::timeout(
            Duration::from_secs(35),
            session.turn(
                "Inspect the image",
                None,
                &AtomicBool::new(false),
                &Frontend,
            ),
        )
        .await
        .unwrap();
        if recover {
            assert_eq!(result.unwrap().text, "Recovered final answer");
            assert_eq!(model.calls.load(Ordering::SeqCst), 2);
        } else {
            assert!(result.is_err(), "incomplete responses must not complete");
            let failure = result
                .as_ref()
                .unwrap_err()
                .downcast_ref::<OupTurnFailure>()
                .expect("failed OUP terminal must preserve its typed current-turn result");
            if reasoning_only {
                assert!(
                    failure.partial.text.is_empty(),
                    "reasoning is not an assistant answer"
                );
                let calls = model.calls.load(Ordering::SeqCst) as u64;
                assert_eq!(failure.partial.usage.input_tokens, 12 * calls);
                assert_eq!(failure.partial.usage.output_tokens, 7 * calls);
            } else {
                assert_eq!(
                    failure.partial.text,
                    "First I need to inspect the image and then I will"
                );
                assert_eq!(
                    failure.partial.usage.input_tokens + failure.partial.usage.output_tokens,
                    19
                );
                assert_eq!(
                    failure.terminal_error.as_ref().unwrap().code,
                    "output_truncated"
                );
            }
        }
        let mut terminals = Vec::new();
        while let Ok(frame) = events.try_recv() {
            if let Ok(rpc) = serde_json::from_value::<RpcNotification<serde_json::Value>>(frame)
                && let Ok(UiNotification::EnvelopeV2(envelope)) =
                    UiNotification::from_rpc_notification(rpc)
                && let PayloadV2::TurnTerminal { outcome, .. } = envelope.envelope.payload
            {
                terminals.push(outcome);
            }
        }
        assert_eq!(
            terminals,
            vec![if recover {
                TurnTerminalOutcome::Completed
            } else {
                TurnTerminalOutcome::Errored
            }]
        );
        let history = session
            .hydrate()
            .await
            .unwrap()
            .messages
            .unwrap_or_default();
        if !reasoning_only {
            assert!(
                history
                    .iter()
                    .any(|m| m.content == "First I need to inspect the image and then I will"),
                "the actual partial output must survive reopen"
            );
            // The terminal can arrive before the post-turn accountant; wait
            // for that bounded local cleanup rather than racing its snapshot.
            let charged = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let goal_key =
                        default_agent_orchestrator().scoped_goal_key(&session.session_id);
                    assert_eq!(
                        default_agent_orchestrator()
                            .goal_status_for_test(&goal_key)
                            .as_deref(),
                        Some("active")
                    );
                    if default_agent_orchestrator()
                        .goal_counters_for_test(&goal_key)
                        .unwrap()
                        .0
                        >= 19
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            assert!(
                charged.is_ok(),
                "truncated interactive work still consumes goal tokens"
            );
            let goal_key = default_agent_orchestrator().scoped_goal_key(&session.session_id);
            assert_eq!(
                default_agent_orchestrator()
                    .goal_counters_for_test(&goal_key)
                    .unwrap()
                    .0,
                19,
                "truncated work is charged exactly once"
            );
            assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        } else if !recover {
            assert!(
                !history
                    .iter()
                    .any(|m| m.content.contains("Session Summary"))
            );
            assert!(
                model.calls.load(Ordering::SeqCst) <= 10,
                "empty recovery must remain bounded"
            );
        }
        session.close().await.unwrap();
        if !reasoning_only {
            let reopened = OupSession::open(
                state,
                key,
                workspace.path(),
                octos_agent::EffectivePermissions::workspace_write(),
            )
            .await
            .unwrap();
            let history = reopened
                .hydrate()
                .await
                .unwrap()
                .messages
                .unwrap_or_default();
            assert!(
                history
                    .iter()
                    .any(|m| m.content == "First I need to inspect the image and then I will"),
                "the actual partial output must survive connection close and reopen"
            );
            reopened.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn terminal_integrity_oup_preserves_truncation_without_success() {
        terminal_integrity_case(false, false, octos_llm::StopReason::MaxTokens).await;
    }

    #[tokio::test]
    async fn should_report_only_current_turn_usage_for_repeated_oup_failures() {
        use crate::commands::acp::{SessionAgentFactory, TestAgentFactory};
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let model = Arc::new(TerminalModel {
            stop: octos_llm::StopReason::MaxTokens,
            reasoning_only: false,
            recover: false,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let factory = TestAgentFactory::new(
            model.clone(),
            data.path().to_owned(),
            workspace.path().to_owned(),
        );
        let state = factory.oup_state().await.unwrap();
        let session = OupSession::open(
            state,
            SessionKey::with_profile(
                octos_core::MAIN_PROFILE_ID,
                "acp",
                &uuid::Uuid::now_v7().to_string(),
            ),
            workspace.path(),
            octos_agent::EffectivePermissions::workspace_write(),
        )
        .await
        .unwrap();
        for _ in 0..2 {
            let error = session
                .turn(
                    "Continue the explanation",
                    None,
                    &AtomicBool::new(false),
                    &Frontend,
                )
                .await
                .unwrap_err();
            let failure = error.downcast_ref::<OupTurnFailure>().unwrap();
            assert_eq!(
                failure.partial.usage,
                EnvelopeTokenUsage {
                    input_tokens: 12,
                    output_tokens: 7,
                    ..Default::default()
                },
                "never substitute cumulative session cost for this failed turn"
            );
            assert_eq!(
                failure.partial.text,
                "First I need to inspect the image and then I will"
            );
        }
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn terminal_integrity_oup_recovers_reasoning_only() {
        terminal_integrity_case(true, true, octos_llm::StopReason::EndTurn).await;
    }

    #[tokio::test]
    async fn terminal_integrity_oup_exhausted_reasoning_only_errors() {
        terminal_integrity_case(true, false, octos_llm::StopReason::EndTurn).await;
    }

    struct ToolThenEmptyModel(std::sync::atomic::AtomicUsize, bool, Option<&'static str>);

    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for ToolThenEmptyModel {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            let call = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(octos_llm::ChatResponse {
                content: match call {
                    0 => Some("OLD-FINAL-DO-NOT-REUSE".into()),
                    1 => Some("PRETOOL-DO-NOT-REUSE".into()),
                    _ => self.2.map(str::to_owned),
                },
                reasoning_content: (call >= 2).then(|| "reasoning is not a final answer".into()),
                tool_calls: if call == 1 || (call >= 2 && self.1) {
                    vec![octos_core::ToolCall {
                        id: format!("incomplete-list-{call}"),
                        name: "list_dir".into(),
                        arguments: json!({"path":"."}),
                        metadata: None,
                    }]
                } else {
                    vec![]
                },
                stop_reason: match call {
                    0 => octos_llm::StopReason::EndTurn,
                    1 => octos_llm::StopReason::ToolUse,
                    _ => octos_llm::StopReason::MaxTokens,
                },
                usage: octos_llm::TokenUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    ..Default::default()
                },
                provider_index: None,
            })
        }
        fn provider_name(&self) -> &str {
            "local"
        }
        fn model_id(&self) -> &str {
            "tool-then-empty"
        }
    }

    #[tokio::test]
    async fn should_not_attach_prior_or_pretool_answer_to_failed_oup_turn() {
        pretool_answer_failure_case(false, None).await;
    }

    #[tokio::test]
    async fn should_not_attach_pretool_answer_when_truncated_tool_call_has_no_final() {
        pretool_answer_failure_case(true, None).await;
    }

    #[tokio::test]
    async fn should_preserve_exact_nonstream_partial_after_pretool_activity() {
        pretool_answer_failure_case(true, Some("ACTUAL-NONSTREAM-PARTIAL")).await;
    }

    async fn pretool_answer_failure_case(
        truncated_tool_call: bool,
        final_content: Option<&'static str>,
    ) {
        use crate::commands::acp::{SessionAgentFactory, TestAgentFactory};
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let factory = TestAgentFactory::new(
            Arc::new(ToolThenEmptyModel(
                std::sync::atomic::AtomicUsize::new(0),
                truncated_tool_call,
                final_content,
            )),
            data.path().to_owned(),
            workspace.path().to_owned(),
        );
        let session = OupSession::open(
            factory.oup_state().await.unwrap(),
            SessionKey::with_profile(
                octos_core::MAIN_PROFILE_ID,
                "acp",
                &uuid::Uuid::now_v7().to_string(),
            ),
            workspace.path(),
            octos_agent::EffectivePermissions::workspace_write(),
        )
        .await
        .unwrap();
        assert_eq!(
            session
                .turn("first answer", None, &AtomicBool::new(false), &Frontend)
                .await
                .unwrap()
                .text,
            "OLD-FINAL-DO-NOT-REUSE"
        );
        let error = tokio::time::timeout(
            Duration::from_secs(35),
            session.turn(
                "inspect then answer",
                None,
                &AtomicBool::new(false),
                &Frontend,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        let failure = error
            .downcast_ref::<OupTurnFailure>()
            .expect("typed terminal failure");
        assert_eq!(failure.partial.text, final_content.unwrap_or_default());
        let history = session.hydrate().await.unwrap().messages.unwrap();
        assert!(
            history
                .iter()
                .any(|row| row.content == "OLD-FINAL-DO-NOT-REUSE")
        );
        session.close().await.unwrap();
    }

    #[test]
    fn should_require_current_turn_canonical_identity_for_error_partial() {
        let current = HashMap::from([("current-final".into(), "ACTUAL-FINAL".into())]);
        for (data, expected) in [
            (None, ""),
            (
                Some(json!({"partial_result": {"session_result": null}})),
                "",
            ),
            (Some(json!({"partial_result": "malformed"})), ""),
            (
                Some(json!({"partial_result": {"session_result": {
                    "message_id": "previous-turn-final", "committed_seq": 1
                }}})),
                "",
            ),
            (
                Some(json!({"partial_result": {"session_result": {
                    "message_id": "current-final", "committed_seq": 5
                }}})),
                "ACTUAL-FINAL",
            ),
        ] {
            let error = TurnTerminalError {
                code: "output_truncated".into(),
                message: "failed".into(),
                data,
            };
            assert_eq!(
                authoritative_partial_answer(Some(&error), &current),
                expected
            );
        }
        assert!(authoritative_partial_answer(None, &current).is_empty());
    }

    struct PendingModel {
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
        dropped: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct ProviderDrop(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ProviderDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for PendingModel {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            let _drop = ProviderDrop(self.dropped.clone());
            self.started.notify_one();
            self.release.notified().await;
            Ok(octos_llm::ChatResponse {
                content: Some("Other connection completed".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: Default::default(),
                provider_index: None,
            })
        }
        fn provider_name(&self) -> &str {
            "local"
        }
        fn model_id(&self) -> &str {
            "pending-terminal-test"
        }
    }

    #[tokio::test]
    async fn terminal_integrity_close_cancels_only_owned_turns() {
        use crate::commands::acp::{SessionAgentFactory, TestAgentFactory};
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let model = Arc::new(PendingModel {
            started: Default::default(),
            release: Default::default(),
            dropped: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let factory = TestAgentFactory::new(
            model.clone(),
            data.path().to_owned(),
            workspace.path().to_owned(),
        );
        let state = factory.oup_state().await.unwrap();
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let key = SessionKey::with_profile(
                octos_core::MAIN_PROFILE_ID,
                "acp",
                &uuid::Uuid::now_v7().to_string(),
            );
            let session = OupSession::open(
                state.clone(),
                key,
                workspace.path(),
                octos_agent::EffectivePermissions::workspace_write(),
            )
            .await
            .unwrap();
            session.client.request(methods::TURN_START, json!({ "session_id": session.session_id, "turn_id": TurnId::new(), "input": [InputItem::Text { text: "wait for release".into() }] })).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), model.started.notified())
                .await
                .unwrap();
            sessions.push(session);
        }
        tokio::time::timeout(Duration::from_secs(3), sessions[0].close())
            .await
            .expect(
                "explicit embedded close must cancel owned work, not race the ten-second EOF drain",
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while model.dropped.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            model.dropped.load(Ordering::SeqCst),
            1,
            "closing A must not cancel B"
        );
        model.release.notify_waiters();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let history = sessions[1]
                    .hydrate()
                    .await
                    .unwrap()
                    .messages
                    .unwrap_or_default();
                if history
                    .iter()
                    .any(|m| m.content == "Other connection completed")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        sessions[1].close().await.unwrap();
        assert_eq!(model.dropped.load(Ordering::SeqCst), 2);
    }

    #[async_trait::async_trait]
    impl OupFrontend for Frontend {
        async fn event(&self, _event: UiNotification) -> Result<Option<UiCommand>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn local_frontends_share_oup_persistence_and_reopen_context() {
        use crate::runtime::local_oup::{LocalOupOptions, bootstrap, local_profile};
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let model = Arc::new(RecordingModel {
            inputs: std::sync::Mutex::new(Vec::new()),
        });
        let config = crate::config::Config {
            provider: Some("local".into()),
            model: Some("oup-mock".into()),
            memory: Some(crate::config::MemoryConfig {
                refresh: Some(crate::config::MemoryRefreshConfig {
                    enabled: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let state = bootstrap(LocalOupOptions {
            profile: local_profile("migration", &config),
            config,
            data_dir: data.path().to_owned(),
            config_home: data.path().to_owned(),
            no_retry: true,
            provider: Some(model.clone()),
            tool_profile: None,
            save_episodes: false,
        })
        .await
        .unwrap();
        let key = SessionKey::with_profile("migration", "acp", "reopen");
        let permissions = octos_agent::EffectivePermissions::workspace_write();
        let cancelled = AtomicBool::new(false);
        let session = OupSession::open(state.clone(), key.clone(), workspace.path(), permissions)
            .await
            .unwrap();
        let first = tokio::time::timeout(
            Duration::from_secs(30),
            session.turn("first migration prompt", None, &cancelled, &Frontend),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first.text, "canonical-answer-1");
        session.close().await.unwrap();
        let reopened = OupSession::open(state, key, workspace.path(), permissions)
            .await
            .unwrap();
        let history = reopened.hydrate().await.unwrap().messages.unwrap();
        assert!(
            history
                .iter()
                .any(|message| message.content == "canonical-answer-1")
        );
        let second = tokio::time::timeout(
            Duration::from_secs(30),
            reopened.turn("second migration prompt", None, &cancelled, &Frontend),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(second.text, "canonical-answer-2");
        assert!(
            model.inputs.lock().unwrap()[1]
                .iter()
                .any(|message| message.content.contains("canonical-answer-1"))
        );
        reopened.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_runtime_loads_workspace_plugins_without_cross_cwd_leakage() {
        use crate::commands::acp::{SessionAgentFactory, TestAgentFactory};
        use std::os::unix::fs::PermissionsExt;
        let data = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let plugin = first.path().join(".octos/plugins/demo");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("manifest.json"), r#"{
            "name":"demo", "version":"1.0",
            "tools":[{"name":"workspace_probe","description":"test","input_schema":{"type":"object","properties":{}}}]
        }"#).unwrap();
        let executable = plugin.join("demo");
        std::fs::write(&executable, "#!/bin/sh\necho ok\n").unwrap();
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let model = Arc::new(RecordingModel {
            inputs: std::sync::Mutex::new(Vec::new()),
        });
        let factory = TestAgentFactory::new(model, data.path().to_owned(), first.path().to_owned());
        let state = factory.oup_state().await.unwrap();
        let profile = &state.profiles[octos_core::MAIN_PROFILE_ID];
        let a = state
            .session_cache
            .get_or_init(
                profile,
                SessionKey::with_profile(octos_core::MAIN_PROFILE_ID, "acp", "workspace-a"),
                Some(first.path().to_owned()),
            )
            .await
            .unwrap();
        let b = state
            .session_cache
            .get_or_init(
                profile,
                SessionKey::with_profile(octos_core::MAIN_PROFILE_ID, "acp", "workspace-b"),
                Some(second.path().to_owned()),
            )
            .await
            .unwrap();
        let tool = a
            .tools
            .get("workspace_probe")
            .expect("session must load its own project plugins");
        assert!(
            !Arc::ptr_eq(
                a.profile.pipeline_factory.as_ref().unwrap(),
                profile.pipeline_factory.as_ref().unwrap(),
            ),
            "project plugin discovery must also rebind the child pipeline factory"
        );
        assert!(
            Arc::ptr_eq(
                b.profile.pipeline_factory.as_ref().unwrap(),
                profile.pipeline_factory.as_ref().unwrap(),
            ),
            "a workspace without project plugins keeps the shared factory"
        );
        let plugin = tool
            .as_any()
            .downcast_ref::<octos_agent::plugins::PluginTool>()
            .unwrap();
        assert_eq!(plugin.work_dir(), Some(a.workspace_root.as_path()));
        assert!(
            b.tools.get("workspace_probe").is_none(),
            "project plugins must not leak between ACP cwds"
        );
    }
}
