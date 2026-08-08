//! `peer_handoff` — LLM-initiated peer staging (#1801 v3).
//!
//! Sessions are client-connection-coupled, so the MODEL cannot open a peer
//! session itself: this tool only STAGES the peer server-side (durable
//! brief + optional fenced worktree) through an injected host callback, and
//! the host emits a durable `peer/staged` notification asking the user's
//! CLIENT to open the staged session in the background.
//!
//! The tool is deliberately NOT registered anywhere by default — construction
//! requires the staging callback, which only the serve/WS turn path can
//! provide (gateway/chat/ACP have no client that can open sessions). The
//! serve wiring also owns the governance rails: depth-1 (peer sessions never
//! see the tool) and the per-turn handoff cap live at the wiring site, not
//! here.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolResult};

/// Hard cap on the brief payload (bytes). Mirrors the server-side
/// `peer/prepare` cap — a brief is a task contract, not blob storage.
pub const PEER_HANDOFF_BRIEF_MAX_BYTES: usize = 64 * 1024;

/// Upper bound (chars) on a peer NAME — a short, human-readable handle, not a
/// payload. The host derives the directory slug from it (capped separately).
pub const PEER_HANDOFF_NAME_MAX_CHARS: usize = 64;

/// Parsed, validated handoff arguments delivered to the host callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerHandoffRequest {
    /// The complete task contract for the peer (trimmed, non-empty,
    /// `<=` [`PEER_HANDOFF_BRIEF_MAX_BYTES`]).
    pub brief: String,
    /// Required human-readable NAME — the peer's primary address ("let Edison
    /// do X"). Trimmed, non-empty, `<=` [`PEER_HANDOFF_NAME_MAX_CHARS`] chars.
    /// The host derives the directory slug from it and REJECTS a duplicate
    /// (case-insensitive) rather than auto-suffixing.
    pub name: String,
    /// Whether the host should fence the peer in its own git worktree.
    pub worktree: bool,
    /// Optional model LANE for this peer: the KEY of a `sub_provider`
    /// configured in the master's profile (e.g. `"cheap"`, `"strong"`).
    /// `None` (or an empty/whitespace value, normalized to `None` by
    /// [`PeerHandoffTool::execute`]) keeps the peer on the profile's primary
    /// model. The host validates the key against the profile's configured
    /// lanes and, on a match, records it beside the brief so the peer's turns
    /// resolve that provider; an unknown key is a warning, not a failure.
    pub model: Option<String>,
}

/// Staged-peer facts the host callback returns on success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerHandoffStaged {
    /// Directory slug reserved under the profile's `peers/` root.
    pub slug: String,
    /// Session topic the client opens (`peer-<slug>`).
    pub topic: String,
    /// Absolute path of the durable brief written for the peer.
    pub brief_path: String,
    /// Working directory the peer session runs in.
    pub cwd: String,
    /// Fence branch (`peer/<slug>`) when a worktree was created.
    pub worktree_branch: Option<String>,
    /// Optional model-lane note the host surfaces back to the model in the
    /// tool's success output — e.g. a warning that the requested `model` lane
    /// was not found among the profile's configured `sub_providers` (so the
    /// peer falls back to the primary model). `None` when no lane was
    /// requested or the requested lane resolved cleanly.
    pub model_note: Option<String>,
}

/// Host staging callback. Synchronous by design: the tool needs the staged
/// facts to compose its result text, and staging is bounded local work
/// (directory reserve + optional `git worktree add` + one atomic write).
pub type PeerHandoffCallback =
    Arc<dyn Fn(PeerHandoffRequest) -> Result<PeerHandoffStaged, String> + Send + Sync>;

/// `peer_handoff` tool. See the module docs for the staging/open split.
pub struct PeerHandoffTool {
    stage: PeerHandoffCallback,
}

impl PeerHandoffTool {
    /// Build the tool around the host's staging callback. There is no
    /// callback-free constructor on purpose: without a host that can stage
    /// peers AND a client that opens them, the tool must not exist.
    pub fn new(stage: PeerHandoffCallback) -> Self {
        Self { stage }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    brief: String,
    name: String,
    #[serde(default)]
    worktree: bool,
    #[serde(default)]
    model: Option<String>,
}

fn failure(output: impl Into<String>) -> ToolResult {
    ToolResult {
        output: output.into(),
        success: false,
        ..Default::default()
    }
}

#[async_trait]
impl Tool for PeerHandoffTool {
    fn name(&self) -> &str {
        "peer_handoff"
    }

