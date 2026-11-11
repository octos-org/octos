/// UPCR-2026-RF506E103 — whether one history row belongs in this connection's
/// `session/hydrate` / `session/rollback` transcript. The record of a
/// delivered workspace peer message reaches only connections that negotiated
/// `peer.team_message_record.v1`; every other connection keeps the transcript
/// it had before the record existed. `seq` stays the history index, so an
/// omitted record never renumbers another row.
fn hydrate_shows_row(row: &Message, features: ConnectionUiFeatures) -> bool {
    features.peer_team_message_record_v1
        || row.source.as_deref() != Some(workspace_team::MESSAGE_SOURCE)
}

/// Per UPCR-2026-009: bundle the chat-state projection into one RPC.
///
/// Atomicity invariant (codex's review ask): the ledger snapshot and the
/// returned `cursor` are read in one critical section via
/// [`UiProtocolLedger::snapshot_with_cursor`]. A concurrent appender cannot
/// land an event with cursor ≤ result.cursor that the client did not also
/// observe — so a follow-up `session/hydrate { after: result.cursor }`
/// returns only events strictly after the snapshot, with no gap.
// The handler threads the ledger, approval/question stores, turn registry,
// and the caller's profile scope plus negotiated features as one flat
// per-request dependency list.
#[allow(clippy::too_many_arguments)]
async fn handle_session_hydrate(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    approvals: &PendingApprovalStore,
    questions: &PendingQuestionStore,
    active_turns: &SharedActiveTurns,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    params: SessionHydrateParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }
    if params.include.len() > SESSION_HYDRATE_INCLUDE_MAX {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(format!(
                "session/hydrate include too large: {} > {}",
                params.include.len(),
                SESSION_HYDRATE_INCLUDE_MAX
            ))
            .with_data(json!({
                "kind": "include_too_large",
                "limit": SESSION_HYDRATE_INCLUDE_MAX,
            })),
        );
        return;
    }

    // Atomic snapshot of (events ≥ after, head cursor) — closes the
    // codex-flagged gap where reading events and head separately could
    // miss any event committed in between.
    let (mut replayed, head_cursor) =
        match ledger.snapshot_with_cursor(&params.session_id, params.after.as_ref()) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = send_rpc_error(ws, Some(id), error);
                return;
            }
        };
    replayed.retain(|event| {
        replayed_event_visible_to_connection(event, ws.connection_id, approvals, questions)
    });

    let include_set = HydrateIncludeSet::from_request(&params.include);
    // #919.1: route to the profile's session manager when the connection
    // has a profile scope. Turn persistence writes to
    // `SessionRuntime.sessions`; reads must hit the same handle or we'd
    // report `unknown_session` on reconnect-hydrate.
    let Some(sessions) = resolve_sessions_for_lookup(
        state,
        connection_profile_id,
        routed_profile_id,
        &params.session_id,
    )
    .await
    else {
        let _ = send_rpc_error(
            ws,
            Some(id),
            runtime_unavailable_error("Sessions not available"),
        );
        return;
    };
    // Reject unknown sessions per UPCR-2026-009 error model. The session
    // must already exist (typically via a prior `session/open` call); we
    // do NOT auto-create on hydrate.
    {
        let mut sessions_guard =
            match super::session_mutation_proxy::lock_sessions(sessions.clone()).await {
                Ok(guard) => guard,
                Err(_) => {
                    let _ = send_rpc_error(
                        ws,
                        Some(id),
                        runtime_unavailable_error("Session storage is busy; please retry"),
                    );
                    return;
                }
            };
        if !sessions_guard.session_known(&params.session_id) {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::unknown_session(params.session_id.0.clone()),
            );
            return;
        }
    }
    // Hydrate carries the same canonical v2 records as live delivery. Route
    // historical retained source records through the projector too, so an old
    // on-disk turn/spawn_complete row remains readable without restoring a
    // legacy wire lane.
    let replayed_envelopes = if features.projection_envelope_v2 && include_set.messages {
        Some(
            replayed
                .iter()
                .filter_map(|event| {
                    let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) =
                        project_lifecycle_event_to_v2_wire(ledger, &event.event, &event.cursor)?
                    else {
                        return None;
                    };
                    matches!(
                        &envelope.envelope.payload,
                        PayloadV2::BackgroundChildCompleted { .. }
                    )
                    .then_some(envelope.envelope)
                })
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };
    let replayed_tool_envelopes = if features.projection_envelope_v2 && include_set.messages {
        Some(
            replayed
                .iter()
                .filter_map(|event| {
                    let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) =
                        project_lifecycle_event_to_v2_wire(ledger, &event.event, &event.cursor)?
                    else {
                        return None;
                    };
                    hydrate_replays_tool_payload(&envelope.envelope).then_some(envelope.envelope)
                })
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };

    let (replayed_projection_envelopes, projection_thread_sequences) =
        if features.projection_envelope_v2 && include_set.messages {
            let projected = replayed
                .iter()
                .filter_map(|event| {
                    let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) =
                        project_lifecycle_event_to_v2_wire(ledger, &event.event, &event.cursor)?
                    else {
                        return None;
                    };
                    Some(envelope.envelope)
                })
                .collect::<Vec<_>>();
            let (events, checkpoints) = compact_hydrate_projection_replay(projected);
            (Some(events), Some(checkpoints))
        } else {
            (None, None)
        };

    let expose_message_id = features.projection_envelope_v2 && include_set.messages;
    // One complete-or-error scan for Changes and canonical identities, off the
    // async executor. Never replace the client snapshot with a partial history.
    let retained = if include_set.messages {
        let ledger = Arc::clone(ledger);
        let session = params.session_id.clone();
        let through = head_cursor.clone();
        let references = tokio::task::spawn_blocking(move || {
            ledger.retained_hydrate_references(&session, &through, expose_message_id)
        })
        .await
        .map_err(|error| {
            warn!(%error, "hydrate reference worker failed");
            super::ui_protocol_ledger::hydrate_references_error("worker_failed", None)
        })
        .and_then(|result| result);
        match references {
            Ok(references) => references,
            Err(error) => {
                let _ = send_rpc_error(ws, Some(id), error);
                return;
            }
        }
    } else {
        Vec::new()
    };
    let canonical_envelopes = if expose_message_id {
        retained
            .iter()
            .filter(|event| event.cursor.seq <= head_cursor.seq)
            .filter_map(|event| {
                let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) =
                    project_lifecycle_event_to_v2_wire(ledger, &event.event, &event.cursor)?
                else {
                    return None;
                };
                matches!(
                    envelope.envelope.payload,
                    PayloadV2::AssistantPersisted { .. }
                        | PayloadV2::BackgroundChildCompleted { .. }
                )
                .then_some(envelope)
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let claimed_message_ids = canonical_envelopes
        .iter()
        .filter(|notification| notification.session_id == params.session_id)
        .filter_map(|notification| match &notification.envelope.payload {
            PayloadV2::AssistantPersisted { meta, .. } => Some(meta.message_id.as_str()),
            PayloadV2::BackgroundChildCompleted { message_id, .. } => Some(message_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();

    // Lock once; gather all the in-memory chat state we need so the
    // result reflects a single sessions-side snapshot.
    let (messages, threads_projection, context, context_state) = {
        let mut sessions_guard =
            match super::session_mutation_proxy::lock_sessions(sessions.clone()).await {
                Ok(guard) => guard,
                Err(_) => {
                    let _ = send_rpc_error(
                        ws,
                        Some(id),
                        runtime_unavailable_error("Session storage is busy; please retry"),
                    );
                    return;
                }
            };
        let data_dir = sessions_guard.data_dir();
        let session = sessions_guard.get_or_create(&params.session_id).await;
        let (context, context_state) = if features.context_lifecycle_available() {
            let (context, context_state) =
                appui_context_inspection_snapshot(&data_dir, &params.session_id, &session.messages);
            context_snapshot_for_features(Some(context), Some(context_state), features)
        } else {
            (None, None)
        };
        let canonical_identities = hydrated_canonical_message_identities(
            &params.session_id,
            &session.messages,
            &canonical_envelopes,
        );
        let messages = if include_set.messages {
            Some(
                session
                    .messages
                    .iter()
                    // Named over the whole transcript: `after` may skip the
                    // row that made a result's call (UPCR-2026-039).
                    .zip(hydrated_tool_names(&session.messages))
                    .enumerate()
                    .filter(|(seq, _)| match params.after.as_ref() {
                        Some(after) => *seq as u64 > after.seq,
                        None => true,
                    })
                    .filter(|(_, (msg, _))| hydrate_shows_row(msg, features))
                    .map(|(seq, (msg, tool_name))| {
                        let canonical_identity = canonical_identities.get(&seq);
                        let seq = seq as u64;
                        // V2 clients get a transcript identity that matches
                        // assistant_persisted/background_child_completed.
                        let message_id = if expose_message_id {
                            canonical_identity.map(|(id, _)| id.clone()).or_else(|| {
                                let candidate = format!(
                                    "{}:{seq}:{}",
                                    params.session_id.0,
                                    msg.timestamp.timestamp_nanos_opt().unwrap_or(0),
                                );
                                // A rejected claim must not re-enter as the
                                // fallback ID: the client correlates by ID even
                                // when source is absent. Leave it unbound.
                                (!claimed_message_ids.contains(candidate.as_str()))
                                    .then_some(candidate)
                            })
                        } else {
                            None
                        };
                        let source = canonical_identity
                            .filter(|(_, background)| *background)
                            .map(|_| "background".to_owned());
                        HydratedMessage {
                            seq,
                            role: msg.role.as_str().to_owned(),
                            content: msg.content.clone(),
                            turn_id: None, // Message struct does not carry typed turn_id today
                            thread_id: msg.thread_id.clone(),
                            client_message_id: msg.client_message_id.clone(),
                            persisted_at: msg.timestamp,
                            message_id,
                            source,
                            // The store persists reasoning; without this the
                            // "· reasoning" block vanishes on every client
                            // restart. Same v2 gate as message_id/source.
                            reasoning_content: if expose_message_id {
                                msg.reasoning_content.clone()
                            } else {
                                None
                            },
                            // P1.3 fix: surface canonical-ledger media so a
                            // client reconnecting after a disconnect can
                            // re-render the same `.md` / `.mp3` / `.pptx`
                            // attachment carried by the v2 projection.
                            media: msg.media.clone(),
                            // UPCR-2026-039: ungated like media. A client
                            // without v2 tool envelopes (a stdio host) names
                            // reloaded tool rows from these.
                            tool_call_id: msg.tool_call_id.clone(),
                            tool_name,
                            tool_calls: hydrated_tool_calls(msg),
                            peer_message: workspace_team::message_record(msg),
                        }
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let threads_projection = if include_set.threads || include_set.turns {
            Some(build_thread_graph_entries(session))
        } else {
            None
        };
        (messages, threads_projection, context, context_state)
    };

    let threads = if include_set.threads {
        threads_projection
            .clone()
            .map(|(threads, _orphans)| threads)
    } else {
        None
    };

    let turns = if include_set.turns {
        let projected_threads = threads_projection
            .as_ref()
            .map(|(t, _)| t.clone())
            .unwrap_or_default();
        Some(
            collect_session_turns(
                &params.session_id,
                active_turns,
                &replayed,
                &projected_threads,
            )
            .await,
        )
    } else {
        None
    };

    let pending_approvals = if include_set.pending_approvals {
        Some(
            approvals
                .pending_for_session(&params.session_id)
                .into_iter()
                .filter(|approval| {
                    pending_approval_visible_to_connection(approvals, approval, ws.connection_id.0)
                })
                .collect(),
        )
    } else {
        None
    };

    // UPCR-2026-023: hydrate still-pending structured user-questions alongside
    // pending approvals (same `pending_approvals` include section), but only
    // for a connection that negotiated `user_question.v1` — a client without
    // the capability has no `user_question/respond` path, so the section is
    // omitted (not `null`) exactly like a non-negotiated wire event is
    // filtered out. Mirrors the `session/open` pending-question replay gate.
    let pending_questions = if include_set.pending_approvals && features.user_question_v1 {
        Some(
            questions
                .pending_for_session(&params.session_id)
                .into_iter()
                .filter(|question| {
                    pending_question_visible_to_connection(questions, question, ws.connection_id.0)
                })
                .collect(),
        )
    } else {
        None
    };

    let file_mutations = include_set.messages.then(|| {
        retained
            .iter()
            .filter_map(|entry| super::ui_protocol_ledger::file_mutation(&entry.event).cloned())
            .collect()
    });

    let result = SessionHydrateResult {
        file_mutations,
        session_id: params.session_id,
        cursor: head_cursor,
        context,
        context_state,
        messages,
        threads,
        turns,
        pending_approvals,
        pending_questions,
        replayed_envelopes,
        replayed_tool_envelopes,
        replayed_projection_envelopes,
        projection_thread_sequences,
    };
    send_hydrate_result(ws, id, result);
}
