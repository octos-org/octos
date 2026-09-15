//! Redacted fingerprints of the provider-normalized, cache-relevant prompt.
//!
//! Provider APIs serialize the same [`octos_core::Message`] differently. A
//! cache invariant measured before that conversion can therefore be a false
//! positive. Providers build these manifests from their final request structs
//! after normalization, while deliberately excluding transport controls such
//! as `cache_control`, `prompt_cache_key`, and generation parameters. No
//! prompt text, tool schema, media bytes, or hidden reasoning is retained.
//!
//! Set `RUST_LOG=octos.prompt_cache=trace` to enable provider manifest
//! construction. Adding `OCTOS_PROMPT_CACHE_MANIFEST_JSONL=/path/to/log.jsonl`
//! writes the same redacted observations for offline soak analysis: one
//! `manifest` row per request, a `usage` row once provider usage is
//! correlated, and a standalone `usage_unmatched` row when usage arrives with
//! no manifest to enrich (TRACE disabled, or the manifest already evicted).
//! `OCTOS_PROMPT_CACHE_OBSERVER_CAPACITY` adjusts the retained in-process
//! event/stream bound (default 1024, hard-capped at 16384).

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{PromptCacheContext, TokenUsage, current_router_context};

const SCHEMA: &str = "octos.provider-cache-input-manifest.v1";
const OBSERVATION_SCHEMA: &str = "octos.provider-cache-observation.v1";
const DEFAULT_OBSERVER_CAPACITY: usize = 1_024;
const MAX_OBSERVER_CAPACITY: usize = 16_384;

tokio::task_local! {
    static CACHE_OBSERVATION_SCOPE: PromptCacheObservationScope;
}

#[derive(Clone, Debug)]
struct PromptCacheObservationScope {
    affinity_hash: String,
    epoch_hash: String,
    agent_iteration: u32,
    attempt: u32,
}

/// Run one provider attempt with its cache identity available to the final
/// provider serializer. OUP supplies real session/turn identity separately via
/// [`crate::RouterContext`]; this scope adds the redacted provider affinity and
/// distinguishes retry attempts. It intentionally does not invent a turn ID
/// for callers outside an OUP/router scope.
pub async fn with_prompt_cache_observation_context<F, T>(
    context: Option<&PromptCacheContext>,
    agent_iteration: u32,
    attempt: u32,
    future: F,
) -> T
where
    F: Future<Output = T>,
{
    let Some(context) = context else {
        return future.await;
    };
    CACHE_OBSERVATION_SCOPE
        .scope(
            PromptCacheObservationScope {
                affinity_hash: hash_identifier(&context.affinity_key),
                epoch_hash: hash_identifier(&context.epoch_id),
                agent_iteration,
                attempt,
            },
            future,
        )
        .await
}

/// Shared operator kill-switch for provider-specific prompt-cache controls.
/// Semantic ledger and compaction correctness remain active when this is
/// false; only optional wire affinity/breakpoint features are disabled.
pub(crate) fn prompt_cache_features_enabled_from(env_value: Option<&str>) -> bool {
    match env_value {
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => true,
    }
}

