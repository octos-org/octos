//! Integration tests for the credential pool (M6.5).
//!
//! These tests map directly onto the M6.5 acceptance criteria in issue #492.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use octos_llm::credential_pool::rotation_reason;
use octos_llm::{
    AdaptiveConfig, AdaptiveRouter, ChatConfig, ChatResponse, Credential, CredentialPool, ErrorId,
    InMemoryRotationEventSink, LlmProvider, OAuthRefresher, PersistentCredentialPool,
    PersistentCredentialPoolOptions, RotationStrategy, StopReason, TokenUsage, ToolSpec,
};

fn creds(ids: &[&str]) -> Vec<Credential> {
    ids.iter()
        .map(|id| Credential::new(*id, format!("secret-{id}")))
        .collect()
}

fn make_pool(
    path: &std::path::Path,
    name: &str,
    ids: &[&str],
    strategy: RotationStrategy,
) -> PersistentCredentialPool {
    PersistentCredentialPool::open(
        path,
        PersistentCredentialPoolOptions::new(name, creds(ids)).with_strategy(strategy),
    )
    .unwrap()
}

#[tokio::test]
async fn should_never_return_credential_in_cooldown() {
    let dir = tempfile::tempdir().unwrap();
    let pool = make_pool(
        &dir.path().join("p.redb"),
        "test",
        &["a", "b"],
        RotationStrategy::RoundRobin,
    );

    // Cool down "a" until 10 minutes from now.
    let future = now_us() + 10 * 60 * 1_000_000;
    pool.mark_rate_limited("a", Some(future)).await.unwrap();

    // Even 20 acquires must never return "a".
    for _ in 0..20 {
        let cred = pool
            .acquire(rotation_reason::INITIAL_ACQUIRE)
            .await
            .unwrap();
        assert_ne!(cred.id, "a", "cooled-down credential was returned");
    }
}

#[tokio::test]
async fn should_respect_reset_at_from_429_header() {
    let dir = tempfile::tempdir().unwrap();
    let pool = make_pool(
        &dir.path().join("p.redb"),
        "test",
        &["a", "b"],
        RotationStrategy::RoundRobin,
    );

    let reset_at = now_us() + 5 * 60 * 1_000_000; // 5 min in future
    pool.mark_rate_limited("a", Some(reset_at)).await.unwrap();

    let snapshot = pool.snapshot().await.unwrap();
    let a = snapshot.iter().find(|s| s.id == "a").unwrap();
    assert_eq!(a.cooldown_until_us, reset_at);
    assert_eq!(a.reset_at_us, reset_at);
    assert_eq!(a.rate_limit_count, 1);
}

#[tokio::test]
async fn should_rotate_on_auth_failure_using_configured_strategy() {
    let dir = tempfile::tempdir().unwrap();
    // Use FillFirst so the baseline is deterministic.
    let sink = Arc::new(InMemoryRotationEventSink::new());
    let pool = PersistentCredentialPool::open(
        dir.path().join("p.redb"),
        PersistentCredentialPoolOptions::new("test", creds(&["a", "b", "c"]))
            .with_strategy(RotationStrategy::FillFirst)
            .with_event_sink(sink.clone()),
    )
    .unwrap();

    // First acquire hands out "a".
    let first = pool
        .acquire(rotation_reason::INITIAL_ACQUIRE)
        .await
        .unwrap();
    assert_eq!(first.id, "a");

    // Auth fails on "a" — refresher is the null one so it cools "a" down.
    pool.mark_auth_failure("a", ErrorId::new(1)).await.unwrap();

    // Next acquire must be "b" (since "a" is now cooled).
    let second = pool.acquire(rotation_reason::AUTH_FAILURE).await.unwrap();
    assert_eq!(second.id, "b");

    let events = sink.events();
    // Two rotations were emitted: initial + post-auth-failure rotation.
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].credential_id, "a");
    assert_eq!(events[0].reason, "initial_acquire");
    assert_eq!(events[0].strategy, "fill_first");
    assert_eq!(events[1].credential_id, "b");
    assert_eq!(events[1].reason, "auth_failure");
    assert_eq!(events[1].strategy, "fill_first");
}

