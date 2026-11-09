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
    /// `None` = the caller did NOT say (the tool argument was omitted) — the
    /// host then decides from its collision predicate (#20a smart fencing:
    /// multi-goal / branch-diverged / unfenced-in-flight ⇒ fence).
    /// `Some(true)`/`Some(false)` are explicit and always respected (an
    /// explicit `false` that contradicts the predicate is honored with a
    /// warning in `PeerHandoffStaged.model_note`).
    pub worktree: Option<bool>,
    /// Optional model LANE for this peer: the KEY of a `sub_provider`
    /// configured in the master's profile (e.g. `"cheap"`, `"strong"`).
    /// `None` (or an empty/whitespace value, normalized to `None` by
    /// [`PeerHandoffTool::execute`]) keeps the peer on the profile's primary
    /// model. The host validates the key against the profile's configured
    /// lanes and, on a match, records it beside the brief so the peer's turns
    /// resolve that provider; an unknown key is a warning, not a failure.
    pub model: Option<String>,
    /// Optional GOAL ID this peer is working on. When set, the peer's findings
    /// and escalations are scoped to this goal in the ledger.
    pub goal_id: Option<String>,
    /// Optional TASK ID within the goal. When set, the peer's work is tracked
    /// as part of this specific task.
    pub task_id: Option<String>,
    /// Optional cumulative token limit for this peer's turns. Omit when the
    /// user did not specify a budget.
    pub token_budget: Option<u64>,
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
    /// Cumulative peer token limit written at staging, if requested.
    pub token_budget: Option<u64>,
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
    worktree: Option<bool>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    goal_id: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    token_budget: Option<u64>,
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
        "Hand self-contained work to a durable peer session with its own workspace and \
         lifecycle; keep working yourself. Use for work that outlives this turn, needs \
         separate permissions, or needs independent user steering. Give it a short unique \
         name and a complete brief: it cannot see this conversation. Address it later with \
         peer_send_input, peer_close or peer_gather. Results go to the session strip and \
         blackboard, not this turn; use spawn if you need the result this turn. Handoff \
         does NOT end your turn: continue remaining work unless the peer took over all of it."
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
                    "description": "Fence the peer in its own git worktree on branch peer/<slug>. Omit for the host's collision-aware default (auto-fences when the run could collide with another goal/peer). Pass true for code changes that must not collide with this session's working tree; pass false to force sharing (a warning is noted when that contradicts the collision check)."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model lane for this peer — the KEY of a sub_provider configured in your profile (e.g. \"cheap\", \"strong\"). Omit to use the profile's primary model."
                },
                "token_budget": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional cumulative token budget for this peer. Set only when the user requested a token limit; the peer stops accepting new turns after the limit is reached."
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
        if input.token_budget == Some(0) {
            return Ok(failure("token_budget must be a positive integer"));
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
            goal_id: input.goal_id,
            task_id: input.task_id,
            token_budget: input.token_budget,
        };
        match (self.stage)(request) {
            Ok(staged) => {
                let mut output = format!(
                    "Staged peer '{name}' (slug {slug}, brief at {brief_path}, cwd {cwd}). \
                     Address it later by name with peer_send_input / peer_close. The user's \
                     client opens it in the background; its result lands on the blackboard \
                     at peers/{slug}/result.md (#27f: when the peer session itself writes \
                     the final result.md it must ALSO write a one-line sidecar file \
                     `.result-owner` containing `peer` next to it — that claims \
                     single-writer ownership and stops the runtime's turn-summary \
                     copy from clobbering the peer's final version). Do not wait for it — and do not \
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
                if let Some(limit) = staged.token_budget {
                    output.push_str(&format!(" Peer token budget: {limit}."));
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
