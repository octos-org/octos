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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{PromptCacheContext, TokenUsage, current_router_context};

const SCHEMA: &str = "octos.provider-cache-input-manifest.v2";
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
        let input_hash = hash_value(json!({
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
    hash_value(json!({
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
    hash_value(json!({
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

/// Return `value` with object keys sorted deeply. `serde_json::to_vec` emits
/// keys in the Map backend's order — sorted (BTreeMap) without the
/// `preserve_order` feature, insertion order (IndexMap) with it — so hashing
/// the raw serialization would fingerprint the same logical input
/// differently depending on the build graph's feature unification. Sorting
/// the keys pins the bytes to the content; reordering preserves byte length,
/// so segment byte accounting is unchanged.
///
/// Exported for sibling prompt-cache hashing that must pin the same bytes to
/// the same content (octos-agent's fingerprint identities hash foreign JSON
/// through this before digesting).
pub fn canonical_object_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map
                .into_iter()
                .map(|(key, item)| (key, canonical_object_keys(item)))
                .collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical_object_keys).collect()),
        other => other,
    }
}

fn fingerprint_segments(values: Vec<(String, Value)>) -> Vec<PromptCacheInputSegment> {
    values
        .into_iter()
        .map(|(kind, value)| {
            let bytes = serde_json::to_vec(&canonical_object_keys(value)).unwrap_or_default();
            PromptCacheInputSegment {
                kind,
                hash: hash_bytes(&bytes),
                normalized_bytes: bytes.len(),
            }
        })
        .collect()
}

fn hash_segments(segments: &[PromptCacheInputSegment]) -> String {
    hash_value(json!(
        segments
            .iter()
            .map(|segment| (&segment.kind, &segment.hash))
            .collect::<Vec<_>>()
    ))
}

fn hash_value(value: Value) -> String {
    hash_bytes(&serde_json::to_vec(&canonical_object_keys(value)).unwrap_or_default())
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Immutable shared ownership for a registry's complete RFC-0 tool list.
/// Keeping the Vec private is a cache-safety invariant: even the unique Arc
/// owner cannot mutate the slice while a weak artifact-cache entry names it.
#[derive(Debug)]
pub struct SharedToolSpecs {
    specs: Vec<crate::types::ToolSpec>,
}

impl SharedToolSpecs {
    pub fn new(specs: Vec<crate::types::ToolSpec>) -> Self {
        Self { specs }
    }

    pub fn as_slice(&self) -> &[crate::types::ToolSpec] {
        &self.specs
    }
}

impl std::ops::Deref for SharedToolSpecs {
    type Target = [crate::types::ToolSpec];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

/// Precomputed, redacted fingerprint data for one registry-owned tool list.
/// The cache contains no schema text; callers still retain and send every
/// original `ToolSpec` in RFC-0 order on every request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSpecCacheSegment {
    pub kind: String,
    pub hash: String,
    pub estimated_tokens: usize,
}

#[derive(Clone, Debug)]
pub struct ToolSpecCacheArtifacts {
    pub segments: Arc<Vec<ToolSpecCacheSegment>>,
    pub estimated_context_tokens: u32,
}

struct ToolSpecCacheEntry {
    key: (usize, usize),
    specs: Weak<SharedToolSpecs>,
    artifacts: Arc<ToolSpecCacheArtifacts>,
}

// A small process-wide LRU is shared by registries rather than keyed by
// session. Weak entries retain no schemas after the last live snapshot drops;
// SharedToolSpecs' private Vec prevents mutation while an entry names it. The
// 32-entry bound prevents unbounded metadata retention and pointer-reuse ABA
// is rejected by a failed Weak upgrade or an exact slice-identity check.
const TOOL_SPEC_CACHE_CAPACITY: usize = 32;
static TOOL_SPEC_CACHE: OnceLock<Mutex<VecDeque<ToolSpecCacheEntry>>> = OnceLock::new();

fn tool_spec_cache() -> &'static Mutex<VecDeque<ToolSpecCacheEntry>> {
    TOOL_SPEC_CACHE.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn tool_spec_cache_key(tools: &[crate::types::ToolSpec]) -> (usize, usize) {
    (tools.as_ptr() as usize, tools.len())
}

/// Register artifacts for one immutable, shared registry snapshot. Lookup
/// verifies exact slice identity and a live owner, preventing pointer-reuse
/// ABA from attaching old authorization data to a new list.
pub fn register_tool_spec_cache(
    specs: &Arc<SharedToolSpecs>,
    segments: Vec<ToolSpecCacheSegment>,
    estimated_context_tokens: u32,
) {
    if segments.len() != specs.len() {
        return;
    }
    let key = tool_spec_cache_key(specs.as_slice());
    let artifacts = Arc::new(ToolSpecCacheArtifacts {
        segments: Arc::new(segments),
        estimated_context_tokens,
    });
    let mut cache = tool_spec_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(position) = cache.iter().position(|entry| entry.key == key) {
        cache.remove(position);
    } else if cache.len() == TOOL_SPEC_CACHE_CAPACITY {
        cache.pop_front();
    }
    cache.push_back(ToolSpecCacheEntry {
        key,
        specs: Arc::downgrade(specs),
        artifacts,
    });
}

/// Return precomputed artifacts only when `tools` is the exact live shared
/// slice that was registered. Ordinary vectors and subslices retain the old
/// uncached behavior.
pub fn cached_tool_spec_artifacts(
    tools: &[crate::types::ToolSpec],
) -> Option<Arc<ToolSpecCacheArtifacts>> {
    let key = tool_spec_cache_key(tools);
    let mut cache = tool_spec_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let position = cache.iter().position(|entry| entry.key == key)?;
    let matches = cache[position].specs.upgrade().is_some_and(|specs| {
        specs.as_slice().as_ptr() == tools.as_ptr() && specs.len() == tools.len()
    });
    if !matches {
        cache.remove(position);
        return None;
    }
    let entry = cache.remove(position).expect("cache position still exists");
    let artifacts = entry.artifacts.clone();
    cache.push_back(entry);
    Some(artifacts)
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