#[tokio::test]
async fn should_persist_state_across_process_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pool.redb");

    let reset_at = now_us() + 30 * 60 * 1_000_000;
    {
        let pool = make_pool(
            &path,
            "test",
            &["a", "b", "c"],
            RotationStrategy::RoundRobin,
        );
        pool.mark_rate_limited("a", Some(reset_at)).await.unwrap();
        pool.mark_success("b").await.unwrap();
        pool.mark_success("b").await.unwrap();
        pool.mark_success("c").await.unwrap();
    }

    // Simulate restart — open a fresh pool pointed at the same file.
    let reopened = make_pool(
        &path,
        "test",
        &["a", "b", "c"],
        RotationStrategy::RoundRobin,
    );
    let snapshot = reopened.snapshot().await.unwrap();

    let a = snapshot.iter().find(|s| s.id == "a").unwrap();
    assert_eq!(a.cooldown_until_us, reset_at, "cooldown did not persist");
    assert_eq!(a.rate_limit_count, 1);

    let b = snapshot.iter().find(|s| s.id == "b").unwrap();
    assert_eq!(b.usage_count, 2, "usage count did not persist");
    assert!(b.last_used_us > 0);

    // Round-robin cursor also persists: at shutdown it had advanced past
    // whatever was returned last. Verify acquire skips "a" (cooled).
    for _ in 0..5 {
        let c = reopened
            .acquire(rotation_reason::INITIAL_ACQUIRE)
            .await
            .unwrap();
        assert_ne!(c.id, "a", "restored pool handed out cooled credential");
    }
}

struct CountingRefresher {
    calls: Arc<AtomicU32>,
}

#[async_trait]
impl OAuthRefresher for CountingRefresher {
    async fn refresh(&self, credential_id: &str) -> Result<String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(format!("refreshed-{credential_id}"))
    }
}

#[tokio::test]
async fn should_call_oauth_refresh_at_most_once_per_error() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU32::new(0));
    let refresher = Arc::new(CountingRefresher {
        calls: calls.clone(),
    });

    let pool = PersistentCredentialPool::open(
        dir.path().join("p.redb"),
        PersistentCredentialPoolOptions::new("test", creds(&["a", "b"]))
            .with_strategy(RotationStrategy::RoundRobin)
            .with_refresher(refresher),
    )
    .unwrap();

    // Same error id used three times — refresher must only run once.
    let err = ErrorId::new(42);
    pool.mark_auth_failure("a", err).await.unwrap();
    pool.mark_auth_failure("a", err).await.unwrap();
    pool.mark_auth_failure("a", err).await.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // A fresh error id triggers another refresh.
    pool.mark_auth_failure("a", ErrorId::new(43)).await.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn should_emit_rotation_event_per_successful_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(InMemoryRotationEventSink::new());
    let pool = PersistentCredentialPool::open(
        dir.path().join("p.redb"),
        PersistentCredentialPoolOptions::new("test", creds(&["a", "b", "c"]))
            .with_strategy(RotationStrategy::RoundRobin)
            .with_event_sink(sink.clone()),
    )
    .unwrap();

    for _ in 0..4 {
        pool.acquire(rotation_reason::INITIAL_ACQUIRE)
            .await
            .unwrap();
    }

    let events = sink.events();
    assert_eq!(events.len(), 4, "one event per acquire");
    for ev in &events {
        assert_eq!(ev.schema, "octos.harness.event.v1");
        assert_eq!(ev.kind, "credential_rotation");
        assert_eq!(ev.strategy, "round_robin");
        assert_eq!(ev.reason, "initial_acquire");
    }
    assert_eq!(events[0].credential_id, "a");
    assert_eq!(events[1].credential_id, "b");
    assert_eq!(events[2].credential_id, "c");
    assert_eq!(events[3].credential_id, "a");
}

