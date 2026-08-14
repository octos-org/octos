//! In-process peer HOST: what a non-`serve` runtime needs to actually run a
//! staged peer and let its master answer it.
//!
//! `octos serve` hosts peers by opening a second WebSocket session per peer;
//! everything else (the parked-prompt registry, the slug→session wire map) is
//! process-global and transport-agnostic. This module supplies the two pieces
//! `serve` gets from its WS layer, so `octos chat --peers` can drive a peer in
//! the SAME process as its master:
//!
//! 1. [`register_peer_wire`] — publish the peer's `SessionKey` under
//!    `"{profile}:peer:{slug}"`. Without this `peer_list` never reports the peer
//!    as `awaiting_input` and `peer_respond` refuses with "not open", because
//!    both derive the peer's TRUSTED session key from that registry (#P1-1) and
//!    never from a caller-supplied argument.
//! 2. [`ParkingApprovalRequester`] / [`ParkingQuestionRequester`] — the peer's
//!    approval / user-question bridge. Instead of prompting the console (only
//!    the MASTER talks to the console) it registers the pending entry in the
//!    process-global [`crate::contracts::contract_stores`] and awaits the
//!    oneshot, exactly like `UiProtocolApprovalRequester` does on the WS path.
//!    That oneshot is what `peer_respond` resolves.
//!
//! The pairing is the whole mechanism: the peer parks in the same `HashMap` the
//! master resolves from, so no wire, no IPC, and no filesystem marker sits
//! between them.

use std::sync::Arc;

use async_trait::async_trait;
use octos_core::SessionKey;
use octos_core::ui_protocol::{
    ApprovalDecision, ApprovalId, ApprovalRequestedEvent, QuestionId, TurnId,
    UserQuestionRequestedEvent,
};

use octos_agent::tools::{
    ToolApprovalDecision, ToolApprovalRequest, ToolApprovalRequester, UserQuestionOutcome,
    UserQuestionRequest, UserQuestionRequester,
};

use super::{
    PeerPendingKind, peer_io, peer_pending_prompt_summary, peer_wire_key, peer_wire_registry,
    staged_peer_dir,
};
use crate::contracts::{UiProtocolContractStores, contract_stores};

/// The `SessionKey` a chat-hosted peer runs its turns under.
///
/// Shape matters in three places, so it is minted in ONE function:
/// * the topic must be `peer-<slug>` — `peer_slug_and_profile` (and therefore
///   the depth-1 guard, the wake path, and result recording) keys off it;
/// * the profile segment must parse — `SessionKey::profile_id()` only sees it
///   in `{profile}:{channel}:{chat_id}` form with a KNOWN channel, and `cli` is
///   one;
/// * the base key must match the master's Phase-1 goal session
///   (`<profile>:cli:chat`) so the peer is recognisably a child of that chat.
pub(crate) fn chat_peer_session_key(profile_id: &str, slug: &str) -> SessionKey {
    SessionKey::with_profile_topic(profile_id, "cli", "chat", &format!("peer-{slug}"))
}

/// Publish the peer's live session under its wire key.
///
/// `peer_list` joins the blackboard to the parked-prompt store through this
/// map, and `peer_respond` derives the peer's trusted session from it. A peer
/// that is running but unregistered is invisible to both — it parks forever
/// with no way for the master to see or answer it.
pub(crate) fn register_peer_wire(profile_id: &str, slug: &str) -> SessionKey {
    let session = chat_peer_session_key(profile_id, slug);
    peer_wire_registry().register(peer_wire_key(profile_id, slug), session.clone());
    session
}

/// Everything a peer session needs at boot, read from its staged directory.
///
/// Mirrors the serve-side peer boot (`api::ui_protocol`, the
/// `with_goal_id` / `with_originator_session` block): the goal binding and the
/// originator are captured ONCE here rather than re-read per tool call, so a
/// mid-turn swap of `peers/<slug>/originator` cannot rebind a known goal to a
/// different session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PeerBoot {
    /// The goal this peer was handed off under, if any.
    pub(crate) goal_id: Option<String>,
    /// Sub-task within the goal, if the master named one.
    pub(crate) task_id: Option<String>,
    /// The MASTER session that staged this peer — the only session
    /// `peer_respond` will accept an answer from.
    pub(crate) originator: Option<String>,
    /// The brief the peer runs as its first user turn.
    pub(crate) brief: Option<String>,
}