pub(crate) fn prompt_cache_features_enabled() -> bool {
    prompt_cache_features_enabled_from(std::env::var("OCTOS_PROMPT_CACHING").ok().as_deref())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheInputSegment {
    pub kind: String,
    pub hash: String,
    pub normalized_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheInputManifest {
    pub schema: String,
    pub provider: String,
    pub model: String,
    pub epoch_id: Option<String>,
    pub stable_prefix_hash: String,
    pub conversation_hash: String,
    pub input_hash: String,
    pub stable_segments: Vec<PromptCacheInputSegment>,
    pub conversation_segments: Vec<PromptCacheInputSegment>,
    pub normalized_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheInputComparison {
    pub compatible_route: bool,
    pub stable_prefix_matches: bool,
    pub conversation_prefix_segments: usize,
    pub reusable_normalized_bytes: usize,
    pub invalidation_reason: Option<String>,
}

/// Numeric provider usage correlated to one exact redacted request
/// observation. It deliberately omits response text and semantic-checkpoint
/// IDs because a third-party runtime could put arbitrary content in them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheObservedUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

/// One redacted runtime observation. The manifest is flattened so manifest
/// rows remain directly consumable by `prompt_cache_manifest_diff`; the
/// analyzer ignores the second, usage-enrichment row for the same sequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheObservation {
    pub observation_schema: String,
    pub event_kind: String,
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_sequence: Option<u64>,
    /// Hash of the complete correlation tuple (OUP session/turn when present,
    /// otherwise affinity, plus epoch/provider/model/attempt).
    pub request_key_hash: String,
    pub observed_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_iteration: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    pub relation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidation_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<PromptCacheInputComparison>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<PromptCacheObservedUsage>,
    #[serde(flatten)]
    pub manifest: PromptCacheInputManifest,
}

#[derive(Clone, Debug)]
struct CorrelationIdentity {
    stream_key: String,
    session_hash: Option<String>,
    turn_hash: Option<String>,
    affinity_hash: Option<String>,
    agent_iteration: Option<u32>,
    attempt: Option<u32>,
}

#[derive(Clone)]
struct LatestObservation {
    sequence: u64,
    manifest: PromptCacheInputManifest,
}

#[derive(Default)]
struct ObserverState {
    next_sequence: u64,
    latest_by_stream: HashMap<String, LatestObservation>,
    stream_lru: VecDeque<String>,
    observations: VecDeque<PromptCacheObservation>,
}

/// Bounded, concurrency-safe in-process observer for exact provider-normalized
/// prefixes. It keeps only redacted hashes, dimensions, token counts, and
/// routing identity. `capacity` bounds both retained observations and active
/// correlation streams.
pub struct PromptCacheObserver {
    capacity: usize,
    state: Mutex<ObserverState>,
    sink: Option<Mutex<File>>,
}

impl PromptCacheObserver {
    pub fn in_memory(capacity: usize) -> Self {
        Self::new(capacity, None)
    }

    /// Append observations to a redacted JSONL file. The file contains a
    /// `manifest` row and, when usage becomes available, a second `usage` row
    /// sharing the same sequence. Opening the sink is explicit and fallible;
    /// the production environment adapter logs a redacted warning on failure.
    pub fn with_jsonl_path(capacity: usize, path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self::new(capacity, Some(file)))
    }

    fn new(capacity: usize, sink: Option<File>) -> Self {
        Self {
            capacity: capacity.clamp(1, MAX_OBSERVER_CAPACITY),
            state: Mutex::new(ObserverState::default()),
            sink: sink.map(Mutex::new),
        }
    }

    /// Snapshot retained observations in emission order. Intended for
    /// diagnostics/tests; it never returns prompt bodies.
    pub fn snapshot(&self) -> Vec<PromptCacheObservation> {
        lock_unpoisoned(&self.state)
            .observations
            .iter()
            .cloned()
            .collect()
    }

    fn observe(
        &self,
        manifest: &PromptCacheInputManifest,
        identity: CorrelationIdentity,
    ) -> PromptCacheObservation {
        let redacted_manifest = manifest.redacted();
        // Relation chains are per lane: a hedge/failover lane alternating
        // inside one session must not read as a route change of its sibling,
        // nor cross-link `previous_sequence` across lanes.
        let stream_key = format!(
            "{}|provider:{}|model:{}",
            identity.stream_key, redacted_manifest.provider, redacted_manifest.model
        );
        let mut state = lock_unpoisoned(&self.state);
        let previous = state.latest_by_stream.get(&stream_key).cloned();
        let (relation, invalidation_reason, comparison) = previous
            .as_ref()
            .map(|previous| classify_transition(&previous.manifest, &redacted_manifest))
            .unwrap_or_else(|| ("initialized".to_owned(), None, None));

        state.next_sequence = state.next_sequence.wrapping_add(1).max(1);
        let sequence = state.next_sequence;
        let observation = PromptCacheObservation {
            observation_schema: OBSERVATION_SCHEMA.to_owned(),
            event_kind: "manifest".to_owned(),
            sequence,
            previous_sequence: previous.as_ref().map(|previous| previous.sequence),
            request_key_hash: request_key_hash(&identity, &redacted_manifest),
            observed_at_unix_ms: unix_time_ms(),
            session_hash: identity.session_hash,
            turn_hash: identity.turn_hash,
            affinity_hash: identity.affinity_hash,
            agent_iteration: identity.agent_iteration,
            attempt: identity.attempt,
            relation,
            invalidation_reason,
            comparison,
            usage: None,
            manifest: redacted_manifest.clone(),
        };

        state.latest_by_stream.insert(
            stream_key.clone(),
            LatestObservation {
                sequence,
                manifest: redacted_manifest.clone(),
            },
        );
        touch_lru(&mut state.stream_lru, &stream_key);
        while state.stream_lru.len() > self.capacity {
            if let Some(evicted) = state.stream_lru.pop_front() {
                state.latest_by_stream.remove(&evicted);
            }
        }
        push_bounded(&mut state.observations, observation.clone(), self.capacity);
        drop(state);
        self.write_jsonl(&observation);
        observation
    }

    /// Attach usage to the newest unenriched manifest with the same
    /// correlation tuple and route. When nothing matches (TRACE disabled, or
    /// the manifest already evicted under load) a standalone `usage_unmatched`
    /// row is retained and written instead, so the sink stays 1:1 auditable
    /// against provider billing rather than dropping the numbers silently.
    fn record_usage(
        &self,
        identity: &CorrelationIdentity,
        provider: &str,
        model: &str,
        epoch_id: Option<&str>,
        usage: &TokenUsage,
    ) -> PromptCacheObservation {
        let epoch_hash = epoch_id.map(hash_identifier);
        let observed_usage = PromptCacheObservedUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        };
        let mut state = lock_unpoisoned(&self.state);
        let position = state.observations.iter().rposition(|observation| {
            observation.event_kind == "manifest"
                && observation.session_hash == identity.session_hash
                && observation.turn_hash == identity.turn_hash
                && observation.affinity_hash == identity.affinity_hash
                && observation.agent_iteration == identity.agent_iteration
                && observation.attempt == identity.attempt
                && observation.manifest.provider == provider
                && observation.manifest.model == model
                && observation.manifest.epoch_id == epoch_hash
                && observation.usage.is_none()
        });
        let usage_observation = match position {
            Some(index) => {
                let matched = &mut state.observations[index];
                matched.usage = Some(observed_usage);
                let mut usage_observation = matched.clone();
                usage_observation.event_kind = "usage".to_owned();
                usage_observation
            }
            None => {
                let placeholder = PromptCacheInputManifest::from_normalized_segments(
                    provider,
                    model,
                    epoch_id,
                    Vec::new(),
                    Vec::new(),
                )
                .redacted();
                state.next_sequence = state.next_sequence.wrapping_add(1).max(1);
                let orphan = PromptCacheObservation {
                    observation_schema: OBSERVATION_SCHEMA.to_owned(),
                    event_kind: "usage_unmatched".to_owned(),
                    sequence: state.next_sequence,
                    previous_sequence: None,
                    request_key_hash: request_key_hash(identity, &placeholder),
                    observed_at_unix_ms: unix_time_ms(),
                    session_hash: identity.session_hash.clone(),
                    turn_hash: identity.turn_hash.clone(),
                    affinity_hash: identity.affinity_hash.clone(),
                    agent_iteration: identity.agent_iteration,
                    attempt: identity.attempt,
                    relation: "unmatched".to_owned(),
                    invalidation_reason: None,
                    comparison: None,
                    usage: Some(observed_usage),
                    manifest: placeholder,
                };
                push_bounded(&mut state.observations, orphan.clone(), self.capacity);
                orphan
            }
        };
        drop(state);
        self.write_jsonl(&usage_observation);
        usage_observation
    }

    fn write_jsonl(&self, observation: &PromptCacheObservation) {
        let Some(sink) = self.sink.as_ref() else {
            return;
        };
        let encoded = match serde_json::to_vec(observation) {
            Ok(encoded) => encoded,
            Err(error) => {
                tracing::warn!(
                    target: "octos.prompt_cache",
                    error = %error,
                    "could not serialize redacted prompt-cache observation"
                );
                return;
            }
        };
        let result = (|| -> io::Result<()> {
            let mut sink = lock_unpoisoned(sink);
            sink.write_all(&encoded)?;
            sink.write_all(b"\n")?;
            sink.flush()
        })();
        if let Err(error) = result {
            tracing::warn!(
                target: "octos.prompt_cache",
                error_kind = ?error.kind(),
                "could not append redacted prompt-cache observation"
            );
        }
    }
}

