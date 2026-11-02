//! Redacted prompt-prefix observability.
//!
//! Prompt caches reuse the provider-normalized model-input prefix, not the
//! literal HTTP JSON body and not semantically-similar text. Phase 0 records a
//! provider-neutral manifest at the final agent/LLM boundary: only hashes,
//! roles, counts, and approximate token sizes leave this module. Provider
//! adapters can later add their own normalized manifests without logging raw
//! prompt or tool-schema content.

use octos_core::Message;
use octos_llm::{
    PromptCacheContext, SemanticCheckpointHint, ToolSpec, ToolSpecCacheSegment,
    cached_tool_spec_artifacts, canonical_object_keys,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PROMPT_FINGERPRINT_SCHEMA: &str = "octos.prompt-cache-fingerprint.v1";

#[cfg(test)]
thread_local! {
    static PROMPT_HASH_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PROMPT_ESTIMATE_SERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_prompt_serialization_counters() {
    PROMPT_HASH_CALLS.with(|counter| counter.set(0));
    PROMPT_ESTIMATE_SERIALIZATIONS.with(|counter| counter.set(0));
}

#[cfg(test)]
fn prompt_hash_calls() -> usize {
    PROMPT_HASH_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn prompt_estimate_serializations() -> usize {
    PROMPT_ESTIMATE_SERIALIZATIONS.with(std::cell::Cell::get)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct PromptSegmentFingerprint {
    pub kind: String,
    pub hash: String,
    pub estimated_tokens: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct PromptFingerprint {
    pub schema: &'static str,
    pub input_hash: String,
    pub stable_prefix_hash: String,
    pub conversation_hash: String,
    pub system_segments: Vec<PromptSegmentFingerprint>,
    pub tool_segments: Vec<PromptSegmentFingerprint>,
    pub conversation_segments: Vec<PromptSegmentFingerprint>,
    pub estimated_tokens: usize,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PromptPrefixComparison {
    pub stable_prefix_matches: bool,
    pub conversation_prefix_segments: usize,
    pub reusable_estimated_tokens: usize,
    pub invalidation_reason: Option<&'static str>,
}

impl PromptFingerprint {
    pub fn message_count(&self) -> usize {
        self.system_segments.len() + self.conversation_segments.len()
    }

    pub fn tool_count(&self) -> usize {
        self.tool_segments.len()
    }

    /// Safe to log: the value contains no prompt, reasoning, media payload, or
    /// tool-schema text. Hashes still correlate repeated content, so emit this
    /// at TRACE rather than including it in normal user-visible diagnostics.
    pub fn redacted_manifest(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| {
            json!({
                "schema": PROMPT_FINGERPRINT_SCHEMA,
                "error": "manifest_serialization_failed",
            })
        })
    }
}

fn uncached_tool_segments(tools: &[ToolSpec]) -> Vec<PromptSegmentFingerprint> {
    tools
        .iter()
        .map(|tool| {
            let stable = json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            });
            let estimated_tokens = estimate_json_tokens(&stable);
            PromptSegmentFingerprint {
                kind: format!("tool:{}", tool.name),
                hash: hash_json(stable),
                estimated_tokens,
            }
        })
        .collect()
}

/// Build the exact same tool fingerprints as `uncached_tool_segments`, but only
/// once per immutable registry snapshot. Any serialization error declines the
/// cache so the original path (and its established error behavior) remains the
/// fallback rather than an error being hidden behind a stale artifact.
fn cacheable_tool_segments_with_encoder(
    tools: &[ToolSpec],
    mut encode: impl FnMut(&Value) -> serde_json::Result<Vec<u8>>,
) -> Option<Vec<ToolSpecCacheSegment>> {
    tools
        .iter()
        .map(|tool| {
            let stable = json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            });
            let estimated_tokens = encode(&stable).ok()?.len().div_ceil(4).max(1);
            let canonical = canonical_object_keys(stable);
            let canonical_bytes = encode(&canonical).ok()?;
            Some(ToolSpecCacheSegment {
                kind: format!("tool:{}", tool.name),
                hash: format!("sha256:{:x}", Sha256::digest(canonical_bytes)),
                estimated_tokens,
            })
        })
        .collect()
}

pub(crate) fn cacheable_tool_segments(tools: &[ToolSpec]) -> Option<Vec<ToolSpecCacheSegment>> {
    cacheable_tool_segments_with_encoder(tools, serde_json::to_vec)
}

pub(super) fn fingerprint_prompt(messages: &[Message], tools: &[ToolSpec]) -> PromptFingerprint {
    let mut system_segments = Vec::new();
    let mut conversation_segments = Vec::new();
    // Position-aware: only the LEADING run of System rows is stable prefix. A
    // System row that follows any non-System row is volatile tail data (a hook
    // note, a checkpoint) and belongs to the conversation projection; counting
    // it as prefix reported its churn as `stable_prefix_changed` and rotated
    // the non-OUP fallback epoch mid-turn.
    let mut in_leading_system_run = true;
    for message in messages {
        let stable = json!({
            "role": message.role.as_str(),
            "content": message.content,
            "media": message.media,
            "tool_calls": message.tool_calls,
            "tool_call_id": message.tool_call_id,
            "reasoning_content": message.reasoning_content,
        });
        let estimated_tokens = estimate_json_tokens(&stable);
        let segment = PromptSegmentFingerprint {
            kind: format!("message:{}", message.role.as_str()),
            hash: hash_json(stable),
            estimated_tokens,
        };
        if in_leading_system_run && message.role == octos_core::MessageRole::System {
            system_segments.push(segment);
        } else {
            in_leading_system_run = false;
            conversation_segments.push(segment);
        }
    }

    // Preserve registry order. Reordering tools changes the provider-visible
    // stable prefix even when the set of tool names is identical. Registry-
    // owned immutable slices carry exact precomputed segments; every other
    // slice retains the original uncached implementation.
    let tool_segments = cached_tool_spec_artifacts(tools)
        .map(|artifacts| {
            artifacts
                .segments
                .iter()
                .map(|segment| PromptSegmentFingerprint {
                    kind: segment.kind.clone(),
                    hash: segment.hash.clone(),
                    estimated_tokens: segment.estimated_tokens,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| uncached_tool_segments(tools));

    let stable_prefix_hash = hash_segments(system_segments.iter().chain(tool_segments.iter()));
    let conversation_hash = hash_segments(conversation_segments.iter());
    let input_hash = hash_json(json!({
        "schema": PROMPT_FINGERPRINT_SCHEMA,
        "stable_prefix_hash": stable_prefix_hash,
        "conversation_hash": conversation_hash,
    }));
    let estimated_tokens = system_segments
        .iter()
        .chain(tool_segments.iter())
        .chain(conversation_segments.iter())
        .map(|segment| segment.estimated_tokens)
        .sum();

    PromptFingerprint {
        schema: PROMPT_FINGERPRINT_SCHEMA,
        input_hash,
        stable_prefix_hash,
        conversation_hash,
        system_segments,
        tool_segments,
        conversation_segments,
        estimated_tokens,
    }
}

/// Cache identity is derived from SESSION identity only. `AdaptiveRouter`
/// re-runs slot selection on every call, so hashing `provider_name()` /
/// `model_id()` here made `prompt_cache_key` and the fallback epoch flap with a
/// circuit breaker and routed the request away from the server holding the
/// prefix (ADR, OpenAI-compatible adapters). Two endpoints sharing one key is
/// harmless: they are separate cache spaces. Route attribution comes from the
/// per-call fingerprint manifests, never from these hashes.
pub(super) fn build_prompt_cache_context(
    fingerprint: &PromptFingerprint,
    messages: &[Message],
    session_key: Option<&str>,
    authoritative_epoch_id: Option<&str>,
    emit_semantic_boundaries: bool,
) -> PromptCacheContext {
    // Never put a raw session/channel/user id on the provider wire. The
    // deterministic hash is stable across turns and bounded below OpenAI's
    // documented 64-character prompt_cache_key limit.
    let affinity_hash = hash_json(json!({
        "schema": PROMPT_FINGERPRINT_SCHEMA,
        "session": session_key.unwrap_or("anonymous"),
    }));
    let affinity_key = format!(
        "octos-{}",
        affinity_hash
            .trim_start_matches("sha256:")
            .chars()
            .take(58)
            .collect::<String>()
    );
    // Fallback epoch (no authoritative OUP manager): observability-only. It
    // names "same session, same stable prefix" for the manifests and must not
    // rotate with the dynamic route; route identity is attributed from the
    // manifests' provider/model fields instead.
    let epoch_id = authoritative_epoch_id
        .map(str::to_owned)
        .unwrap_or_else(|| {
            hash_json(json!({
                "schema": "octos.prompt-cache-epoch.v1",
                "session": session_key.unwrap_or("anonymous"),
                "stable_prefix_hash": fingerprint.stable_prefix_hash,
            }))
        });

    let stable_tokens = fingerprint
        .system_segments
        .iter()
        .chain(fingerprint.tool_segments.iter())
        .map(|segment| segment.estimated_tokens)
        .sum::<usize>();
    let total_tokens = fingerprint.estimated_tokens;
    let mut prefix_tokens = stable_tokens;
    let mut rolling_hash = fingerprint.stable_prefix_hash.clone();
    let eligible_boundaries = semantic_boundary_kinds(messages);
    let semantic_boundaries = if emit_semantic_boundaries {
        fingerprint
            .conversation_segments
            .iter()
            .zip(eligible_boundaries)
            .enumerate()
            .filter_map(|(index, (segment, boundary_kind))| {
                prefix_tokens = prefix_tokens.saturating_add(segment.estimated_tokens);
                rolling_hash = hash_json(json!({
                    "previous": rolling_hash,
                    "kind": segment.kind,
                    "segment": segment.hash,
                }));
                let boundary_kind = boundary_kind?;
                Some(SemanticCheckpointHint {
                    boundary_id: format!(
                        "{boundary_kind}-{index}-{}",
                        rolling_hash
                            .trim_start_matches("sha256:")
                            .chars()
                            .take(16)
                            .collect::<String>()
                    ),
                    boundary_kind: boundary_kind.to_owned(),
                    prefix_hash: rolling_hash.clone(),
                    prefix_token_estimate: prefix_tokens,
                    estimated_recompute_tokens: total_tokens.saturating_sub(prefix_tokens),
                    checkpoint_priority: match boundary_kind {
                        "tool_interaction" => 220,
                        "user_turn" | "context_event" => 200,
                        "assistant_final" => 160,
                        _ => 128,
                    },
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    PromptCacheContext {
        affinity_key,
        epoch_id,
        stable_prefix_hash: fingerprint.stable_prefix_hash.clone(),
        semantic_boundaries,
    }
}

/// Mark only boundaries at which every preceding semantic interaction is
/// complete. In particular, a parallel assistant tool-call batch has no
/// checkpoint after the call row or an intermediate result; the boundary is
/// emitted only after all expected results are present. This keeps optional
/// local recurrent-state restoration correctness-neutral.
fn semantic_boundary_kinds(messages: &[Message]) -> Vec<Option<&'static str>> {
    // Same row set as `fingerprint_prompt`'s conversation segments (skip only
    // the leading System run) so the two zip together position-for-position;
    // a tail System row is a conversation row that carries no boundary.
    let leading_system_rows = messages
        .iter()
        .take_while(|message| message.role == octos_core::MessageRole::System)
        .count();
    let conversation = messages[leading_system_rows..].iter().collect::<Vec<_>>();
    conversation
        .iter()
        .enumerate()
        .map(|(index, message)| match message.role {
            octos_core::MessageRole::User => {
                if message.content.trim_start().starts_with("<context_event") {
                    Some("context_event")
                } else {
                    Some("user_turn")
                }
            }
            octos_core::MessageRole::Assistant => {
                if message
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty())
                {
                    None
                } else {
                    Some("assistant_final")
                }
            }
            octos_core::MessageRole::Tool => {
                let Some(call_index) = (0..index).rev().find(|candidate| {
                    conversation[*candidate]
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty())
                        && conversation[*candidate + 1..index]
                            .iter()
                            .all(|row| row.role == octos_core::MessageRole::Tool)
                }) else {
                    return Some("orphan_tool_output");
                };
                let expected = conversation[call_index]
                    .tool_calls
                    .as_ref()
                    .expect("candidate has tool calls")
                    .iter()
                    .map(|call| call.id.as_str())
                    .collect::<std::collections::HashSet<_>>();
                let observed = conversation[call_index + 1..=index]
                    .iter()
                    .filter_map(|row| row.tool_call_id.as_deref())
                    .collect::<std::collections::HashSet<_>>();
                expected.is_subset(&observed).then_some("tool_interaction")
            }
            octos_core::MessageRole::System => None,
        })
        .collect()
}

#[cfg(test)]
pub(super) fn compare_prompt_prefixes(
    previous: &PromptFingerprint,
    current: &PromptFingerprint,
) -> PromptPrefixComparison {
    if previous.stable_prefix_hash != current.stable_prefix_hash {
        return PromptPrefixComparison {
            stable_prefix_matches: false,
            conversation_prefix_segments: 0,
            reusable_estimated_tokens: 0,
            invalidation_reason: Some("stable_prefix_changed"),
        };
    }

    let conversation_prefix_segments = previous
        .conversation_segments
        .iter()
        .zip(current.conversation_segments.iter())
        .take_while(|(left, right)| left.hash == right.hash && left.kind == right.kind)
        .count();
    let stable_tokens = previous
        .system_segments
        .iter()
        .chain(previous.tool_segments.iter())
        .map(|segment| segment.estimated_tokens)
        .sum::<usize>();
    let conversation_tokens = previous
        .conversation_segments
        .iter()
        .take(conversation_prefix_segments)
        .map(|segment| segment.estimated_tokens)
        .sum::<usize>();
    let previous_is_prefix = conversation_prefix_segments == previous.conversation_segments.len();

    PromptPrefixComparison {
        stable_prefix_matches: true,
        conversation_prefix_segments,
        reusable_estimated_tokens: stable_tokens + conversation_tokens,
        invalidation_reason: (!previous_is_prefix).then_some("historical_projection_changed"),
    }
}

fn hash_segments<'a>(segments: impl Iterator<Item = &'a PromptSegmentFingerprint>) -> String {
    hash_json(Value::Array(
        segments
            .map(|segment| {
                json!({
                    "kind": segment.kind,
                    "hash": segment.hash,
                })
            })
            .collect(),
    ))
}

fn estimate_json_tokens(value: &Value) -> usize {
    #[cfg(test)]
    PROMPT_ESTIMATE_SERIALIZATIONS.with(|counter| counter.set(counter.get() + 1));
    serde_json::to_vec(value)
        .map(|bytes| bytes.len().div_ceil(4).max(1))
        .unwrap_or(1)
}

/// Canonicalize before hashing: the fingerprint inputs embed foreign JSON
/// (provider tool-call arguments, registered tool schemas) whose key order
/// octos does not choose, and with serde_json's `preserve_order` feature on —
/// the octos-cli build graph's shape — `to_vec` would otherwise fingerprint
/// that order, splitting one logical input across cache identities (the
/// octos-agent face of the #2633 key-order class).
fn hash_json(value: Value) -> String {
    #[cfg(test)]
    PROMPT_HASH_CALLS.with(|counter| counter.set(counter.get() + 1));
    let bytes = serde_json::to_vec(&canonical_object_keys(value)).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::{MessageRole, ToolCall};

    fn tool(name: &str, description: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn manifest_contains_no_raw_prompt_or_tool_text() {
        let fingerprint = fingerprint_prompt(
            &[
                Message::system("TOP_SECRET_SYSTEM"),
                Message::user("TOP_SECRET_USER"),
            ],
            &[tool("read_secret", "TOP_SECRET_TOOL_DESCRIPTION")],
        );

        let logged = fingerprint.redacted_manifest().to_string();
        assert!(!logged.contains("TOP_SECRET_SYSTEM"));
        assert!(!logged.contains("TOP_SECRET_USER"));
        assert!(!logged.contains("TOP_SECRET_TOOL_DESCRIPTION"));
        assert!(logged.contains("message:system"));
        assert!(logged.contains("tool:read_secret"));
    }

    #[test]
    fn cache_context_hashes_session_identity_and_emits_ordered_boundaries() {
        let fingerprint = fingerprint_prompt(
            &[
                Message::system("stable"),
                Message::user("one"),
                Message::assistant("two"),
            ],
            &[tool("read", "read")],
        );
        let context = build_prompt_cache_context(
            &fingerprint,
            &[
                Message::system("stable"),
                Message::user("one"),
                Message::assistant("two"),
            ],
            Some("private-user-session"),
            None,
            true,
        );

        assert_eq!(context.affinity_key.len(), 64);
        assert!(!context.affinity_key.contains("private-user-session"));
        assert_eq!(context.semantic_boundaries.len(), 2);
        assert!(
            context.semantic_boundaries[0].prefix_token_estimate
                < context.semantic_boundaries[1].prefix_token_estimate
        );
        assert_ne!(
            context.semantic_boundaries[0].prefix_hash,
            context.semantic_boundaries[1].prefix_hash
        );

        let hosted = build_prompt_cache_context(
            &fingerprint,
            &[
                Message::system("stable"),
                Message::user("one"),
                Message::assistant("two"),
            ],
            Some("private-user-session"),
            None,
            false,
        );
        assert!(hosted.semantic_boundaries.is_empty());
        assert_eq!(hosted.affinity_key, context.affinity_key);
    }

    #[test]
    fn foreign_json_key_order_does_not_split_the_cache_identity() {
        // Provider tool-call arguments and registered tool schemas arrive as
        // foreign JSON whose key order octos does not choose. With
        // serde_json's `preserve_order` feature — unified on for the whole
        // workspace build through octos-cli's agent-client-protocol
        // dependency, so the `cargo test --workspace` CI lane runs this
        // order-sensitive — the same logical payload under two key orders
        // previously hashed to two identities, splitting the cache on a
        // serialization difference. The standalone per-crate lane's sorted
        // backend canonicalizes either way; to reproduce the order-sensitive
        // shape in isolation, run with
        // `-F serde_json/preserve_order`.
        let mut first = Message::assistant("");
        first.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_owned(),
            name: "read".to_owned(),
            arguments: json!({"path": "a", "options": {"depth": 2, "follow": true}}),
            metadata: None,
        }]);
        let mut second = Message::assistant("");
        second.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_owned(),
            name: "read".to_owned(),
            arguments: serde_json::from_value(json!({
                "options": {"follow": true, "depth": 2},
                "path": "a",
            }))
            .unwrap(),
            metadata: None,
        }]);

        let forward_schema = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}, "depth": {"type": "number"}},
        });
        let reversed_schema = serde_json::from_value(json!({
            "properties": {"depth": {"type": "number"}, "path": {"type": "string"}},
            "type": "object",
        }))
        .unwrap();

        let left = fingerprint_prompt(
            &[first],
            &[ToolSpec {
                name: "read".to_owned(),
                description: "read".to_owned(),
                input_schema: forward_schema,
            }],
        );
        let right = fingerprint_prompt(
            &[second],
            &[ToolSpec {
                name: "read".to_owned(),
                description: "read".to_owned(),
                input_schema: reversed_schema,
            }],
        );

        assert_eq!(left.input_hash, right.input_hash);
        assert_eq!(left.stable_prefix_hash, right.stable_prefix_hash);
        assert_eq!(left.conversation_hash, right.conversation_hash);
    }

    #[test]
    fn array_element_order_remains_part_of_the_identity() {
        // Only object keys are canonicalized. Arrays carry wire semantics
        // (content blocks, argument lists), so reordering one must change
        // the identity in every build shape.
        let mut first = Message::assistant("");
        first.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_owned(),
            name: "read".to_owned(),
            arguments: json!({"paths": ["a", "b"]}),
            metadata: None,
        }]);
        let mut second = Message::assistant("");
        second.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_owned(),
            name: "read".to_owned(),
            arguments: json!({"paths": ["b", "a"]}),
            metadata: None,
        }]);

        assert_ne!(
            fingerprint_prompt(&[first], &[]).input_hash,
            fingerprint_prompt(&[second], &[]).input_hash
        );
    }

    #[test]
    fn the_wire_affinity_key_is_pinned_for_a_fixed_session() {
        // affinity_key is the one value that reaches the provider wire
        // (OpenAI's prompt_cache_key). Its literal happens to be byte-identical
        // under both serde_json backends because "schema" sorts before
        // "session"; pin the digest so a future edit that could reintroduce a
        // build-shape dependence (a key sorting before "schema", a nested
        // object) has to be a conscious wire-contract decision.
        let fingerprint = fingerprint_prompt(&[Message::user("x")], &[]);
        let context = build_prompt_cache_context(
            &fingerprint,
            &[Message::user("x")],
            Some("fixed-session"),
            None,
            false,
        );

        assert_eq!(
            context.affinity_key,
            "octos-660c00211854e5b54c69ba97e33e9dd0cf886218679a667080152723b4"
        );
    }

    #[test]
    fn append_only_conversation_reuses_the_previous_input() {
        let previous = fingerprint_prompt(
            &[Message::system("stable"), Message::user("one")],
            &[tool("read", "read")],
        );
        let current = fingerprint_prompt(
            &[
                Message::system("stable"),
                Message::user("one"),
                Message::assistant("two"),
            ],
            &[tool("read", "read")],
        );

        let comparison = compare_prompt_prefixes(&previous, &current);
        assert!(comparison.stable_prefix_matches);
        assert_eq!(comparison.conversation_prefix_segments, 1);
        assert_eq!(comparison.invalidation_reason, None);
        assert!(comparison.reusable_estimated_tokens > 0);
    }

    #[test]
    fn changing_system_or_tools_invalidates_the_stable_prefix() {
        let previous = fingerprint_prompt(
            &[Message::system("stable"), Message::user("one")],
            &[tool("read", "old")],
        );
        let changed_system = fingerprint_prompt(
            &[Message::system("changed"), Message::user("one")],
            &[tool("read", "old")],
        );
        let changed_tool = fingerprint_prompt(
            &[Message::system("stable"), Message::user("one")],
            &[tool("read", "new")],
        );

        for current in [&changed_system, &changed_tool] {
            let comparison = compare_prompt_prefixes(&previous, current);
            assert!(!comparison.stable_prefix_matches);
            assert_eq!(comparison.reusable_estimated_tokens, 0);
            assert_eq!(
                comparison.invalidation_reason,
                Some("stable_prefix_changed")
            );
        }
    }

    #[test]
    fn a_deep_edit_reuses_only_messages_before_the_edit() {
        let mut assistant = Message::assistant("");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_owned(),
            name: "read".to_owned(),
            arguments: json!({"path": "a"}),
            metadata: None,
        }]);
        let previous = fingerprint_prompt(
            &[
                Message::system("stable"),
                Message::user("one"),
                assistant.clone(),
                Message::user("three"),
            ],
            &[],
        );
        assistant.tool_calls.as_mut().unwrap()[0].arguments = json!({"path": "b"});
        let current = fingerprint_prompt(
            &[
                Message::system("stable"),
                Message::user("one"),
                assistant,
                Message::user("three"),
            ],
            &[],
        );

        let comparison = compare_prompt_prefixes(&previous, &current);
        assert!(comparison.stable_prefix_matches);
        assert_eq!(comparison.conversation_prefix_segments, 1);
        assert_eq!(
            comparison.invalidation_reason,
            Some("historical_projection_changed")
        );
    }

    #[test]
    fn timestamps_do_not_invalidate_a_prompt_segment() {
        let first = Message::user("same");
        let mut later = first.clone();
        later.timestamp = first.timestamp + chrono::Duration::seconds(30);

        let left = fingerprint_prompt(&[first], &[]);
        let right = fingerprint_prompt(&[later], &[]);
        assert_eq!(left.input_hash, right.input_hash);
    }

    #[test]
    fn role_is_part_of_the_segment_identity() {
        let mut assistant = Message::assistant("same");
        assistant.role = MessageRole::Assistant;
        let user = Message::user("same");

        let left = fingerprint_prompt(&[user], &[]);
        let right = fingerprint_prompt(&[assistant], &[]);
        assert_ne!(left.input_hash, right.input_hash);
    }

    #[test]
    fn should_classify_tail_system_rows_as_conversation_when_they_follow_non_system_rows() {
        let tools = [tool("read", "read")];
        let messages = [
            Message::system("stable"),
            Message::user("one"),
            Message::system("volatile checkpoint"),
            Message::user("two"),
        ];
        let base = fingerprint_prompt(&[Message::system("stable"), Message::user("one")], &tools);
        let with_tail = fingerprint_prompt(&messages, &tools);

        // Only the LEADING System run is stable prefix; a System row that
        // follows a non-System row is volatile tail data and must neither
        // count as a system segment nor move the stable prefix hash.
        assert_eq!(
            base.stable_prefix_hash, with_tail.stable_prefix_hash,
            "a System row outside the leading run must not change the stable prefix"
        );
        assert_eq!(with_tail.system_segments.len(), 1);
        assert_eq!(with_tail.conversation_segments.len(), 3);
        assert_eq!(with_tail.conversation_segments[1].kind, "message:system");

        // Boundary hints stay aligned with the conversation segments: the
        // tail System row carries no checkpoint, the two user rows do, and the
        // final boundary covers the whole prompt.
        let context =
            build_prompt_cache_context(&with_tail, &messages, Some("session"), None, true);
        assert_eq!(
            context
                .semantic_boundaries
                .iter()
                .map(|hint| hint.boundary_kind.as_str())
                .collect::<Vec<_>>(),
            vec!["user_turn", "user_turn"]
        );
        assert_eq!(
            context.semantic_boundaries[1].prefix_token_estimate,
            with_tail.estimated_tokens
        );
        assert_eq!(context.semantic_boundaries[1].estimated_recompute_tokens, 0);
    }

    #[test]
    fn should_keep_affinity_and_fallback_epoch_stable_when_route_selection_flaps() {
        // AdaptiveRouter re-runs provider selection per call; a circuit-breaker
        // flap must not rotate prompt_cache_key (the endpoint would route away
        // from the server holding the prefix) or the non-OUP fallback epoch.
        // Route identity is no longer an input at all, so two builds for the
        // same session under different routes are the same call: identity is
        // session-only, bounded, and never the raw session key. (The loop-level
        // guard with an actually flapping provider lives in loop_runner_tests.)
        let messages = [Message::system("stable"), Message::user("request")];
        let fingerprint = fingerprint_prompt(&messages, &[tool("read", "read")]);
        let primary = build_prompt_cache_context(
            &fingerprint,
            &messages,
            Some("private-user-session"),
            None,
            false,
        );
        let after_route_flap = build_prompt_cache_context(
            &fingerprint,
            &messages,
            Some("private-user-session"),
            None,
            false,
        );
        let other_session = build_prompt_cache_context(
            &fingerprint,
            &messages,
            Some("another-session"),
            None,
            false,
        );
        let other_prefix = build_prompt_cache_context(
            &fingerprint_prompt(&messages, &[tool("read", "changed")]),
            &messages,
            Some("private-user-session"),
            None,
            false,
        );

        assert_eq!(primary.affinity_key, after_route_flap.affinity_key);
        assert_eq!(primary.epoch_id, after_route_flap.epoch_id);
        assert!(primary.affinity_key.len() <= 64);
        assert!(!primary.affinity_key.contains("private-user-session"));
        // Still bounded to the session: a different session gets its own key
        // space, and the fallback epoch still tracks the stable prefix.
        assert_ne!(primary.affinity_key, other_session.affinity_key);
        assert_eq!(primary.affinity_key, other_prefix.affinity_key);
        assert_ne!(primary.epoch_id, other_prefix.epoch_id);
    }

    #[test]
    fn authoritative_oup_epoch_overrides_agent_local_derivation() {
        let messages = [Message::system("stable"), Message::user("request")];
        let fingerprint = fingerprint_prompt(&messages, &[]);
        let context = build_prompt_cache_context(
            &fingerprint,
            &messages,
            Some("session"),
            Some("oup-epoch-after-compaction"),
            false,
        );
        assert_eq!(context.epoch_id, "oup-epoch-after-compaction");
    }

    #[test]
    fn checkpoint_hints_skip_incomplete_parallel_tool_batches() {
        let mut calls = Message::assistant("");
        calls.tool_calls = Some(vec![
            ToolCall {
                id: "call_a".to_owned(),
                name: "read".to_owned(),
                arguments: json!({"path": "a"}),
                metadata: None,
            },
            ToolCall {
                id: "call_b".to_owned(),
                name: "read".to_owned(),
                arguments: json!({"path": "b"}),
                metadata: None,
            },
        ]);
        let tool_result = |id: &str| {
            let mut result = Message::assistant(format!("result for {id}"));
            result.role = MessageRole::Tool;
            result.tool_call_id = Some(id.to_owned());
            result
        };
        let messages = vec![
            Message::system("stable"),
            Message::user("inspect both"),
            calls,
            tool_result("call_a"),
            tool_result("call_b"),
        ];
        let fingerprint = fingerprint_prompt(&messages, &[]);
        let context =
            build_prompt_cache_context(&fingerprint, &messages, Some("session"), None, true);

        assert_eq!(
            context
                .semantic_boundaries
                .iter()
                .map(|hint| hint.boundary_kind.as_str())
                .collect::<Vec<_>>(),
            vec!["user_turn", "tool_interaction"]
        );
        assert!(
            context.semantic_boundaries[1].prefix_token_estimate
                > context.semantic_boundaries[0].prefix_token_estimate
        );
    }

    /// Opt-in S6 reproduction, run alone to keep allocation accounting isolated:
    /// `cargo test -p octos-agent s6_repeated_tools_bench -- --ignored --nocapture`.
    mod s6_bench {
        #![allow(unsafe_code)] // GlobalAlloc requires unsafe; only test accounting uses it.

        use super::*;
        use crate::tools::{Tool, ToolRegistry, ToolResult};
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::Instant;

        static ENABLED: AtomicBool = AtomicBool::new(false);
        static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
        static BYTES: AtomicUsize = AtomicUsize::new(0);

        struct CountingAllocator;

        // SAFETY: this forwards every operation to System and only adds atomic
        // counters; it never changes layout, pointer ownership, or alignment.
        unsafe impl GlobalAlloc for CountingAllocator {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                if ENABLED.load(Ordering::Relaxed) {
                    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                    BYTES.fetch_add(layout.size(), Ordering::Relaxed);
                }
                // SAFETY: forwarded with the caller's unchanged layout.
                unsafe { System.alloc(layout) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                // SAFETY: forwarded with the pointer/layout supplied by the caller.
                unsafe { System.dealloc(ptr, layout) }
            }
        }

        #[global_allocator]
        static ALLOCATOR: CountingAllocator = CountingAllocator;

        struct BenchTool {
            name: String,
            description: String,
            schema: Value,
        }

        #[async_trait::async_trait]
        impl Tool for BenchTool {
            fn name(&self) -> &str {
                &self.name
            }

            fn description(&self) -> &str {
                &self.description
            }

            fn input_schema(&self) -> Value {
                self.schema.clone()
            }

            async fn execute(&self, _args: &Value) -> eyre::Result<ToolResult> {
                unreachable!("benchmark never executes tools")
            }
        }

        fn registry_for_tests(count: usize) -> ToolRegistry {
            let mut registry = ToolRegistry::new();
            for index in 0..count {
                registry.register(BenchTool {
                    name: format!("bench_tool_{index:02}"),
                    description: format!("description {index}"),
                    schema: json!({"type": "object", "index": index}),
                });
            }
            registry
        }

        #[test]
        fn shared_specs_reuse_the_same_allocation_without_changing_json() {
            let registry = registry_for_tests(2);
            let first = registry.specs_shared();
            let second = registry.specs_shared();
            assert!(std::sync::Arc::ptr_eq(&first, &second));
            assert_eq!(
                serde_json::to_vec(first.as_slice()).unwrap(),
                serde_json::to_vec(&registry.specs()).unwrap()
            );
        }

        #[test]
        fn fingerprint_reuses_precomputed_tool_segments() {
            let registry = registry_for_tests(2);
            let specs = registry.specs_shared();
            let owned = registry.specs();
            let expected = fingerprint_prompt(&[], &owned);
            let first = fingerprint_prompt(&[], specs.as_slice());
            assert_eq!(expected, first);
            assert_eq!(
                octos_llm::context::estimate_request_tokens_base(&[], &owned),
                octos_llm::context::estimate_request_tokens_base(&[], specs.as_slice())
            );
            reset_prompt_serialization_counters();
            let second = fingerprint_prompt(&[], specs.as_slice());
            assert_eq!(first, second);
            assert_eq!(prompt_hash_calls(), 3);
            assert_eq!(prompt_estimate_serializations(), 0);
        }

        #[test]
        fn serialization_failure_declines_cache_artifacts() {
            let specs = std::sync::Arc::new(vec![tool("read", "read")]);
            let artifacts = cacheable_tool_segments_with_encoder(specs.as_slice(), |_| {
                Err(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "injected serialization failure",
                )))
            });
            assert!(artifacts.is_none());
            assert!(cached_tool_spec_artifacts(specs.as_slice()).is_none());
            // The old implementation remains available and produces the same
            // manifest instead of receiving a partial cached success.
            let fingerprint = fingerprint_prompt(&[], specs.as_slice());
            assert_eq!(fingerprint.tool_count(), 1);
        }

        #[test]
        fn all_visibility_inputs_replace_the_shared_snapshot() {
            let mut registry = registry_for_tests(3);
            let initial = registry.specs_shared();
            assert_eq!(fingerprint_prompt(&[], initial.as_slice()).tool_count(), 3);

            registry.set_provider_policy(crate::tools::ToolPolicy {
                deny: vec!["bench_tool_02".to_owned()],
                ..Default::default()
            });
            let provider_filtered = registry.specs_shared();
            assert_eq!(provider_filtered.len(), 2);
            assert!(!std::sync::Arc::ptr_eq(&initial, &provider_filtered));

            registry.set_context_filter(vec!["missing-tag".to_owned()]);
            let context_filtered = registry.specs_shared();
            assert_eq!(context_filtered.len(), 2, "untagged tools still pass RFC-0");
            assert!(!std::sync::Arc::ptr_eq(
                &provider_filtered,
                &context_filtered
            ));

            registry.set_host_tool_allowlist(Some(&["bench_tool_00".to_owned()]));
            assert_eq!(registry.specs_shared().len(), 1);
            registry.mark_spawn_only("bench_tool_00", None);
            assert_eq!(registry.specs_shared().len(), 1);
            registry.mark_internal_hidden("bench_tool_00");
            assert!(registry.specs_shared().is_empty());

            let mut added = registry_for_tests(1);
            let before = added.specs_shared();
            added.register(BenchTool {
                name: "bench_tool_01".to_owned(),
                description: "added".to_owned(),
                schema: json!({"type": "object"}),
            });
            assert_eq!(added.specs_shared().len(), 2);
            added.retain(|name| name == "bench_tool_00");
            let remaining = added.specs_shared();
            assert_eq!(remaining.len(), 1);
            assert!(!std::sync::Arc::ptr_eq(&before, &remaining));
        }

        #[test]
        fn concurrent_shared_reads_never_observe_a_partial_roster() {
            use std::sync::{Arc, RwLock};

            let registry = Arc::new(RwLock::new(registry_for_tests(1)));
            let mut readers = Vec::new();
            for _ in 0..4 {
                let registry = registry.clone();
                readers.push(std::thread::spawn(move || {
                    for _ in 0..64 {
                        let specs = registry.read().unwrap().specs_shared();
                        let fingerprint = fingerprint_prompt(&[], specs.as_slice());
                        assert_eq!(fingerprint.tool_count(), specs.len());
                        let json = serde_json::to_vec(specs.as_slice()).unwrap();
                        assert!(!json.is_empty());
                    }
                }));
            }
            for index in 1..8 {
                registry.write().unwrap().register(BenchTool {
                    name: format!("bench_tool_{index:02}"),
                    description: "concurrent".to_owned(),
                    schema: json!({"type": "object"}),
                });
            }
            for reader in readers {
                reader.join().unwrap();
            }
            assert_eq!(registry.read().unwrap().specs_shared().len(), 8);
        }

        fn measure(label: &str, run: impl FnOnce()) {
            reset_prompt_serialization_counters();
            ALLOCATIONS.store(0, Ordering::Relaxed);
            BYTES.store(0, Ordering::Relaxed);
            ENABLED.store(true, Ordering::Relaxed);
            let started = Instant::now();
            run();
            let wall = started.elapsed();
            ENABLED.store(false, Ordering::Relaxed);
            println!(
                "{{\"phase\":\"{label}\",\"wall_ms\":{:.3},\"allocations\":{},\"allocated_mib\":{:.3},\"hash_calls\":{},\"estimate_serializations\":{}}}",
                wall.as_secs_f64() * 1000.0,
                ALLOCATIONS.load(Ordering::Relaxed),
                BYTES.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
                prompt_hash_calls(),
                prompt_estimate_serializations(),
            );
        }

        #[test]
        #[ignore = "opt-in S6 benchmark; run without other parallel tests"]
        fn s6_repeated_tools_bench() {
            let mut registry = ToolRegistry::new();
            for index in 0..48 {
                let name = format!("bench_tool_{index:02}");
                registry.register(BenchTool {
                    description: format!("description for {name}"),
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "payload": {
                                "type": "string",
                                "description": "x".repeat(4096),
                                "maxLength": 4096,
                            }
                        }
                    }),
                    name,
                });
            }
            let specs = registry.specs_shared();
            std::hint::black_box(fingerprint_prompt(&[], specs.as_slice()));
            std::hint::black_box(octos_llm::context::estimate_request_tokens_base(
                &[],
                specs.as_slice(),
            ));
            const ROUNDS: usize = 256;

            measure("specs_shared", || {
                for _ in 0..ROUNDS {
                    std::hint::black_box(registry.specs_shared());
                }
            });
            measure("fingerprint", || {
                for _ in 0..ROUNDS {
                    std::hint::black_box(fingerprint_prompt(&[], specs.as_slice()));
                }
            });
            measure("context_estimate", || {
                for _ in 0..ROUNDS {
                    std::hint::black_box(octos_llm::context::estimate_request_tokens_base(
                        &[],
                        specs.as_slice(),
                    ));
                }
            });
        }
    }
}
