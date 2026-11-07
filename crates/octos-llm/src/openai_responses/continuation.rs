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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{ChatResponse, StopReason};

    /// A minimal `ChatResponse` for `remember` — the cache only reads
    /// `stop_reason`, `tool_calls`, and `content`.
    fn response(text: Option<&str>, stop_reason: StopReason) -> ChatResponse {
        ChatResponse {
            content: text.map(str::to_string),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason,
            usage: Default::default(),
            provider_index: None,
        }
    }

    fn end_turn(text: Option<&str>) -> ChatResponse {
        response(text, StopReason::EndTurn)
    }

    /// The follow-up request must replay the assistant turn byte-for-byte as
    /// `remember` hashed it, so build the replay items with the same helper
    /// production uses instead of copying the wire shape.
    fn assistant_replay_items(text: &str) -> Vec<Value> {
        super::super::build_input_messages(&[octos_core::Message::assistant(text)], None)
    }

    fn user_item(content: &str) -> Value {
        json!({"type": "message", "role": "user", "content": content})
    }

    fn request_body(input: &[Value]) -> Value {
        json!({ "model": "test-model", "temperature": 0.5, "input": input })
    }

    /// Runs one abbreviable cycle for `session`: a first request whose input
    /// is one user item, remembered as an end-turn reply `id`.
    fn remember_checkpoint(conts: &Continuations, session: &str, id: &str, reply: &str) {
        let input = vec![user_item("hello")];
        let mut body = request_body(&input);
        let pending = conts
            .prepare(session.to_string(), &mut body)
            .expect("first request prepares");
        conts.remember(pending, id, &end_turn(Some(reply)));
    }

    /// The follow-up input extends the checkpointed one by the assistant
    /// replay plus one new user item.
    fn follow_up_input(reply: &str) -> Vec<Value> {
        let mut input = vec![user_item("hello")];
        input.extend(assistant_replay_items(reply));
        input.push(user_item("and then?"));
        input
    }

    #[test]
    fn should_not_abbreviate_when_the_history_rewrites_the_past() {
        let conts = Continuations::default();
        remember_checkpoint(&conts, "s", "resp_1", "hi there");

        // Same length shape but a different first item: the prefix no longer
        // matches, so the server cannot have the conversation under resp_1.
        let mut input = vec![user_item("goodbye")];
        input.extend(assistant_replay_items("hi there"));
        input.push(user_item("and then?"));
        let mut body = request_body(&input);
        assert!(conts.prepare("s".into(), &mut body).is_some());
        assert_eq!(body.get("previous_response_id"), None);
        assert_eq!(body["input"], json!(input));
    }

    #[test]
    fn should_keep_session_checkpoints_isolated() {
        let conts = Continuations::default();
        remember_checkpoint(&conts, "a", "resp_1", "hi there");

        // Session b sends the very same extending history, but a's
        // checkpoint belongs to a different server-side conversation.
        let input = follow_up_input("hi there");
        let mut body = request_body(&input);
        assert!(conts.prepare("b".into(), &mut body).is_some());
        assert_eq!(body.get("previous_response_id"), None);
        assert_eq!(body["input"], json!(input));
        assert_eq!(body["store"], json!(true));
    }

    #[test]
    fn should_evict_the_whole_table_when_full_and_a_new_session_checks_in() {
        let conts = Continuations::default();
        for i in 0..MAX_SESSIONS {
            let session = format!("session-{i}");
            let input = vec![user_item(&format!("hello {i}"))];
            let mut body = request_body(&input);
            let pending = conts.prepare(session.clone(), &mut body).unwrap();
            conts.remember(pending, &format!("resp_{i}"), &end_turn(Some("ok")));
        }

        // The table is at capacity; one more new session clears it wholesale.
        remember_checkpoint(&conts, "session-new", "resp_new", "fresh");

        // The probe input extends session-0's checkpoint, so it WOULD
        // abbreviate to resp_0 if the table had survived — asserting None
        // here is what actually pins the wholesale eviction.
        let mut input = vec![user_item("hello 0")];
        input.extend(assistant_replay_items("ok"));
        input.push(user_item("more"));
        let mut body = request_body(&input);
        assert!(conts.prepare("session-0".into(), &mut body).is_some());
        assert_eq!(
            body.get("previous_response_id"),
            None,
            "a full table is dropped in one go when a new session arrives"
        );
    }

    #[test]
    fn should_only_retry_definite_missing_continuations_on_400_or_404() {
        assert!(missing_response(
            400,
            "previous_response_id not found",
            "resp_1"
        ));
        assert!(missing_response(404, "previous response expired", "resp_1"));
        for status in [200u16, 401, 429, 500] {
            assert!(
                !missing_response(status, "previous_response_id not found", "resp_1"),
                "{status}: only 400/404 may auto-retry"
            );
        }
    }
}