impl PromptCacheInputManifest {
    /// Construct a manifest from values produced by the provider's final
    /// serializer. Segment order must match the provider's logical cache-input
    /// order. Rolling cache markers are stripped before this function is
    /// called because moving a provider-approved breakpoint does not change
    /// the token prefix it points at.
    pub(crate) fn from_normalized_segments(
        provider: impl Into<String>,
        model: impl Into<String>,
        epoch_id: Option<&str>,
        stable: Vec<(String, Value)>,
        conversation: Vec<(String, Value)>,
    ) -> Self {
        let provider = provider.into();
        let model = model.into();
        let stable_segments = fingerprint_segments(stable);
        let conversation_segments = fingerprint_segments(conversation);
        let stable_prefix_hash = hash_segments(&stable_segments);
        let conversation_hash = hash_segments(&conversation_segments);
        let input_hash = hash_value(&json!({
            "schema": SCHEMA,
            "provider": provider,
            "model": model,
            "stable_prefix_hash": stable_prefix_hash,
            "conversation_hash": conversation_hash,
        }));
        let normalized_bytes = stable_segments
            .iter()
            .chain(conversation_segments.iter())
            .map(|segment| segment.normalized_bytes)
            .sum();

        Self {
            schema: SCHEMA.to_owned(),
            provider,
            model,
            epoch_id: epoch_id.map(str::to_owned),
            stable_prefix_hash,
            conversation_hash,
            input_hash,
            stable_segments,
            conversation_segments,
            normalized_bytes,
        }
    }

    /// Compare exact normalized segments. This is also the core of the
    /// offline analyzer used on redacted soak logs: no prompt bodies are
    /// needed to prove the longest reusable prefix.
    pub fn compare_prefix(&self, next: &Self) -> PromptCacheInputComparison {
        if self.schema != next.schema {
            return PromptCacheInputComparison {
                compatible_route: false,
                stable_prefix_matches: false,
                conversation_prefix_segments: 0,
                reusable_normalized_bytes: 0,
                invalidation_reason: Some("provider_serializer_changed".to_owned()),
            };
        }
        if self.provider != next.provider || self.model != next.model {
            return PromptCacheInputComparison {
                compatible_route: false,
                stable_prefix_matches: false,
                conversation_prefix_segments: 0,
                reusable_normalized_bytes: 0,
                invalidation_reason: Some("model_route_changed".to_owned()),
            };
        }
        if self.stable_segments != next.stable_segments {
            return PromptCacheInputComparison {
                compatible_route: true,
                stable_prefix_matches: false,
                conversation_prefix_segments: 0,
                reusable_normalized_bytes: 0,
                invalidation_reason: Some("stable_prefix_changed".to_owned()),
            };
        }
        if self.epoch_id != next.epoch_id {
            return PromptCacheInputComparison {
                compatible_route: true,
                stable_prefix_matches: true,
                conversation_prefix_segments: 0,
                reusable_normalized_bytes: 0,
                invalidation_reason: Some("cache_epoch_changed".to_owned()),
            };
        }

        let conversation_prefix_segments = self
            .conversation_segments
            .iter()
            .zip(&next.conversation_segments)
            .take_while(|(left, right)| left == right)
            .count();
        let reusable_normalized_bytes = self
            .stable_segments
            .iter()
            .map(|segment| segment.normalized_bytes)
            .sum::<usize>()
            + self
                .conversation_segments
                .iter()
                .take(conversation_prefix_segments)
                .map(|segment| segment.normalized_bytes)
                .sum::<usize>();
        let invalidation_reason = (conversation_prefix_segments
            < self
                .conversation_segments
                .len()
                .min(next.conversation_segments.len()))
        .then_some("old_history_changed".to_owned());

        PromptCacheInputComparison {
            compatible_route: true,
            stable_prefix_matches: true,
            conversation_prefix_segments,
            reusable_normalized_bytes,
            invalidation_reason,
        }
    }

    fn redacted(&self) -> Self {
        let mut redacted = self.clone();
        redacted.epoch_id = redacted.epoch_id.as_deref().map(hash_identifier);
        redacted
    }