/// Read a staged peer's boot context. Every field is independently optional:
/// a goal-less or brief-less peer is degraded, not fatal. Returns `None` only
/// when `slug` is not a real staged peer dir (unsafe slug, symlink, no
/// `brief.md`) — the same gate every other peer read uses.
pub(crate) fn read_peer_boot(peers_root: &std::path::Path, slug: &str) -> Option<PeerBoot> {
    let dir = staged_peer_dir(peers_root, slug)?;
    let mut boot = PeerBoot::default();
    if let Some(body) = peer_io::read_peer_file(&dir, "goal", peer_io::PEER_FILE_READ_CAP_SMALL) {
        let mut lines = body.lines();
        boot.goal_id = lines
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        boot.task_id = lines
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        // A task id without a goal id is meaningless — drop it rather than
        // threading a dangling sub-task through the agent.
        if boot.goal_id.is_none() {
            boot.task_id = None;
        }
    }
    boot.originator =
        peer_io::read_peer_file(&dir, "originator", peer_io::PEER_FILE_READ_CAP_SMALL)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
    boot.brief = peer_io::read_peer_file(&dir, "brief.md", peer_io::PEER_FILE_READ_CAP_LARGE)
        .filter(|s| !s.trim().is_empty());
    Some(boot)
}

/// How a parked peer prompt is announced. The peer must NOT write to the
/// console prompt (that belongs to the master), but a silent park looks like a
/// hang, so the host injects a one-line notice sink.
pub(crate) type PeerParkNotice = Arc<dyn Fn(PeerPendingKind, &str, &str) + Send + Sync>;

/// The peer's approval bridge: park in the process-global store, wait for the
/// master.
///
/// Deliberately NOT the console requester. `octos chat` has exactly one
/// terminal and it belongs to the master; a peer that prompted there would race
/// the master's own prompt and could be answered by whoever typed first. Parking
/// instead makes the master's `peer_list` → `peer_respond` the ONLY way in,
/// which is also the authorization boundary (`peer_respond` is
/// originator-only).
pub(crate) struct ParkingApprovalRequester {
    session: SessionKey,
    turn: TurnId,
    contracts: Arc<UiProtocolContractStores>,
    notice: PeerParkNotice,
    slug: String,
}

impl ParkingApprovalRequester {
    pub(crate) fn new(session: SessionKey, slug: String, notice: PeerParkNotice) -> Self {
        Self {
            session,
            turn: TurnId::new(),
            contracts: contract_stores(),
            notice,
            slug,
        }
    }
}

#[async_trait]
impl ToolApprovalRequester for ParkingApprovalRequester {
    async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        let approval_id = ApprovalId::new();
        let event = ApprovalRequestedEvent::generic(
            self.session.clone(),
            approval_id.clone(),
            self.turn.clone(),
            request.tool_name,
            request.title.clone(),
            request.body.clone(),
        );
        let summary = peer_pending_prompt_summary(&event.title, &event.body);
        // Register BEFORE announcing: the notice tells the master it can run
        // `peer_list`, and that must never be true before the entry exists.
        let rx = self.contracts.approvals.request_runtime(event);
        (self.notice)(PeerPendingKind::Approval, &self.slug, &summary);
        match rx.await {
            Ok(ApprovalDecision::Approve) => ToolApprovalDecision::Approve,
            // Deny, an unknown forward-compat decision, and a dropped sender
            // (the peer was closed while parked) all fail CLOSED.
            _ => ToolApprovalDecision::Deny,
        }
    }
}

/// The peer's `ask_user_question` bridge. Same parking contract as
/// [`ParkingApprovalRequester`]; a dropped sender means the question was
/// cancelled (peer closed / turn interrupted), which the tool renders as
/// cancelled rather than inventing an answer.
pub(crate) struct ParkingQuestionRequester {
    session: SessionKey,
    turn: TurnId,
    contracts: Arc<UiProtocolContractStores>,
    notice: PeerParkNotice,
    slug: String,
}

impl ParkingQuestionRequester {
    pub(crate) fn new(session: SessionKey, slug: String, notice: PeerParkNotice) -> Self {
        Self {
            session,
            turn: TurnId::new(),
            contracts: contract_stores(),
            notice,
            slug,
        }
    }
}

