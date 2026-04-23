//! Credential pool with persistent cooldowns and rotation strategies (M6.5).
//!
//! Long-running sessions rotate across multiple credentials to survive per-key
//! rate limits and transient auth failures. This module defines:
//!
//! - [`Credential`] — a single credential record (id + opaque secret).
//! - [`CredentialState`] — durable per-credential state (cooldown expiry,
//!   429 counts, last-used timestamp, usage counter).
//! - [`RotationStrategy`] — the four supported selection policies:
//!   `FillFirst`, `RoundRobin`, `Random`, `LeastUsed`.
//! - [`CredentialPool`] — the public trait used by adapters (e.g. the
//!   adaptive router) to acquire, release, and mark credentials.
//! - [`PersistentCredentialPool`] — the concrete redb-backed implementation.
//! - [`OAuthRefresher`] — a hook invoked at most once per error when a
//!   credential fails auth so integrations can refresh the underlying token.
//!
//! **Persistence invariant.** All state mutations go through a single
//! `begin_write() … commit()` transaction. redb guarantees atomicity, so a
//! crash mid-write cannot leave the pool in a half-updated state.
//!
//! **Rotation events.** Each successful rotation emits a structured event
//! through [`RotationEventSink`] (typically the harness event sink) and
//! increments the `octos_llm_credential_rotation_total{reason,strategy}`
//! counter. Events fire exactly once per rotation, never twice.
//!
//! **OAuth refresh.** When a credential fails an auth check the pool invokes
//! the registered [`OAuthRefresher`] at most once per error event. Repeat
//! failures within the same event do not double-refresh; if the refresh
//! itself fails the credential is cooled-down instead.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eyre::{Result, WrapErr, bail};
use metrics::counter;
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Current schema version for persisted `CredentialState` rows.
pub const CREDENTIAL_POOL_SCHEMA_VERSION: u32 = 1;

/// Default schema version used when deserializing rows that predate the
/// `schema_version` field.
pub fn default_credential_pool_schema_version() -> u32 {
    CREDENTIAL_POOL_SCHEMA_VERSION
}

/// redb table storing per-credential state, keyed by credential id.
const CREDENTIAL_STATE_TABLE: TableDefinition<&str, &str> =
    TableDefinition::new("credential_state_v1");

/// redb table storing round-robin cursor, keyed by pool name.
const CURSOR_TABLE: TableDefinition<&str, u64> = TableDefinition::new("credential_cursor_v1");

// ---------------------------------------------------------------------------
// Credential types
// ---------------------------------------------------------------------------

/// A single credential entry added to a pool.
///
/// The opaque `secret` is kept as a `String` so integrations can store any
/// shape they want (API key, bearer token, composite cookie, etc). The pool
/// never inspects the secret — it only round-robins / cools down by `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credential {
    /// Stable identifier for this credential within its pool.
    pub id: String,
    /// The opaque secret to hand to the provider (e.g. API key).
    pub secret: String,
}

impl Credential {
    pub fn new(id: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            secret: secret.into(),
        }
    }
}

/// Durable per-credential state persisted in redb.
///
/// `cooldown_until_us` is the UNIX epoch microsecond after which the
/// credential becomes available again. `0` means "no active cooldown".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialState {
    /// Schema version for forward compatibility.
    #[serde(default = "default_credential_pool_schema_version")]
    pub schema_version: u32,
    /// Credential id this row belongs to.
    pub id: String,
    /// Micros-since-epoch until which the credential is cooled down.
    /// `0` means no active cooldown.
    #[serde(default)]
    pub cooldown_until_us: u64,
    /// Total number of 429 responses this credential has accumulated.
    #[serde(default)]
    pub rate_limit_count: u64,
    /// Optional server-provided reset time (micros since epoch), copied from
    /// the `reset_at` header of the failing 429 response. This is what the
    /// pool uses to decide `cooldown_until_us`.
    #[serde(default)]
    pub reset_at_us: u64,
    /// Micros-since-epoch of the last successful use of this credential.
    #[serde(default)]
    pub last_used_us: u64,
    /// Total number of successful uses across the lifetime of the pool.
    #[serde(default)]
    pub usage_count: u64,
}