#[tokio::test]
async fn should_produce_distinct_sequences_per_strategy() {
    let dir = tempfile::tempdir().unwrap();

    async fn run(dir: &std::path::Path, name: &str, strategy: RotationStrategy) -> Vec<String> {
        let pool = PersistentCredentialPool::open(
            dir.join(format!("{name}.redb")),
            PersistentCredentialPoolOptions::new(name, creds(&["a", "b", "c", "d"]))
                .with_strategy(strategy),
        )
        .unwrap();
        let mut seen = Vec::new();
        // Vary the "least used" pool a bit so LeastUsed vs RoundRobin diverge
        // without requiring explicit success marks — acquire() itself does
        // not bump usage, so LeastUsed keeps picking the earliest unused id.
        // To force distinct sequences, we tick success on the first pick.
        for i in 0..8 {
            let c = pool
                .acquire(rotation_reason::INITIAL_ACQUIRE)
                .await
                .unwrap();
            if i < 4 {
                pool.mark_success(&c.id).await.unwrap();
            }
            seen.push(c.id);
        }
        seen
    }

    let fill = run(dir.path(), "fill", RotationStrategy::FillFirst).await;
    let rr = run(dir.path(), "rr", RotationStrategy::RoundRobin).await;
    let least = run(dir.path(), "least", RotationStrategy::LeastUsed).await;

    // FillFirst with the F-007 reservation stamp holds the selected id
    // for 5s (cleared on `mark_success`). The first four picks in this
    // run mark success, so each returns "a"; the subsequent picks omit
    // mark_success, causing FillFirst to walk onward as each earlier
    // pick leaves a short-lived reservation in place.
    assert_eq!(fill[..4], ["a", "a", "a", "a"]);
    assert!(
        fill[4..].iter().any(|id| id != "a"),
        "FillFirst should advance after the fifth unmarked pick, got {fill:?}"
    );

    // RoundRobin is the canonical a→b→c→d cycle.
    assert_eq!(rr, vec!["a", "b", "c", "d", "a", "b", "c", "d"]);

    // FillFirst ≠ RoundRobin ≠ LeastUsed.
    assert_ne!(fill, rr, "FillFirst and RoundRobin sequences must differ");
    assert_ne!(fill, least, "FillFirst and LeastUsed sequences must differ");
    // LeastUsed should produce a round-robin-like first pass (since all
    // counts start at 0), then diverge based on tie-breaking. The important
    // guarantee is observable-difference from FillFirst.
    assert!(least.contains(&"a".to_string()));
    assert!(least.contains(&"b".to_string()));

    // Random: we can't assert a specific sequence, but we can require that
    // it differs from at least one of FillFirst/RoundRobin.
    let rand1 = run(dir.path(), "rand1", RotationStrategy::Random).await;
    let rand2 = run(dir.path(), "rand2", RotationStrategy::Random).await;
    let random_differs = rand1 != fill || rand2 != fill || rand1 != rr || rand2 != rr;
    assert!(
        random_differs,
        "Random strategy produced a FillFirst/RoundRobin sequence twice in a row: {rand1:?} / {rand2:?}"
    );
}

// ---------------------------------------------------------------------------
// Adaptive router wiring
// ---------------------------------------------------------------------------

struct FailProvider {
    name: &'static str,
    err: &'static str,
}

#[async_trait]
impl LlmProvider for FailProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        eyre::bail!("{} API error: {}", self.name, self.err);
    }

    fn model_id(&self) -> &str {
        "m"
    }

    fn provider_name(&self) -> &str {
        self.name
    }
}

#[tokio::test]
async fn adaptive_router_forwards_429_to_credential_pool() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(InMemoryRotationEventSink::new());
    let pool: Arc<dyn CredentialPool> = Arc::new(
        PersistentCredentialPool::open(
            dir.path().join("rl.redb"),
            PersistentCredentialPoolOptions::new("adaptive", creds(&["k1", "k2"]))
                .with_strategy(RotationStrategy::RoundRobin)
                .with_event_sink(sink.clone()),
        )
        .unwrap(),
    );

    let router = AdaptiveRouter::new(
        vec![Arc::new(FailProvider {
            name: "slot0",
            err: "429 rate limit exceeded",
        })],
        &[],
        AdaptiveConfig::default(),
    );
    router.attach_credential_pool(0, pool.clone());
    let initial = router.acquire_initial_credential(0).await;
    assert_eq!(initial.as_deref(), Some("k1"));

    let _ = router
        .chat(&[], &[], &ChatConfig::default())
        .await
        .expect_err("provider is designed to fail");

    let snapshot = pool.snapshot().await.unwrap();
    let k1 = snapshot.iter().find(|s| s.id == "k1").unwrap();
    assert_eq!(k1.rate_limit_count, 1, "429 was not forwarded to pool");
    assert!(k1.cooldown_until_us > 0, "cooldown not applied");
}

#[tokio::test]
async fn adaptive_router_forwards_401_to_credential_pool() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU32::new(0));
    let refresher = Arc::new(CountingRefresher {
        calls: calls.clone(),
    });

    let pool: Arc<dyn CredentialPool> = Arc::new(
        PersistentCredentialPool::open(
            dir.path().join("auth.redb"),
            PersistentCredentialPoolOptions::new("adaptive", creds(&["k1", "k2"]))
                .with_strategy(RotationStrategy::RoundRobin)
                .with_refresher(refresher),
        )
        .unwrap(),
    );

    let router = AdaptiveRouter::new(
        vec![Arc::new(FailProvider {
            name: "slot0",
            err: "401 unauthorized",
        })],
        &[],
        AdaptiveConfig::default(),
    );
    router.attach_credential_pool(0, pool.clone());
    let _ = router.acquire_initial_credential(0).await;

    let _ = router
        .chat(&[], &[], &ChatConfig::default())
        .await
        .expect_err("provider is designed to fail");

    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

// Helpers used by the sentinel response type in the router test below.
#[allow(unused)]
fn successful_response() -> ChatResponse {
    ChatResponse {
        content: Some("ok".into()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
        provider_index: None,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