    /// Observer entry point for a provider's final request shape. Providers
    /// call this only while the `octos.prompt_cache` target is enabled at
    /// TRACE — see [`record_prompt_cache_usage`] for the correlation contract
    /// that follows from that (deliberate) gating.
    pub(crate) fn trace(&self) {
        let observation =
            global_observer().observe(self, current_correlation(self.epoch_id.as_deref()));
        tracing::trace!(
            target: "octos.prompt_cache",
            provider = %self.provider,
            model = %self.model,
            epoch_id = ?observation.manifest.epoch_id,
            sequence = observation.sequence,
            relation = %observation.relation,
            invalidation_reason = ?observation.invalidation_reason,
            manifest = %serde_json::to_string(&observation).unwrap_or_else(|_| "{\"error\":\"manifest_serialization_failed\"}".to_owned()),
            "provider-normalized prompt cache input manifest"
        );
    }
}

/// Attach cache-read/write usage to the most recent provider-normalized
/// manifest for this OUP turn and provider route.
///
/// Correlation contract: manifests exist only while the `octos.prompt_cache`
/// tracing target is enabled at TRACE (providers gate manifest construction on
/// it), so a `usage` enrichment row can only be produced under that target.
/// Outside it — or when the manifest was already evicted under load — the
/// observer records a standalone `usage_unmatched` row instead of dropping the
/// numbers, keeping the sink 1:1 auditable. The gating is deliberate (manifest
/// hashing is skipped entirely at lower levels); do not rewire it here.
pub fn record_prompt_cache_usage(
    context: Option<&PromptCacheContext>,
    provider: &str,
    model: &str,
    agent_iteration: u32,
    attempt: u32,
    usage: &TokenUsage,
) {
    let identity = current_correlation(context.map(|context| context.epoch_id.as_str()));
    let identity = CorrelationIdentity {
        agent_iteration: Some(agent_iteration),
        attempt: Some(attempt),
        affinity_hash: context.map(|context| hash_identifier(&context.affinity_key)),
        ..identity
    };
    let observation = global_observer().record_usage(
        &identity,
        provider,
        model,
        context.map(|context| context.epoch_id.as_str()),
        usage,
    );
    if observation.event_kind == "usage" {
        tracing::trace!(
            target: "octos.prompt_cache",
            provider,
            model,
            sequence = observation.sequence,
            epoch_id = ?observation.manifest.epoch_id,
            cache_read_tokens = usage.cache_read_tokens,
            cache_write_tokens = usage.cache_write_tokens,
            "provider cache usage correlated to prompt manifest"
        );
    } else {
        tracing::trace!(
            target: "octos.prompt_cache",
            provider,
            model,
            sequence = observation.sequence,
            "provider cache usage had no matching normalized manifest; recorded as usage_unmatched"
        );
    }
}

fn request_key_hash(identity: &CorrelationIdentity, manifest: &PromptCacheInputManifest) -> String {
    hash_value(&json!({
        "schema": "octos.prompt-cache-observation-key.v1",
        "session_hash": identity.session_hash,
        "turn_hash": identity.turn_hash,
        "affinity_hash": identity.affinity_hash,
        "epoch_hash": manifest.epoch_id,
        "provider": manifest.provider,
        "model": manifest.model,
        "agent_iteration": identity.agent_iteration,
        "attempt": identity.attempt,
    }))
}

fn global_observer() -> &'static PromptCacheObserver {
    static OBSERVER: OnceLock<PromptCacheObserver> = OnceLock::new();
    OBSERVER.get_or_init(|| {
        let capacity = std::env::var("OCTOS_PROMPT_CACHE_OBSERVER_CAPACITY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_OBSERVER_CAPACITY)
            .clamp(1, MAX_OBSERVER_CAPACITY);
        let Some(path) = std::env::var_os("OCTOS_PROMPT_CACHE_MANIFEST_JSONL") else {
            return PromptCacheObserver::in_memory(capacity);
        };
        PromptCacheObserver::with_jsonl_path(capacity, path).unwrap_or_else(|error| {
            tracing::warn!(
                target: "octos.prompt_cache",
                error_kind = ?error.kind(),
                "could not open redacted prompt-cache JSONL sink"
            );
            PromptCacheObserver::in_memory(capacity)
        })
    })
}

fn current_correlation(epoch_id: Option<&str>) -> CorrelationIdentity {
    let router = current_router_context();
    let scope = CACHE_OBSERVATION_SCOPE.try_with(Clone::clone).ok();
    let session_hash = router.session_id.as_deref().map(hash_identifier);
    let turn_hash = router.turn_id.as_deref().map(hash_identifier);
    let affinity_hash = scope.as_ref().map(|scope| scope.affinity_hash.clone());
    let epoch_hash = epoch_id
        .map(hash_identifier)
        .or_else(|| scope.as_ref().map(|scope| scope.epoch_hash.clone()))
        .unwrap_or_else(|| "none".to_owned());
    let stream_key = if let Some(session_hash) = session_hash.as_ref() {
        format!("session:{session_hash}")
    } else if let Some(affinity_hash) = affinity_hash.as_ref() {
        format!("affinity:{affinity_hash}")
    } else {
        // No OUP/router or agent affinity scope is available. This fallback
        // remains redacted and route-local, but deliberately does not claim a
        // session or turn identity.
        format!("unattributed:{epoch_hash}")
    };
    CorrelationIdentity {
        stream_key,
        session_hash,
        turn_hash,
        affinity_hash,
        agent_iteration: scope.as_ref().map(|scope| scope.agent_iteration),
        attempt: scope.as_ref().map(|scope| scope.attempt),
    }
}