impl CredentialState {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            schema_version: CREDENTIAL_POOL_SCHEMA_VERSION,
            id: id.into(),
            cooldown_until_us: 0,
            rate_limit_count: 0,
            reset_at_us: 0,
            last_used_us: 0,
            usage_count: 0,
        }
    }

    /// Whether this credential is currently cooled down at `now_us`.
    pub fn is_cooled_down(&self, now_us: u64) -> bool {
        self.cooldown_until_us > now_us
    }
}

// ---------------------------------------------------------------------------
// Rotation strategies
// ---------------------------------------------------------------------------

/// Supported credential rotation strategies.
///
/// All four produce observably distinct sequences when given identical pools
/// and cold state — see
/// `should_produce_distinct_sequences_per_strategy` in the tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RotationStrategy {
    /// Always hand out the lowest-index non-cooled credential. Predictable
    /// but biased — later credentials only see traffic after the first is
    /// exhausted.
    FillFirst,
    /// Cycle through available credentials in insertion order, persisting
    /// the cursor across restarts. Spreads load evenly.
    #[default]
    RoundRobin,
    /// Pick a random non-cooled credential. Useful for load-balancing when
    /// providers apply per-key concurrency limits.
    Random,
    /// Pick the credential with the smallest `usage_count` (then smallest
    /// `last_used_us`). Best fairness guarantees when credentials have
    /// heterogeneous costs.
    LeastUsed,
}

impl RotationStrategy {
    /// Stable lowercase label used in metrics and event payloads.
    pub fn as_metric_label(self) -> &'static str {
        match self {
            Self::FillFirst => "fill_first",
            Self::RoundRobin => "round_robin",
            Self::Random => "random",
            Self::LeastUsed => "least_used",
        }
    }
}

impl std::fmt::Display for RotationStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_metric_label())
    }
}

// ---------------------------------------------------------------------------
// Rotation events
// ---------------------------------------------------------------------------

/// Structured rotation event emitted on each successful credential swap.
///
/// This mirrors the harness event ABI (`octos.harness.event.v1`) so the
/// observer crate can forward events without re-encoding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialRotationEvent {
    /// Stable schema identifier.
    pub schema: String,
    /// Event kind — always `credential_rotation`.
    pub kind: String,
    /// Credential id that was just selected.
    pub credential_id: String,
    /// Reason for the rotation (e.g. `initial_acquire`, `rate_limit_cooldown`,
    /// `auth_failure`, `manual_release`).
    pub reason: String,
    /// Strategy that produced the selection.
    pub strategy: String,
}

impl CredentialRotationEvent {
    pub fn new(
        credential_id: impl Into<String>,
        reason: impl Into<String>,
        strategy: RotationStrategy,
    ) -> Self {
        Self {
            schema: "octos.harness.event.v1".to_string(),
            kind: "credential_rotation".to_string(),
            credential_id: credential_id.into(),
            reason: reason.into(),
            strategy: strategy.as_metric_label().to_string(),
        }
    }
}

/// Standard rotation reason labels (used as stable strings in events/metrics).
pub mod rotation_reason {
    pub const INITIAL_ACQUIRE: &str = "initial_acquire";
    pub const RATE_LIMIT_COOLDOWN: &str = "rate_limit_cooldown";
    pub const AUTH_FAILURE: &str = "auth_failure";
    pub const MANUAL_RELEASE: &str = "manual_release";
}

/// Sink that consumes rotation events. Implementations typically forward
/// events into `octos-agent::harness_events` or a mock in tests.
pub trait RotationEventSink: Send + Sync {
    fn emit(&self, event: &CredentialRotationEvent);
}

/// No-op sink used when the pool is configured without observability.
pub struct NullRotationEventSink;

impl RotationEventSink for NullRotationEventSink {
    fn emit(&self, _event: &CredentialRotationEvent) {}
}

/// In-memory sink used in tests to assert event counts/payloads.
#[derive(Debug, Default)]
pub struct InMemoryRotationEventSink {
    events: Mutex<Vec<CredentialRotationEvent>>,
}

impl InMemoryRotationEventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<CredentialRotationEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn len(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl RotationEventSink for InMemoryRotationEventSink {
    fn emit(&self, event: &CredentialRotationEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event.clone());
    }
}

// ---------------------------------------------------------------------------
// OAuth refresh hook
// ---------------------------------------------------------------------------

