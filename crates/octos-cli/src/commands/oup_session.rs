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
