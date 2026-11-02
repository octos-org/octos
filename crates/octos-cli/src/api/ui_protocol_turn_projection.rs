fn terminal_times_for_lifecycle(
    state: &TurnLifecycleState,
    completed_at: Option<DateTime<Utc>>,
    completed_at_durable: Option<bool>,
) -> (Option<DateTime<Utc>>, Option<bool>) {
    if matches!(
        state,
        TurnLifecycleState::Active | TurnLifecycleState::Interrupting
    ) {
        (None, None)
    } else {
        (completed_at, completed_at_durable)
    }
}

#[derive(Debug, Default, Clone)]
struct TurnLedgerProjection {
    state: Option<TurnLifecycleState>,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    completed_at_durable: Option<bool>,
    thread_id: Option<String>,
}

/// Project a turn's lifecycle from ledger notifications and canonical v2
/// envelopes. Returns `state = None` when the ledger has no record.
fn project_turn_from_ledger(
    target: &TurnId,
    events: &[LedgeredUiProtocolEvent],
) -> TurnLedgerProjection {
    let mut projection = TurnLedgerProjection::default();
    for ev in events {
        let UiProtocolLedgerEvent::Notification(notification) = &ev.event else {
            continue;
        };
        match notification {
            UiNotification::TurnStarted(started) if started.turn_id == *target => {
                projection.started_at = Some(started.timestamp);
                if projection.state.is_none() {
                    projection.state = Some(TurnLifecycleState::Active);
                }
            }
            UiNotification::TurnCompleted(completed) if completed.turn_id == *target => {
                projection.completed_at = ev.recorded_at;
                projection.completed_at_durable = ev.recorded_at.map(|_| true);
                projection.state = Some(TurnLifecycleState::Completed);
            }
            UiNotification::TurnError(errored) if errored.turn_id == *target => {
                projection.completed_at = ev.recorded_at;
                projection.completed_at_durable = (errored.code != "orphaned_by_restart")
                    .then_some(ev.recorded_at.map(|_| true))
                    .flatten();
                projection.state = Some(if errored.code == "interrupted" {
                    TurnLifecycleState::Interrupted
                } else {
                    TurnLifecycleState::Errored
                });
            }
            UiNotification::EnvelopeV2(envelope)
                if envelope.envelope.turn_id == target.0.to_string()
                    && projection.thread_id.is_none() =>
            {
                projection.thread_id = Some(envelope.envelope.thread_id.clone());
            }
            _ => {}
        }
    }
    projection
}

/// Combine the active-turn registry view with the ledger projection to
/// build the `turns` section of `session/hydrate`. Output is sorted by
/// `started_at` so consumers render turns in lifecycle order.
async fn collect_session_turns(
    session_id: &SessionKey,
    active_turns: &SharedActiveTurns,
    events: &[LedgeredUiProtocolEvent],
    threads: &[ThreadGraphEntry],
) -> Vec<HydratedTurn> {
    use std::collections::HashMap;

    // First: collect every turn_id we've seen in the ledger.
    let mut projections: HashMap<TurnId, TurnLedgerProjection> = HashMap::new();
    for ev in events {
        let UiProtocolLedgerEvent::Notification(notification) = &ev.event else {
            continue;
        };
        let turn_id = match notification {
            UiNotification::TurnStarted(e) => Some(e.turn_id.clone()),
            UiNotification::TurnCompleted(e) => Some(e.turn_id.clone()),
            UiNotification::TurnError(e) => Some(e.turn_id.clone()),
            _ => None,
        };
        let Some(turn_id) = turn_id else {
            continue;
        };
        if !projections.contains_key(&turn_id) {
            projections.insert(turn_id.clone(), TurnLedgerProjection::default());
        }
    }
    for turn_id in projections.keys().cloned().collect::<Vec<_>>() {
        let proj = project_turn_from_ledger(&turn_id, events);
        projections.insert(turn_id, proj);
    }

    // Overlay the active-turn registry's live state for the active turn,
    // if any.
    {
        let registry = active_turns.lock().await;
        if let Some(entry) = registry.get(session_id) {
            let live = {
                let state = entry.state.lock().await;
                turn_state_to_lifecycle(&state)
            };
            let proj = projections.entry(entry.turn_id.clone()).or_default();
            (proj.completed_at, proj.completed_at_durable) =
                terminal_times_for_lifecycle(&live, proj.completed_at, proj.completed_at_durable);
            proj.state = Some(live);
        }
    }

    // Backfill thread_id from the thread graph when the ledger projection
    // didn't surface one yet for this turn.
    let mut turns: Vec<HydratedTurn> = projections
        .into_iter()
        .map(|(turn_id, proj)| {
            let thread_id = proj.thread_id.clone().or_else(|| {
                threads
                    .iter()
                    .find(|t| t.turn_id.as_ref() == Some(&turn_id))
                    .map(|t| t.thread_id.clone())
            });
            HydratedTurn {
                turn_id,
                state: proj.state.unwrap_or(TurnLifecycleState::Unknown),
                started_at: proj.started_at,
                completed_at: proj.completed_at,
                completed_at_durable: proj.completed_at_durable,
                thread_id,
            }
        })
        .collect();
    turns.sort_by_key(|t| t.started_at.unwrap_or_else(Utc::now));
    turns
}