fn classify_transition(
    previous: &PromptCacheInputManifest,
    current: &PromptCacheInputManifest,
) -> (String, Option<String>, Option<PromptCacheInputComparison>) {
    let comparison = previous.compare_prefix(current);
    if previous.schema != current.schema {
        return (
            "serializer_changed".to_owned(),
            Some("provider_serializer_changed".to_owned()),
            Some(comparison),
        );
    }
    if previous.provider != current.provider || previous.model != current.model {
        return (
            "route_changed".to_owned(),
            Some("model_route_changed".to_owned()),
            Some(comparison),
        );
    }
    if previous.stable_segments != current.stable_segments {
        let reason = classify_stable_change(previous, current).to_owned();
        return (
            "stable_prefix_changed".to_owned(),
            Some(reason),
            Some(comparison),
        );
    }
    if previous.epoch_id != current.epoch_id {
        return (
            "epoch_rotated".to_owned(),
            Some("cache_epoch_changed".to_owned()),
            Some(comparison),
        );
    }

    let shared = common_conversation_segments(previous, current);
    if shared
        < previous
            .conversation_segments
            .len()
            .min(current.conversation_segments.len())
    {
        return (
            "old_history_changed".to_owned(),
            Some("old_history_changed".to_owned()),
            Some(comparison),
        );
    }
    if previous.conversation_segments == current.conversation_segments {
        return ("exact_retry".to_owned(), None, Some(comparison));
    }
    if shared == previous.conversation_segments.len() {
        return ("append_only".to_owned(), None, Some(comparison));
    }
    if shared == current.conversation_segments.len() {
        return ("suffix_truncated".to_owned(), None, Some(comparison));
    }
    (
        "old_history_changed".to_owned(),
        Some("old_history_changed".to_owned()),
        Some(comparison),
    )
}

fn classify_stable_change(
    previous: &PromptCacheInputManifest,
    current: &PromptCacheInputManifest,
) -> &'static str {
    let differing_kinds = previous
        .stable_segments
        .iter()
        .map(|segment| segment.kind.as_str())
        .zip(
            current
                .stable_segments
                .iter()
                .map(|segment| segment.kind.as_str()),
        )
        .filter_map(|(left, right)| (left != right).then_some((left, right)))
        .flat_map(|(left, right)| [left, right])
        .chain(
            previous
                .stable_segments
                .iter()
                .skip(current.stable_segments.len())
                .map(|segment| segment.kind.as_str()),
        )
        .chain(
            current
                .stable_segments
                .iter()
                .skip(previous.stable_segments.len())
                .map(|segment| segment.kind.as_str()),
        )
        .collect::<Vec<_>>();

    // Hash differences at equal positions matter too, so classify from all
    // changed positions rather than only kind-list changes.
    let changed_at_equal_position = previous
        .stable_segments
        .iter()
        .zip(&current.stable_segments)
        .filter(|(left, right)| left != right)
        .flat_map(|(left, right)| [left.kind.as_str(), right.kind.as_str()]);
    let kinds = differing_kinds
        .into_iter()
        .chain(changed_at_equal_position)
        .collect::<Vec<_>>();
    if !kinds.is_empty() && kinds.iter().all(|kind| kind.starts_with("tool:")) {
        "tool_schema_changed"
    } else if !kinds.is_empty()
        && kinds.iter().all(|kind| {
            kind.contains("system") || kind.contains("developer") || kind == &"system_instruction"
        })
    {
        "stable_instructions_changed"
    } else {
        "stable_prefix_changed"
    }
}

fn common_conversation_segments(
    previous: &PromptCacheInputManifest,
    current: &PromptCacheInputManifest,
) -> usize {
    previous
        .conversation_segments
        .iter()
        .zip(&current.conversation_segments)
        .take_while(|(left, right)| left == right)
        .count()
}

fn touch_lru(lru: &mut VecDeque<String>, key: &str) {
    if let Some(index) = lru.iter().position(|candidate| candidate == key) {
        lru.remove(index);
    }
    lru.push_back(key.to_owned());
}

fn push_bounded<T>(values: &mut VecDeque<T>, value: T, capacity: usize) {
    if values.len() == capacity {
        values.pop_front();
    }
    values.push_back(value);
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Per-process random salt mixed into identifier hashes. Correlation only
/// needs intra-process stability, while an unsalted SHA-256 of a low-entropy
/// identifier (e.g. `telegram:<numeric chat id>`) would be enumerable offline
/// from the JSONL sink.
fn process_salt() -> &'static str {
    static SALT: OnceLock<String> = OnceLock::new();
    SALT.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        // `RandomState` is keyed from OS entropy per process/thread; two fresh
        // instances give distinct keyed outputs without a new dependency.
        let draw = || {
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        };
        format!("{:016x}{:016x}", draw(), draw())
    })
}

fn hash_identifier(value: &str) -> String {
    hash_value(&json!({
        "schema": "octos.prompt-cache-observer-identifier.v2",
        "salt": process_salt(),
        "value": value,
    }))
}

/// Remove provider transport markers that are allowed to roll forward without
/// changing the cacheable token content. The returned value is a clone so the
/// actual outbound request is never mutated.
///
/// Marker keys are stripped everywhere. The marker-induced wrapper collapse
/// ([`collapse_marker_wrapper`]) is applied only where a provider rewrites a
/// plain string into a one-element text block to carry a marker: each
/// `messages[*].content` and the top-level `system` block array. A literal
/// one-element text array anywhere else (tool schemas, tool inputs) is real
/// request content and keeps its exact wire shape.
pub(crate) fn without_cache_markers(mut value: Value) -> Value {
    fn strip_markers(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("cache_control");
                map.remove("prompt_cache_key");
                map.remove("prompt_cache_retention");
                map.remove("cachedContent");
                for child in map.values_mut() {
                    strip_markers(child);
                }
            }
            Value::Array(values) => {
                for child in values.iter_mut() {
                    strip_markers(child);
                }
            }
            _ => {}
        }
    }
    strip_markers(&mut value);
    if let Some(system) = value.get_mut("system") {
        collapse_marker_wrapper(system);
    }
    if let Some(messages) = value.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut() {
            if let Some(content) = message.get_mut("content") {
                collapse_marker_wrapper(content);
            }
        }
    }
    value
}

