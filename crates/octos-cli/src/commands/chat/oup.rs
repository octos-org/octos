//! Terminal presentation for OUP. Model history and tools stay server-owned.

use super::*;
use crate::commands::oup_text::AssistantTextProjection;

use octos_core::ui_protocol::{
    ApprovalDecision, ApprovalRespondParams, PayloadV2, TurnInterruptParams, UiCommand,
    UiNotification, UserQuestionRespondParams,
};

use crate::commands::oup_session::{OupFrontend, OupSession};
use crate::runtime::local_oup::{
    LocalOupOptions, bootstrap, bootstrap_ephemeral, local_profile, resolve_stored_profile,
};

struct TerminalFrontend {
    json: bool,
    verbose: bool,
    approvals: CliApprovalRequester,
    segments: std::sync::Mutex<AssistantTextProjection>,
    input_active: AtomicBool,
    pending_prompts: std::sync::Mutex<Vec<UiNotification>>,
    peers: Option<crate::commands::oup_peers::OupPeerHost>,
}

fn finish_turn<T>(result: Result<T>, shutdown: Result<()>) -> Result<T> {
    if let Err(error) = shutdown {
        eprintln!("OUP shutdown warning: {error}");
    }
    result
}

#[async_trait::async_trait]
impl OupFrontend for TerminalFrontend {
    async fn event(&self, event: UiNotification) -> Result<Option<UiCommand>> {
        if let Some(peers) = &self.peers {
            peers.event(&event);
        }
        if self.input_active.load(Ordering::Acquire)
            && matches!(
                &event,
                UiNotification::ApprovalRequested(_) | UiNotification::UserQuestionRequested(_)
            )
        {
            // Rustyline owns stdin until the current line is submitted. A
            // background question must not launch a competing stdin reader.
            self.pending_prompts.lock().unwrap().push(event);
            eprintln!("\nA background turn needs your response. Press Enter to open its prompt.");
            return Ok(None);
        }
        match event {
            UiNotification::EnvelopeV2(event) if !self.json => match event.envelope.payload {
                PayloadV2::AssistantDelta {
                    text,
                    assistant_segment_id,
                } => {
                    let text = self
                        .segments
                        .lock()
                        .unwrap()
                        .delta(&assistant_segment_id, &text);
                    print!("{text}");
                    io::stdout().flush()?;
                }
                PayloadV2::AssistantPersisted {
                    text,
                    assistant_segment_id,
                    ..
                } => {
                    let text = self
                        .segments
                        .lock()
                        .unwrap()
                        .persisted(&assistant_segment_id, &text);
                    print!("{text}");
                    io::stdout().flush()?;
                }
                PayloadV2::ReasoningDelta { text } if self.verbose => eprint!("{text}"),
                PayloadV2::ToolStart { name, .. } if self.verbose => {
                    eprintln!("\nTool: {name}")
                }
                PayloadV2::ToolEnd {
                    error: Some(error), ..
                } => eprintln!("\nTool error: {error}"),
                _ => {}
            },
            UiNotification::ApprovalRequested(event) => {
                let decision = self
                    .approvals
                    .request_approval(ToolApprovalRequest {
                        tool_id: event.approval_id.0.to_string(),
                        tool_name: event.tool_name,
                        title: event.title,
                        body: event.body,
                        command: None,
                        cwd: None,
                    })
                    .await;
                return Ok(Some(UiCommand::ApprovalRespond(
                    ApprovalRespondParams::new(
                        event.session_id,
                        event.approval_id,
                        if decision == ToolApprovalDecision::Approve {
                            ApprovalDecision::Approve
                        } else {
                            ApprovalDecision::Deny
                        },
                    ),
                )));
            }
            UiNotification::UserQuestionRequested(event) => {
                let outcome = CliUserQuestionRequester
                    .request_user_question(UserQuestionRequest {
                        title: event.title,
                        body: event.body,
                        questions: event.questions,
                    })
                    .await;
                return Ok(Some(match outcome {
                    UserQuestionOutcome::Answered(answers) => {
                        UiCommand::UserQuestionRespond(UserQuestionRespondParams::new(
                            event.session_id,
                            event.question_id,
                            answers,
                        ))
                    }
                    _ => UiCommand::TurnInterrupt(TurnInterruptParams {
                        session_id: event.session_id,
                        turn_id: event.turn_id,
                    }),
                }));
            }
            UiNotification::Warning(event) => eprintln!("Warning: {}", event.message),
            _ => {}
        }
        Ok(None)
    }
}