    fn description(&self) -> &str {
        "Hand a SELF-CONTAINED piece of work to a sovereign peer session with its \
         own durable brief, workspace, and lifecycle, and keep working yourself. \
         Give the peer a short, unique NAME — it is the peer's primary address \
         (\"let Edison do X\"); you reach it later by name with peer_send_input / \
         peer_close / peer_gather. Use when the work outlives this turn, needs its \
         own workspace or safety envelope, or the user may steer it separately. \
         You will NOT receive the result in this turn — the peer reports to the \
         user's session strip and the shared blackboard. Handing work off does NOT \
         end your turn: delegating is what lets you make progress on your own \
         remaining work in parallel, so continue it immediately unless the peer \
         took over everything you had left. For work whose result THIS turn needs \
         to continue reasoning, use spawn instead. The brief is a complete task \
         contract: include all context the peer needs (it cannot see this \
         conversation)."
    }

    fn tags(&self) -> &[&str] {
        // Same visibility surface as `spawn` — the choice contrast in the
        // description only works if both tools survive the same tag filters.
        &["gateway"]
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        // Stateful: reserves peer dirs, may create git worktrees, and burns
        // the per-turn handoff budget — keep calls serialized.
        super::ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name", "brief"],
            "properties": {
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": PEER_HANDOFF_NAME_MAX_CHARS,
                    "description": "Short, unique, human-readable name for the peer — its primary address (e.g. \"Edison\"). You reach the peer later by this name. Must be unique among your peers."
                },
                "brief": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": PEER_HANDOFF_BRIEF_MAX_BYTES,
                    "description": "Complete task contract for the peer. It cannot see this conversation: include the goal, all needed context/paths, constraints, and what a finished result looks like."
                },
                "worktree": {
                    "type": "boolean",
                    "description": "Fence the peer in its own git worktree on branch peer/<slug> (default false). Use for code changes that must not collide with this session's working tree."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model lane for this peer — the KEY of a sub_provider configured in your profile (e.g. \"cheap\", \"strong\"). Omit to use the profile's primary model."
                }
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: Input = match serde_json::from_value(args.clone()) {
            Ok(input) => input,
            Err(err) => {
                return Ok(failure(format!(
                    "invalid peer_handoff arguments: {err}. Required: \
                     {{\"name\": string, \"brief\": string}}; optional: \"worktree\" (boolean)."
                )));
            }
        };
        let name = input.name.trim();
        if name.is_empty() {
            return Ok(failure(
                "name is required — a short, unique, human-readable handle for the peer \
                 (e.g. \"Edison\"); it is how you address the peer later.",
            ));
        }
        if name.chars().count() > PEER_HANDOFF_NAME_MAX_CHARS {
            return Ok(failure(format!(
                "name exceeds {PEER_HANDOFF_NAME_MAX_CHARS} characters — keep it a short handle."
            )));
        }
        let brief = input.brief.trim();
        if brief.is_empty() {
            return Ok(failure(
                "brief is required — a complete task contract for the peer (it cannot \
                 see this conversation).",
            ));
        }
        if brief.len() > PEER_HANDOFF_BRIEF_MAX_BYTES {
            return Ok(failure(format!(
                "brief exceeds {PEER_HANDOFF_BRIEF_MAX_BYTES} bytes — a brief is a task \
                 contract; keep the payload in the workspace and reference it by path."
            )));
        }
        // Normalize the optional model lane: trim and drop an empty/whitespace
        // value to `None` so the host only ever sees a real lane key.
        let model = input
            .model
            .as_deref()
            .map(str::trim)
            .filter(|lane| !lane.is_empty())
            .map(str::to_owned);
        let request = PeerHandoffRequest {
            brief: brief.to_owned(),
            name: name.to_owned(),
            worktree: input.worktree,
            model,
        };
        match (self.stage)(request) {
            Ok(staged) => {
                let mut output = format!(
                    "Staged peer '{name}' (slug {slug}, brief at {brief_path}, cwd {cwd}). \
                     Address it later by name with peer_send_input / peer_close. The user's \
                     client opens it in the background; its result lands on the blackboard \
                     at peers/{slug}/result.md. Do not wait for it — and do not \
                     stop here: if you still have work of your own, continue it \
                     now in this same turn. Only finish if the peer took over \
                     everything that was left.",
                    slug = staged.slug,
                    brief_path = staged.brief_path,
                    cwd = staged.cwd,
                );
                // Surface any model-lane note (e.g. an unknown lane fell back to
                // the primary model) back to the model in the same result.
                if let Some(note) = &staged.model_note {
                    output.push(' ');
                    output.push_str(note);
                }
                Ok(ToolResult {
                    output,
                    success: true,
                    ..Default::default()
                })
            }
            Err(err) => Ok(failure(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn tool_with_recorder() -> (PeerHandoffTool, Arc<Mutex<Vec<PeerHandoffRequest>>>) {
        let seen: Arc<Mutex<Vec<PeerHandoffRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_cb = seen.clone();
        let tool = PeerHandoffTool::new(Arc::new(move |request: PeerHandoffRequest| {
            seen_cb.lock().unwrap().push(request.clone());
            Ok(PeerHandoffStaged {
                slug: "ci-fix".to_owned(),
                topic: "peer-ci-fix".to_owned(),
                brief_path: "/data/peers/ci-fix/brief.md".to_owned(),
                cwd: "/work/peers/ci-fix/wt".to_owned(),
                worktree_branch: request.worktree.then(|| "peer/ci-fix".to_owned()),
                model_note: None,
            })
        }));
        (tool, seen)
    }

    #[tokio::test]
    async fn should_reject_and_skip_callback_when_brief_missing() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cb = calls.clone();
        let tool = PeerHandoffTool::new(Arc::new(move |_request| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            Err("must not run".to_owned())
        }));

        let result = tool
            .execute(&json!({ "name": "Edison" }))
            .await
            .expect("validation failure is a tool result, not an Err");
        assert!(!result.success);
        assert!(
            result.output.contains("brief"),
            "schema hint names the missing field: {}",
            result.output
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "callback must not fire");
    }

    #[tokio::test]
    async fn should_reject_and_skip_callback_when_brief_blank() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cb = calls.clone();
        let tool = PeerHandoffTool::new(Arc::new(move |_request| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            Err("must not run".to_owned())
        }));

        let result = tool
            .execute(&json!({ "brief": "   \n\t ", "name": "Edison" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("brief is required"));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "callback must not fire");
    }

    #[tokio::test]
    async fn should_reject_and_skip_callback_when_brief_oversized() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cb = calls.clone();
        let tool = PeerHandoffTool::new(Arc::new(move |_request| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            Err("must not run".to_owned())
        }));

