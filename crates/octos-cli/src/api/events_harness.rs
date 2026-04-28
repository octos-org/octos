//! `GET /api/events/harness` — filtered SSE stream of harness events.
//!
//! Subscribes to the shared [`super::SseBroadcaster`] and streams every
//! JSON frame it emits. The optional `kinds` query parameter filters by
//! the frame's top-level `"kind"` field; when absent or empty every
//! frame is forwarded. `dispatch_id` is accepted for wire-compatibility
//! with the M7.8 validator script but is not itself a filter beyond
//! what upstream frames already encode.
//!
//! Historically this route was described in comments (swarm.rs) as an
//! invariant for the M7.6 dashboard + M7.8 live gate but had never been
//! registered in the router, so requests landed in the static-file
//! fallback and returned `307 Location: /admin/`. That trapped
//! Playwright's `apiRequestContext` in a redirect loop and handed HTML
//! to callers expecting `text/event-stream`. See the live-sweep
//! regression against `release/coding-blue`.
//!
//! Auth is inherited from `user_auth_middleware`: an admin Bearer token
//! or an authenticated user session. The handler never mutates server
//! state — it only reads from the broadcaster.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::Deserialize;

use super::AppState;

/// Query parameters for `/api/events/harness`.
#[derive(Debug, Deserialize, Default)]
pub struct HarnessEventsQuery {
    /// Comma-separated list of kinds to include. Both snake_case
    /// (`swarm_dispatch`) and CamelCase (`SwarmDispatch`) match the
    /// frame's `"kind"` field after case-insensitive, underscore-agnostic
    /// normalisation so existing callers (`validate-m7-swarm-live.sh`,
    /// the dashboard's `LiveView`) keep working regardless of which
    /// casing they send.
    #[serde(default)]
    pub kinds: Option<String>,
    /// Accepted for wire-compat but not currently a server-side filter:
    /// upstream frames already carry the dispatch id, so clients may
    /// filter locally. Retained so curl/validator invocations don't
    /// 400 on the query string.
    #[serde(default)]
    #[allow(dead_code)]
    pub dispatch_id: Option<String>,
}

/// Handler for `GET /api/events/harness`.
pub async fn events_harness(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HarnessEventsQuery>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.broadcaster.subscribe();
    let allow = parse_kinds(query.kinds.as_deref());

    let stream = futures::stream::unfold((rx, allow), |(mut rx, allow)| async move {
        loop {
            match rx.recv().await {
                Ok(data) => {
                    if !frame_matches(&data, &allow) {
                        continue;
                    }
                    let event: Result<Event, Infallible> = Ok(Event::default().data(data));
                    return Some((event, (rx, allow)));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Parse a comma-separated `kinds` list into a normalised set. Returns
/// `None` when no filter is requested (empty string, all-whitespace, or
/// parameter absent), meaning "pass every frame through".
fn parse_kinds(raw: Option<&str>) -> Option<HashSet<String>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let kinds: HashSet<String> = raw
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(normalise_kind)
        .collect();
    if kinds.is_empty() { None } else { Some(kinds) }
}

/// `SwarmDispatch` / `swarm_dispatch` / `swarm-dispatch` all collapse to
/// `swarmdispatch` so either casing matches.
fn normalise_kind(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Given a raw JSON frame, return whether it should be forwarded.
/// `None` filter = forward everything. Frames without a `"kind"` field
/// are forwarded only when no filter is set.
fn frame_matches(frame: &str, allow: &Option<HashSet<String>>) -> bool {
    let Some(allow) = allow else {
        return true;
    };
    let value: serde_json::Value = match serde_json::from_str(frame) {
        Ok(v) => v,
        Err(_) => return false,
    };
    // Accept both top-level "kind" and nested "payload.kind" to match
    // the two shapes produced by the broadcaster (progress-derived
    // frames flatten the kind; typed HarnessEvent JSONs sometimes nest).
    let kind = value.get("kind").and_then(|v| v.as_str()).or_else(|| {
        value
            .get("payload")
            .and_then(|p| p.get("kind"))
            .and_then(|v| v.as_str())
    });
    match kind {
        Some(k) => allow.contains(&normalise_kind(k)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kinds_empty_returns_none() {
        assert!(parse_kinds(None).is_none());
        assert!(parse_kinds(Some("")).is_none());
        assert!(parse_kinds(Some("   ")).is_none());
        assert!(parse_kinds(Some(",, ,")).is_none());
    }

    #[test]
    fn parse_kinds_collapses_casing_and_separators() {
        let set = parse_kinds(Some("SwarmDispatch,sub_agent_dispatch,CostAttribution")).unwrap();
        assert!(set.contains("swarmdispatch"));
        assert!(set.contains("subagentdispatch"));
        assert!(set.contains("costattribution"));
    }

    #[test]
    fn frame_matches_forwards_all_when_no_filter() {
        assert!(frame_matches(r#"{"kind":"anything"}"#, &None));
        assert!(frame_matches("not-json", &None));
        assert!(frame_matches(r#"{}"#, &None));
    }

    #[test]
    fn frame_matches_top_level_kind() {
        let allow = parse_kinds(Some("SwarmDispatch"));
        assert!(frame_matches(
            r#"{"kind":"swarm_dispatch","dispatch_id":"abc"}"#,
            &allow
        ));
        assert!(!frame_matches(r#"{"kind":"progress"}"#, &allow));
    }

    #[test]
    fn frame_matches_nested_payload_kind() {
        let allow = parse_kinds(Some("CostAttribution"));
        assert!(frame_matches(
            r#"{"payload":{"kind":"cost_attribution","amount":0.1}}"#,
            &allow
        ));
    }

    #[test]
    fn frame_matches_drops_untyped_frame_under_filter() {
        let allow = parse_kinds(Some("SwarmDispatch"));
        assert!(!frame_matches(r#"{}"#, &allow));
        assert!(!frame_matches("garbage", &allow));
    }
}