#[async_trait]
impl UserQuestionRequester for ParkingQuestionRequester {
    async fn request_user_question(&self, request: UserQuestionRequest) -> UserQuestionOutcome {
        let question_id = QuestionId::new();
        let event = UserQuestionRequestedEvent::new(
            self.session.clone(),
            question_id,
            self.turn.clone(),
            request.title.clone(),
            request.body.clone(),
            request.questions,
        );
        let summary = peer_pending_prompt_summary(&event.title, &event.body);
        let rx = self.contracts.user_questions.request_runtime(event);
        (self.notice)(PeerPendingKind::Question, &self.slug, &summary);
        match rx.await {
            Ok(answers) => UserQuestionOutcome::Answered(answers),
            Err(_) => UserQuestionOutcome::Cancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peers::{peer_pending_summaries, peer_respond_resolve, stage_peer};
    use std::path::Path;

    fn noop_notice() -> PeerParkNotice {
        Arc::new(|_, _, _| {})
    }

    /// The topic must be `peer-<slug>` and the profile must be recoverable —
    /// every downstream peer path (`peer_slug_and_profile`, the depth-1 guard,
    /// the wire key) reads one or the other off this key.
    #[test]
    fn should_mint_a_peer_topic_session_key_that_parses_back() {
        let key = chat_peer_session_key("dev", "alpha");
        assert_eq!(key.topic(), Some("peer-alpha"));
        assert_eq!(key.profile_id(), Some("dev"));
        assert_eq!(key.base_key(), "dev:cli:chat");
        assert_eq!(
            crate::peers::peer_slug_and_profile(&key),
            Some(("dev", "alpha")),
            "the peer bookkeeping split must recognise a chat-hosted peer",
        );
    }

    /// A chat-hosted peer must NOT get `peer_handoff` — depth-1 is enforced by
    /// the topic, so minting the key wrong would silently re-enable recursion.
    #[test]
    fn should_refuse_peer_handoff_for_a_chat_hosted_peer_session() {
        assert!(!crate::peers::peer_handoff_allowed_for_session(
            &chat_peer_session_key("dev", "alpha")
        ));
    }

    /// THE de-risk test, and the load-bearing claim of the whole feature: a
    /// peer parked by its own requester and the master's `peer_respond` meet in
    /// ONE process-global registry, with no wire and no filesystem marker in
    /// between.
    ///
    /// Drives the REAL production functions end to end — `stage_peer` writes
    /// the staging + originator, `register_peer_wire` publishes the trusted
    /// session, `ParkingApprovalRequester` registers the oneshot, and
    /// `peer_respond_resolve` (the exact fn the `peer_respond` tool callback
    /// calls) resolves it. If any of those were still wire-bound this test
    /// could not compile, let alone pass, without `--features api`.
    #[tokio::test]
    async fn should_resolve_a_parked_peer_approval_through_peer_respond_in_one_process() {
        let temp = tempfile::tempdir().expect("tempdir");
        let peers_root = temp.path().join("peers");
        std::fs::create_dir_all(&peers_root).expect("peers root");
        let workspace = temp.path().join("ws");
        std::fs::create_dir_all(&workspace).expect("workspace");

        // A UNIQUE profile per run: `contract_stores()` and the wire registry
        // are process-global, so a fixed id would let concurrent tests collide.
        let profile = format!("chat-peer-{}", uuid::Uuid::now_v7().simple());
        let master = format!("{profile}:cli:chat");

        let staged = stage_peer(
            &peers_root,
            &workspace,
            "worker",
            Some("worker"),
            Some(master.as_str()),
            "do the thing",
            false,
            None,
            None,
        )
        .expect("stage");

        let session = register_peer_wire(&profile, &staged.slug);

        // The peer parks. Its requester blocks on the store's oneshot exactly
        // as it would inside a real turn, so we drive it on a task.
        let requester =
            ParkingApprovalRequester::new(session.clone(), staged.slug.clone(), noop_notice());
        let parked = tokio::spawn(async move {
            requester
                .request_approval(ToolApprovalRequest {
                    tool_id: "t1".into(),
                    tool_name: "shell".into(),
                    title: "run tests".into(),
                    body: "cargo test".into(),
                    command: Some("cargo test".into()),
                    cwd: None,
                })
                .await
        });

        // Wait until the entry is visible in the AUTHORITATIVE store — that is
        // precisely what `peer_list` projects as `awaiting_input`.
        let contracts = contract_stores();
        let mut pending = Vec::new();
        for _ in 0..200 {
            pending = peer_pending_summaries(&contracts, &session);
            if !pending.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            pending.len(),
            1,
            "the peer's park must be visible to the master through the shared store",
        );
        assert_eq!(pending[0].kind, PeerPendingKind::Approval);

        // And the MASTER's actual `peer_list` tool must say so — the store
        // being right is necessary but not sufficient; the master only ever
        // learns about the park through this callback's rendered text.
        let listing = crate::peers::build_peer_list_callback(
            peers_root.clone(),
            Vec::new(),
            contract_stores(),
            profile.clone(),
        )()
        .expect("peer_list renders");
        assert!(
            listing.contains("awaiting_input") && listing.contains(&staged.slug),
            "the master's peer_list must surface the parked peer: {listing}",
        );
        assert!(
            listing.contains(&pending[0].id),
            "peer_list must name the pending id so peer_respond can target it: {listing}",
        );

        // The master answers. `peer_respond_resolve` authorizes against the
        // recorded originator and resolves through the same store.
        let decided = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = decided.clone();
        peer_respond_resolve(
            &peers_root,
            &master,
            &profile,
            &contracts,
            &move |_event, _tool| {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
            octos_agent::PeerRespondRequest {
                slug: staged.slug.clone(),
                id: None,
                decision: Some("approve".into()),
                answers: None,
            },
        )
        .expect("master resolves the peer's parked approval");

        assert_eq!(
            parked.await.expect("peer task"),
            ToolApprovalDecision::Approve,
            "resolving the store entry must UNBLOCK the peer's own requester",
        );
        assert_eq!(
            decided.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the approval/decided sink must fire once, attributing the master",
        );
    }

    /// Only the recorded originator may answer. A stranger's `peer_respond`
    /// must be refused BEFORE the store is touched, leaving the peer parked.
    #[tokio::test]
    async fn should_refuse_peer_respond_from_a_session_that_did_not_stage_the_peer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let peers_root = temp.path().join("peers");
        std::fs::create_dir_all(&peers_root).expect("peers root");
        let workspace = temp.path().join("ws");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let profile = format!("chat-peer-{}", uuid::Uuid::now_v7().simple());
        let master = format!("{profile}:cli:chat");

        let staged = stage_peer(
            &peers_root,
            &workspace,
            "worker",
            Some("worker"),
            Some(master.as_str()),
            "brief",
            false,
            None,
            None,
        )
        .expect("stage");
        register_peer_wire(&profile, &staged.slug);

        let err = peer_respond_resolve(
            &peers_root,
            "someone-else:cli:chat",
            &profile,
            &contract_stores(),
            &|_e, _t| {},
            octos_agent::PeerRespondRequest {
                slug: staged.slug.clone(),
                id: None,
                decision: Some("approve".into()),
                answers: None,
            },
        )
        .expect_err("a non-originator must be refused");
        assert!(
            err.to_lowercase().contains("originator") || err.to_lowercase().contains("not "),
            "the refusal must be model-visible and explain why: {err}",
        );
    }

    /// The boot context is what binds a chat-hosted peer to its master's goal.
    /// A peer staged WITH a goal must report it; a goal-less peer must report
    /// `None` rather than a dangling task id.
    #[test]
    fn should_read_goal_and_originator_from_a_staged_peer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let peers_root = temp.path().join("peers");
        std::fs::create_dir_all(&peers_root).expect("peers root");
        let workspace = temp.path().join("ws");
        std::fs::create_dir_all(&workspace).expect("workspace");

        let staged = stage_peer(
            &peers_root,
            &workspace,
            "bound",
            Some("bound"),
            Some("m:cli:chat"),
            "the brief body",
            false,
            Some("goal-123"),
            Some("task-9"),
        )
        .expect("stage");
        let boot = read_peer_boot(&peers_root, &staged.slug).expect("staged peer boots");
        assert_eq!(boot.goal_id.as_deref(), Some("goal-123"));
        assert_eq!(boot.task_id.as_deref(), Some("task-9"));
        assert_eq!(boot.originator.as_deref(), Some("m:cli:chat"));
        assert!(
            boot.brief
                .as_deref()
                .unwrap_or_default()
                .contains("the brief body"),
            "the brief drives the peer's first turn: {:?}",
            boot.brief,
        );

        let plain = stage_peer(
            &peers_root,
            &workspace,
            "plain",
            Some("plain"),
            Some("m:cli:chat"),
            "b",
            false,
            None,
            None,
        )
        .expect("stage");
        let boot = read_peer_boot(&peers_root, &plain.slug).expect("staged peer boots");
        assert_eq!(boot.goal_id, None);
        assert_eq!(
            boot.task_id, None,
            "no goal must never leave a dangling task"
        );
    }

    /// An unstaged slug has no boot context — the host must not invent one and
    /// start an agent against a directory that was never staged.
    #[test]
    fn should_not_boot_a_peer_that_was_never_staged() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_peer_boot(Path::new(temp.path()), "ghost"), None);
    }
}