/// Anthropic can only attach `cache_control` to a content block. It therefore
/// rewrites a plain string into a one-element text-block array while that
/// message is the rolling checkpoint, then returns it to string form when the
/// checkpoint advances. Both forms tokenize identically. Once the marker is
/// removed, collapse that marker-induced wrapper so an append is not
/// misclassified as an old-history edit.
fn collapse_marker_wrapper(value: &mut Value) {
    let collapsed_text = value
        .as_array()
        .filter(|values| values.len() == 1)
        .and_then(|values| values.first())
        .and_then(Value::as_object)
        .filter(|block| {
            block.len() == 2 && block.get("type").and_then(Value::as_str) == Some("text")
        })
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(text) = collapsed_text {
        *value = Value::String(text);
    }
}

fn fingerprint_segments(values: Vec<(String, Value)>) -> Vec<PromptCacheInputSegment> {
    values
        .into_iter()
        .map(|(kind, value)| {
            let bytes = serde_json::to_vec(&value).unwrap_or_default();
            PromptCacheInputSegment {
                kind,
                hash: hash_bytes(&bytes),
                normalized_bytes: bytes.len(),
            }
        })
        .collect()
}

fn hash_segments(segments: &[PromptCacheInputSegment]) -> String {
    hash_value(&json!(
        segments
            .iter()
            .map(|segment| (&segment.kind, &segment.hash))
            .collect::<Vec<_>>()
    ))
}