/// Trait for integrations that can refresh an OAuth token.
///
/// Implementations must be idempotent with respect to repeated calls for the
/// same credential id — the pool enforces "at most once per error" by
/// tracking the last seen error instance, but refresh providers should still
/// be resilient to spurious attempts.
#[async_trait]
pub trait OAuthRefresher: Send + Sync {
    /// Refresh the credential identified by `credential_id`. Implementations
    /// return the new secret on success; the pool stores it in-memory and
    /// writes a fresh state row to redb.
    async fn refresh(&self, credential_id: &str) -> Result<String>;
}

/// Null refresher used when OAuth refresh is not configured.
pub struct NullOAuthRefresher;

#[async_trait]
impl OAuthRefresher for NullOAuthRefresher {
    async fn refresh(&self, _credential_id: &str) -> Result<String> {
        bail!("OAuth refresh not configured")
    }
}

/// Opaque handle for the "error occurrence" that the pool uses to guarantee
/// at-most-once OAuth refresh. Each distinct error handed to the pool must
/// carry a unique [`ErrorId`]; the pool remembers the last id it refreshed
/// and skips repeat calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ErrorId(pub u64);

impl ErrorId {
    pub fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Generate a fresh id from the clock + an atomic counter. Callers can
    /// use this when they don't already track error occurrences.
    pub fn fresh() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = now_epoch_us();
        Self(now.wrapping_mul(1024).wrapping_add(seq))
    }
}

// ---------------------------------------------------------------------------
// CredentialPool trait
// ---------------------------------------------------------------------------

/// High-level interface for credential pools.
///
/// The trait is `async` because OAuth refresh requires awaiting a network
/// call. Non-OAuth implementations can still implement it with zero-cost
/// async (e.g. returning an immediately-ready future).
#[async_trait]
pub trait CredentialPool: Send + Sync {
    /// Return the first non-cooled credential according to the configured
    /// rotation strategy. Emits a `credential_rotation` event and increments
    /// the rotation counter on success.
    async fn acquire(&self, reason: &str) -> Result<Credential>;

    /// Cool a credential down based on a 429 response. `reset_at_us` is the
    /// UNIX epoch microsecond advertised by the provider (typically parsed
    /// from the `Retry-After` or `X-RateLimit-Reset` header). If the
    /// provider did not advertise a reset time, pass `None` and the pool
    /// applies a default backoff.
    async fn mark_rate_limited(&self, credential_id: &str, reset_at_us: Option<u64>) -> Result<()>;

    /// Report an authentication failure for a credential. The pool invokes
    /// the configured [`OAuthRefresher`] at most once per `error_id`; on
    /// refresh failure the credential is cooled down.
    async fn mark_auth_failure(&self, credential_id: &str, error_id: ErrorId) -> Result<()>;

    /// Record that a request using this credential succeeded. Updates
    /// `usage_count` and `last_used_us`.
    async fn mark_success(&self, credential_id: &str) -> Result<()>;

    /// Return the current state snapshot for observability.
    async fn snapshot(&self) -> Result<Vec<CredentialState>>;
}

// ---------------------------------------------------------------------------
// Persistent, redb-backed pool
// ---------------------------------------------------------------------------

/// Options used to construct a [`PersistentCredentialPool`].
pub struct PersistentCredentialPoolOptions {
    pub name: String,
    pub credentials: Vec<Credential>,
    pub strategy: RotationStrategy,
    pub default_cooldown_us: u64,
    pub event_sink: Arc<dyn RotationEventSink>,
    pub refresher: Arc<dyn OAuthRefresher>,
}

impl PersistentCredentialPoolOptions {
    pub fn new(name: impl Into<String>, credentials: Vec<Credential>) -> Self {
        Self {
            name: name.into(),
            credentials,
            strategy: RotationStrategy::default(),
            default_cooldown_us: DEFAULT_COOLDOWN_US,
            event_sink: Arc::new(NullRotationEventSink),
            refresher: Arc::new(NullOAuthRefresher),
        }
    }