struct SignalTask(tokio::task::JoinHandle<()>);
impl Drop for SignalTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ChatCommand {
    pub(super) async fn run_oup(mut self) -> Result<()> {
        self.message = reconcile_one_shot_prompt(self.message.take(), self.prompt.take())?;
        if self.json && self.message.is_none() {
            eyre::bail!("--json requires --message (interactive --json is not supported)");
        }
        let cwd = self
            .cwd
            .clone()
            .unwrap_or(std::env::current_dir()?)
            .canonicalize()
            .wrap_err("resolve chat workspace")?;
        let ctx = super::super::resolve_command_context(self.data_dir.clone())?;
        let stored_profile = resolve_stored_profile(self.profile.as_deref(), &ctx.data_dir)?;
        let mut config = if let Some(file) = &self.config {
            Config::from_file(file)?
        } else if let Some(profile) = &stored_profile {
            crate::profiles::config_from_profile(profile, None, None)
        } else {
            Config::load_with_context(&cwd, &ctx)?
        };
        detach_route_on_provider_override(&mut config, self.provider.as_deref());
        config.provider = self.provider.clone().or(config.provider);
        config.model = self.model.clone().or(config.model);
        config.base_url = self.base_url.clone().or(config.base_url);
        config.api_type = self.api_type.clone().or(config.api_type);
        config.max_iterations = Some(self.max_iterations);
        if let Some(effort) = self.effort {
            config
                .gateway
                .get_or_insert_with(Default::default)
                .reasoning_effort = Some(effort.into());
        }
        let mut tool_profile = match resolve_profile(&self.profile) {
            Ok((profile, _)) => profile,
            Err(_) if stored_profile.is_some() => resolve_profile(&None)?.0,
            Err(error) => return Err(error),
        };
        if self.goals {
            let mut wanted = CHAT_GOAL_TOOLS.to_vec();
            if self.peers {
                wanted.extend_from_slice(CHAT_PEER_TOOLS);
            }
            widen_allow_list(&mut tool_profile.tools, &wanted);
        }
        let permissions = resolve_chat_permissions(
            self.dangerously_bypass_approvals_and_sandbox,
            self.sandbox,
            self.ask_for_approval,
        )?;
        if permissions.is_dangerous() {
            eprintln!(
                "{}",
                "⚠ full access — host writes and network without approval"
                    .red()
                    .bold()
            );
        }
        // Isolate this run's transcript/runtime persistence, not its profile
        // context. Memory, skills, tool configuration and explicit shared-state
        // tools retain their real profile root; automatic episode saving is off.
        let ephemeral = self
            .no_session_persistence
            .then(|| tempfile::Builder::new().prefix("octos-chat-oup-").tempdir())
            .transpose()?;
        let data_dir = ephemeral
            .as_ref()
            .map(|dir| dir.path().to_owned())
            .unwrap_or_else(|| ctx.data_dir.clone());
        let mut profile =
            stored_profile.unwrap_or_else(|| local_profile(octos_core::MAIN_PROFILE_ID, &config));
        profile.config.env_vars = config.env_vars.clone();
        profile.config.gateway.max_iterations = config.max_iterations;
        let profile_id = profile.id.clone();
        let options = LocalOupOptions {
            config,
            profile,
            data_dir: data_dir.clone(),
            config_home: ctx.data_dir.clone(),
            no_retry: self.no_retry,
            provider: None,
            tool_profile: Some(tool_profile),
            save_episodes: !self.no_session_persistence,
        };
        let state = if self.no_session_persistence {
            bootstrap_ephemeral(options, &ctx.data_dir).await?
        } else {
            bootstrap(options).await?
        };
        let runtime = &state.profiles[&profile_id];
        let model = runtime.primary_model_id.clone();
        let tool_config = runtime.tool_config.clone();
        if !self.json {
            eprintln!("Model: {model}");
        }
        if self.goals {
            let orchestrator = crate::autonomy::agent_orchestrator::default_agent_orchestrator();
            orchestrator
                .configure_goal_scopes_sidecar(runtime.data_dir.join("goal-scopes.json"))?;
            orchestrator.configure_supervisor_store(runtime.data_dir.join("supervisor"))?;
        }
        let session_key = if self.goals {
            octos_core::SessionKey(chat_goal_session_key(&profile_id))
        } else {
            octos_core::SessionKey::with_profile(
                &profile_id,
                "cli",
                &uuid::Uuid::now_v7().to_string(),
            )
        };
        let session = OupSession::open(state.clone(), session_key, &cwd, permissions).await?;
        let frontend = TerminalFrontend {
            json: self.json,
            verbose: self.verbose,
            approvals: CliApprovalRequester::default(),
            segments: std::sync::Mutex::new(AssistantTextProjection::default()),
            input_active: AtomicBool::new(false),
            pending_prompts: std::sync::Mutex::new(Vec::new()),
            peers: self
                .peers
                .then(|| crate::commands::oup_peers::OupPeerHost::new(state, permissions)),
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let _signal_task = SignalTask(tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                signal.store(true, Ordering::Release);
            }
        }));
        // The bootstrap carries all effort values, including Disabled (the
        // legacy OUP wire enum has no Disabled variant).
        let effort = None;
        if let Some(message) = self.message {
            let result = session.turn(&message, effort, &cancelled, &frontend).await;
            if let Some(peers) = &frontend.peers {
                peers.close().await;
            }
            let result = finish_turn(result, session.close().await)?;
            if result.interrupted {
                eyre::bail!("turn interrupted");
            }
            if self.json {
                println!(
                    "{}",
                    ChatJsonResult {
                        text: result.text,
                        model: result.model.unwrap_or(model),
                        input_tokens: result.usage.input_tokens.try_into().unwrap_or(u32::MAX),
                        output_tokens: result.usage.output_tokens.try_into().unwrap_or(u32::MAX),
                    }
                    .to_json_line()
                );
            } else {
                println!();
            }
            return Ok(());
        }
        let history_dir = data_dir.join("history");
        std::fs::create_dir_all(&history_dir)?;
        let history_path = history_dir.join("chat_history");
        let mut readline = DefaultEditor::new()?;
        let _ = readline.load_history(&history_path);
        println!("octos chat (OUP) — /exit or Ctrl+C to quit");
        loop {
            frontend.input_active.store(true, Ordering::Release);
            let (input_send, input_recv) = tokio::sync::oneshot::channel();
            // A blocking terminal read cannot be cancelled by Tokio. Keep it
            // outside Tokio's blocking pool so a dead OUP connection can shut
            // down the runtime without waiting forever for another keystroke.
            std::thread::spawn(move || {
                let line = readline.readline("you> ");
                let _ = input_send.send((readline, line));
            });
            let (next, line) = tokio::select! {
                input = input_recv => input?,
                result = session.listen(&frontend) => {
                    result?;
                    eyre::bail!("OUP session ended while awaiting input");
                }
            };
            frontend.input_active.store(false, Ordering::Release);
            readline = next;
            let line = match line {
                Ok(line) => line,
                Err(
                    rustyline::error::ReadlineError::Interrupted
                    | rustyline::error::ReadlineError::Eof,
                ) => break,
                Err(error) => {
                    eprintln!("Input error: {error}");
                    break;
                }
            };
            let pending = std::mem::take(&mut *frontend.pending_prompts.lock().unwrap());
            for prompt in pending {
                if let Some(command) = frontend.event(prompt).await? {
                    let request = command.into_rpc_request("terminal-reply")?;
                    session
                        .client
                        .request(&request.method, request.params)
                        .await?;
                }
            }
            let input = line.trim();
            if input.is_empty() {
                continue;
            }
            let _ = readline.add_history_entry(input);
            if EXIT_COMMANDS.contains(&input.to_lowercase().as_str()) {
                break;
            }
            if input == "/config" || input.starts_with("/config ") {
                println!(
                    "{}",
                    tool_config
                        .handle_config_command(input.trim_start_matches("/config").trim())
                        .await
                );
                continue;
            }
            cancelled.store(false, Ordering::Release);
            if let Err(error) = session.turn(input, effort, &cancelled, &frontend).await {
                eprintln!("Error: {error}");
            }
            println!();
        }
        let _ = readline.save_history(&history_path);
        if let Some(peers) = &frontend.peers {
            peers.close().await;
        }
        session.close().await?;
        println!("Goodbye!");
        Ok(())
    }
}

#[cfg(test)]
mod terminal_integrity_tests {
    use super::*;

    #[test]
    fn terminal_integrity_close_error_preserves_primary_error() {
        let result = finish_turn::<()>(
            Err(eyre!("primary admission error")),
            Err(eyre!("cleanup failed")),
        );
        assert_eq!(result.unwrap_err().to_string(), "primary admission error");
    }

    #[test]
    fn terminal_integrity_close_error_preserves_successful_answer() {
        let result = finish_turn(Ok("real final answer"), Err(eyre!("cleanup failed")));
        assert_eq!(result.unwrap(), "real final answer");
    }
}