fn hash_value(value: &Value) -> String {
    hash_bytes(&serde_json::to_vec(value).unwrap_or_default())
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str) -> CorrelationIdentity {
        CorrelationIdentity {
            stream_key: format!("stream:{name}"),
            session_hash: Some(hash_identifier(&format!("session:{name}"))),
            turn_hash: Some(hash_identifier(&format!("turn:{name}"))),
            affinity_hash: Some(hash_identifier(&format!("affinity:{name}"))),
            agent_iteration: Some(3),
            attempt: Some(0),
        }
    }

    fn manifest(conversation: &[&str]) -> PromptCacheInputManifest {
        manifest_for("provider", "model", conversation)
    }

    fn manifest_for(
        provider: &str,
        model: &str,
        conversation: &[&str],
    ) -> PromptCacheInputManifest {
        PromptCacheInputManifest::from_normalized_segments(
            provider,
            model,
            Some("epoch"),
            vec![("system".to_owned(), json!({"text": "secret system"}))],
            conversation
                .iter()
                .enumerate()
                .map(|(index, text)| (format!("message:{index}"), json!({"text": text})))
                .collect(),
        )
    }

    #[test]
    fn should_attach_usage_to_the_serving_lane_manifest_when_lanes_alternate_after_failover() {
        // Lane A emits a manifest, fails over to lane B which serves the
        // request. Usage reported with lane B's (correctly forwarded)
        // metadata must enrich B's manifest, never A's.
        let observer = PromptCacheObserver::in_memory(8);
        let identity = identity("shared-session");
        let a_event = observer.observe(
            &manifest_for("lane-a", "model-a", &["prompt"]),
            identity.clone(),
        );
        let b_event = observer.observe(
            &manifest_for("lane-b", "model-b", &["prompt"]),
            identity.clone(),
        );
        let usage = TokenUsage {
            input_tokens: 10,
            cache_read_tokens: 77,
            ..TokenUsage::default()
        };

        let attached = observer.record_usage(&identity, "lane-b", "model-b", Some("epoch"), &usage);

        assert_eq!(attached.event_kind, "usage");
        assert_eq!(attached.sequence, b_event.sequence);
        assert_eq!(attached.manifest.provider, "lane-b");
        assert_eq!(attached.usage.as_ref().unwrap().cache_read_tokens, 77);
        let snapshot = observer.snapshot();
        let a_row = snapshot
            .iter()
            .find(|observation| observation.sequence == a_event.sequence)
            .unwrap();
        assert!(
            a_row.usage.is_none(),
            "lane A's manifest must not absorb lane B's usage"
        );
    }

    #[test]
    fn should_only_collapse_marker_wrappers_at_message_content_and_system_positions() {
        // A literal one-element text array inside a tool schema or a tool_use
        // input is real request content: two byte-different wire shapes must
        // not hash-equal.
        let schema_array = without_cache_markers(json!({
            "tools": [{"name": "t", "input_schema": {"examples": [{"type": "text", "text": "x"}]}}]
        }));
        let schema_string = without_cache_markers(json!({
            "tools": [{"name": "t", "input_schema": {"examples": "x"}}]
        }));
        assert_ne!(schema_array, schema_string);

        let input_array = without_cache_markers(json!({
            "messages": [{"role": "assistant", "content": [{"type": "tool_use", "id": "1",
                "name": "t", "input": {"blocks": [{"type": "text", "text": "x"}]}}]}]
        }));
        let input_string = without_cache_markers(json!({
            "messages": [{"role": "assistant", "content": [{"type": "tool_use", "id": "1",
                "name": "t", "input": {"blocks": "x"}}]}]
        }));
        assert_ne!(input_array, input_string);

        // The marker-induced wrapper at the message-content position and the
        // top-level system block array still collapse to their string forms.
        let marked = without_cache_markers(json!({
            "system": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
            ]}]
        }));
        assert_eq!(
            marked,
            json!({"system": "sys", "messages": [{"role": "user", "content": "hi"}]})
        );
    }

    #[test]
    fn should_chain_each_lane_separately_when_lanes_alternate_within_one_session() {
        let observer = PromptCacheObserver::in_memory(8);
        let identity = identity("failover-session");
        let a_first = observer.observe(
            &manifest_for("lane-a", "model-a", &["one"]),
            identity.clone(),
        );
        let b = observer.observe(
            &manifest_for("lane-b", "model-b", &["one"]),
            identity.clone(),
        );
        let a_next = observer.observe(
            &manifest_for("lane-a", "model-a", &["one", "two"]),
            identity.clone(),
        );

        assert_eq!(a_first.relation, "initialized");
        assert_eq!(
            b.relation, "initialized",
            "a hedge/failover lane inside one session is not a route change of lane A: {b:?}"
        );
        assert_eq!(b.previous_sequence, None);
        assert_eq!(
            a_next.relation, "append_only",
            "lane A must chain to its own previous manifest, not lane B's: {a_next:?}"
        );
        assert_eq!(a_next.previous_sequence, Some(a_first.sequence));
        assert_eq!(a_next.invalidation_reason, None);
    }

    #[test]
    fn should_emit_unmatched_usage_record_when_no_manifest_matches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("orphan.jsonl");
        let observer = PromptCacheObserver::with_jsonl_path(4, &path).unwrap();
        let identity = identity("usage-only-session");
        let usage = TokenUsage {
            input_tokens: 21,
            cache_read_tokens: 13,
            ..TokenUsage::default()
        };

        let orphan = observer.record_usage(&identity, "lane-a", "model-a", Some("epoch"), &usage);

        assert_eq!(orphan.event_kind, "usage_unmatched");
        assert_eq!(orphan.usage.as_ref().unwrap().cache_read_tokens, 13);
        assert_eq!(orphan.manifest.provider, "lane-a");
        assert_eq!(orphan.manifest.model, "model-a");
        assert_eq!(orphan.previous_sequence, None);
        assert!(observer.snapshot().iter().any(|observation| {
            observation.sequence == orphan.sequence && observation.event_kind == "usage_unmatched"
        }));
        drop(observer);

        let contents = std::fs::read_to_string(path).unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{contents}");
        let row: PromptCacheObservation = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(row.event_kind, "usage_unmatched");
        assert!(!contents.contains("usage-only-session"));
        assert!(!contents.contains("epoch\""));
    }

    #[test]
    fn should_salt_identifier_hashes_per_process_while_staying_stable_within_it() {
        let identifier = "telegram:123456789";
        let first = hash_identifier(identifier);
        assert_eq!(
            first,
            hash_identifier(identifier),
            "correlation needs intra-process stability"
        );
        assert!(first.starts_with("sha256:"));
        // The unsalted v1 construction is enumerable offline for low-entropy
        // channel identifiers; the salted hash must not reproduce it.
        let unsalted_v1 = hash_value(&json!({
            "schema": "octos.prompt-cache-observer-identifier.v1",
            "value": identifier,
        }));
        assert_ne!(
            first, unsalted_v1,
            "identifier hashes must mix a per-process salt"
        );
        assert_ne!(first, hash_identifier("telegram:123456780"));
    }

    #[test]
    fn redacted_manifest_proves_append_only_prefix_without_prompt_text() {
        let first = manifest(&["first secret"]);
        let second = manifest(&["first secret", "second secret"]);
        let comparison = first.compare_prefix(&second);

        assert!(comparison.compatible_route);
        assert!(comparison.stable_prefix_matches);
        assert_eq!(comparison.conversation_prefix_segments, 1);
        assert!(comparison.reusable_normalized_bytes > 0);
        assert_eq!(comparison.invalidation_reason, None);

        let encoded = serde_json::to_string(&first).unwrap();
        assert!(!encoded.contains("secret system"));
        assert!(!encoded.contains("first secret"));
    }

    #[test]
    fn cache_control_marker_is_not_part_of_exact_token_prefix() {
        let with_marker = without_cache_markers(json!({
            "type": "text",
            "text": "same",
            "cache_control": {"type": "ephemeral"}
        }));
        let without_marker = without_cache_markers(json!({"type": "text", "text": "same"}));
        assert_eq!(with_marker, without_marker);
    }

    #[test]
    fn provider_cache_kill_switch_does_not_depend_on_process_environment() {
        assert!(prompt_cache_features_enabled_from(None));
        assert!(prompt_cache_features_enabled_from(Some("on")));
        for disabled in ["0", " false ", "OFF", "No"] {
            assert!(!prompt_cache_features_enabled_from(Some(disabled)));
        }
    }

    #[test]
    fn observer_classifies_all_changes_available_in_normalized_manifests() {
        let base = manifest(&["one", "two"]);

        let mut route = base.clone();
        route.model = "other-model".to_owned();
        assert_eq!(
            classify_transition(&base, &route).1.as_deref(),
            Some("model_route_changed")
        );

        let mut serializer = base.clone();
        serializer.schema = "octos.provider-cache-input-manifest.v2".to_owned();
        assert_eq!(
            classify_transition(&base, &serializer).1.as_deref(),
            Some("provider_serializer_changed")
        );

        let mut epoch = base.clone();
        epoch.epoch_id = Some("other-epoch".to_owned());
        assert_eq!(
            classify_transition(&base, &epoch).1.as_deref(),
            Some("cache_epoch_changed")
        );

        let mut instructions = base.clone();
        instructions.stable_segments[0].hash = "sha256:changed".to_owned();
        assert_eq!(
            classify_transition(&base, &instructions).1.as_deref(),
            Some("stable_instructions_changed")
        );

        let mut tool_base = base.clone();
        tool_base.stable_segments = vec![PromptCacheInputSegment {
            kind: "tool:0".to_owned(),
            hash: "sha256:old".to_owned(),
            normalized_bytes: 10,
        }];
        let mut tool_changed = tool_base.clone();
        tool_changed.stable_segments[0].hash = "sha256:new".to_owned();
        assert_eq!(
            classify_transition(&tool_base, &tool_changed).1.as_deref(),
            Some("tool_schema_changed")
        );

        let edited = manifest(&["edited", "two"]);
        assert_eq!(
            classify_transition(&base, &edited).1.as_deref(),
            Some("old_history_changed")
        );
        assert_eq!(
            classify_transition(&base, &manifest(&["one", "two", "three"])).0,
            "append_only"
        );
        assert_eq!(
            classify_transition(&base, &manifest(&["one"])).0,
            "suffix_truncated"
        );
        assert_eq!(classify_transition(&base, &base).0, "exact_retry");
    }

    #[test]
    fn observer_is_bounded_isolates_streams_and_never_retains_prompt_or_identity() {
        let observer = PromptCacheObserver::in_memory(2);
        let alpha_first = manifest(&["ALPHA_SECRET"]);
        let alpha_next = manifest(&["ALPHA_SECRET", "NEXT_SECRET"]);
        let beta = manifest(&["BETA_SECRET"]);

        observer.observe(&alpha_first, identity("alpha-private"));
        let beta_event = observer.observe(&beta, identity("beta-private"));
        let alpha_event = observer.observe(&alpha_next, identity("alpha-private"));

        assert_eq!(beta_event.relation, "initialized");
        assert_eq!(alpha_event.relation, "append_only");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.len(), 2);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        for secret in [
            "ALPHA_SECRET",
            "NEXT_SECRET",
            "BETA_SECRET",
            "alpha-private",
            "beta-private",
        ] {
            assert!(!encoded.contains(secret));
        }
    }

    #[test]
    fn observer_serializes_concurrent_writers_and_keeps_a_bounded_window() {
        let observer = std::sync::Arc::new(PromptCacheObserver::in_memory(32));
        let workers = (0..8)
            .map(|worker| {
                let observer = observer.clone();
                std::thread::spawn(move || {
                    for request in 0..20 {
                        let text = format!("worker-{worker}-request-{request}");
                        observer
                            .observe(&manifest(&[&text]), identity(&format!("worker-{worker}")));
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }

        let snapshot = observer.snapshot();
        assert_eq!(snapshot.len(), 32);
        assert!(
            snapshot
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
        );
        assert_eq!(
            snapshot
                .iter()
                .map(|observation| observation.sequence)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            snapshot.len()
        );
    }

    #[test]
    fn jsonl_sink_is_analyzer_compatible_and_correlates_numeric_usage() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.jsonl");
        let observer = PromptCacheObserver::with_jsonl_path(8, &path).unwrap();
        let identity = identity("private-session");
        let manifest = manifest(&["PROMPT_SECRET"]);
        let event = observer.observe(&manifest, identity.clone());
        let usage = TokenUsage {
            input_tokens: 13,
            output_tokens: 5,
            cache_read_tokens: 8,
            cache_write_tokens: 3,
            ..TokenUsage::default()
        };
        let usage_event = observer.record_usage(
            &identity,
            &manifest.provider,
            &manifest.model,
            manifest.epoch_id.as_deref(),
            &usage,
        );
        assert_eq!(usage_event.sequence, event.sequence);
        assert_eq!(usage_event.event_kind, "usage");
        assert_eq!(usage_event.usage.unwrap().cache_read_tokens, 8);
        drop(observer);

        let contents = std::fs::read_to_string(path).unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        let raw_manifest: PromptCacheInputManifest = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(raw_manifest.input_hash, manifest.input_hash);
        let enriched: PromptCacheObservation = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(enriched.event_kind, "usage");
        assert!(!contents.contains("PROMPT_SECRET"));
        assert!(!contents.contains("private-session"));
        assert!(!contents.contains("epoch\""));
    }

    #[tokio::test]
    async fn correlation_uses_hashed_oup_ids_and_scoped_affinity() {
        let context = PromptCacheContext {
            affinity_key: "octos-safe-affinity".to_owned(),
            epoch_id: "epoch-private".to_owned(),
            stable_prefix_hash: "sha256:stable".to_owned(),
            semantic_boundaries: Vec::new(),
        };
        let correlation = crate::with_router_context(
            crate::RouterContext {
                session_id: Some("raw-session".to_owned()),
                turn_id: Some("raw-turn".to_owned()),
            },
            with_prompt_cache_observation_context(Some(&context), 7, 2, async {
                current_correlation(Some("epoch-private"))
            }),
        )
        .await;

        assert_eq!(correlation.agent_iteration, Some(7));
        assert_eq!(correlation.attempt, Some(2));
        for value in [
            correlation.session_hash.as_deref().unwrap(),
            correlation.turn_hash.as_deref().unwrap(),
            correlation.affinity_hash.as_deref().unwrap(),
        ] {
            assert!(value.starts_with("sha256:"));
            assert!(!value.contains("raw-"));
            assert!(!value.contains("octos-safe-affinity"));
        }
    }
}
