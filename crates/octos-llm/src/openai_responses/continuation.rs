//! Response IDs abbreviate an exact, session-scoped conversation prefix.
//! The caller always supplies full history, so edits, compaction, cache misses
//! and server restarts can recover without relying on server-side persistence.
use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::build_input_messages;
use crate::{ChatResponse, StopReason};

const MAX_SESSIONS: usize = 128;
const MAX_ITEMS: usize = 8192;

#[derive(Default)]
pub(super) struct Continuations(Mutex<HashMap<String, Entry>>);

struct Entry {
    id: String,
    settings: [u8; 32],
    history: Vec<[u8; 32]>,
}

pub(super) struct Pending {
    session: String,
    settings: [u8; 32],
    history: Vec<[u8; 32]>,
}

fn digest(value: &Value) -> [u8; 32] {
    Sha256::digest(value.to_string().as_bytes()).into()
}

impl Continuations {
    pub fn prepare(&self, session: String, body: &mut Value) -> Option<Pending> {
        let input = body.get("input")?.as_array()?;
        if input.is_empty() || input.len() > MAX_ITEMS {
            return None;
        }
        let history: Vec<_> = input.iter().map(digest).collect();
        let mut settings = body.clone();
        settings.as_object_mut()?.remove("input");
        settings.as_object_mut()?.remove("stream");
        let settings = digest(&settings);
        // Poisoning or eviction only disables this optional optimization.
        if let Ok(entries) = self.0.lock()
            && let Some(entry) = entries.get(&session)
            && entry.settings == settings
            && history.len() > entry.history.len()
            && history.starts_with(&entry.history)
        {
            body["input"] = Value::Array(input[entry.history.len()..].to_vec());
            body["previous_response_id"] = entry.id.clone().into();
        }
        body["store"] = true.into();
        Some(Pending {
            session,
            settings,
            history,
        })
    }

    pub fn remember(&self, pending: Pending, id: &str, response: &ChatResponse) {
        // Tool IDs can be rewritten by message repair. Keep replaying those
        // turns explicitly until their provider IDs survive that boundary.
        // A preceding text checkpoint can still abbreviate their input.
        if !id.starts_with("resp_")
            || response.stop_reason != StopReason::EndTurn
            || !response.tool_calls.is_empty()
        {
            return;
        }
        let Some(text) = response.content.as_ref().filter(|text| !text.is_empty()) else {
            return;
        };
        let mut history = pending.history;
        history.extend(
            // Assistant text carries no media, so no scope root is needed.
            build_input_messages(&[octos_core::Message::assistant(text)], None)
                .iter()
                .map(digest),
        );
        if let Ok(mut entries) = self.0.lock() {
            if entries.len() >= MAX_SESSIONS && !entries.contains_key(&pending.session) {
                entries.clear();
            }
            entries.insert(
                pending.session,
                Entry {
                    id: id.into(),
                    settings: pending.settings,
                    history,
                },
            );
        }
    }

    pub fn forget_id(&self, id: &str) {
        if let Ok(mut entries) = self.0.lock() {
            entries.retain(|_, entry| entry.id != id);
        }
    }
}

/// Only a definite missing continuation is safe to retry automatically.
/// Other failures may follow inference or tool execution and must propagate.
pub(super) fn missing_response(status: u16, text: &str, id: &str) -> bool {
    if !matches!(status, 400 | 404) {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    (lower.contains("previous_response_id")
        || lower.contains("previous response")
        || text.contains(id))
        && (lower.contains("not found")
            || lower.contains("expired")
            || lower.contains("does not exist"))
}