        let oversized = "x".repeat(PEER_HANDOFF_BRIEF_MAX_BYTES + 1);
        let result = tool
            .execute(&json!({ "brief": oversized, "name": "Edison" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .output
                .contains(&PEER_HANDOFF_BRIEF_MAX_BYTES.to_string()),
            "cap named in the error: {}",
            result.output
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "callback must not fire");
    }

    #[tokio::test]
    async fn should_reject_and_skip_callback_when_name_missing() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cb = calls.clone();
        let tool = PeerHandoffTool::new(Arc::new(move |_request| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            Err("must not run".to_owned())
        }));

        let result = tool
            .execute(&json!({ "brief": "Has a brief but no name." }))
            .await
            .expect("validation failure is a tool result, not an Err");
        assert!(!result.success);
        assert!(
            result.output.contains("name"),
            "schema hint names the missing field: {}",
            result.output
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "callback must not fire");
    }

    #[tokio::test]
    async fn should_reject_and_skip_callback_when_name_blank() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cb = calls.clone();
        let tool = PeerHandoffTool::new(Arc::new(move |_request| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            Err("must not run".to_owned())
        }));

        let result = tool
            .execute(&json!({ "brief": "A real brief.", "name": "   " }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("name is required"));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "callback must not fire");
    }

    #[tokio::test]
    async fn should_reject_and_skip_callback_when_name_oversized() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cb = calls.clone();
        let tool = PeerHandoffTool::new(Arc::new(move |_request| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            Err("must not run".to_owned())
        }));

        let oversized = "n".repeat(PEER_HANDOFF_NAME_MAX_CHARS + 1);
        let result = tool
            .execute(&json!({ "brief": "A real brief.", "name": oversized }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .output
                .contains(&PEER_HANDOFF_NAME_MAX_CHARS.to_string()),
            "cap named in the error: {}",
            result.output
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "callback must not fire");
    }

    #[tokio::test]
    async fn should_invoke_callback_with_parsed_args_when_valid() {
        let (tool, seen) = tool_with_recorder();

        let result = tool
            .execute(&json!({
                "brief": "  Fix the flaky bus test; repro in crates/octos-bus.  ",
                "name": "  CI Fix  ",
                "worktree": true,
            }))
            .await
            .unwrap();
        assert!(result.success, "unexpected failure: {}", result.output);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0],
            PeerHandoffRequest {
                brief: "Fix the flaky bus test; repro in crates/octos-bus.".to_owned(),
                name: "CI Fix".to_owned(),
                worktree: true,
                model: None,
            },
            "brief/name are trimmed, worktree passes through"
        );

        // The result addresses the peer by NAME and teaches the fire-and-forget
        // contract (the recorder maps it to slug `ci-fix`).
        assert!(result.output.contains("Staged peer 'CI Fix'"));
        assert!(result.output.contains("slug ci-fix"));
        assert!(result.output.contains("/data/peers/ci-fix/brief.md"));
        assert!(result.output.contains("cwd /work/peers/ci-fix/wt"));
        assert!(result.output.contains("peers/ci-fix/result.md"));
        assert!(result.output.contains("Do not wait for it"));
    }

    /// #1918: the master stalled after every handoff — it read "promote work
    /// OUT" + "you will NOT receive the result in this turn" + "Do not wait for
    /// it" as "your work is finished", and only resumed when the user nudged it.
    /// Every signal pointed at ending the turn and nothing pointed back.
    ///
    /// "Do not wait" answers whether to BLOCK; it never answered whether to
    /// CONTINUE. Both surfaces must now say so explicitly, because they are the
    /// only two the model sees — octos has no central role prompt to carry this.
    /// The result matters most: it is in context at the exact moment the model
    /// chooses between another tool call and ending the turn.
    #[tokio::test]
    async fn should_tell_the_caller_to_keep_working_after_a_handoff() {
        let (tool, _seen) = tool_with_recorder();
        let result = tool
            .execute(&json!({
                "name": "Edison",
                "brief": "Investigate the flaky bus test.",
            }))
            .await
            .expect("handoff stages");

        assert!(result.success);
        let output = &result.output;
        assert!(
            output.contains("do not stop here"),
            "the handoff result must tell the caller to continue its own work, got: {output}"
        );
        assert!(
            output.contains("Only finish if the peer took over"),
            "the result must name the ONE case where ending the turn is right, \
             else 'keep working' reads as 'never finish', got: {output}"
        );

        // The description is the other half: it primed the stall by framing a
        // handoff as work leaving the caller.
        let description = tool.description();
        assert!(
            description.contains("keep working yourself"),
            "the description must not imply the caller's work ends here"
        );
        assert!(
            description.contains("does NOT end your turn"),
            "the description must state that a handoff does not end the turn"
        );
    }

    #[tokio::test]
    async fn should_default_worktree_when_omitted() {
        let (tool, seen) = tool_with_recorder();

        let result = tool
            .execute(&json!({ "brief": "Just the brief.", "name": "Solo" }))
            .await
            .unwrap();
        assert!(result.success);

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0],
            PeerHandoffRequest {
                brief: "Just the brief.".to_owned(),
                name: "Solo".to_owned(),
                worktree: false,
                model: None,
            }
        );
    }

    #[tokio::test]
    async fn should_thread_model_lane_through_to_callback_trimmed() {
        let (tool, seen) = tool_with_recorder();

        let result = tool
            .execute(&json!({
                "brief": "Grunt work.",
                "name": "Grunt",
                "model": "  strong  ",
            }))
            .await
            .unwrap();
        assert!(result.success, "unexpected failure: {}", result.output);

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].model.as_deref(),
            Some("strong"),
            "model lane is trimmed and threaded into the staging request"
        );
    }

    #[tokio::test]
    async fn should_normalize_blank_model_lane_to_none() {
        let (tool, seen) = tool_with_recorder();

        let result = tool
            .execute(&json!({
                "brief": "Grunt work.",
                "name": "Grunt",
                "model": "   \n\t ",
            }))
            .await
            .unwrap();
        assert!(result.success);

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].model, None,
            "a blank/whitespace model lane normalizes to None (use the primary model)"
        );
    }

    #[tokio::test]
    async fn should_append_model_note_from_host_to_success_output() {
        // The host may report that the requested lane was unknown; the note is
        // appended to the (still successful) staging result verbatim.
        let tool = PeerHandoffTool::new(Arc::new(|request: PeerHandoffRequest| {
            let note = request
                .model
                .as_ref()
                .map(|lane| format!("model lane '{lane}' not found — using the primary model."));
            Ok(PeerHandoffStaged {
                slug: "grunt".to_owned(),
                topic: "peer-grunt".to_owned(),
                brief_path: "/data/peers/grunt/brief.md".to_owned(),
                cwd: "/work".to_owned(),
                worktree_branch: None,
                model_note: note,
            })
        }));

        let result = tool
            .execute(&json!({
                "brief": "Grunt work.",
                "name": "Grunt",
                "model": "bogus",
            }))
            .await
            .unwrap();
        assert!(result.success, "an unknown lane warns, it does not fail");
        assert!(
            result.output.contains("Staged peer 'Grunt'"),
            "the staged confirmation is still present: {}",
            result.output
        );
        assert!(
            result
                .output
                .contains("model lane 'bogus' not found — using the primary model."),
            "the host's model-lane note is appended to the output: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn should_surface_callback_error_as_tool_failure() {
        let tool = PeerHandoffTool::new(Arc::new(|_request| {
            Err("a peer named 'Edison' already exists".to_owned())
        }));

        let result = tool
            .execute(&json!({ "brief": "Over budget.", "name": "Edison" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.output, "a peer named 'Edison' already exists");
    }
}
