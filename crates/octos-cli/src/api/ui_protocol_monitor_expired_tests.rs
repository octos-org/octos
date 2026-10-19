use super::*;

/// #2498 — the per-session budget binds what the PRODUCTION sink wiring
/// appends to the owning session's durable stream: 45 real watcher-site
/// active→expired transitions land exactly the per-window cap in the ledger
/// and the suppressed tail stays silent. The durable transitions themselves
/// are untouched (every re-arm really did expire the monitor); only the
/// client-side tap is bounded.
#[test]
fn should_cap_monitor_expired_at_the_per_session_budget_through_the_production_sink() {
    use crate::autonomy::agent_orchestrator::{
        AgentOrchestrator as _, InProcessAgentOrchestrator, MONITOR_EXPIRED_MAX_PER_WINDOW,
        MonitorCreateRequest,
    };

    let _guard = crate::autonomy::agent_orchestrator::monitor_expired_test_guard();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let state = Arc::new(AppState::empty_for_tests());
        spawn_monitor_expired_sink(state.clone());
        let ledger = event_ledger(&state).await;
        let owner = SessionKey("local:mon-expiry-budget-owner".into());
        let orchestrator = InProcessAgentOrchestrator::default();
        let created = orchestrator
            .create_monitor(MonitorCreateRequest {
                session_id: owner.clone(),
                profile_id: "alpha".to_owned(),
                spec: crate::autonomy::monitor_runtime::MonitorSpec {
                    name: "budget-watch".to_owned(),
                    argv: vec!["sh".to_owned(), "-c".to_owned(), "true".to_owned()],
                    filter_regex: None,
                    batch_ms: crate::autonomy::monitor_runtime::MONITOR_DEFAULT_BATCH_MS,
                    mode: crate::autonomy::monitor_runtime::MonitorMode::Poll { interval_secs: 3 },
                    timeout_secs: None,
                    persistent: false,
                    max_events_per_hour: 5,
                    goal_id: None,
                    cwd: None,
                },
                data_dir: None,
            })
            .expect("create monitor");
        let monitor_id = created["monitor_id"]
            .as_str()
            .expect("monitor id")
            .to_owned();

        // Forty-five watcher-site transitions, each re-armed past-due the way
        // `monitor/resume` refuses to (#2367): every one is a real
        // active→expired transition through the production emit site.
        for _ in 0..45 {
            orchestrator.rearm_monitor_past_deadline_for_test(&monitor_id);
            orchestrator.expire_monitor(&monitor_id, "timeout");
        }

        // The drain appends asynchronously over the bounded channel: wait for
        // the ledger count to settle, then pin it at exactly the cap.
        let resume_from = UiCursor {
            stream: owner.0.clone(),
            seq: 0,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let (mut count, mut stable) = (0usize, 0usize);
        loop {
            let replay = ledger
                .replay_after(&owner, Some(&resume_from))
                .unwrap_or_default();
            let delivered = replay
                .iter()
                .filter(|entry| {
                    matches!(
                        &entry.event,
                        UiProtocolLedgerEvent::Notification(UiNotification::MonitorExpired(_))
                    )
                })
                .count();
            if delivered == count && delivered > 0 {
                stable += 1;
            } else {
                stable = 0;
            }
            count = delivered;
            if stable >= 5 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "monitor/expired ledger count never settled (last: {count})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(
            count, MONITOR_EXPIRED_MAX_PER_WINDOW as usize,
            "the production wiring appends only the per-session budget to the durable stream"
        );
        // The durable side is unaffected: the record really did expire.
        let status = orchestrator.monitor_status_for_test(&monitor_id).unwrap().0;
        assert_eq!(status, "expired");
    });
}