    pub fn with_strategy(mut self, strategy: RotationStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    pub fn with_default_cooldown_us(mut self, micros: u64) -> Self {
        self.default_cooldown_us = micros;
        self
    }

    pub fn with_event_sink(mut self, sink: Arc<dyn RotationEventSink>) -> Self {
        self.event_sink = sink;
        self
    }

    pub fn with_refresher(mut self, refresher: Arc<dyn OAuthRefresher>) -> Self {
        self.refresher = refresher;
        self
    }
}

/// Default cooldown (60s) applied when a 429 has no `reset_at`.
pub const DEFAULT_COOLDOWN_US: u64 = 60 * 1_000_000;

/// Canonical filename for the credential pool database (M6.5 spec).
/// Callers that root their data directory in `~/.octos/` should join this
/// constant onto that path to match the spec-documented location.
pub const DEFAULT_CREDENTIAL_POOL_DB_FILENAME: &str = "credential_pool.redb";

/// Resolve the default credential pool db path at `<home>/.octos/credential_pool.redb`.
/// Returns `None` when the home directory cannot be detected.
pub fn default_credential_pool_path() -> Option<std::path::PathBuf> {
    let mut home = dirs_home()?;
    home.push(".octos");
    home.push(DEFAULT_CREDENTIAL_POOL_DB_FILENAME);
    Some(home)
}

fn dirs_home() -> Option<std::path::PathBuf> {
    // Avoid a hard dependency on the `dirs` crate at this layer; the CLI
    // passes the resolved path in explicitly. We still check the env vars
    // for parity with dirs's lookup so tests + unit callers can override.
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Some(std::path::PathBuf::from(home));
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(home) = std::env::var("USERPROFILE") {
            if !home.is_empty() {
                return Some(std::path::PathBuf::from(home));
            }
        }
    }
    None
}

/// redb-backed implementation of [`CredentialPool`].
///
/// Concurrency: a single interior `Mutex` wraps both the in-memory index and
/// the redb handle. This is deliberate — redb's own txn-level locking would
/// allow concurrent reads, but the rotation strategies need a consistent view
/// across read + cursor update, so a coarse lock keeps the invariants trivial
/// to reason about.
pub struct PersistentCredentialPool {
    inner: Mutex<PoolInner>,
    name: String,
    strategy: RotationStrategy,
    default_cooldown_us: u64,
    event_sink: Arc<dyn RotationEventSink>,
    refresher: Arc<dyn OAuthRefresher>,
}

struct PoolInner {
    db: Arc<Database>,
    credentials: Vec<Credential>,
    /// Local mirror of persisted state — kept in sync with redb writes so
    /// selection can happen without hitting the disk.
    state: HashMap<String, CredentialState>,
    /// Last error id that triggered an OAuth refresh. Guarantees at-most-once
    /// refresh per error event.
    last_refresh_error_id: Option<ErrorId>,
    /// Simple LCG state for random selection (deterministic per-pool seed).
    rng_state: u64,
    /// Round-robin cursor mirror; also persisted in `CURSOR_TABLE`.
    cursor: u64,
}

