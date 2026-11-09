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