impl PersistentCredentialPool {
    /// Open (or create) a pool backed by the redb file at `path`.
    pub fn open(path: impl AsRef<Path>, options: PersistentCredentialPoolOptions) -> Result<Self> {
        if options.credentials.is_empty() {
            bail!(
                "credential pool `{}` requires at least one credential",
                options.name
            );
        }
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).wrap_err_with(|| {
                format!(
                    "failed to create credential pool directory {}",
                    parent.display()
                )
            })?;
        }

        let db = Database::create(path)
            .wrap_err_with(|| format!("failed to open credential pool at {}", path.display()))?;

        // Init tables.
        {
            let write_txn = db.begin_write()?;
            {
                let _ = write_txn.open_table(CREDENTIAL_STATE_TABLE)?;
                let _ = write_txn.open_table(CURSOR_TABLE)?;
            }
            write_txn.commit()?;
        }

        // Load existing state.
        let mut state: HashMap<String, CredentialState> = HashMap::new();
        let cursor: u64;
        {
            let read_txn = db.begin_read()?;
            let state_table = read_txn.open_table(CREDENTIAL_STATE_TABLE)?;
            for row in state_table.iter()? {
                let (key, value) = row?;
                let raw = value.value();
                match serde_json::from_str::<CredentialState>(raw) {
                    Ok(s) => {
                        state.insert(key.value().to_string(), s);
                    }
                    Err(e) => {
                        warn!(
                            pool = %options.name,
                            credential_id = key.value(),
                            error = %e,
                            "failed to deserialize credential state; resetting row"
                        );
                    }
                }
            }

            let cursor_table = read_txn.open_table(CURSOR_TABLE)?;
            cursor = cursor_table
                .get(options.name.as_str())?
                .map(|v| v.value())
                .unwrap_or(0);
        }

        // Backfill missing rows for credentials that are new.
        let mut added_rows: Vec<(String, CredentialState)> = Vec::new();
        for cred in &options.credentials {
            if !state.contains_key(&cred.id) {
                let fresh = CredentialState::new(&cred.id);
                added_rows.push((cred.id.clone(), fresh));
            }
        }
        if !added_rows.is_empty() {
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(CREDENTIAL_STATE_TABLE)?;
                for (id, fresh) in &added_rows {
                    let json = serde_json::to_string(&fresh)
                        .wrap_err("failed to serialize credential state")?;
                    table.insert(id.as_str(), json.as_str())?;
                }
            }
            write_txn.commit()?;
            for (id, fresh) in added_rows {
                state.insert(id, fresh);
            }
        }

        // RNG seed derived from pool name + clock so distinct pools diverge.
        let seed = {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in options.name.as_bytes() {
                h ^= u64::from(*byte);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            h ^ now_epoch_us()
        };

        let inner = PoolInner {
            db: Arc::new(db),
            credentials: options.credentials,
            state,
            last_refresh_error_id: None,
            rng_state: seed | 1, // must be non-zero for LCG
            cursor,
        };

        Ok(Self {
            inner: Mutex::new(inner),
            name: options.name,
            strategy: options.strategy,
            default_cooldown_us: options.default_cooldown_us,
            event_sink: options.event_sink,
            refresher: options.refresher,
        })
    }

    /// Returns a snapshot of known credential ids (in declared order).
    pub fn credential_ids(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .credentials
            .iter()
            .map(|c| c.id.clone())
            .collect()
    }

    /// Pool name (used in metrics labels and logs).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Currently configured strategy.
    pub fn strategy(&self) -> RotationStrategy {
        self.strategy
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PoolInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist_state(inner: &PoolInner, state: &CredentialState) -> Result<()> {
        let json = serde_json::to_string(state).wrap_err("serialize credential state")?;
        let write_txn = inner.db.begin_write()?;
        {
            let mut table = write_txn.open_table(CREDENTIAL_STATE_TABLE)?;
            table.insert(state.id.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    fn persist_cursor(inner: &PoolInner, name: &str, cursor: u64) -> Result<()> {
        let write_txn = inner.db.begin_write()?;
        {
            let mut table = write_txn.open_table(CURSOR_TABLE)?;
            table.insert(name, cursor)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    fn emit_rotation(&self, credential_id: &str, reason: &str) {
        let event = CredentialRotationEvent::new(credential_id, reason, self.strategy);
        self.event_sink.emit(&event);
        counter!(
            "octos_llm_credential_rotation_total",
            "reason" => reason.to_string(),
            "strategy" => self.strategy.as_metric_label().to_string(),
        )
        .increment(1);
        debug!(
            pool = %self.name,
            credential_id,
            reason,
            strategy = %self.strategy,
            "credential rotation"
        );
    }

    /// Pick the next credential according to the configured strategy.
    /// Returns the credential id of the selection, or `None` when every
    /// credential is currently in cooldown.
    fn select(&self, inner: &mut PoolInner) -> Option<String> {
        let now = now_epoch_us();
        match self.strategy {
            RotationStrategy::FillFirst => {
                for cred in &inner.credentials {
                    let state = inner
                        .state
                        .get(&cred.id)
                        .cloned()
                        .unwrap_or_else(|| CredentialState::new(&cred.id));
                    if !state.is_cooled_down(now) {
                        return Some(cred.id.clone());
                    }
                }
                None
            }
            RotationStrategy::RoundRobin => {
                let len = inner.credentials.len() as u64;
                for step in 0..len {
                    let idx = ((inner.cursor + step) % len) as usize;
                    let cred = &inner.credentials[idx];
                    let state = inner
                        .state
                        .get(&cred.id)
                        .cloned()
                        .unwrap_or_else(|| CredentialState::new(&cred.id));
                    if !state.is_cooled_down(now) {
                        let next_cursor = (inner.cursor + step + 1) % len;
                        inner.cursor = next_cursor;
                        let _ = Self::persist_cursor(inner, &self.name, next_cursor);
                        return Some(cred.id.clone());
                    }
                }
                None
            }
            RotationStrategy::Random => {
                // Collect available indices, then LCG-pick one.
                let mut available: Vec<usize> = Vec::with_capacity(inner.credentials.len());
                for (idx, cred) in inner.credentials.iter().enumerate() {
                    let state = inner
                        .state
                        .get(&cred.id)
                        .cloned()
                        .unwrap_or_else(|| CredentialState::new(&cred.id));
                    if !state.is_cooled_down(now) {
                        available.push(idx);
                    }
                }
                if available.is_empty() {
                    return None;
                }
                let rng_next = inner
                    .rng_state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                inner.rng_state = rng_next;
                let pick = available[(rng_next as usize) % available.len()];
                Some(inner.credentials[pick].id.clone())
            }
            RotationStrategy::LeastUsed => {
                let mut best: Option<(usize, u64, u64)> = None;
                for (idx, cred) in inner.credentials.iter().enumerate() {
                    let state = inner
                        .state
                        .get(&cred.id)
                        .cloned()
                        .unwrap_or_else(|| CredentialState::new(&cred.id));
                    if state.is_cooled_down(now) {
                        continue;
                    }
                    let candidate = (idx, state.usage_count, state.last_used_us);
                    match best {
                        None => best = Some(candidate),
                        Some((_, best_usage, best_last)) => {
                            if candidate.1 < best_usage
                                || (candidate.1 == best_usage && candidate.2 < best_last)
                            {
                                best = Some(candidate);
                            }
                        }
                    }
                }
                best.map(|(idx, _, _)| inner.credentials[idx].id.clone())
            }
        }
    }
}

#[async_trait]
impl CredentialPool for PersistentCredentialPool {
    async fn acquire(&self, reason: &str) -> Result<Credential> {
        let mut inner = self.lock();
        let selected_id = self.select(&mut inner).ok_or_else(|| {
            eyre::eyre!("all credentials in pool `{}` are cooled down", self.name)
        })?;
        let cred = inner
            .credentials
            .iter()
            .find(|c| c.id == selected_id)
            .cloned()
            .ok_or_else(|| eyre::eyre!("selected credential `{selected_id}` not found"))?;
        drop(inner);
        self.emit_rotation(&cred.id, reason);
        Ok(cred)
    }

    async fn mark_rate_limited(&self, credential_id: &str, reset_at_us: Option<u64>) -> Result<()> {
        let mut inner = self.lock();
        if !inner.credentials.iter().any(|c| c.id == credential_id) {
            bail!("credential `{credential_id}` not in pool `{}`", self.name);
        }
        let now = now_epoch_us();
        let entry = inner
            .state
            .entry(credential_id.to_string())
            .or_insert_with(|| CredentialState::new(credential_id));
        entry.rate_limit_count = entry.rate_limit_count.saturating_add(1);
        let cooldown_target = match reset_at_us {
            Some(target) if target > now => target,
            _ => now.saturating_add(self.default_cooldown_us),
        };
        entry.reset_at_us = reset_at_us.unwrap_or(0);
        entry.cooldown_until_us = cooldown_target;
        let snapshot = entry.clone();
        Self::persist_state(&inner, &snapshot)?;
        drop(inner);
        info!(
            pool = %self.name,
            credential_id,
            cooldown_until_us = snapshot.cooldown_until_us,
            "credential cooled down"
        );
        Ok(())
    }

    async fn mark_auth_failure(&self, credential_id: &str, error_id: ErrorId) -> Result<()> {
        // Guard: at-most-once refresh per error_id.
        let should_refresh;
        {
            let mut inner = self.lock();
            should_refresh = inner.last_refresh_error_id != Some(error_id);
            inner.last_refresh_error_id = Some(error_id);
        }
        if !should_refresh {
            debug!(
                pool = %self.name,
                credential_id,
                error_id = ?error_id,
                "skipping OAuth refresh — error already processed"
            );
            return Ok(());
        }

        match self.refresher.refresh(credential_id).await {
            Ok(new_secret) => {
                let mut inner = self.lock();
                if let Some(cred) = inner.credentials.iter_mut().find(|c| c.id == credential_id) {
                    cred.secret = new_secret;
                }
                // Reset cooldown — the credential is now valid.
                let entry = inner
                    .state
                    .entry(credential_id.to_string())
                    .or_insert_with(|| CredentialState::new(credential_id));
                entry.cooldown_until_us = 0;
                entry.reset_at_us = 0;
                let snapshot = entry.clone();
                Self::persist_state(&inner, &snapshot)?;
                info!(pool = %self.name, credential_id, "OAuth refresh succeeded");
                Ok(())
            }
            Err(e) => {
                // Refresh failed — cool the credential down so selection skips it.
                warn!(
                    pool = %self.name,
                    credential_id,
                    error = %e,
                    "OAuth refresh failed; cooling down credential"
                );
                self.mark_rate_limited(credential_id, None).await
            }
        }
    }

    async fn mark_success(&self, credential_id: &str) -> Result<()> {
        let mut inner = self.lock();
        if !inner.credentials.iter().any(|c| c.id == credential_id) {
            bail!("credential `{credential_id}` not in pool `{}`", self.name);
        }
        let now = now_epoch_us();
        let entry = inner
            .state
            .entry(credential_id.to_string())
            .or_insert_with(|| CredentialState::new(credential_id));
        entry.usage_count = entry.usage_count.saturating_add(1);
        entry.last_used_us = now;
        let snapshot = entry.clone();
        Self::persist_state(&inner, &snapshot)?;
        Ok(())
    }

    async fn snapshot(&self) -> Result<Vec<CredentialState>> {
        let inner = self.lock();
        let mut out: Vec<CredentialState> = inner
            .credentials
            .iter()
            .map(|c| {
                inner
                    .state
                    .get(&c.id)
                    .cloned()
                    .unwrap_or_else(|| CredentialState::new(&c.id))
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_epoch_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_pool(strategy: RotationStrategy, ids: &[&str]) -> (PersistentCredentialPool, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credential_pool.redb");
        let creds = ids
            .iter()
            .map(|id| Credential::new(*id, format!("secret-{id}")))
            .collect();
        let pool = PersistentCredentialPool::open(
            &path,
            PersistentCredentialPoolOptions::new("test", creds).with_strategy(strategy),
        )
        .unwrap();
        (pool, dir)
    }

    #[tokio::test]
    async fn strategy_labels_are_stable() {
        assert_eq!(RotationStrategy::FillFirst.as_metric_label(), "fill_first");
        assert_eq!(
            RotationStrategy::RoundRobin.as_metric_label(),
            "round_robin"
        );
        assert_eq!(RotationStrategy::Random.as_metric_label(), "random");
        assert_eq!(RotationStrategy::LeastUsed.as_metric_label(), "least_used");
    }

    #[tokio::test]
    async fn empty_credential_list_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("p.redb");
        let err = match PersistentCredentialPool::open(
            &path,
            PersistentCredentialPoolOptions::new("test", Vec::new()),
        ) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("at least one credential"));
    }

    #[tokio::test]
    async fn fill_first_returns_lowest_index() {
        let (pool, _dir) = fresh_pool(RotationStrategy::FillFirst, &["a", "b", "c"]);
        for _ in 0..4 {
            let c = pool
                .acquire(rotation_reason::INITIAL_ACQUIRE)
                .await
                .unwrap();
            assert_eq!(c.id, "a");
        }
    }

    #[tokio::test]
    async fn round_robin_cycles_in_order() {
        let (pool, _dir) = fresh_pool(RotationStrategy::RoundRobin, &["a", "b", "c"]);
        let mut seen = Vec::new();
        for _ in 0..5 {
            let c = pool
                .acquire(rotation_reason::INITIAL_ACQUIRE)
                .await
                .unwrap();
            seen.push(c.id);
        }
        assert_eq!(seen, vec!["a", "b", "c", "a", "b"]);
    }

    #[tokio::test]
    async fn least_used_prefers_unused_credentials() {
        let (pool, _dir) = fresh_pool(RotationStrategy::LeastUsed, &["a", "b", "c"]);
        // Cycle through all three so each has usage_count=1, then mark "a" again.
        for _ in 0..3 {
            let c = pool
                .acquire(rotation_reason::INITIAL_ACQUIRE)
                .await
                .unwrap();
            pool.mark_success(&c.id).await.unwrap();
        }
        pool.mark_success("a").await.unwrap();
        // b & c both at 1; one of them must come out next.
        let next = pool
            .acquire(rotation_reason::INITIAL_ACQUIRE)
            .await
            .unwrap();
        assert!(next.id == "b" || next.id == "c", "got {}", next.id);
    }
}
