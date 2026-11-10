//! Peer-agent staging, addressing, and parked-prompt plumbing.
//!
//! Lifted VERBATIM out of the `api`-gated `api::ui_protocol` tree (Phase 3 of
//! bringing peer-agent goal into `octos chat`, mirroring the Phase 0 autonomy
//! extraction and the Phase 3 `crate::contracts` extraction). No logic changed:
//! only module placement and item visibility (`pub(crate)`), so the serve/WS
//! path keeps calling the exact same functions through a glob re-import.
//!
//! What stayed behind in `api::ui_protocol`: everything that touches `AppState`
//! or a `WsConnection` — `register_peer_wire_session`, `evict_peer_wire_session`,
//! `wake_master_on_peer_awaiting_input`, `write_peer_result_if_peer_session`,
//! the `peer/prepare` + `peer/gather` RPC handlers, the fleet-synthesis
//! scheduler, the model-lane provider builders (they need `SessionRuntime`),
//! and `build_peer_close_callback`. Those are genuinely serve-shaped.
//!
//! What moved: the pure filesystem/registry layer that a peer HOST needs
//! regardless of transport —
//!
//! * addressing: [`peer_slug_is_safe`], [`name_to_slug`], [`staged_peer_dir`],
//!   [`resolve_peer_name_to_slug`], [`peer_slug_and_profile`]
//! * fd-anchored peer-file I/O: [`peer_io`]
//! * staging: [`stage_peer`] and the `peer_handoff` callback builder
//! * the blackboard reader + `peer_list` renderer
//! * the parked-prompt projection and [`peer_respond_resolve`]
//! * the process-global [`peer_wire_registry`] mapping `"{profile}:peer:{slug}"`
//!   to the peer's live `SessionKey`
//!
//! The wire registry and `crate::contracts::contract_stores()` are BOTH
//! process-global `OnceLock`s. That is the whole reason a single-process
//! `octos chat --peers` can work: the peer's own approval/question requester
//! registers its oneshot in the same registry the master's `peer_respond`
//! resolves it from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::Utc;
use octos_core::SessionKey;
use octos_core::ui_protocol::{
    ApprovalDecidedEvent, ApprovalDecision, ApprovalId, PeerStagedEvent, RpcError,
    UserQuestionRespondParams,
};
use tracing::{debug, info, warn};

use crate::autonomy::agent_orchestrator::default_agent_orchestrator;
use crate::build_cache::pool::{BuildCacheConfig, Slot, SlotOutcome};
use crate::contracts::UiProtocolContractStores;

pub(crate) mod app_binding;
pub(crate) mod host_tools;
pub(crate) mod purge;
mod recovery;
pub(crate) mod session_tool_list;
pub(crate) mod shared_history;
pub(crate) mod turn_origin;
pub(crate) mod workspace_team;
pub(crate) use recovery::*;
// task-evo-peer-turn-status — the typed lifetime projection lives in
// `recovery` (next to its writers); the derivation below uses both.
use recovery::{LifetimePhase, trusted_lifetime_projection};

/// Cap a string at `cap` bytes on a char boundary; returns (text, truncated).
///
/// Not peer-specific, but every remaining caller is (`peer_pending_prompt_summary`,
/// `read_peer_blackboard`, `compose_peer_list_text`), so it travelled with them
/// rather than growing a third home. `api::ui_protocol` picks it back up
/// through its `crate::peers::*` glob.
pub(crate) fn capped_utf8(text: String, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut cut = cap;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut text = text;
    text.truncate(cut);
    (text, true)
}

/// #436 — serve-side registry mapping `"{profile}:peer:{slug}"` → the peer
/// session's wire `SessionKey`, populated on `session/open` for `peer-<slug>`
/// sessions. `peer_send_input` reads this to resolve a slug to the
/// continuation-queue key it enqueues an injected turn under: the serve
/// process has no gateway `ActorRegistry` to populate the inbox registry, so
/// the tool delivers via the master continuation queue instead. A stale entry
/// (a peer that has since closed) is harmless — the enqueued continuation is
/// durable and drains when the peer next reconnects — so entries are not
/// evicted on disconnect; a bounded cap prevents unbounded growth on a
/// long-lived serve that opens many distinct peers.
#[derive(Default)]
pub(crate) struct PeerWireRegistry {
    pub(crate) by_key: std::sync::Mutex<HashMap<String, SessionKey>>,
}

/// Soft cap on the peer-wire registry. A new key past the cap is dropped (that
/// peer is not injectable until re-opened); existing keys still refresh.
pub(crate) const PEER_WIRE_REGISTRY_MAX: usize = 8192;

impl PeerWireRegistry {
    /// Register (or UPDATE) the slug→wire mapping. Latest open wins (#436 P1
    /// #1): a reconnect under a fresh client-chosen wire key overwrites the
    /// prior mapping so resolution always targets the CURRENT session.
    pub(crate) fn register(&self, key: String, session_id: SessionKey) {
        let mut map = self
            .by_key
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if map.len() >= PEER_WIRE_REGISTRY_MAX && !map.contains_key(&key) {
            tracing::warn!(
                key = %key,
                cap = PEER_WIRE_REGISTRY_MAX,
                "peer wire registry at capacity; skipping new peer registration"
            );
            return;
        }
        map.insert(key, session_id);
    }

    pub(crate) fn resolve(&self, key: &str) -> Option<SessionKey> {
        self.by_key
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .cloned()
    }

    /// Evict the mapping for `key` ONLY when it still points at `session_id`
    /// (#436 P1 #5). The conditional guard is race-safe: if the peer already
    /// reopened under a newer wire key (register overwrote the value), a late
    /// close of the OLD session must not clobber the fresh mapping. Returns
    /// whether an entry was removed. Also frees a slot against the cap.
    #[cfg(feature = "api")]
    pub(crate) fn evict_if_value(&self, key: &str, session_id: &SessionKey) -> bool {
        let mut map = self
            .by_key
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if map.get(key) == Some(session_id) {
            map.remove(key);
            true
        } else {
            false
        }
    }
}

pub(crate) fn peer_wire_registry() -> &'static PeerWireRegistry {
    static PEER_WIRE_REGISTRY: OnceLock<PeerWireRegistry> = OnceLock::new();
    PEER_WIRE_REGISTRY.get_or_init(PeerWireRegistry::default)
}

/// #1868 Phase 1 — maps a staged peer to the `TaskSupervisor` task enrolled on
/// its MASTER's session, so the peer's retirement can mark that task terminal.
///
/// Peers were never registered with the supervisor at all (`goal_tool.rs` had
/// zero supervisor references), which is why the master's task count never
/// showed a peer, why peers had no cancel token, and why the in-flight liveness
/// rule added in #2014 had to read `state.agents` directly instead of asking
/// the supervisor. Enrolling them puts all three kinds of supervised work —
/// sub-agents, background tasks, peers — behind one source of truth.
///
/// Keyed by the same `"{profile}:peer:{slug}"` string as [`peer_wire_registry`]
/// so both registries are addressed identically; the VALUE is the supervisor's
/// task id. Process-global for the same reason the wire registry is: a peer is
/// staged on one path and retired on another.
/// What the registry holds for one staged peer: the supervisor task id, plus
/// the liveness lease that keeps the per-turn orphan sweep off it (#2035).
///
/// The lease lives HERE, for exactly the peer's supervised lifetime — bound at
/// staging, dropped by `take` on the close path. Holding it anywhere shorter
/// (the staging closure, the turn's supervisor) would put it out of scope
/// while the peer is still working, which is the defect.
pub(crate) struct PeerTaskBinding {
    task_id: String,
    _liveness: octos_agent::TaskLivenessLease,
}

#[derive(Default)]
pub(crate) struct PeerTaskRegistry {
    pub(crate) by_key: std::sync::Mutex<HashMap<String, PeerTaskBinding>>,
}

impl PeerTaskRegistry {
    /// Bind `key` to a supervisor task id and take a liveness lease on it. A
    /// re-stage under the same key overwrites, mirroring
    /// [`PeerWireRegistry::register`]'s latest-open-wins.
    pub(crate) fn bind(&self, key: String, task_id: String) {
        let mut map = self
            .by_key
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if map.len() >= PEER_WIRE_REGISTRY_MAX && !map.contains_key(&key) {
            tracing::warn!(
                key = %key,
                cap = PEER_WIRE_REGISTRY_MAX,
                "peer task registry at capacity; peer will not be supervised"
            );
            return;
        }
        // A re-stage that resolves to the SAME task id must keep the existing
        // lease: replacing it would drop the old one AFTER the new insert, and
        // `Drop` clears the live-set entry unconditionally — briefly unleasing
        // a task that is still live. Different id ⇒ the old peer is superseded
        // and its lease should indeed be released.
        if map.get(&key).is_some_and(|bound| bound.task_id == task_id) {
            return;
        }
        let binding = PeerTaskBinding {
            _liveness: octos_agent::TaskLivenessLease::new(task_id.clone()),
            task_id,
        };
        map.insert(key, binding);
    }

    /// Take the task id for `key`, removing the binding and releasing its
    /// liveness lease. Retirement is exactly-once: a second close finds nothing
    /// and must NOT re-mark a task terminal (the supervisor's terminal guard
    /// would reject it anyway, but a second `mark_completed` would also race a
    /// task id later reused).
    pub(crate) fn take(&self, key: &str) -> Option<String> {
        self.by_key
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(key)
            .map(|bound| bound.task_id)
    }

    pub(crate) fn take_if_task(&self, key: &str, task_id: &str) -> Option<String> {
        let mut map = self
            .by_key
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if map.get(key).is_some_and(|bound| bound.task_id == task_id) {
            map.remove(key).map(|bound| bound.task_id)
        } else {
            None
        }
    }
}

pub(crate) fn peer_task_registry() -> &'static PeerTaskRegistry {
    static PEER_TASK_REGISTRY: OnceLock<PeerTaskRegistry> = OnceLock::new();
    PEER_TASK_REGISTRY.get_or_init(PeerTaskRegistry::default)
}

/// #1868 Phase 1 — bind a staged peer to a supervised task.
///
/// The task is registered against the MASTER's session, not the peer's: it
/// represents "this master has a peer working for it", which is exactly the
/// question liveness asks.
///
/// Returns `None` when the supervisor refuses the registration — `register`
/// signals that with an EMPTY-STRING sentinel, and binding it would make the
/// close path try to retire a task that never existed. The peer then runs
/// UNSUPERVISED (no task row, no cancel token); callers should say so loudly.
///
/// Production registration goes through the strict variant
/// ([`bind_peer_supervised_task_with_workspace_strict`]); only tests bind
/// this legacy shape, so the api-gated `allow(dead_code)` is load-bearing
/// (rustc-verified: without it, an api lib build warns "never used").
#[cfg(any(feature = "api", test))]
#[cfg_attr(feature = "api", allow(dead_code))]
pub(crate) fn bind_peer_supervised_task(
    supervisor: &octos_agent::TaskSupervisor,
    registry_key: String,
    master_session: &str,
) -> Option<String> {
    let task_id = supervisor.register("peer_handoff", &registry_key, Some(master_session));
    if task_id.is_empty() {
        return None;
    }
    peer_task_registry().bind(registry_key, task_id.clone());
    Some(task_id)
}

/// #21 (round-4, codex #17 B3) — STRICT workspace-scoped binding: the FIRST
/// durable task row already carries the workspace stamp, and a failed first
/// write rolls the whole registration back (no half-bound task, no registry
/// entry). Returns the task id on success, `None` on refusal, and
/// `Err(io)` when the durable write failed (the caller must surface the
/// bind failure — the peer still stages, but unsupervised).
#[cfg(any(feature = "api", test))]
pub(crate) fn bind_peer_supervised_task_with_workspace_strict(
    supervisor: &octos_agent::TaskSupervisor,
    registry_key: String,
    master_session: &str,
    master_workspace_scope: Option<&str>,
) -> Result<Option<String>, std::io::Error> {
    match supervisor.try_register_peer_with_workspace(
        "peer_handoff",
        &registry_key,
        Some(master_session),
        master_workspace_scope,
    ) {
        Ok(task_id) => {
            peer_task_registry().bind(registry_key, task_id.clone());
            Ok(Some(task_id))
        }
        Err(octos_agent::RegisterTaskError::WorkspacePersistFailed { source, .. }) => {
            Err(std::io::Error::other(source))
        }
        // Cap/parent-terminal refusals: the supervisor refused the
        // registration outright (same shape as the legacy empty-string
        // sentinel) — not an I/O failure.
        Err(_) => Ok(None),
    }
}

/// #21 (round-4, codex #17 B3) — LOSSLESS workspace scope encoding.
///
/// `Path::to_str()` collapses every non-UTF-8 path to `None`, which made
/// the stamp and the `/stop` purge argument disagree with each other on
/// exotic cwds (stamp side: `None`; purge side: possibly `Some` via a
/// different derivation — both endpoints silently losing the workspace
/// identity). New wire stamps are version-tagged hex encodings of the path's raw
/// [`OsStr`] bytes: every representable path round-trips exactly, and two
/// DIFFERENT non-UTF-8 roots encode to two DIFFERENT scopes (never
/// accidentally clearing each other). An empty/absent root encodes to
/// `None` (unstamped, legacy shape).
pub(crate) fn workspace_scope_encode(root: &std::path::Path) -> Option<String> {
    crate::autonomy::workspace_scope::WorkspaceScope::peer_stamp(root)
}

/// #1868 Phase 1 — retire the task bound at staging, on the CLOSE path only.
///
/// Deliberately not called on turn terminal: a peer runs many turns and would
/// otherwise retire after its first, leaving every later turn unsupervised
/// while the master still believed a peer was working for it.
///
/// Retirement is exactly-once — `take` removes the binding, so a second close
/// finds nothing rather than re-marking a terminal task (or, worse, a task id
/// later reused). Returns the retired task id.
#[cfg(any(feature = "api", test))]
pub(crate) fn retire_peer_supervised_task(
    supervisor: &octos_agent::TaskSupervisor,
    profile_id: &str,
    slug: &str,
) -> Option<String> {
    let task_id = peer_task_registry().take(&peer_wire_key(profile_id, slug))?;
    supervisor.mark_completed(&task_id, Vec::new());
    Some(task_id)
}

/// Registry key for a peer session: `"{profile}:peer:{slug}"` (mirrors the
/// gateway inbox registry's key construction in `session_actor`).
pub(crate) fn peer_wire_key(profile_id: &str, slug: &str) -> String {
    format!("{profile_id}:peer:{slug}")
}

/// #14 (codex round 2) — durable task-id binding leaf in the staged dir.
///
/// Written at REGISTRATION time (see [`persist_peer_task_id_binding`]) so the
/// restore-time adoption sweep can exact-match the parked row's task id
/// against what the dir was staged FOR. A forged or stale `tool_call_id`
/// pointing at another peer's staged dir fails that match and is refused.
pub(crate) const PEER_TASK_ID_LEAF: &str = "task-id";

/// New staging starts at `pending`; only a durably written task-id permits
/// finalization to that id. An absent state leaf preserves legacy staging.
const PEER_TASK_ID_STATE_LEAF: &str = "task-id-state";

/// Registration-time task-id binding; callers must handle every write failure.
/// `true` means the write succeeded — file data synced and, on Unix, the
/// directory entry too (Windows has no directory sync, #2515).
#[must_use]
pub(crate) fn persist_peer_task_id_binding(peers_root: &Path, slug: &str, task_id: &str) -> bool {
    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        warn!(
            slug,
            task_id, "peer task-id binding not persisted: no safe staged directory"
        );
        return false;
    };
    if let Err(error) = peer_io::write_peer_file_durable(&dir, PEER_TASK_ID_LEAF, task_id) {
        warn!(slug, task_id, %error, "peer task-id binding not persisted");
        return false;
    }
    true
}

/// #22: gate automatic adoption BEFORE registering a new supervised task.
///
/// Policy: if the task-id write fails, keep the fully registered task under
/// supervision but leave this slug unadoptable. This avoids pretending that
/// two independent stores (task ledger and staged directory) can roll back
/// atomically. Failure to persist the initial gate returns Err BEFORE any
/// registration/binding, preserving #21's strict failure contract. Every
/// post-registration crash window retains `pending` until the exact task-id
/// is durable; Missing task-id can therefore never masquerade as legacy.
#[cfg(any(feature = "api", test))]
pub(crate) fn bind_staged_peer_supervised_task(
    supervisor: &octos_agent::TaskSupervisor,
    registry_key: String,
    master_session: &str,
    workspace_scope: Option<&str>,
    peers_root: &Path,
    slug: &str,
) -> std::io::Result<Option<String>> {
    let dir = staged_peer_dir(peers_root, slug).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no safe staged peer directory",
        )
    })?;
    peer_io::write_peer_file_durable(&dir, PEER_TASK_ID_STATE_LEAF, "pending")?;
    let Some(task_id) = bind_peer_supervised_task_with_workspace_strict(
        supervisor,
        registry_key,
        master_session,
        workspace_scope,
    )?
    else {
        return Ok(None);
    };
    if !persist_peer_task_id_binding(peers_root, slug, &task_id) {
        warn!(slug, %task_id,
            "peer remains fully supervised but automatic adoption is disabled: task-id write failed");
    } else if let Err(error) =
        peer_io::write_peer_file_durable(&dir, PEER_TASK_ID_STATE_LEAF, &task_id)
    {
        // If rename landed but directory fsync failed, the visible final state
        // still names a durable task-id; after a crash it is either final or
        // pending. Neither outcome permits legacy fallback.
        warn!(slug, %task_id, %error,
            "peer adoption gate finalization failed; task remains fully supervised");
    }
    Ok(Some(task_id))
}

/// #8 (continuation-replay review) — adopt orphaned `peer_handoff` tasks whose
/// result already sits on the blackboard.
///
/// The restart orphan sweep parks every `peer_handoff` row it cannot attribute
/// to a live process (`task_supervisor.rs`: "orphaned across restart"). The
/// task row itself carries NO staging path (registration `tool_input` is
/// `None`), so the sweep cannot decide between "peer still working, result
/// coming" and "peer finished before the restart and already wrote its
/// `result.md`". Only octos-cli knows the `peers/<slug>/` layout, so the
/// disambiguation lives HERE: a parked peer task whose staged dir already
/// carries a REGULAR `result.md` has nothing left to wait for and is adopted
/// — `mark_completed` with the blackboard file as its output — instead of
/// idling in `Parked` until a client happens to gather it.
///
/// Slug extraction: the row's `tool_call_id` IS the wire key
/// [`peer_wire_key`] (`"{profile}:peer:{slug}"`). Profile ids may contain
/// colons, so the split is on the LAST `:peer:` occurrence (`rsplit_once`).
/// The slug is then re-validated with [`peer_slug_is_safe`] and resolved
/// through [`staged_peer_dir`], so a malformed or hostile key can never steer
/// the lookup out of `peers/`.
///
/// #14 (codex round 2) — adoption-time IDENTITY validation, beyond the bare
/// slug lookup the #8 sweep had:
///
/// 1. **Full wire-key match.** The row's `tool_call_id` must EQUAL
///    `peer_wire_key(expected_profile, slug)` — the `rsplit` slug extraction
///    alone left the prefix unverified, so a key forged under a DIFFERENT
///    profile (`evil:peer:ourslug`) still resolved OUR staged dir.
/// 2. **Originator cross-check.** The row's `parent_session_key` must equal
///    the staged dir's `originator` record when that record exists; with no
///    record (a profile-scoped `peer_prepare` records none — see
///    `peer_send_input` authorization) it must equal `expected_master_session`
///    instead. A stale row replayed under another session is refused.
/// 3. **Persisted task-id binding.** Registration writes `peers/<slug>/
///    task-id` ([`persist_peer_task_id_binding`]); when present, adoption
///    requires it to EQUAL the row's task id, so a parked row whose wire key
///    names a peer but whose task was never staged into THAT dir cannot claim
///    its result. An ABSENT file permits legacy (pre-#14) fallback to checks
///    1+2 only for a unique Parked wire-key candidate with no new staging
///    finalization gate. Invalid identity files never permit this fallback.
///
/// ## B: settle-binding restoration BEFORE `mark_completed`
///
/// The change-feed settle listener resolves the task→goal binding from the
/// IN-MEMORY `goal_task_ledger_bindings` map, which a restart empties. The
/// staged dir's `goal` file (`goal_id\ntask_id`, written by [`stage_peer`])
/// survives; when its task-id line matches THIS row (or is blank — the file
/// predates the task-id column), the sweep re-stashes the binding via
/// [`default_agent_orchestrator().record_goal_task_registration`] FIRST, so
/// the adoption's terminal transition settles the goal ledger row instead of
/// leaving it `running` forever. That recorder is idempotent per task id
/// (re-stash installs a fresh generation), so double-binding is safe.
///
/// `TaskStatus::Parked` is NOT terminal, so `mark_completed` is the legal,
/// documented adoption path. Filtering on `status == Parked` makes the sweep
/// naturally idempotent: an already-adopted (Completed) row is skipped on any
/// later restore instead of re-marking a terminal task. Returns the number of
/// tasks adopted. A parked peer WITHOUT a result stays `Parked` (debug log
/// only) — the client may still adopt it. Ungated (unlike
/// [`bind_peer_supervised_task`]): the gateway session actor wires it without
/// the `api` feature.
///
/// #15 RA-4 — ORIGINATOR-LESS FALLBACK contract: when the staged dir carries
/// no `originator` leaf (legacy staging predating #14's binding files), the
/// adoption falls back to comparing the ROW's `parent_session_key` against
/// `expected_master_session` — so the caller MUST pass the master session
/// the row is expected to belong to (the installer's own session key), and
/// rows whose `parent_session_key` names another master are refused. When the
/// leaf exists it is authoritative and this fallback never runs.
pub(crate) fn adopt_parked_peer_tasks_with_results(
    supervisor: &octos_agent::TaskSupervisor,
    expected_profile: &str,
    expected_master_session: &str,
    profile_data_dir: &Path,
    restored: &[octos_agent::BackgroundTask],
) -> usize {
    use peer_io::PeerFileRead;

    let peers_root = profile_data_dir.join("peers");
    // Freeze candidate counts before any terminal transition. Picking one
    // ambiguous legacy row would let a later sweep consume the same result
    // for its twin; leave ALL such candidates Parked until identity is known.
    let mut candidates_by_wire_key = HashMap::<&str, usize>::new();
    for task in restored.iter().filter(|task| {
        task.tool_name == "peer_handoff" && task.status == octos_agent::TaskStatus::Parked
    }) {
        *candidates_by_wire_key
            .entry(&task.tool_call_id)
            .or_default() += 1;
    }
    let mut adopted = 0usize;
    for task in restored {
        if task.tool_name != "peer_handoff" || task.status != octos_agent::TaskStatus::Parked {
            continue;
        }
        let Some(slug) = task
            .tool_call_id
            .rsplit_once(":peer:")
            .map(|(_, slug)| slug)
        else {
            debug!(
                task_id = %task.id,
                tool_call_id = %task.tool_call_id,
                "parked peer task without a ':peer:' wire key; cannot locate its blackboard"
            );
            continue;
        };
        if !peer_slug_is_safe(slug) {
            debug!(
                task_id = %task.id,
                tool_call_id = %task.tool_call_id,
                "parked peer task wire key carries an unsafe slug; refusing path lookup"
            );
            continue;
        }
        // #14 check 1 — the FULL wire key must match what THIS profile would
        // have minted for this slug. A prefix forged under another profile
        // resolves the same staged dir post-rsplit, so the equality check is
        // what fences it.
        if task.tool_call_id != peer_wire_key(expected_profile, slug) {
            debug!(
                task_id = %task.id,
                tool_call_id = %task.tool_call_id,
                expected_profile,
                "parked peer task wire key does not match the expected profile; \
                 refusing adoption"
            );
            continue;
        }
        let Some(dir) = staged_peer_dir(&peers_root, slug) else {
            debug!(
                task_id = %task.id,
                slug,
                "parked peer task has no staged dir (or it failed the safety gate); leaving Parked"
            );
            continue;
        };
        // #39: modern logical lifetimes are recovered before the orphan
        // sweep using their generation and result digest. A surviving parked
        // row has no such proof; an old result.md must not override it.
        // Only a missing receipt permits the branch's legacy adoption path.
        if !matches!(
            peer_io::read_peer_identity_file(&dir, "lifetime.json"),
            PeerFileRead::Missing
        ) {
            debug!(task_id = %task.id, slug,
                "peer lifetime authority prevents legacy result adoption");
            continue;
        }
        // #14 check 2 — the staged dir's originator must own this row. A
        // recorded originator is authoritative (cross-session rows are
        // refused even when the restoring session IS the supervisor's
        // session, e.g. two masters staged into the same profile); with no
        // record, fall back to the installer's session.
        let task_master = task.parent_session_key.as_deref().unwrap_or("");
        let originator_matches = match peer_io::read_peer_identity_file(&dir, "originator") {
            PeerFileRead::Valid(recorded) => recorded.trim() == task_master,
            PeerFileRead::Missing => task_master == expected_master_session,
            PeerFileRead::Invalid(error) => {
                warn!(task_id = %task.id, slug, %error,
                        "invalid peer originator; refusing adoption, leaving Parked");
                continue;
            }
        };
        if !originator_matches {
            debug!(
                task_id = %task.id,
                slug,
                task_master,
                "parked peer task's master session does not own the staged dir; \
                 refusing adoption"
            );
            continue;
        }
        let has_task_id = match peer_io::read_peer_identity_file(&dir, PEER_TASK_ID_LEAF) {
            PeerFileRead::Valid(recorded_id) if recorded_id.trim() == task.id => true,
            PeerFileRead::Valid(recorded_id) => {
                debug!(task_id = %task.id, recorded_id = %recorded_id.trim(), slug,
                    "parked peer task id does not match the staged binding; refusing adoption");
                continue;
            }
            PeerFileRead::Invalid(error) => {
                warn!(task_id = %task.id, slug, %error,
                    "invalid peer task-id; refusing adoption, leaving Parked");
                continue;
            }
            PeerFileRead::Missing => {
                if candidates_by_wire_key.get(task.tool_call_id.as_str()) != Some(&1) {
                    warn!(task_id = %task.id, slug,
                        "ambiguous legacy peer wire key; refusing adoption, leaving Parked");
                    continue;
                }
                false
            }
        };
        match peer_io::read_peer_identity_file(&dir, PEER_TASK_ID_STATE_LEAF) {
            PeerFileRead::Missing => {} // Legacy staging has no finalization gate.
            PeerFileRead::Valid(state) if has_task_id && state.trim() == task.id => {}
            PeerFileRead::Valid(_) => {
                warn!(task_id = %task.id, slug,
                    "peer task-id binding is not finalized; refusing adoption, leaving Parked");
                continue;
            }
            PeerFileRead::Invalid(error) => {
                warn!(task_id = %task.id, slug, %error,
                    "invalid peer adoption gate; refusing adoption, leaving Parked");
                continue;
            }
        }
        if !peer_io::peer_regular_file_exists(&dir, "result.md") {
            debug!(
                task_id = %task.id,
                slug,
                "parked peer task has no result.md yet; leaving Parked (client may still adopt)"
            );
            continue;
        }
        // #14 B — restore the in-memory task→goal settle binding BEFORE the
        // terminal transition, so the change-feed settle lands the ledger
        // row. The `goal` file is authoritative only for the task it was
        // staged with (its second line); a blank line predates the task-id
        // column, and check 3 already vouched for the row when the `task-id`
        // leaf exists.
        if let Some(goal_body) =
            peer_io::read_peer_file(&dir, "goal", peer_io::PEER_FILE_READ_CAP_SMALL)
        {
            let mut lines = goal_body.lines();
            let goal_id = lines.next().map(str::trim).unwrap_or("");
            let bound_task_id = lines.next().map(str::trim).unwrap_or("");
            if !goal_id.is_empty() && (bound_task_id.is_empty() || bound_task_id == task.id) {
                default_agent_orchestrator().record_goal_task_registration(
                    profile_data_dir,
                    expected_profile,
                    goal_id,
                    task,
                );
            }
        }
        let result_path = dir.join("result.md").display().to_string();
        supervisor.mark_completed(&task.id, vec![result_path]);
        info!(
            task_id = %task.id,
            slug,
            "orphaned peer adopted: result.md already on the blackboard"
        );
        adopted += 1;
    }
    adopted
}

/// Split a `peer-<slug>` session key into `(profile_id, slug)`, or `None` for
/// a non-peer or unprofiled session.
/// The peer whose token budget (#2500) a session's turns are charged to: the
/// peer itself for `peer-<slug>`, and the OWNING peer for a request context
/// `peerctx-<slug>.<id>` (UPCR-2026-034/035), so an app cannot spend outside
/// its budget by working through contexts.
pub(crate) fn budget_peer_slug(session_id: &SessionKey) -> Option<&str> {
    if let Some((_, slug)) = peer_slug_and_profile(session_id) {
        return Some(slug);
    }
    let topic = session_id.topic()?;
    topic.strip_prefix(app_binding::PEER_CONTEXT_TOPIC_PREFIX)?;
    let (slug, context_id) = app_binding::parse_context_topic(topic)?;
    (peer_slug_is_safe(slug) && app_binding::validate_context_id(context_id).is_ok())
        .then_some(slug)
}

pub(crate) fn peer_slug_and_profile(session_id: &SessionKey) -> Option<(&str, &str)> {
    // NOT a peer session. The overwhelmingly common case, and the only one where
    // `None` is uninteresting — every caller correctly skips peer bookkeeping.
    let slug = session_id
        .topic()
        .and_then(|topic| topic.strip_prefix("peer-"))?;

    // Past this point the topic SAYS `peer-…`, so something intended a peer
    // session. Each rejection below still returns `None` (callers must treat it
    // as a non-peer session — that is the #436 fence), but it is now LOUD.
    //
    // Why: all ~10 callers do `let Some(..) = .. else { return }`, which is
    // right for "not a peer" and silently wrong for "malformed peer key". A peer
    // whose key is rejected here keeps running and looks healthy while its wire
    // registration, result recording, blackboard writes, awaiting-input wake and
    // fleet synthesis ALL no-op. That failure is invisible at every layer — the
    // peer produces work nobody records — so the only place it can be reported
    // is here, where the reason is still known.

    // #436 security — the topic-derived slug feeds `Path::join` (closed marker,
    // peers dir) and the wire-key registry. Reject an unsafe one (e.g. a
    // `peer-/tmp/x` or `peer-../x` topic) HERE so EVERY caller treats it as a
    // NON-peer session rather than a path that escapes `peers/`.
    if slug.is_empty() || !peer_slug_is_safe(slug) {
        warn!(
            session = %session_id,
            "peer session key has an unusable slug; peer bookkeeping (results, \
             blackboard, wake, synthesis) is DISABLED for this session"
        );
        return None;
    }

    // A peer key with no profile component. Nothing downstream can address the
    // peer without one — `peers_root` is per-profile and the wire key is
    // `{profile}:peer:{slug}` — so this silently disables the same bookkeeping.
    let Some(profile_id) = session_id.profile_id() else {
        warn!(
            session = %session_id,
            slug,
            "peer session key has NO profile component; peer bookkeeping \
             (results, blackboard, wake, synthesis) is DISABLED for this \
             session. Peer keys must be `{{profile}}:{{channel}}:{{chat}}#peer-{{slug}}`"
        );
        return None;
    };
    Some((profile_id, slug))
}

/// Upper bound (bytes) on a peer slug — a slug is a short handle, not a
/// payload. Aligns with [`name_to_slug`]'s cap so a derived slug always
/// satisfies [`peer_slug_is_safe`].
pub(crate) const PEER_SLUG_MAX_BYTES: usize = 64;

/// Reject a peer slug that could escape `peers/` or mis-key the wire registry:
/// empty, over-long, a `.`/`..` component, any path separator / NUL, a drive /
/// alternate-data-stream `:`, a control char (`< 0x20`), or a trailing `.`/
/// space (which some filesystems strip → a DIFFERENT real path). A slug is a
/// single path component (a dir name under `peers/`). New slugs emitted by
/// `reserve_peer_dir` / [`name_to_slug`] contain only `[a-z0-9-]`; legacy
/// percent-escaped handles remain readable. Called at the TOP of
/// the `peer_close` / `peer_send_input` callbacks (after resolving a name to a
/// slug) before any path join or wire-key op. Mirrors
/// `octos_core::session_scope::is_safe_session_id`, hardened for cross-platform.
pub(crate) fn peer_slug_is_safe(slug: &str) -> bool {
    if slug.is_empty() || slug.len() > PEER_SLUG_MAX_BYTES {
        return false;
    }
    if slug == "." || slug == ".." {
        return false;
    }
    // A trailing dot or space aliases to a different real path on Windows.
    if slug.ends_with('.') || slug.ends_with(' ') {
        return false;
    }
    // Path separators, NUL/control chars (incl. 0x7f DEL), and the drive/ADS colon.
    !slug
        .bytes()
        .any(|b| matches!(b, b'/' | b'\\' | b':') || b < 0x20 || b == 0x7f)
}

/// Derive a filesystem/URL-safe ASCII slug from a peer's display NAME:
/// lowercase, each run of non-`[a-z0-9]` collapses to a single `-`, trim
/// leading/trailing `-`, cap at [`PEER_SLUG_MAX_BYTES`] bytes. A name with NO
/// ASCII alphanumerics (a CJK / emoji display name — `爱迪生`, `🔬`) has no
/// readable slug, so it falls back to a stable FNV-1a hash of the trimmed,
/// lowercased name: `peer-<16 hex>`. The DISPLAY name (unicode, stored in
/// `peers/<slug>/name`) is what users see and address; the slug is only the
/// directory handle and resolution is by the name file, so the same name always
/// yields the same base slug. Named peers reject duplicates; title-seeded
/// peers add a bounded numeric suffix when reserving a directory. Returns
/// `None` ONLY for a blank / whitespace-only name (which has no peer at all).
pub(crate) fn name_to_slug(name: &str) -> Option<String> {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !slug.is_empty() && !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let mut slug = slug.trim_matches('-').to_owned();
    // All retained bytes are ASCII, so a byte cut is a char boundary; re-trim a
    // dash the cut may have exposed.
    if slug.len() > PEER_SLUG_MAX_BYTES {
        slug.truncate(PEER_SLUG_MAX_BYTES);
        slug = slug.trim_end_matches('-').to_owned();
    }
    if slug.is_empty() {
        // No ASCII handle (pure CJK / emoji / punctuation): hash the normalized
        // unicode name into a stable ASCII slug so the peer is still addressable
        // (by its display name, via the `name` file).
        let key = name.trim().to_lowercase();
        if key.is_empty() {
            return None; // blank / whitespace-only — not a name at all
        }
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in key.bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        slug = format!("peer-{hash:016x}");
    }
    peer_slug_is_safe(&slug).then_some(slug)
}

/// The REAL, staged peer directory for `slug` under `peers_root`, or `None`
/// when it is not safe to touch. EVERY peer-dir access — reads AND the
/// close-marker write — routes through this so a hostile or stray
/// `peers/<slug>` SYMLINK can never redirect I/O outside `peers_root`. Returns
/// `Some(dir)` ONLY when: [`peer_slug_is_safe`], `peers_root.join(slug)` is a
/// REAL directory that is NOT a symlink (`symlink_metadata` inspects the LINK,
/// not its target), and it carries the `brief.md` staging contract.
pub(crate) fn staged_peer_dir(peers_root: &Path, slug: &str) -> Option<PathBuf> {
    if !peer_slug_is_safe(slug) {
        return None;
    }
    let dir = peers_root.join(slug);
    let meta = std::fs::symlink_metadata(&dir).ok()?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }
    if !peer_io::peer_regular_file_exists(&dir, "brief.md") {
        return None;
    }
    Some(dir)
}

/// fd-anchored, symlink/FIFO/DoS-safe I/O for the per-session peer files under
/// `peers/<slug>/` (octos#1824). [`staged_peer_dir`] validates the `<slug>`
/// directory by PATH; a subsequent path-based `std::fs` read/write then races a
/// parent swap — an attacker who can write under `peers/` replaces `<slug>` (or
/// a leaf) with a symlink between the check and the I/O, and the plain read/
/// `atomic_write` follows the swap. It also accepts a FIFO/device leaf, so a
/// hostile `model`/`brief.md` FIFO parks a turn on an unbounded blocking read.
///
/// Every op here re-opens the peer DIR fd `O_NOFOLLOW|O_DIRECTORY` (so a
/// symlinked `<slug>` is refused, not followed) and resolves the LEAF relative
/// to that pinned inode with `openat`/`renameat`/`unlinkat` — no path is ever
/// re-walked after the anchor, closing the parent-swap race. Reads open
/// `O_NOFOLLOW|O_NONBLOCK`, `fstat` the opened handle, require a regular file
/// (`S_ISREG` — a FIFO/device/dir/symlink is refused before any `read`), and
/// bound the read to a cap. This mirrors the anchored pattern already used by
/// `api::memory_panel` for the memory-panel reads.
pub(crate) mod peer_io {
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A cancelled host-tool task may still append its audit after the turn
    /// reports a terminal. Serialize leaf mutations with final directory
    /// removal: neither an append nor an atomic writer's temporary file may
    /// appear while `peer/purge` is enumerating and deleting the directory.
    /// This is separate from the round-publication lock (whose holders call
    /// these helpers). Idle entries retain only weak references and are reaped.
    pub(crate) fn mutation_lock(path: &Path) -> std::sync::Arc<std::sync::Mutex<()>> {
        use std::{
            collections::HashMap,
            path::PathBuf,
            sync::{Arc, Mutex, OnceLock, Weak},
        };
        static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
        // Normalize parent aliases without following a replaced final leaf.
        // The actual I/O still performs its own fd-anchored no-follow checks.
        let key = path
            .parent()
            .zip(path.file_name())
            .and_then(|(parent, leaf)| dunce::canonicalize(parent).ok().map(|p| p.join(leaf)))
            .unwrap_or_else(|| path.to_owned());
        let mut locks = LOCKS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        locks.retain(|_, lock| lock.strong_count() != 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    /// Read cap for the large peer files (`brief.md`, `result.md`). Over-cap
    /// content reads as absent (`None`) rather than a truncated prefix, matching
    /// `memory_panel`'s over-cap posture. 1 MiB is far above any legitimate
    /// brief/result (the write side caps `result.md` at 256 KiB and the gather
    /// display re-caps to tens of KiB).
    pub(crate) const PEER_FILE_READ_CAP_LARGE: usize = 1024 * 1024;

    /// Read cap for the small control files (`model`, `originator`, `name`,
    /// `turns.txt`, `closed`). These hold a lane key, a session id, a display
    /// name, or a compact line index — all KB-scale.
    pub(crate) const PEER_FILE_READ_CAP_SMALL: usize = 64 * 1024;

    /// Raw directory-scan budget for [`peer_dir_count_prefixed`]. A legitimate
    /// peer has a handful of `result-<n>.md` files; this only bounds a hostile
    /// flood so a directory stuffed with entries can't stall a turn.
    pub(crate) const PEER_DIR_SCAN_CAP: usize = 100_000;

    /// Process-unique suffix source for temp filenames, so concurrent
    /// atomic writes to the same leaf never collide on the `O_EXCL` create.
    static TMP_UNIQ: AtomicU64 = AtomicU64::new(0);

    /// Only a missing leaf in a successfully opened peer directory is legacy
    /// absence. Safety failures and I/O errors must not weaken identity checks.
    #[derive(Debug)]
    pub(crate) enum PeerFileRead {
        Missing,
        Valid(String),
        Invalid(std::io::Error),
    }

    /// A `.<leaf>.tmp-<pid>-<uniq>` sibling name for the atomic temp file. The
    /// leading `.` keeps it out of `result-*` globs (e.g.
    /// `count_peer_result_versions`), which a bare `<leaf>.tmp` would otherwise
    /// pollute for `result-<n>.md`.
    fn tmp_name(leaf: &str) -> String {
        let uniq = TMP_UNIQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        format!(".{leaf}.tmp-{pid}-{uniq}")
    }

    /// Read a peer leaf file, anchored on the peer dir fd. Returns the content
    /// when the leaf is a REGULAR file no larger than `cap` bytes; `None` for a
    /// symlinked dir/leaf, a FIFO/device/dir leaf, an over-cap file, invalid
    /// UTF-8, or any I/O error.
    pub(crate) fn read_peer_file(peer_dir: &Path, leaf: &str, cap: usize) -> Option<String> {
        imp::read_peer_file(peer_dir, leaf, cap).ok().flatten()
    }

    /// Control files must distinguish an absent legacy leaf from an invalid
    /// or unreadable one; treating both as absent would disable a budget.
    pub(crate) fn read_peer_control_file(
        peer_dir: &Path,
        leaf: &str,
        cap: usize,
    ) -> std::io::Result<Option<String>> {
        imp::read_peer_file(peer_dir, leaf, cap)
    }

    pub(crate) fn read_peer_identity_file(peer_dir: &Path, leaf: &str) -> PeerFileRead {
        match imp::read_peer_file(peer_dir, leaf, PEER_FILE_READ_CAP_SMALL) {
            Ok(None) => PeerFileRead::Missing,
            Ok(Some(content)) if !content.trim().is_empty() => PeerFileRead::Valid(content),
            Ok(Some(_)) => PeerFileRead::Invalid(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "empty peer identity",
            )),
            Err(error) => PeerFileRead::Invalid(error),
        }
    }

    fn read_regular_file(file: std::fs::File, cap: usize) -> std::io::Result<String> {
        use std::io::Read;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "peer leaf is not a regular file",
            ));
        }
        let mut content = String::new();
        let read = file
            .take((cap as u64).saturating_add(1))
            .read_to_string(&mut content)?;
        if read > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "peer leaf exceeds read cap",
            ));
        }
        Ok(content)
    }

    /// Atomically replace a peer leaf file (temp + fsync + rename), every step
    /// relative to the peer dir fd. A symlinked dir/leaf is refused (never
    /// followed); on any write/rename error the temp file is best-effort
    /// removed.
    pub(crate) fn write_peer_file_atomic(
        peer_dir: &Path,
        leaf: &str,
        content: &str,
    ) -> std::io::Result<()> {
        let lock = mutation_lock(peer_dir);
        let _mutation = lock.lock().unwrap_or_else(|p| p.into_inner());
        imp::write_peer_file_atomic(peer_dir, leaf, content, false)
    }

    /// Identity writes require BOTH file data and the renamed directory entry
    /// to be synced. Unlike ordinary peer output, directory-sync errors must
    /// propagate to the registration/adoption gate. Unix delivers that in
    /// full; Windows has no directory sync, so it is best-effort there — the
    /// file data is written and the rename is atomic, but sync errors never
    /// occur (#2515).
    pub(crate) fn write_peer_file_durable(
        peer_dir: &Path,
        leaf: &str,
        content: &str,
    ) -> std::io::Result<()> {
        let lock = mutation_lock(peer_dir);
        let _mutation = lock.lock().unwrap_or_else(|p| p.into_inner());
        imp::write_peer_file_atomic(peer_dir, leaf, content, true)
    }

    /// Append `line` to a peer leaf file (the `turns.txt` index), anchored on
    /// the peer dir fd. Creates the file if absent; refuses a symlinked dir/
    /// leaf and a non-regular (FIFO/device) leaf. Opened `O_NONBLOCK` so a
    /// planted FIFO fails fast instead of parking the writer on the missing
    /// reader.
    pub(crate) fn append_peer_line(peer_dir: &Path, leaf: &str, line: &str) -> std::io::Result<()> {
        let lock = mutation_lock(peer_dir);
        let _mutation = lock.lock().unwrap_or_else(|p| p.into_inner());
        imp::append_peer_line(peer_dir, leaf, line, false)
    }

    /// Append and sync a control record before another turn may be admitted.
    /// The sync is real on Unix and best-effort on Windows (#2515) — the line
    /// is written either way.
    pub(crate) fn append_peer_line_durable(
        peer_dir: &Path,
        leaf: &str,
        line: &str,
    ) -> std::io::Result<()> {
        let lock = mutation_lock(peer_dir);
        let _mutation = lock.lock().unwrap_or_else(|p| p.into_inner());
        imp::append_peer_line(peer_dir, leaf, line, true)
    }

    /// `true` when the peer leaf exists as a REGULAR file, resolved under the
    /// peer dir fd with a no-follow stat (`S_ISREG` required). A symlinked/FIFO/
    /// dir/device leaf — or a symlinked peer dir — reads as absent. Replaces the
    /// path-following `dir.join(leaf).is_file()` status probes so an existence
    /// gate (e.g. the `closed` close-marker) can't be redirected by a parent- or
    /// leaf-swap (#1824).
    pub(crate) fn peer_regular_file_exists(peer_dir: &Path, leaf: &str) -> bool {
        imp::peer_file_mtime(peer_dir, leaf).is_some()
    }

    /// The mtime of a peer leaf REGULAR file, resolved under the peer dir fd
    /// with the same no-follow `S_ISREG` gate as [`peer_regular_file_exists`].
    /// `None` for a symlinked/FIFO/dir leaf, a symlinked peer dir, or a stat
    /// error. Used where the mtime AFFECTS behavior (the ready-note freshness
    /// gate), so a swapped leaf can neither park nor mislead it.
    pub(crate) fn peer_file_mtime(peer_dir: &Path, leaf: &str) -> Option<std::time::SystemTime> {
        imp::peer_file_mtime(peer_dir, leaf)
    }

    /// Count REGULAR-file entries whose name starts with `prefix`, enumerating
    /// the peer dir through its own `O_NOFOLLOW|O_DIRECTORY` fd (`fdopendir`) —
    /// never a path `read_dir`, so swapping `<slug>` to a symlink after
    /// [`staged_peer_dir`] cannot redirect the scan into an attacker's tree
    /// (#1824). A symlinked/dir/FIFO entry matching the prefix is NOT counted
    /// (so it can't inflate a version number), and the raw scan stops after
    /// `cap` entries so a hostile flood can't stall the turn. Any failure
    /// (symlinked peer dir, open/read error) → 0.
    pub(crate) fn peer_dir_count_prefixed(peer_dir: &Path, prefix: &str, cap: usize) -> usize {
        imp::peer_dir_count_prefixed(peer_dir, prefix, cap)
    }

    /// task-evo-peer-turn-status — LIST the regular-file leaf names under
    /// `prefix` (fd-anchored, scan-capped, symlink/refuse semantics identical
    /// to [`peer_dir_count_prefixed`], #1824), sorted by name. `None` only
    /// when the dir cannot be opened or the scan errors mid-way (fail-closed:
    /// a partial list must never pose as complete).
    pub(crate) fn peer_dir_list_prefixed(
        peer_dir: &Path,
        prefix: &str,
        cap: usize,
    ) -> Option<Vec<String>> {
        imp::peer_dir_list_prefixed(peer_dir, prefix, cap).ok()
    }

    /// `true` when `peer_dir` exists as a REAL (non-symlink) directory, opened
    /// `O_NOFOLLOW|O_DIRECTORY` — a symlinked `<slug>` is refused. Anchored
    /// replacement for a path-following `is_dir()` gate on a per-slug peer dir.
    pub(crate) fn peer_dir_exists(peer_dir: &Path) -> bool {
        imp::peer_dir_exists(peer_dir)
    }

    #[cfg(unix)]
    mod imp {
        use std::ffi::CStr;
        use std::io::Write;
        use std::os::fd::OwnedFd;
        use std::os::unix::fs::OpenOptionsExt;
        use std::path::Path;

        use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, fsync, openat, renameat, unlinkat};

        /// Open the peer dir as an `O_NOFOLLOW|O_DIRECTORY` fd: a symlinked
        /// `<slug>` is refused here (belt-and-braces over `staged_peer_dir`'s
        /// path check, and the anchor that makes the leaf ops race-free).
        fn open_peer_dir(peer_dir: &Path) -> std::io::Result<OwnedFd> {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(
                    libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
                .open(peer_dir)
                .map(OwnedFd::from)
        }

        pub(crate) fn read_peer_file(
            peer_dir: &Path,
            leaf: &str,
            cap: usize,
        ) -> std::io::Result<Option<String>> {
            let dir = open_peer_dir(peer_dir)?;
            // NONBLOCK: a FIFO opened plain `O_RDONLY` blocks until a writer
            // appears — a planted FIFO must never park the caller (#1824).
            let fd = match openat(
                &dir,
                leaf,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            // fstat and bounded read use the OPENED handle, never a path rewalk.
            super::read_regular_file(std::fs::File::from(fd), cap).map(Some)
        }

        pub(crate) fn write_peer_file_atomic(
            peer_dir: &Path,
            leaf: &str,
            content: &str,
            durable: bool,
        ) -> std::io::Result<()> {
            let dir = open_peer_dir(peer_dir)?;
            let tmp = super::tmp_name(leaf);
            // O_EXCL|O_NOFOLLOW: create a fresh regular temp beside the leaf,
            // never following/clobbering a pre-existing name.
            let fd = openat(
                &dir,
                tmp.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(std::io::Error::from)?;
            let mut file = std::fs::File::from(fd);
            let written = file
                .write_all(content.as_bytes())
                .and_then(|()| file.sync_all());
            if let Err(err) = written {
                let _ = unlinkat(&dir, tmp.as_str(), AtFlags::empty());
                return Err(err);
            }
            // renameat relative to the SAME dir fd — atomic in-dir replace that
            // a parent swap cannot redirect.
            if let Err(err) = renameat(&dir, tmp.as_str(), &dir, leaf) {
                let _ = unlinkat(&dir, tmp.as_str(), AtFlags::empty());
                return Err(err.into());
            }
            let synced = fsync(&dir);
            if durable {
                synced?;
            }
            Ok(())
        }

        pub(crate) fn append_peer_line(
            peer_dir: &Path,
            leaf: &str,
            line: &str,
            durable: bool,
        ) -> std::io::Result<()> {
            let dir = open_peer_dir(peer_dir)?;
            // O_APPEND create; O_NOFOLLOW refuses a symlinked leaf; O_NONBLOCK
            // makes a planted FIFO fail fast (ENXIO, no reader) instead of
            // parking the writer.
            let fd = openat(
                &dir,
                leaf,
                OFlags::WRONLY
                    | OFlags::CREATE
                    | OFlags::APPEND
                    | OFlags::NOFOLLOW
                    | OFlags::CLOEXEC
                    | OFlags::NONBLOCK,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(std::io::Error::from)?;
            let mut file = std::fs::File::from(fd);
            // Regular files only: refuse a device/FIFO leaf that slipped past
            // the open (e.g. a FIFO with a live reader).
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "peer leaf is not a regular file",
                ));
            }
            file.write_all(line.as_bytes())?;
            if durable {
                file.sync_data()?;
            }
            Ok(())
        }

        pub(crate) fn peer_file_mtime(
            peer_dir: &Path,
            leaf: &str,
        ) -> Option<std::time::SystemTime> {
            let dir = open_peer_dir(peer_dir).ok()?;
            // Anchored no-follow open + fstat (NONBLOCK so a planted FIFO can't
            // park the probe); regular files only, then read the mtime off the
            // opened handle. No content is read.
            let fd = openat(
                &dir,
                leaf,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .ok()?;
            let meta = std::fs::File::from(fd).metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            meta.modified().ok()
        }

        pub(crate) fn peer_dir_exists(peer_dir: &Path) -> bool {
            // O_NOFOLLOW|O_DIRECTORY succeeds only for a REAL non-symlink dir.
            open_peer_dir(peer_dir).is_ok()
        }

        pub(crate) fn peer_dir_count_prefixed(peer_dir: &Path, prefix: &str, cap: usize) -> usize {
            let Ok(dirfd) = open_peer_dir(peer_dir) else {
                return 0;
            };
            // fdopendir on the anchored fd — entries come from THIS inode, never
            // a re-walked path, so a swapped `<slug>` can't redirect the scan.
            let Ok(mut dir) = Dir::read_from(&dirfd) else {
                return 0;
            };
            let prefix = prefix.as_bytes();
            let mut count = 0usize;
            let mut scanned = 0usize;
            // Fetch AT MOST `cap` entries: the budget is checked BEFORE each
            // read, so the cap+1'th entry is never even fetched.
            while scanned < cap {
                let Some(next) = dir.next() else {
                    break;
                };
                // A mid-scan read error is a FAILURE, not a short scan: return 0
                // (the documented contract), never a partial count.
                let Ok(entry) = next else {
                    return 0;
                };
                scanned += 1;
                if !entry.file_name().to_bytes().starts_with(prefix) {
                    continue;
                }
                // Regular files only — a symlinked/dir/FIFO `result-*` entry
                // must not inflate the version count.
                match entry.file_type() {
                    FileType::RegularFile => count += 1,
                    // d_type unavailable on this FS → classify with a no-follow
                    // stat before counting.
                    FileType::Unknown if entry_is_regular(&dirfd, entry.file_name()) => count += 1,
                    _ => {}
                }
            }
            count
        }

        /// task-evo-peer-turn-status — LIST the regular-file leaf names under
        /// `prefix` (fd-anchored, scan-capped, symlink/refuse semantics identical
        /// to [`peer_dir_count_prefixed`], #1824), sorted by name. Returns
        /// `Err(FileTooLarge)` when the scan reached its entry budget — the
        /// budget counts ALL SCANNED entries (not just prefix hits), so a dir
        /// full of unrelated files can also exhaust it; a truncated list must
        /// never pose as complete. `Err` also when the dir cannot be opened or a
        /// read fails mid-scan.
        pub(crate) fn peer_dir_list_prefixed(
            peer_dir: &Path,
            prefix: &str,
            cap: usize,
        ) -> std::io::Result<Vec<String>> {
            let dirfd = open_peer_dir(peer_dir)?;
            let mut dir = Dir::read_from(&dirfd)?;
            let prefix = prefix.as_bytes();
            let mut names: Vec<String> = Vec::new();
            let mut scanned = 0usize;
            while scanned < cap {
                let next = dir.next().transpose()?;
                let Some(entry) = next else {
                    // Directory exhausted BEFORE the budget: the list is
                    // complete.
                    names.sort();
                    return Ok(names);
                };
                scanned += 1;
                if !entry.file_name().to_bytes().starts_with(prefix) {
                    continue;
                }
                match entry.file_type() {
                    FileType::RegularFile => {
                        names.push(entry.file_name().to_string_lossy().into_owned())
                    }
                    // d_type unavailable on this FS → classify with a no-follow
                    // stat before listing.
                    FileType::Unknown if entry_is_regular(&dirfd, entry.file_name()) => {
                        names.push(entry.file_name().to_string_lossy().into_owned())
                    }
                    _ => {}
                }
            }
            // The loop exited because the BUDGET was exhausted — more entries
            // may exist beyond `cap`, so the list is potentially partial.
            Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "peer dir scan hit its entry cap: list is truncated",
            ))
        }

        /// No-follow `S_ISREG` check of `name` relative to the peer dir fd, for
        /// the rare filesystem that returns `DT_UNKNOWN` from `readdir`.
        fn entry_is_regular(dirfd: &OwnedFd, name: &CStr) -> bool {
            openat(
                dirfd,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .ok()
            .and_then(|fd| std::fs::File::from(fd).metadata().ok())
            .is_some_and(|meta| meta.is_file())
        }
    }

    #[cfg(not(unix))]
    mod imp {
        use std::io::Write;
        use std::path::Path;

        /// Refuse a symlinked peer dir; require a real directory. Non-unix
        /// serve is dev-only (matching the `symlink_metadata` fallback
        /// `memory_panel` uses for the same reads), so a path-anchored check
        /// with a documented multi-syscall TOCTOU window is acceptable here.
        fn peer_dir_ok(peer_dir: &Path) -> bool {
            std::fs::symlink_metadata(peer_dir)
                .map(|m| !m.file_type().is_symlink() && m.is_dir())
                .unwrap_or(false)
        }

        pub(crate) fn read_peer_file(
            peer_dir: &Path,
            leaf: &str,
            cap: usize,
        ) -> std::io::Result<Option<String>> {
            let parent = std::fs::symlink_metadata(peer_dir)?;
            if parent.file_type().is_symlink() || !parent.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "peer dir is not a real directory",
                ));
            }
            let path = peer_dir.join(leaf);
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "peer leaf is not a regular file",
                ));
            }
            super::read_regular_file(std::fs::File::open(&path)?, cap).map(Some)
        }

        pub(crate) fn write_peer_file_atomic(
            peer_dir: &Path,
            leaf: &str,
            content: &str,
            durable: bool,
        ) -> std::io::Result<()> {
            if !peer_dir_ok(peer_dir) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "peer dir is not a real directory",
                ));
            }
            let path = peer_dir.join(leaf);
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "refusing to follow a symlinked peer leaf",
                ));
            }
            let tmp = peer_dir.join(super::tmp_name(leaf));
            {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)?;
                if let Err(err) = file
                    .write_all(content.as_bytes())
                    .and_then(|()| file.sync_all())
                {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(err);
                }
            }
            if let Err(err) = std::fs::rename(&tmp, &path) {
                let _ = std::fs::remove_file(&tmp);
                return Err(err);
            }
            if durable {
                // Windows cannot repeat this sync: a plain open of a directory
                // is denied outright (os error 5) — only FILE_FLAG_BACKUP_
                // SEMANTICS reaches a directory handle, and FlushFileBuffers
                // is not a documented directory-metadata sync there. The
                // rename above already made the record fully visible, so
                // Windows runs best-effort durable instead of failing every
                // budgeted stage and prepare (#2515); Unix keeps the
                // fail-closed dir sync.
            }
            Ok(())
        }

        pub(crate) fn append_peer_line(
            peer_dir: &Path,
            leaf: &str,
            line: &str,
            durable: bool,
        ) -> std::io::Result<()> {
            if !peer_dir_ok(peer_dir) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "peer dir is not a real directory",
                ));
            }
            let path = peer_dir.join(leaf);
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                if meta.file_type().is_symlink() || !meta.is_file() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "peer leaf is not a regular file",
                    ));
                }
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            file.write_all(line.as_bytes())?;
            if durable {
                // Windows cannot sync an append handle either: an append-mode
                // open holds FILE_APPEND_DATA without FILE_WRITE_DATA, and
                // FlushFileBuffers requires the write right, so sync_data
                // failed with os error 5 on every budgeted admission (#2515).
                // The line is written either way, so Windows runs best-effort
                // durable instead; Unix syncs for real.
            }
            Ok(())
        }

        pub(crate) fn peer_file_mtime(
            peer_dir: &Path,
            leaf: &str,
        ) -> Option<std::time::SystemTime> {
            if !peer_dir_ok(peer_dir) {
                return None;
            }
            let meta = std::fs::symlink_metadata(peer_dir.join(leaf)).ok()?;
            if meta.file_type().is_symlink() || !meta.is_file() {
                return None;
            }
            meta.modified().ok()
        }

        pub(crate) fn peer_dir_exists(peer_dir: &Path) -> bool {
            peer_dir_ok(peer_dir)
        }

        pub(crate) fn peer_dir_count_prefixed(peer_dir: &Path, prefix: &str, cap: usize) -> usize {
            if !peer_dir_ok(peer_dir) {
                return 0;
            }
            let Ok(mut read_dir) = std::fs::read_dir(peer_dir) else {
                return 0;
            };
            let mut count = 0usize;
            let mut scanned = 0usize;
            // Fetch AT MOST `cap` entries (budget checked before each read); a
            // mid-scan read error returns 0, never a partial count.
            while scanned < cap {
                let Some(next) = read_dir.next() else {
                    break;
                };
                let Ok(entry) = next else {
                    return 0;
                };
                scanned += 1;
                if !entry.file_name().to_string_lossy().starts_with(prefix) {
                    continue;
                }
                if std::fs::symlink_metadata(entry.path())
                    .is_ok_and(|m| !m.file_type().is_symlink() && m.is_file())
                {
                    count += 1;
                }
            }
            count
        }

        /// task-evo-peer-turn-status — LIST variant of the count above, same
        /// non-unix dev-only path-anchored semantics (#1824 documented TOCTOU
        /// window). Sorted; `Err(FileTooLarge)` when the scan budget (which
        /// counts ALL scanned entries, hits and non-hits alike) is exhausted
        /// — a potentially-partial list never poses as complete.
        pub(crate) fn peer_dir_list_prefixed(
            peer_dir: &Path,
            prefix: &str,
            cap: usize,
        ) -> std::io::Result<Vec<String>> {
            if !peer_dir_ok(peer_dir) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "peer dir is not a real directory",
                ));
            }
            let mut read_dir = std::fs::read_dir(peer_dir)?;
            let mut names: Vec<String> = Vec::new();
            let mut scanned = 0usize;
            while scanned < cap {
                let next = read_dir.next().transpose()?;
                let Some(entry) = next else {
                    names.sort();
                    return Ok(names);
                };
                scanned += 1;
                let file_name = entry.file_name().to_string_lossy().into_owned();
                if !file_name.starts_with(prefix) {
                    continue;
                }
                if std::fs::symlink_metadata(entry.path())
                    .is_ok_and(|m| !m.file_type().is_symlink() && m.is_file())
                {
                    names.push(file_name);
                }
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "peer dir scan hit its entry cap: list is truncated",
            ))
        }
    }
}

/// Build-cache pool integration for peers (outer-loop #4,
/// docs/build-cache-pool.md §4/§7.4).
///
/// The slot lifecycle is ONE PEER TURN, not the peer session: staging
/// acquires the FIRST turn's slot and records it in
/// `peers/<slug>/build-cache`; the serve boot ADOPTS that slot on turn 1
/// (§4.1 adopt rule — the staging flock is held by the SAME process, so a
/// fresh `acquire` would EWOULDBLOCK and grab a second slot) and acquires
/// fresh on later turns; the turn terminal releases (primary), with close /
/// interrupt / eviction / rollback as idempotent safety nets (§4.2).
pub(crate) mod build_cache_peer {
    use std::path::{Path, PathBuf};

    use super::RpcError;
    use crate::build_cache::pool::{
        BuildCacheConfig, HolderInfo, Slot, SlotOutcome, SlotPurpose, acquire, release,
    };
    use crate::build_cache::repo_key_for_path;

    /// The per-peer read-back leaf (`peers/<slug>/build-cache`): one line,
    /// the slot dir path held by the peer's CURRENT turn (§7.4). Written by
    /// whichever side acquired (staging on turn 1, boot on later turns);
    /// boot turn 1 reads it back and ADOPTS instead of double-acquiring.
    pub(crate) const LEAF: &str = "build-cache";

    /// The pool root for a profile data dir (§1.1): beside `peers/`, so slot
    /// and peer metadata share a lifecycle root and (by default, with the
    /// pool inside the octos home) peer sandboxes can read it without an
    /// extra read grant.
    pub(crate) fn pool_root(data_dir: &Path) -> PathBuf {
        data_dir.join("build-cache")
    }

    fn err_text(slug: &str, err: crate::build_cache::BuildCacheError) -> String {
        format!("build-cache slot for peer '{slug}': {err}")
    }

    /// Acquire the peer's FIRST-turn slot during staging and record it in
    /// `peers/<slug>/build-cache` (§7.4). Called from `stage_peer` AFTER the
    /// dir is reserved but BEFORE `brief.md` (the visibility gate): a peer
    /// that boots can always read its slot back. Space-gate / pool-exhausted
    /// failures roll the staging back (the caller passes the error through
    /// as an `RpcError`, fail-fast per §3.2 step 5 — never queue).
    pub(crate) fn acquire_for_staging(
        peers_root: &Path,
        workspace_root: &Path,
        slug: &str,
        goal_id: Option<&str>,
        task_id: Option<&str>,
        config: &BuildCacheConfig,
    ) -> Result<Slot, RpcError> {
        let data_dir = peers_root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| peers_root.to_path_buf());
        let repo_key = repo_key_for_path(workspace_root).ok_or_else(|| {
            RpcError::internal_error(format!(
                "build-cache slot for peer '{slug}': cannot canonicalize workspace root {} \
                 (repo-key derivation failed)",
                workspace_root.display()
            ))
        })?;
        let holder = HolderInfo {
            slug: Some(slug.to_owned()),
            goal_id: goal_id.filter(|s| !s.is_empty()).map(str::to_owned),
            task_id: task_id.filter(|s| !s.is_empty()).map(str::to_owned),
            purpose_note: None,
            // Peer slots are held by THIS serve process; no override (#6's
            // override exists only for the CLI's cross-process verify slot).
            pid_override: None,
        };
        let slot = acquire(
            &pool_root(&data_dir),
            &repo_key,
            SlotPurpose::Peer,
            config,
            &holder,
        )
        .map_err(|e| RpcError::internal_error(err_text(slug, e)))?;
        Ok(slot)
    }

    /// Acquire and record as one operation, releasing on a failed write.
    /// Both staging and turn boot must leave no live-pid holder on failure.
    pub(crate) fn acquire_recorded(
        peers_root: &Path,
        workspace_root: &Path,
        slug: &str,
        goal_id: Option<&str>,
        task_id: Option<&str>,
        config: &BuildCacheConfig,
    ) -> Result<Slot, RpcError> {
        let mut slot =
            acquire_for_staging(peers_root, workspace_root, slug, goal_id, task_id, config)?;
        if let Err(err) = record_slot(&peers_root.join(slug), &slot) {
            release_slot(&mut slot, SlotOutcome::Cancelled);
            return Err(RpcError::internal_error(format!(
                "failed to record build-cache slot for peer '{slug}': {err}"
            )));
        }
        Ok(slot)
    }

    /// Atomically retain the claim and its tracker for the exact turn owner.
    pub(crate) fn slot_for_owned_turn(
        peers_root: &Path,
        workspace_root: &Path,
        slug: &str,
        owner: &super::BuildCacheTurnOwner,
    ) -> Result<Option<super::BuildCacheSlotView>, RpcError> {
        let peer_dir = peers_root.join(slug);
        let clone = peer_dir.join("wt");
        let key = super::build_cache_slot_registry_key(peers_root, slug);
        let registry = super::build_cache_slot_registry();
        let mut map = registry.by_key.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get(&key)
            && let super::BuildCacheSlotState::Active(current) = &entry.state
            && current != owner
        {
            return Err(super::build_cache_owned_error());
        }
        // Ownership is checked before eligibility: a rejected session cannot
        // retire another turn's claim after a repository/config change.
        let config = super::build_cache_config_for(peers_root);
        if !clone.is_dir()
            || super::wire_fenced_peer_build_cache(&clone, workspace_root)
                != super::PeerBuildCache::Shared
            || config.is_none()
        {
            let entry = map.remove(&key);
            drop(map);
            if let Some(entry) = entry {
                entry.release(SlotOutcome::Cancelled);
            }
            return Ok(None);
        }
        if let Some(entry) = map.get_mut(&key) {
            entry.state = super::BuildCacheSlotState::Active(owner.clone());
            return Ok(Some(entry.view()));
        }
        let slot = acquire_for_turn(peers_root, &peer_dir, &clone, slug, &config.unwrap())?;
        let entry = super::BuildCacheSlotEntry {
            slot,
            usage: Default::default(),
            state: super::BuildCacheSlotState::Active(owner.clone()),
        };
        let view = entry.view();
        map.insert(key, entry);
        Ok(Some(view))
    }

    fn acquire_for_turn(
        peers_root: &Path,
        peer_dir: &Path,
        clone: &Path,
        slug: &str,
        config: &BuildCacheConfig,
    ) -> Result<Slot, RpcError> {
        // The session workspace is the clone. As in collect_peer_branch,
        // origin identifies the SOURCE repository: hashing the clone here
        // would split the bounded source pool on the second turn.
        let origin = std::process::Command::new("git")
            .arg("-C")
            .arg(clone)
            .args(["config", "--get", "remote.origin.url"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .filter(|origin| !origin.is_empty())
            .ok_or_else(|| {
                RpcError::internal_error(format!(
                    "build-cache slot for peer '{slug}': cannot resolve source repository"
                ))
            })?;
        let goal = super::peer_io::read_peer_file(
            peer_dir,
            "goal",
            super::peer_io::PEER_FILE_READ_CAP_SMALL,
        );
        let mut lines = goal.as_deref().unwrap_or_default().lines();
        let goal_id = lines.next().map(str::trim).filter(|s| !s.is_empty());
        let task_id = lines.next().map(str::trim).filter(|s| !s.is_empty());
        acquire_recorded(
            peers_root,
            Path::new(&origin),
            slug,
            goal_id,
            task_id,
            config,
        )
    }

    /// Persist the held slot to `peers/<slug>/build-cache` (§7.4). The file
    /// is a read-back channel, not the truth — the flock + `holder.json`
    /// are. Production callers use acquire_recorded so write failures
    /// surface and release the newly acquired slot at staging and turn boot.
    pub(crate) fn record_slot(peer_dir: &Path, slot: &Slot) -> std::io::Result<()> {
        super::peer_io::write_peer_file_atomic(peer_dir, LEAF, &slot.path.to_string_lossy())
    }

    /// Release a held slot, mapping the peer outcome onto the pool's
    /// outcome enum (§3.4 — the outcome is diagnostics-only; release
    /// behavior is identical for every arm). Idempotent by construction:
    /// `pool::release` is a no-op once `holder.json` is gone, so the turn
    /// terminal, the close callback, and eviction may all fire.
    pub(crate) fn release_slot(slot: &mut Slot, outcome: SlotOutcome) {
        if let Err(err) = release(slot, outcome) {
            tracing::warn!(%err, outcome = ?outcome, "build-cache slot release failed (crash recovery applies)");
        }
    }
}

/// Process-global map of `"{profile}:peer:{slug}"` → the build-cache slot
/// handle CURRENTLY held for that peer's turn (outer-loop #4).
///
/// WHY a registry at all: an flock belongs to the open file description, so
/// `release` must run on the SAME `Slot` (with its lock fd) that `acquire`
/// returned — re-opening `.lock` by path would mint a second description and
/// `try_lock_exclusive` would EWOULDBLOCK against ourselves. The handle is
/// therefore parked here between the acquire side (staging for turn 1, serve
/// boot for later turns) and the release side (turn terminal primary; close /
/// interrupt / eviction safety nets). Same key shape and process-global
/// lifetime discipline as `peer_wire_registry` / `peer_task_registry`: a peer
/// is staged on one path and released on another.
pub(crate) struct BuildCacheSlotRegistry {
    by_key: std::sync::Mutex<HashMap<String, BuildCacheSlotEntry>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BuildCacheTurnOwner {
    pub session: SessionKey,
    pub turn: octos_core::ui_protocol::TurnId,
    /// Server dispatch identity; transport pins its state allocation while referenced.
    pub generation: usize,
}

pub(crate) struct BuildCacheSlotView {
    pub path: PathBuf,
    pub usage: octos_agent::tools::BuildCacheUsage,
}

enum BuildCacheSlotState {
    Staged,
    Active(BuildCacheTurnOwner),
}

fn build_cache_owned_error() -> RpcError {
    RpcError::invalid_request(
        "build_cache_unavailable: another turn owns this peer's build-cache slot",
    )
    .with_data(serde_json::json!({"code": "build_cache_unavailable"}))
}

struct BuildCacheSlotEntry {
    state: BuildCacheSlotState,
    slot: Slot,
    usage: octos_agent::tools::BuildCacheUsage,
}

impl BuildCacheSlotEntry {
    fn owned_by(&self, owner: &BuildCacheTurnOwner) -> bool {
        matches!(&self.state, BuildCacheSlotState::Active(current) if current == owner)
    }
    fn view(&self) -> BuildCacheSlotView {
        BuildCacheSlotView {
            path: self.slot.path.clone(),
            usage: self.usage.clone(),
        }
    }

    fn release(self, outcome: SlotOutcome) {
        let mut slot = self.slot;
        self.usage.close_and_when_idle(move || {
            build_cache_peer::release_slot(&mut slot, outcome);
        });
    }
}

/// Registry key for one peer's held slot: `"<peers_root>\u{1f}<slug>"`. The
/// separator is a control char no path component or slug may contain, so two
/// peers of different profiles (or a slug that happens to contain the other's
/// root as a prefix) can never collide.
pub(crate) fn build_cache_slot_registry_key(peers_root: &Path, slug: &str) -> String {
    format!("{}\u{1f}{slug}", peers_root.to_string_lossy())
}

impl Default for BuildCacheSlotRegistry {
    fn default() -> Self {
        Self {
            by_key: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

pub(crate) fn build_cache_slot_registry() -> &'static BuildCacheSlotRegistry {
    static REGISTRY: OnceLock<BuildCacheSlotRegistry> = OnceLock::new();
    REGISTRY.get_or_init(BuildCacheSlotRegistry::default)
}

/// Per-`peers_root` build-cache config, installed by the process that owns
/// the peers (serve bootstrap / chat `--peers` host) so `stage_peer` can
/// acquire a first-turn slot WITHOUT growing its already-10-argument
/// signature through ~20 call sites. Absent = the pool is OFF for that root:
/// staging then behaves exactly as before #4 (no slot, no `build-cache`
/// file). This mirrors how `peer_wire_registry` / `peer_task_registry` make
/// process-global state reachable from staging and close alike.
static BUILD_CACHE_CONFIGS: OnceLock<std::sync::Mutex<HashMap<String, BuildCacheConfig>>> =
    OnceLock::new();

/// Install (or replace) the build-cache config for one peers root. Called at
/// bootstrap; a `None` value REMOVES the entry (used by tests to isolate).
/// The bootstrap call site lives in commands/serve.rs behind
/// `#[cfg(feature = "api")]` — under a `--no-default-features` (no-api)
/// build this setter is dead by design; that quadrant is already silenced
/// by the module-level `allow(dead_code)` on `mod peers` (lib.rs), so no
/// entry-level allow is needed here.
pub(crate) fn set_build_cache_config(peers_root: &Path, config: Option<BuildCacheConfig>) {
    let table = BUILD_CACHE_CONFIGS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut map = table.lock().unwrap_or_else(|e| e.into_inner());
    match config {
        Some(cfg) => {
            map.insert(peers_root.to_string_lossy().into_owned(), cfg);
        }
        None => {
            map.remove(&peers_root.to_string_lossy().into_owned());
        }
    }
}

/// The build-cache config in force for one peers root (`None` = pool off).
pub(crate) fn build_cache_config_for(peers_root: &Path) -> Option<BuildCacheConfig> {
    let table = BUILD_CACHE_CONFIGS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let map = table.lock().unwrap_or_else(|e| e.into_inner());
    map.get(&peers_root.to_string_lossy().into_owned()).cloned()
}

impl BuildCacheSlotRegistry {
    /// Bind a staged claim at dispatch, before the turn task can fail or be aborted.
    pub(crate) fn reserve_staged(
        &self,
        key: &str,
        owner: &BuildCacheTurnOwner,
    ) -> Result<(), RpcError> {
        let mut map = self.by_key.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get_mut(key) {
            if matches!(entry.state, BuildCacheSlotState::Active(_)) {
                return Err(build_cache_owned_error());
            }
            entry.state = BuildCacheSlotState::Active(owner.clone());
        }
        Ok(())
    }

    pub(crate) fn release_owned(
        &self,
        key: &str,
        owner: &BuildCacheTurnOwner,
        outcome: SlotOutcome,
    ) {
        self.release_matching(
            |candidate, entry| candidate == key && entry.owned_by(owner),
            outcome,
        );
    }

    pub(crate) fn release_for_slug(
        &self,
        slug: &str,
        owner: &BuildCacheTurnOwner,
        outcome: SlotOutcome,
    ) {
        let suffix = format!("\u{1f}{slug}");
        self.release_matching(
            |key, entry| key.ends_with(&suffix) && entry.owned_by(owner),
            outcome,
        );
    }

    fn release_matching(
        &self,
        matches: impl Fn(&str, &BuildCacheSlotEntry) -> bool,
        outcome: SlotOutcome,
    ) {
        let mut map = self.by_key.lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<_> = map
            .iter()
            .filter(|(key, entry)| matches(key, entry))
            .map(|(key, _)| key.clone())
            .collect();
        let entries: Vec<_> = keys
            .into_iter()
            .filter_map(|key| map.remove(&key))
            .collect();
        drop(map);
        for entry in entries {
            entry.release(outcome);
        }
    }

    /// Park a claimed slot with its per-child usage tracker.
    pub(crate) fn park(&self, key: String, slot: Slot) {
        self.park_with_usage(key, slot);
    }

    pub(crate) fn park_with_usage(
        &self,
        key: String,
        slot: Slot,
    ) -> octos_agent::tools::BuildCacheUsage {
        let usage = octos_agent::tools::BuildCacheUsage::default();
        let incoming = BuildCacheSlotEntry {
            slot,
            usage: usage.clone(),
            state: BuildCacheSlotState::Staged,
        };
        let displaced = {
            let mut map = self.by_key.lock().unwrap_or_else(|e| e.into_inner());
            if map
                .get(&key)
                .is_some_and(|entry| matches!(entry.state, BuildCacheSlotState::Active(_)))
            {
                Some(incoming)
            } else {
                map.insert(key, incoming)
            }
        };
        if let Some(entry) = displaced {
            entry.release(SlotOutcome::Cancelled);
        }
        usage
    }

    /// Close admission before returning the claim; live children retain its lock.
    pub(crate) fn release(&self, key: &str, outcome: SlotOutcome) {
        let entry = self
            .by_key
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
        if let Some(entry) = entry {
            entry.release(outcome);
        }
    }
}

/// `true` when `peers/<slug>/closed` exists — the durable marker written by
/// `peer_close` retiring a peer. Shared by the continuation-drain freshness
/// gates and the reconnect-retarget skip so a closed peer is never a live
/// injection target, even for an injection queued just before the close.
/// Routes through [`staged_peer_dir`] so a symlinked / unsafe slug is never
/// followed.
pub(crate) fn peer_is_closed(peers_root: &Path, slug: &str) -> bool {
    staged_peer_dir(peers_root, slug)
        .is_some_and(|dir| peer_io::peer_regular_file_exists(&dir, "closed"))
}

/// Resolve a peer IDENTIFIER (its display NAME or its slug) to the slug. A name
/// match is case-insensitive against each REAL staged `peers/<slug>/name`; if
/// none matches, an `ident` that is itself a safe, staged dir is returned as-is
/// (slug addressing, and legacy peers that have no `name` file). Both branches
/// route through [`staged_peer_dir`], so a SYMLINKED entry is skipped and never
/// resolved. Returns `None` when nothing matches. Names are the primary
/// address, so callbacks resolve through this BEFORE any auth / path / wire op.
pub(crate) fn resolve_peer_name_to_slug(peers_root: &Path, ident: &str) -> Option<String> {
    let target = ident.trim();
    if target.is_empty() {
        return None;
    }
    let lowered = target.to_lowercase();
    if let Ok(read_dir) = std::fs::read_dir(peers_root) {
        for entry in read_dir.flatten() {
            let slug = entry.file_name().to_string_lossy().into_owned();
            // Only a REAL, staged (non-symlink) peer dir may claim a name.
            let Some(dir) = staged_peer_dir(peers_root, &slug) else {
                continue;
            };
            if let Some(name) =
                peer_io::read_peer_file(&dir, "name", peer_io::PEER_FILE_READ_CAP_SMALL)
            {
                if name.trim().to_lowercase() == lowered {
                    return Some(slug);
                }
            }
        }
    }
    // Fall back to slug addressing: an ident that is a safe, staged, non-symlink
    // dir name.
    staged_peer_dir(peers_root, target).map(|_| target.to_owned())
}

/// #436 P1 #6 — authorize a `peer_send_input` injection: ONLY the peer's
/// recorded ORIGINATOR — the session that staged it via `peer_handoff` /
/// `peer_prepare`, written to `peers/<slug>/originator` — may inject into it.
/// Previously any non-peer session in the same profile could inject into any
/// open staged peer.
///
/// Authorizing by the STABLE originator identity (not the ephemeral wire key)
/// composes with the reconnect wire-resolution fix: the caller is the master
/// session, and the target peer's wire may change across reconnects without
/// affecting this check. Fail-closed: a missing originator record (e.g. a
/// profile-scoped `peer_prepare` that recorded no originator) is unauthorized.
///
/// # Security model (#436 #5 — single-user-per-profile / Option C)
///
/// Authorization is SESSION-scoped WITHIN a single user's own trust domain. In
/// serve, the authenticated identity IS the profile: `authenticated_profile_id`
/// returns the `AuthIdentity::User { id }` id *as* the profile, so a profile is
/// exactly one user's own trust domain.
///
/// - **Cross-USER injection is blocked by profile scoping.** A connection can
///   only open / run turns in sessions under its own profile
///   (`validate_authenticated_session_scope`), so a different user cannot reach
///   another user's peer at all — the strong isolation boundary.
/// - **An LLM cannot cross-session-inject.** `caller_session` is the
///   SERVER-CAPTURED session of the running turn (never a client-supplied
///   argument), and the LLM cannot call `session/open` — so an LLM in a
///   non-owner session sees its own session key ≠ the recorded originator and
///   is rejected. This check blocks LLM-level cross-session injection, the
///   meaningful in-band threat.
/// - **The residual same-user, cross-session "spoof" is by design.** A CLIENT
///   that deliberately `session/open`s the owner session and drives a turn
///   there satisfies the originator check — but that is the USER exercising
///   their own authority within their own profile, not a cross-trust breach.
///   Making it non-spoofable would require a capability / session-access-control
///   model (a per-peer owner token held outside any session-replayable channel;
///   Option A), which is OUT OF SCOPE for the single-user serve model. If
///   serve ever gains sub-user identities or multi-user profiles, revisit here.
pub(crate) fn peer_send_input_authorized(
    peers_root: &Path,
    slug: &str,
    caller_session: &str,
) -> Result<(), String> {
    // Route through `staged_peer_dir`: the originator read must target a REAL,
    // non-symlink staged peer under `peers/`, never a symlinked/unsafe slug that
    // could redirect the read outside the root. Defense-in-depth — callers
    // already resolve the slug, but auth is the boundary and stays self-safe.
    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        return Err(format!(
            "peer session '{slug}' is not a staged peer; cannot authorize input"
        ));
    };
    match peer_io::read_peer_file(&dir, "originator", peer_io::PEER_FILE_READ_CAP_SMALL) {
        Some(recorded) if recorded.trim() == caller_session => Ok(()),
        Some(_) => Err(format!(
            "not the owner of peer session '{slug}' — only the session that \
             staged this peer may send it input"
        )),
        None => Err(format!(
            "peer session '{slug}' has no recorded owner; cannot authorize input"
        )),
    }
}

/// Upper bound on a peer brief. Briefs are task contracts, not payloads — a
/// cap keeps a runaway client from turning the profile dir into blob storage.
#[cfg(feature = "api")]
pub(crate) const PEER_BRIEF_MAX_BYTES: usize = 64 * 1024;

/// Derive a unique directory slug for a peer under `peers/`: sanitized from
/// the title (else the brief's leading words), numeric `-N` suffix on
/// collision. Returns the reserved (created) directory alongside the slug so
/// two concurrent prepares can never race into the same dir —
/// `create_dir` is the atomic claim.
pub(crate) fn reserve_peer_dir(
    peers_root: &Path,
    seed: &str,
) -> Result<(String, PathBuf), RpcError> {
    // Use the same ASCII normalization as named/model-staged peers. Encoding
    // Unicode with `safe_filename` and then truncating can split a %XX escape
    // (or an encoded UTF-8 character), making the returned topic unopenable.
    let base = name_to_slug(seed).unwrap_or_else(|| "peer".to_owned());
    std::fs::create_dir_all(peers_root)
        .map_err(|err| RpcError::internal_error(format!("failed to create peers dir: {err}")))?;
    for attempt in 0..100u32 {
        let slug = if attempt == 0 {
            base.clone()
        } else {
            let suffix = format!("-{}", attempt + 1);
            // The normalized base is ASCII, so this cut is always safe. Leave
            // room for the ENTIRE suffix, including when it gains a digit.
            let end = base.len().min(PEER_SLUG_MAX_BYTES - suffix.len());
            format!("{}{suffix}", base[..end].trim_end_matches('-'))
        };
        let dir = peers_root.join(&slug);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((slug, dir)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(RpcError::internal_error(format!(
                    "failed to reserve peer dir: {err}"
                )));
            }
        }
    }
    Err(RpcError::invalid_params(
        "too many peers with this title — pick a distinct title",
    ))
}

/// True when a staged peer already claims this NAME (case-insensitive, reading
/// each `peers/<slug>/name`) or already occupies the derived SLUG. Guards the
/// NAMED staging path: names are the primary address, so they must be unique.
pub(crate) fn existing_peer_name_conflict(peers_root: &Path, name: &str, slug: &str) -> bool {
    let target = name.trim().to_lowercase();
    let Ok(read_dir) = std::fs::read_dir(peers_root) else {
        return false;
    };
    for entry in read_dir.flatten() {
        let entry_slug = entry.file_name().to_string_lossy().into_owned();
        // Any entry occupying the derived slug path — even a symlink or an
        // unstaged dir — blocks the reservation (`create_dir` fails on it too).
        if entry_slug == slug {
            return true;
        }
        // A NAME collision only counts against a REAL staged peer: route the
        // `name` read through `staged_peer_dir` so a symlinked entry is never
        // followed and never falsely registers as a conflict.
        if let Some(dir) = staged_peer_dir(peers_root, &entry_slug) {
            if let Some(existing) =
                peer_io::read_peer_file(&dir, "name", peer_io::PEER_FILE_READ_CAP_SMALL)
            {
                if existing.trim().to_lowercase() == target {
                    return true;
                }
            }
        }
    }
    false
}

/// Reserve the EXACT slug derived from a peer NAME (no numeric suffix): a named
/// peer must be addressable by its name, so a collision is an ERROR, not an
/// auto-rename. Rejects a name with no usable slug, a duplicate name
/// (case-insensitive), or a slug already taken. `create_dir` is the atomic
/// claim that also closes the check→reserve race.
pub(crate) fn reserve_named_peer_dir(
    peers_root: &Path,
    name: &str,
) -> Result<(String, PathBuf), RpcError> {
    let Some(slug) = name_to_slug(name) else {
        return Err(RpcError::invalid_params(
            "peer name cannot be blank".to_string(),
        ));
    };
    std::fs::create_dir_all(peers_root)
        .map_err(|err| RpcError::internal_error(format!("failed to create peers dir: {err}")))?;
    if existing_peer_name_conflict(peers_root, name, &slug) {
        return Err(RpcError::invalid_params(format!(
            "a peer named '{name}' already exists"
        )));
    }
    let dir = peers_root.join(&slug);
    match std::fs::create_dir(&dir) {
        Ok(()) => Ok((slug, dir)),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Err(
            RpcError::invalid_params(format!("a peer named '{name}' already exists")),
        ),
        Err(err) => Err(RpcError::internal_error(format!(
            "failed to reserve peer dir: {err}"
        ))),
    }
}

/// One staged peer, as produced by [`stage_peer`]: the durable facts a
/// `peer/prepare` result entry (and the `peer/staged` notification) carry.
#[derive(Debug)]
pub(crate) struct StagedPeer {
    pub(crate) slug: String,
    /// #2236 — the fenced-peer build-cache decision (Shared/RepoConfig/None).
    pub(crate) build_cache: PeerBuildCache,
    /// Session topic the client opens (`peer-<slug>`).
    pub(crate) topic: String,
    /// `peers/<slug>/brief.md` under the profile data dir.
    pub(crate) brief_path: PathBuf,
    /// Worktree checkout when fenced, else the workspace root.
    pub(crate) cwd: PathBuf,
    /// `peer/<slug>` when a worktree fence was created.
    pub(crate) worktree_branch: Option<String>,
}

/// A peer budget is independent of the master's goal budget. Usage is keyed
/// by the staged slug, so reopening the peer under a new OUP session id does
/// not reset its allowance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PeerTokenBudgetStatus {
    pub(crate) limit: u64,
    pub(crate) used: u64,
}

pub(crate) fn peer_token_budget_status(
    peers_root: &Path,
    slug: &str,
) -> Result<Option<PeerTokenBudgetStatus>, String> {
    use peer_io::PeerFileRead;

    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        return Ok(None);
    };
    let limit = match peer_io::read_peer_identity_file(&dir, "token_budget") {
        PeerFileRead::Missing => return Ok(None),
        PeerFileRead::Valid(text) => text
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or_else(|| format!("peer '{slug}' has an invalid token budget"))?,
        PeerFileRead::Invalid(error) => {
            return Err(format!("cannot read peer '{slug}' token budget: {error}"));
        }
    };
    match peer_io::read_peer_control_file(&dir, "token_budget_error", 1024) {
        Ok(None) => {}
        Ok(Some(_)) => {
            return Err(format!(
                "peer '{slug}' token budget accounting is unavailable"
            ));
        }
        Err(error) => {
            return Err(format!("cannot check peer '{slug}' token budget: {error}"));
        }
    }
    let mut turns = HashMap::<String, u64>::new();
    match peer_io::read_peer_control_file(&dir, "token_budget_usage", 1024 * 1024) {
        Ok(None) => {}
        Ok(Some(text)) => {
            for line in text.lines() {
                let mut fields = line.split_whitespace();
                let (Some(turn_id), Some(tokens), None) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    return Err(format!("peer '{slug}' has invalid token usage records"));
                };
                let tokens = tokens
                    .parse::<u64>()
                    .map_err(|_| format!("peer '{slug}' has invalid token usage records"))?;
                if turns
                    .insert(turn_id.to_owned(), tokens)
                    .is_some_and(|old| old != tokens)
                {
                    return Err(format!("peer '{slug}' has conflicting token usage records"));
                }
            }
        }
        Err(error) => {
            return Err(format!("cannot read peer '{slug}' token usage: {error}"));
        }
    }
    let used = turns.values().copied().fold(0_u64, u64::saturating_add);
    Ok(Some(PeerTokenBudgetStatus { limit, used }))
}

/// Append one turn's spend. Duplicate terminal delivery is safe: the reader
/// counts each turn id once. This is a turn-boundary budget, like goal budgets.
pub(crate) fn charge_peer_token_budget(
    peers_root: &Path,
    slug: &str,
    turn_id: &str,
    tokens: u64,
) -> Result<(), String> {
    if peer_token_budget_status(peers_root, slug)?.is_none() {
        return Ok(());
    }
    let dir = staged_peer_dir(peers_root, slug)
        .ok_or_else(|| format!("peer '{slug}' is no longer staged"))?;
    if let Err(error) = peer_io::append_peer_line_durable(
        &dir,
        "token_budget_usage",
        &format!("{turn_id} {tokens}\n"),
    ) {
        let _ = peer_io::write_peer_file_durable(&dir, "token_budget_error", &error.to_string());
        return Err(format!("cannot record peer '{slug}' token usage: {error}"));
    }
    Ok(())
}

/// Stage a peer with an optional cumulative token budget. The budget is
/// durable before `brief.md` makes the peer visible to OUP clients.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_peer_with_budget(
    peers_root: &Path,
    workspace_root: &Path,
    seed: &str,
    name: Option<&str>,
    originator: Option<&str>,
    brief: &str,
    worktree: bool,
    goal_id: Option<&str>,
    task_id: Option<&str>,
    token_budget: Option<u64>,
    // UPCR-2026-034 — a host-owned app peer's durable binding (workspace +
    // memory namespace), written BEFORE `brief.md` so the peer never becomes
    // visible unbound. `None` for every agent-staged peer.
    host_binding: Option<&app_binding::PeerHostBinding>,
) -> Result<StagedPeer, RpcError> {
    if token_budget == Some(0) {
        return Err(RpcError::invalid_params(
            "token_budget must be a positive integer",
        ));
    }
    // A NAMED peer reserves its EXACT (name-derived) slug and rejects
    // collisions — a name is the primary address, so it must be unique and
    // stable. An unnamed (legacy `peer/prepare`) peer keeps the auto-suffix
    // seed path.
    let (slug, peer_dir) = match name {
        Some(name) => reserve_named_peer_dir(peers_root, name)?,
        None => reserve_peer_dir(peers_root, seed)?,
    };
    // The fence: a worktree on branch `peer/<slug>` under the peer dir.
    let (cwd, mut build_cache) = if worktree {
        let worktree_path = peer_dir.join("wt");
        let branch = format!("peer/{slug}");
        // Best-effort re-validation immediately before handing the path to git:
        // narrows (to near-zero) the window in which `<slug>` could be swapped
        // to a symlink after reservation, which would redirect git's worktree
        // creation outside `peers_root`. NOTE: this does NOT fully close it —
        // git re-resolves `peers/<slug>/wt` by path itself, so a residual
        // path-resolution TOCTOU is inherent to handing a path to a subprocess
        // (tracked as a follow-up; #1824). Accurate scope: all peer-FILE
        // read/write/enumeration I/O is fd-anchored; only this git-worktree
        // creation path is best-effort re-validated.
        if !peer_io::peer_dir_exists(&peer_dir) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::invalid_params(format!(
                "peer '{slug}' staging directory is no longer a real directory"
            )));
        }
        // A CLONE, not `git worktree add`. A worktree's `.git` is a FILE
        // pointing at `<repo>/.git/worktrees/<name>`, which lives OUTSIDE the
        // peer's sandboxed workspace — so every git command inside a worktree
        // peer failed with `fatal: not a git repository ... exit 128`, and the
        // model "recovered" by running `git init`, destroying the fence. The
        // branch then stayed at the seed commit and no deliverable ever landed.
        // A clone puts the whole `.git` INSIDE `peers/<slug>/wt`, so git works
        // with no sandbox widening, and one peer cannot reach another's refs.
        //
        // `--no-hardlinks` because isolation is the entire point here: the
        // default local-clone optimisation shares object-file inodes with the
        // parent, so a peer writing into its own `.git` could corrupt the
        // source repo's objects. Costs a real object copy per peer; revisit
        // with evidence if staging latency becomes the problem.
        let run_git = |args: &[&std::ffi::OsStr]| -> Result<(), String> {
            match std::process::Command::new("git").args(args).output() {
                Err(err) => Err(format!("failed to run git: {err}")),
                Ok(out) if out.status.success() => Ok(()),
                Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
            }
        };
        let as_os = |s: &str| std::ffi::OsString::from(s);
        // git rejects the `\\?\` extended-length prefix a canonicalized
        // Windows path may carry ("hostname contains invalid characters").
        let clone_src = dunce::simplified(workspace_root);
        let clone_dst = dunce::simplified(&worktree_path);
        let clone_args: Vec<std::ffi::OsString> = vec![
            as_os("clone"),
            as_os("--quiet"),
            as_os("--no-hardlinks"),
            clone_src.as_os_str().to_os_string(),
            clone_dst.as_os_str().to_os_string(),
        ];
        let clone_ref: Vec<&std::ffi::OsStr> = clone_args.iter().map(AsRef::as_ref).collect();
        if let Err(detail) = run_git(&clone_ref) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::invalid_params(format!(
                "git clone failed (is {} a git repo?): {}",
                workspace_root.display(),
                detail
            )));
        }
        // The fence branch now lives in the peer's OWN clone.
        let branch_args: Vec<std::ffi::OsString> = vec![
            as_os("-C"),
            clone_dst.as_os_str().to_os_string(),
            as_os("checkout"),
            as_os("-q"),
            as_os("-b"),
            as_os(&branch),
        ];
        let branch_ref: Vec<&std::ffi::OsStr> = branch_args.iter().map(AsRef::as_ref).collect();
        if let Err(detail) = run_git(&branch_ref) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::invalid_params(format!(
                "git checkout -b {branch} failed in the peer clone: {detail}"
            )));
        }
        // A clone does NOT inherit the source's LOCAL config, so a peer would
        // have no commit identity and every `git commit` would fail. Carry the
        // parent's over when it has one; otherwise git falls back to global.
        for key in ["user.name", "user.email"] {
            let read = std::process::Command::new("git")
                .arg("-C")
                .arg(clone_src)
                .args(["config", "--get", key])
                .output();
            let Ok(out) = read else { continue };
            if !out.status.success() {
                continue;
            }
            let value = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if value.is_empty() {
                continue;
            }
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(clone_dst)
                .args(["config", key, &value])
                .output();
        }
        // Classify the clone first. Shared is published only after the pool
        // allocation below succeeds; no Cargo config is generated.
        let cache = wire_fenced_peer_build_cache(&worktree_path, workspace_root);
        (worktree_path, cache)
    } else {
        (workspace_root.to_path_buf(), PeerBuildCache::None)
    };

    // A local terminal can reconnect only peers from this workspace. Publish
    // this binding before brief.md; never guess the cwd of a legacy peer.
    if let Err(err) = peer_io::write_peer_file_atomic(
        &peer_dir,
        "workspace-root",
        &workspace_root.to_string_lossy(),
    ) {
        cleanup_staged_peer(workspace_root, &slug, &peer_dir);
        return Err(RpcError::internal_error(format!(
            "failed to record peer workspace: {err}"
        )));
    }
    if let Err(err) = peer_io::write_peer_file_atomic(
        &peer_dir,
        "workspace-mode",
        if worktree { "fenced" } else { "shared" },
    ) {
        cleanup_staged_peer(workspace_root, &slug, &peer_dir);
        return Err(RpcError::internal_error(format!(
            "failed to record peer workspace mode: {err}"
        )));
    }

    // codex #6 — record the owner BEFORE brief.md (the visibility gate), atomic
    // + surfaced. A failed owner-write rolls the whole staging back instead of
    // leaving a silently unowned member that a sibling's completion could omit
    // from the fleet and fire synthesis prematurely.
    if let Some(originator) = originator {
        if let Err(err) = peer_io::write_peer_file_atomic(&peer_dir, "originator", originator) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::internal_error(format!(
                "failed to record peer originator: {err}"
            )));
        }
    }

    if let Some(binding) = host_binding {
        if let Err(err) = app_binding::write_host_binding_in(&peer_dir, binding) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::internal_error(format!(
                "failed to record the peer's host binding: {err}"
            )));
        }
    }

    if let Some(limit) = token_budget {
        if let Err(err) =
            peer_io::write_peer_file_durable(&peer_dir, "token_budget", &limit.to_string())
        {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::internal_error(format!(
                "failed to record peer token budget: {err}"
            )));
        }
        if let Err(err) = peer_io::write_peer_file_durable(&peer_dir, "token_budget_usage", "") {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::internal_error(format!(
                "failed to initialize peer token usage: {err}"
            )));
        }
    }

    // Peer-agent-based goal: persist the (goal_id, task_id) pair the master
    // handed off with BEFORE `brief.md`. This ordering is the load-bearing
    // publication invariant: `brief.md` is the visibility gate
    // (`staged_peer_dir` refuses to surface a peer without it), so any peer
    // that becomes visible to the blackboard scan or a peer-boot watcher is
    // GUARANTEED to already carry its goal file. Writing it after brief.md
    // would open a window where a peer boots on the brief and runs its first
    // turn goal-less — a rollback cannot undo an already-started peer.
    //
    // Layout: line 1 = goal_id, line 2 = task_id (may be empty). The goal_id
    // is MANDATORY for the file to exist (no point writing a task_id with no
    // enclosing goal); when the master passes `goal_id = None` the file is
    // omitted entirely and the peer runs goal-less.
    //
    // Failure rolls back the whole staging so the master gets a truthful
    // "handoff failed" rather than a silently goal-less peer.
    if let Some(goal_id_str) = goal_id.map(str::trim).filter(|s| !s.is_empty()) {
        let task_id_str = task_id.map(str::trim).unwrap_or("");
        let body = format!("{goal_id_str}\n{task_id_str}");
        if let Err(err) = peer_io::write_peer_file_atomic(&peer_dir, "goal", &body) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::internal_error(format!(
                "failed to write peer goal context: {err}"
            )));
        }
    }

    // Outer-loop #4 (§4.1/§7.4): acquire the FIRST-turn build-cache slot and
    // record it BEFORE `brief.md` (the visibility gate) — the same ordering
    // discipline as `goal` above: a peer that becomes bootable must already
    // carry its slot record, or boot turn 1 would re-acquire and double-hold
    // two slots of a 2-slot pool. A pool failure (space gate / exhaustion)
    // fails the staging fast (§3.2 step 5) and rolls back.
    //
    // The HANDLE is parked in the process-global registry IMMEDIATELY, so
    // every later staging-failure rollback below (brief / name / record) can
    // release it through the same key as the turn terminal — one shape of
    // release, no per-site `Slot` juggling.
    //
    // Outer-loop #4: config comes from the process side-table (see
    // `set_build_cache_config`) — `None` there keeps the pre-pool behaviour.
    if build_cache == PeerBuildCache::Shared
        && let Some(config) = build_cache_config_for(peers_root)
    {
        match build_cache_peer::acquire_recorded(
            peers_root,
            workspace_root,
            &slug,
            goal_id,
            task_id,
            &config,
        ) {
            Ok(slot) => build_cache_slot_registry()
                .park(build_cache_slot_registry_key(peers_root, &slug), slot),
            Err(err) => {
                cleanup_staged_peer(workspace_root, &slug, &peer_dir);
                return Err(err);
            }
        }
    } else if build_cache == PeerBuildCache::Shared {
        build_cache = PeerBuildCache::None;
    }
    // §4.1 staging-failure rollback: brief.md is the visibility gate and the
    // peer dir is about to be deleted — a peer that never became visible must
    // not keep a slot either. Registry take+release is idempotent.
    let slot_key = build_cache_slot_registry_key(peers_root, &slug);
    let rollback_staged = || {
        build_cache_slot_registry().release(&slot_key, SlotOutcome::Cancelled);
    };
    let brief_path = peer_dir.join("brief.md");
    if let Err(err) = peer_io::write_peer_file_atomic(&peer_dir, "brief.md", brief) {
        rollback_staged();
        cleanup_staged_peer(workspace_root, &slug, &peer_dir);
        return Err(RpcError::internal_error(format!(
            "failed to write brief: {err}"
        )));
    }
    // Record the staging brief as round 1 so the instruction history starts
    // where the work does. `brief.md` above stays authoritative (staging
    // contract + boot prompt); this is the audit copy (#2026).
    record_peer_brief(&peer_dir, brief);

    // Store the display NAME so the peer is addressable by it and readers
    // (`read_peer_blackboard` / `resolve_peer_name_to_slug`) can surface it.
    if let Some(name) = name {
        if let Err(err) = peer_io::write_peer_file_atomic(&peer_dir, "name", name) {
            rollback_staged();
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::internal_error(format!(
                "failed to write peer name: {err}"
            )));
        }
    }

    // NOTE: the slot HANDLE stays parked in the registry from the acquire
    // block above. The serve boot's FIRST turn ADOPTS it out of the registry
    // per §4.1 — same process on the serve path, so the handoff is a map
    // move.
    Ok(StagedPeer {
        topic: format!("peer-{slug}"),
        worktree_branch: worktree.then(|| format!("peer/{slug}")),
        build_cache,
        slug,
        brief_path,
        cwd,
    })
}

/// The `.git` COMMON dir of `workspace_root` — where per-worktree admin dirs
/// live. Resolved via git (not `join(".git")`) because `.git` is a FILE when the
/// workspace is itself a worktree, and the admin dirs then live in the parent
/// repo. `None` when `workspace_root` is not a repo.
pub(crate) fn git_common_dir(workspace_root: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    // `--git-common-dir` may answer relatively (`.git`) — anchor it.
    Some(if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    })
}

/// Drop ONLY the worktree belonging to peer dir `dir`.
///
/// Deliberately NOT `git worktree prune`, which is what this used to do: prune
/// is repo-GLOBAL and removes the admin entry of EVERY worktree whose checkout
/// is currently missing. `git worktree add` registers the admin entry BEFORE the
/// checkout is fully in place, so a sibling peer staged concurrently sits in
/// exactly that window — one peer's rollback silently destroyed another peer's
/// fence, leaving its branch checked out nowhere. Every peer checkout is named
/// `wt` (`peers/<slug>/wt`), so git disambiguates the admin dirs as `wt`, `wt1`,
/// … — which is why this showed up as "the SECOND peer lost its worktree".
///
/// Both steps here are scoped STRICTLY to paths under `dir`, so a sibling is
/// never touched no matter what state it is in.
pub(crate) fn remove_peer_worktree(workspace_root: &Path, dir: &Path) {
    // Normal path: the checkout exists, so git can unregister it by PATH.
    let checkout = dir.join("wt");
    if checkout.is_dir() {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .args(["worktree", "remove", "--force"])
            .arg(&checkout)
            .output();
    }
    // Fallback: a checkout that never finished (or a `remove` git refused)
    // leaves an admin entry behind, and a lingering entry keeps `branch -D`
    // from succeeding — which would burn the slug for any retry. Sweep it by
    // hand, matching ONLY entries whose `gitdir` points inside `dir`.
    let Some(common) = git_common_dir(workspace_root) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else {
        return;
    };
    // Compare canonicalized too: on macOS a temp/staging path reaches git as
    // `/private/var/…` while `dir` is the `/var/…` symlink (or vice versa), and
    // a purely lexical match would silently sweep nothing.
    let canonical = std::fs::canonicalize(dir).ok();
    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_to_string(entry.path().join("gitdir")) else {
            continue;
        };
        let target = Path::new(target.trim());
        let mine = target.starts_with(dir)
            || canonical
                .as_deref()
                .is_some_and(|canonical| target.starts_with(canonical));
        if mine {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Fetch a peer's fence branch out of its own clone and into the workspace repo.
///
/// Peers are staged as CLONES (see `stage_peer`), so `peer/<slug>` exists only
/// inside `peers/<slug>/wt`. Without this the work is invisible from the
/// workspace — `git branch` would not list it and the deliverable would look
/// like it never happened. Run on close, once the peer is done writing.
///
/// Best-effort by design: a peer that never committed, was staged without a
/// worktree, or whose clone is gone simply has nothing to collect, and none of
/// those should fail the close.
#[cfg(feature = "api")]
pub(crate) fn collect_peer_branch(peer_dir: &Path, slug: &str) {
    let clone = peer_dir.join("wt");
    if !clone.join(".git").exists() {
        return;
    }
    // The clone's `origin` IS the workspace repo it was cloned from, so the
    // destination is self-describing — no need to thread a workspace root
    // through the close callback, and it stays correct even if the session's
    // workspace moved after staging.
    let origin = match std::process::Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args(["config", "--get", "remote.origin.url"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        _ => return,
    };
    if origin.is_empty() {
        return;
    }
    let workspace_root = PathBuf::from(origin);
    let branch = format!("peer/{slug}");
    // `+` forces the update: a re-opened peer that committed again must not be
    // refused for a non-fast-forward.
    let refspec = format!("+{branch}:{branch}");
    match std::process::Command::new("git")
        .arg("-C")
        .arg(&workspace_root)
        .args(["fetch", "--no-tags", "--quiet"])
        .arg(&clone)
        .arg(&refspec)
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::info!(slug, branch = %branch, "collected peer branch from its clone");
        }
        Ok(out) => tracing::warn!(
            slug,
            branch = %branch,
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "could not collect the peer branch (it may have committed nothing)"
        ),
        Err(error) => tracing::warn!(slug, %error, "failed to run git to collect the peer branch"),
    }
}

/// Outer-loop #4 (§4.2 fleet rollback): release the first-turn slot a staged
/// peer is still holding, WITHOUT touching the staged dir. Called by the
/// multi-member rollback in the API layer after a sibling member failed — the
/// surviving members' handles sit in the process-global registry from
/// `stage_peer`'s acquire, and dropping the peer dir without this would leak
/// the flock until serve exit (a 2-slot pool is exhausted by the second leak).
/// Idempotent (registry take → None when nothing is held) and a no-op when the
/// pool is off for this root. Call sites are `#[cfg(feature = "api")]`
/// (ui_protocol_transport.rs fleet rollback + close callback) and do not
/// exist in no-api builds; that quadrant is already silenced by the
/// module-level `allow(dead_code)` on `mod peers` (lib.rs).
pub(crate) fn release_staged_peer_build_cache_slot(peers_root: &Path, slug: &str) {
    build_cache_slot_registry().release(
        &build_cache_slot_registry_key(peers_root, slug),
        SlotOutcome::Cancelled,
    );
}

/// Roll back ONE half-staged peer: unregister its OWN worktree, remove its
/// reserved dir, then best-effort `branch -D peer/<slug>` (all no-ops for a
/// member that never got a worktree). The single-member synchronous sibling of
/// [`cleanup_staged_peers`], used inside [`stage_peer`].
pub(crate) fn cleanup_staged_peer(workspace_root: &Path, slug: &str, dir: &Path) {
    // BEFORE `remove_dir_all`: `git worktree remove` needs the checkout on disk
    // to unregister it by path.
    remove_peer_worktree(workspace_root, dir);
    let _ = std::fs::remove_dir_all(dir);
    let branch = format!("peer/{slug}");
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["branch", "-D", &branch])
        .output();
}

/// #1801 v3: per-turn ceiling on `peer_handoff` staging calls. A runaway
/// model cannot fan out an unbounded peer fleet in one turn; the 5th call
/// is rejected with a model-visible error.
pub(crate) const PEER_HANDOFFS_PER_TURN_MAX: u32 = 4;

/// #1801 v3 depth-1 guard: a peer session (topic `peer-<slug>`) never gets
/// the `peer_handoff` tool registered at all — peers cannot hand off
/// recursively, and the tool is not even visible to the model there.
#[cfg(any(feature = "api", test))]
pub(crate) fn peer_handoff_allowed_for_session(session_id: &SessionKey) -> bool {
    !session_id.topic().is_some_and(|topic| {
        topic.starts_with("peer-") || topic.starts_with(app_binding::PEER_CONTEXT_TOPIC_PREFIX)
    })
}

/// #20a (smart worktree fencing) — a collision risk REASON. When the model
/// left `worktree` unspecified, any hit flips the auto-default to FENCED;
/// when the model explicitly said `worktree=false`, each hit becomes a
/// warning in the staged `model_note`. Zero hits keeps the legacy default
/// (unfenced) — the single-goal / single-branch path pays nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FenceCollisionReason {
    /// ① More than one ACTIVE goal exists for this profile in this instance.
    MultipleActiveGoals,
    /// ② The master's tree is not on `main`/`master` — this run already
    /// branched away from trunk, so an unfenced peer would share it.
    MainTreeOnNonDefaultBranch,
    /// ③ Another peer is staged, not closed, has no result yet (in-flight)
    /// AND was not fenced.
    UnfencedPeerInFlight,
}

impl FenceCollisionReason {
    fn note(self) -> &'static str {
        match self {
            Self::MultipleActiveGoals => "multiple active goals in this instance",
            Self::MainTreeOnNonDefaultBranch => "the master's tree is on a non-default branch",
            Self::UnfencedPeerInFlight => "an unfenced peer is already in flight",
        }
    }
}

/// Evaluate the #20a collision predicate. Input cost is exactly one in-memory
/// goal-map scan plus a bounded peers-dir read; the expensive checks
/// (a `git` subprocess for the current branch, the peer scan) only run when
/// no cheaper reason already fired, and NONE of them run when the caller
/// passed an explicit `worktree=true` (see the callback).
fn fence_collision_reasons(
    peers_root: &Path,
    workspace_root: &Path,
    profile_id: &str,
) -> Vec<FenceCollisionReason> {
    let mut reasons = Vec::new();
    // ① Active-goal count: pure in-memory map scan.
    if default_agent_orchestrator().profile_active_goal_count(profile_id) > 1 {
        reasons.push(FenceCollisionReason::MultipleActiveGoals);
        return reasons;
    }
    // ② Master's current branch (the workspace root passed in IS the master's
    // tree — peers fence as CLONES, so a fenced master's root still reads its
    // own branch here). Detached HEAD / non-git / git failure all read as
    // "unknown" and do NOT trigger.
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(dunce::simplified(workspace_root))
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned());
    match branch.as_deref() {
        Some("main") | Some("master") | None | Some("") => {}
        Some(_) => {
            reasons.push(FenceCollisionReason::MainTreeOnNonDefaultBranch);
            return reasons;
        }
    }
    // ③ Scan the blackboard for a staged peer that is STILL IN FLIGHT (no
    // result, not closed) and was NOT fenced.
    if read_peer_blackboard(peers_root, None)
        .iter()
        .any(|row| row.result.is_none() && !row.closed && !row.has_worktree)
    {
        reasons.push(FenceCollisionReason::UnfencedPeerInFlight);
    }
    reasons
}

/// #20a — fold the caller's `worktree` preference and the collision predicate
/// into the EFFECTIVE fence decision plus an optional model-visible warning.
/// Explicit `true` short-circuits (fenced, zero predicate cost); explicit
/// `false` wins but warns on a predicate hit; omitted takes the predicate.
pub(crate) fn resolve_peer_worktree(
    request_worktree: Option<bool>,
    reasons: &[FenceCollisionReason],
) -> (bool, Option<String>) {
    match request_worktree {
        Some(true) => (true, None),
        Some(false) => {
            let warning = (!reasons.is_empty()).then(|| {
                let why = reasons
                    .iter()
                    .map(|reason| reason.note())
                    .collect::<Vec<_>>()
                    .join("; ");
                format!(
                    "warning: you passed worktree=false, but collision risk was detected ({why}) —                      staging this peer UNFENCED as requested; it may collide with concurrent work                      in the shared tree. Omit the worktree argument to auto-fence in this situation."
                )
            });
            (false, warning)
        }
        None => (!reasons.is_empty(), None),
    }
}

/// #1801 v3: build the `peer_handoff` staging callback for ONE turn of the
/// serve/WS path. Turn-scoped state is baked in at wiring time: the
/// profile's `peers/` root, the session's workspace root, the ORIGINATING
/// session key stamped onto the emitted `peer/staged` event, and the
/// per-turn handoff counter enforcing [`PEER_HANDOFFS_PER_TURN_MAX`].
/// `emit_staged` abstracts the durable notification send so tests can
/// observe the event without a live WS connection.
pub(crate) fn build_peer_handoff_callback(
    peers_root: PathBuf,
    workspace_root: PathBuf,
    originating_session: SessionKey,
    profile_id: String,
    // #peer-model — the KEYS of the profile's configured `sub_providers`
    // (model lanes). A `peer_handoff` naming a matching lane records it beside
    // the brief so the peer runs its turns on that provider; an unknown lane
    // is surfaced as a warning note (the peer falls back to the primary
    // model), never a failure.
    available_lanes: Vec<String>,
    handoffs_this_turn: Arc<AtomicU32>,
    emit_staged: Arc<dyn Fn(PeerStagedEvent) + Send + Sync>,
) -> octos_agent::PeerHandoffCallback {
    Arc::new(move |request: octos_agent::PeerHandoffRequest| {
        if handoffs_this_turn.fetch_add(1, Ordering::SeqCst) >= PEER_HANDOFFS_PER_TURN_MAX {
            return Err(format!(
                "peer handoff limit reached for this turn ({PEER_HANDOFFS_PER_TURN_MAX})"
            ));
        }
        // Peers are named: the slug is derived from the (required, validated)
        // name and must be unique — `stage_peer` rejects a duplicate rather
        // than auto-suffixing. `seed` is unused on the named path.
        //
        // codex #6 — record WHO handed off (the originating master session) so
        // the fleet-ownership scan + `peer_results_ready_note` are reliable.
        // `stage_peer` writes it atomically BEFORE brief.md and rolls the
        // staging back on failure, so a peer is never visible-but-ownerless.
        let originator = originating_session.to_string();
        // Peer-agent-based goal AUTO-BIND (#1953): if the master handed off
        // WITHOUT an explicit goal_id but its session has an ACTIVE goal, bind
        // the peer to that goal. The model (esp. k3) does not reliably thread
        // goal_id — it parallelizes goal_create+peer_handoff (the id isn't
        // available yet) or simply omits it — so relying on the LLM leaves
        // every peer goal-less and the whole loop inert. The active goal is
        // the correct default; an explicit goal_id still wins.
        let resolved_goal_id: Option<String> = request.goal_id.clone().or_else(|| {
            default_agent_orchestrator().active_goal_id(&originating_session, &profile_id)
        });
        // #20a — smart fencing default. An explicit `worktree=true` fences
        // with NO predicate evaluation (no syscalls added for the caller who
        // already said "fence"); an explicit/absent value evaluates the
        // collision predicate, and `Some(false)` is still honored — with a
        // model-visible warning recorded in `model_note`.
        let fence_reasons = if request.worktree == Some(true) {
            Vec::new()
        } else {
            fence_collision_reasons(&peers_root, &workspace_root, &profile_id)
        };
        let (effective_worktree, fence_warning) =
            resolve_peer_worktree(request.worktree, &fence_reasons);
        let staged = stage_peer_with_budget(
            &peers_root,
            &workspace_root,
            &request.name,
            Some(&request.name),
            Some(originator.as_str()),
            &request.brief,
            effective_worktree,
            // Explicit goal_id wins; else the master's active goal (auto-bind).
            resolved_goal_id.as_deref(),
            request.task_id.as_deref(),
            request.token_budget,
            None,
        )
        .map_err(|err| err.message)?;
        // #peer-model — optional model lane. Record a VALID lane symlink-safely
        // under the re-validated staged dir; an unknown lane (or a failed
        // record) is a truthful warning (the peer runs on the primary model),
        // never a staging failure.
        let mut model_note = record_peer_model_lane(
            &peers_root,
            &staged.slug,
            request.model.as_deref(),
            &available_lanes,
        );

        // #20a — surface an explicit-false override warning alongside any
        // model-lane note in the SAME `model_note` field the tool already
        // appends to its success output.
        if let Some(warning) = fence_warning {
            model_note = Some(match model_note {
                Some(note) => format!("{note} {warning}"),
                None => warning,
            });
        }
        // #2236 — surface the build-cache decision in the same model_note
        // field (newline-joined, existing content preserved).
        if let Some(line) = staged.build_cache.note_line(&workspace_root) {
            model_note = Some(match model_note {
                Some(note) => format!("{note}\n{line}"),
                None => line,
            });
        }
        // OLP L1 (slice 5): structured observability event. The lane is
        // the RESOLVED one (what record_peer_model_lane actually persisted);
        // an unset/invalid lane resolves to the primary model, which the
        // contract pins as the literal "primary".
        {
            let resolved_lane = read_peer_model_lane(&peers_root, &staged.slug);
            let lane = resolved_lane.as_deref().unwrap_or("primary");
            let session_str = originating_session.0.as_str();
            // events.jsonl lives at the data-dir root = peers_root's parent.
            let data_dir = peers_root
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| peers_root.clone());
            let staged_detail = if effective_worktree {
                format!("peer staged{}", staged.build_cache.detail_suffix())
            } else {
                "peer staged".to_string()
            };
            crate::obs_events::append_obs_event(
                &data_dir,
                &crate::obs_events::ObsEvent::new("peer_staged", &staged_detail)
                    .goal_id(resolved_goal_id.as_deref())
                    .slug(Some(staged.slug.as_str()))
                    .session(Some(session_str))
                    .model_lane(Some(lane)),
            );
        }
        // Durable so reconnect replay still delivers the open request; the
        // client dedups by an already-open session for the topic.
        emit_staged(PeerStagedEvent {
            session_id: originating_session.clone(),
            topic: staged.topic.clone(),
            slug: staged.slug.clone(),
            brief: request.brief.clone(),
            brief_path: staged.brief_path.to_string_lossy().into_owned(),
            cwd: staged.cwd.to_string_lossy().into_owned(),
            worktree_branch: staged.worktree_branch.clone(),
            profile_id: profile_id.clone(),
        });
        Ok(octos_agent::PeerHandoffStaged {
            slug: staged.slug,
            topic: staged.topic,
            brief_path: staged.brief_path.to_string_lossy().into_owned(),
            cwd: staged.cwd.to_string_lossy().into_owned(),
            worktree_branch: staged.worktree_branch,
            model_note,
            token_budget: request.token_budget,
        })
    })
}

/// #peer-model — read a small text file through an `O_NOFOLLOW` open (Unix) so
/// a symlink leaf swapped into a validated peer dir cannot redirect the read to
/// an off-tenant target (mirrors `read_file_no_follow` for the sync peer-file
/// layer). On non-Unix, re-checks `symlink_metadata` first. `None` on any error
/// (missing, symlink, unreadable).
/// #peer-model — read a peer's optional model LANE key from
/// `peers/<slug>/model` (written by the `peer_handoff` staging callback when
/// the master named a VALID `sub_provider` lane). Routed through
/// [`staged_peer_dir`] (real, non-symlink dir with `brief.md`) AND the
/// fd-anchored [`peer_io::read_peer_file`] (openat `O_NOFOLLOW` under the pinned
/// dir fd, regular-file only) so neither a symlinked dir nor a symlinked/FIFO
/// `model` leaf is ever followed. Returns the trimmed lane key, or `None` when
/// the dir is not a real staged peer, the file is absent/symlinked, or empty.
pub(crate) fn read_peer_model_lane(peers_root: &Path, slug: &str) -> Option<String> {
    let dir = staged_peer_dir(peers_root, slug)?;
    let lane = peer_io::read_peer_file(&dir, "model", peer_io::PEER_FILE_READ_CAP_SMALL)?;
    let lane = lane.trim();
    (!lane.is_empty()).then(|| lane.to_owned())
}

/// #peer-model — record a requested model lane for a freshly-staged peer,
/// returning the tool-visible note (`None` = recorded cleanly, or no lane was
/// requested). Validates the (trimmed) lane against the CURRENT
/// `available_lanes`; a match is written symlink-safely under the RE-VALIDATED
/// [`staged_peer_dir`] (never `brief_path.parent()`, which races a parent
/// swap) via the fd-anchored atomic writer ([`peer_io::write_peer_file_atomic`],
/// no-follow openat + renameat under the pinned dir fd). Both an unknown lane
/// and a failed record are TRUTHFUL: they say the peer will run on the primary
/// model, matching what the turn actually does.
/// #2236 — the fenced-peer build-cache decision, for `model_note` and the
/// `peer_staged` event detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerBuildCache {
    /// A slot is held in the source repository's pool; tools receive its env.
    Shared,
    /// Unfenced, non-Cargo, or unregistered pool: no cache override or note.
    None,
    /// The repo's `.cargo/config.toml` or legacy `.cargo/config`: respected.
    RepoConfig,
}

impl PeerBuildCache {
    /// The `model_note` line for this decision (None = no note line).
    pub(crate) fn note_line(&self, _workspace_root: &Path) -> Option<String> {
        match self {
            PeerBuildCache::Shared => {
                Some("build cache: slot pool (CARGO_TARGET_DIR injected per tool call)".to_string())
            }
            PeerBuildCache::RepoConfig => {
                Some("build cache: repo has its own Cargo config, left untouched".to_string())
            }
            PeerBuildCache::None => None,
        }
    }

    /// The `peer_staged` detail suffix for this decision.
    pub(crate) fn detail_suffix(&self) -> &'static str {
        match self {
            PeerBuildCache::Shared => " (build cache: shared)",
            PeerBuildCache::RepoConfig => " (build cache: repo-config)",
            PeerBuildCache::None => "",
        }
    }
}

/// Classify a fenced Cargo peer without changing repository files.
/// Shared is an allocation candidate; staging only publishes it after a
/// configured pool actually provides a slot. RepoConfig always wins over
/// our env injection, which would otherwise override Cargo's config file.
pub(crate) fn wire_fenced_peer_build_cache(
    worktree_path: &Path,
    workspace_root: &Path,
) -> PeerBuildCache {
    if !workspace_root.join("Cargo.toml").is_file() {
        return PeerBuildCache::None;
    }
    if worktree_path.join(".cargo/config.toml").exists()
        || worktree_path.join(".cargo/config").exists()
    {
        return PeerBuildCache::RepoConfig;
    }
    PeerBuildCache::Shared
}

pub(crate) fn record_peer_model_lane(
    peers_root: &Path,
    slug: &str,
    requested: Option<&str>,
    available_lanes: &[String],
) -> Option<String> {
    let lane = requested.map(str::trim).filter(|lane| !lane.is_empty())?;
    if !available_lanes.iter().any(|key| key == lane) {
        let available = if available_lanes.is_empty() {
            "none configured".to_owned()
        } else {
            available_lanes.join(", ")
        };
        return Some(format!(
            "model lane '{lane}' not found (available: {available}) — \
             this peer will use the primary model."
        ));
    }
    let recorded = match staged_peer_dir(peers_root, slug) {
        Some(dir) => peer_io::write_peer_file_atomic(&dir, "model", lane)
            .map_err(|err| eyre::eyre!("failed to write peer model lane: {err}")),
        None => Err(eyre::eyre!("staged peer dir not found for slug {slug}")),
    };
    if let Err(err) = recorded {
        tracing::warn!(
            ?err,
            slug,
            lane,
            "failed to record peer model lane; peer will use the primary model"
        );
        return Some(format!(
            "could not record model lane '{lane}' — this peer will use the primary model."
        ));
    }
    None
}

/// Cap on the human-readable prompt summary shown for a parked prompt.
pub(crate) const PEER_PENDING_PROMPT_CAP: usize = 2048;

/// Kind of interactive prompt a peer session is parked on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerPendingKind {
    Approval,
    Question,
}

impl PeerPendingKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Question => "question",
        }
    }
}

/// One interactive prompt a PEER session is currently parked on, PROJECTED from
/// the process-global pending store (`contract_stores()`), which is the single
/// AUTHORITY for "awaiting input" — not the filesystem. Because that store is
/// in-memory, internally consistent, and shared by peer_list / peer_respond /
/// peer_close (all in the serve process), there is no torn on-disk index, no
/// stale-delete that could hide a still-parked entry, and no marker a peer could
/// park without: the store entry exists the instant `request_runtime` registers
/// the oneshot, so a parked peer is visible and answerable for as long as it
/// remains OPEN. A CLOSED peer never parks at all: `PeerParkGate` (in the
/// serve/WS tree, `api::ui_protocol`) refuses the registration and the close
/// aborts the peer's turn (#1842), so this projection can never hide a live
/// park.
#[derive(Debug, Clone)]
pub(crate) struct PeerPendingSummary {
    pub(crate) kind: PeerPendingKind,
    /// The `ApprovalId`/`QuestionId` as a string — the master targets a specific
    /// prompt by this (`peer_list` lists each id).
    pub(crate) id: String,
    /// Compact prompt summary for display.
    pub(crate) prompt: String,
    /// For a QUESTION: the offered option labels across its questions (a display
    /// hint; real answers are validated by the store against the stored request).
    /// Empty for an approval.
    pub(crate) options: Vec<String>,
}

/// Compact, capped summary of an interactive prompt (title/body) for display.
pub(crate) fn peer_pending_prompt_summary(title: &str, body: &str) -> String {
    let title = title.trim();
    let body = body.trim();
    let combined = if title.is_empty() {
        body.to_owned()
    } else if body.is_empty() || body == title {
        title.to_owned()
    } else {
        format!("{title} — {body}")
    };
    capped_utf8(combined, PEER_PENDING_PROMPT_CAP).0
}

/// Every prompt `session` is currently parked on, read from the AUTHORITATIVE
/// process-global stores. Approvals first, then questions, each group ordered by
/// id — a deterministic order for display and single-default selection.
pub(crate) fn peer_pending_summaries(
    contracts: &UiProtocolContractStores,
    session: &SessionKey,
) -> Vec<PeerPendingSummary> {
    let mut approvals: Vec<PeerPendingSummary> = contracts
        .approvals
        .pending_for_session(session)
        .into_iter()
        .map(|event| PeerPendingSummary {
            kind: PeerPendingKind::Approval,
            id: event.approval_id.0.to_string(),
            prompt: peer_pending_prompt_summary(&event.title, &event.body),
            options: Vec::new(),
        })
        .collect();
    approvals.sort_by(|a, b| a.id.cmp(&b.id));
    let mut questions: Vec<PeerPendingSummary> = contracts
        .user_questions
        .pending_for_session(session)
        .into_iter()
        .map(|event| PeerPendingSummary {
            kind: PeerPendingKind::Question,
            id: event.question_id.0.to_string(),
            prompt: peer_pending_prompt_summary(&event.title, &event.body),
            options: event
                .questions
                .iter()
                .flat_map(|question| question.options.iter().map(|option| option.label.clone()))
                .collect(),
        })
        .collect();
    questions.sort_by(|a, b| a.id.cmp(&b.id));
    approvals.append(&mut questions);
    approvals
}

/// The part of a peer's parked set its ORIGINATOR may answer. A host-owned app
/// peer's tool approvals belong to the person in the app's own UI (ADR 0007,
/// UPCR-2026-034): they are dropped here, so neither `peer_list` nor
/// `peer_respond` offers them to the owning system agent. Every other peer's
/// set is returned unchanged.
pub(crate) fn peer_pending_answerable_by_originator(
    peers_root: &Path,
    slug: &str,
    pendings: Vec<PeerPendingSummary>,
) -> Vec<PeerPendingSummary> {
    if !app_binding::peer_is_host_owned(peers_root, slug) {
        return pendings;
    }
    pendings
        .into_iter()
        .filter(|pending| pending.kind != PeerPendingKind::Approval)
        .collect()
}

/// The refusal `peer_respond` returns for a host-owned app peer's approval.
fn host_owned_approval_refusal(slug: &str) -> String {
    format!(
        "peer '{slug}' is a host-owned app peer: its tool approvals are answered \
         only by the person in the app, not via peer_respond — leave it to them"
    )
}

/// The peer's TRUSTED session key (#P1-1): the wire it runs its turns under,
/// recorded server-side at `session/open`. `None` when the peer is not currently
/// open — it then has no live oneshot to answer or cancel. This is the ONLY
/// slug→session mapping any peer-control path trusts; it never comes from a
/// client argument or an on-disk file.
pub(crate) fn peer_trusted_session(profile_id: &str, slug: &str) -> Option<SessionKey> {
    peer_wire_registry().resolve(&peer_wire_key(profile_id, slug))
}

/// Map the tool's answer entries onto the store's `UserQuestionAnswer[]`,
/// matching each entry to ITS question's options (#new-P2-#2): a bare string
/// answer to a CHOICE question becomes a real label selection for THAT question
/// (so a 2–4-question choice prompt is answerable), while free text passes
/// through where the question allows it. One answer per stored question, in
/// order; a mismatched count/label surfaces the store's typed error rather than
/// resolving incorrectly.
pub(crate) fn peer_respond_build_answers(
    req_answers: &[octos_agent::PeerRespondAnswer],
    questions: &[octos_core::ui_protocol::UserQuestion],
) -> Vec<octos_core::ui_protocol::UserQuestionAnswer> {
    req_answers
        .iter()
        .enumerate()
        .map(|(index, answer)| {
            if answer.selected_labels.is_empty() {
                if let Some(text) = &answer.free_text {
                    if let Some(option) = questions.get(index).and_then(|question| {
                        question
                            .options
                            .iter()
                            .find(|option| option.label.eq_ignore_ascii_case(text))
                    }) {
                        return octos_core::ui_protocol::UserQuestionAnswer {
                            selected_labels: vec![option.label.clone()],
                            free_text: None,
                        };
                    }
                }
            }
            octos_core::ui_protocol::UserQuestionAnswer {
                selected_labels: answer.selected_labels.clone(),
                free_text: answer.free_text.clone(),
            }
        })
        .collect()
}

type PeerApprovalDecidedSink<'a> =
    dyn Fn(&ApprovalDecidedEvent, Option<&str>) -> Result<(), String> + 'a;

/// The cross-session resolution `peer_respond`'s host callback performs — a
/// named fn (rather than an inline closure) so tests exercise the EXACT
/// production path. Authorizes the caller as the peer's recorded originator,
/// derives the peer's TRUSTED session key from the wire registry (#P1-1),
/// selects the targeted parked prompt from the AUTHORITATIVE store (by `id`, or
/// the sole one), and resolves that oneshot via the SAME store the client
/// `approval/respond` / `user_question/respond` RPCs use. For an approval it
/// also emits `approval/decided` + audit via `on_approval_decided` (#P1-5),
/// attributing the MASTER. No filesystem marker is read or written — the store
/// is the source of truth. Every error is a model-visible string.
pub(crate) fn peer_respond_resolve(
    peers_root: &Path,
    origin_session: &str,
    profile_id: &str,
    contracts: &UiProtocolContractStores,
    on_approval_decided: &PeerApprovalDecidedSink<'_>,
    req: octos_agent::PeerRespondRequest,
) -> Result<(), String> {
    // Resolve NAME/slug → real slug (names are the primary address).
    let slug = resolve_peer_name_to_slug(peers_root, &req.slug).ok_or_else(|| {
        format!(
            "no peer named '{ident}' — check the name (or slug) with peer_list",
            ident = req.slug
        )
    })?;
    if !peer_slug_is_safe(&slug) {
        return Err(format!("invalid peer slug '{slug}'"));
    }
    // Only the peer's recorded originator may respond (the same fail-closed
    // check peer_send_input uses; the originator lives in `peers/<slug>/`).
    peer_send_input_authorized(peers_root, &slug, origin_session)?;
    // A retired peer is not awaiting input.
    if peer_is_closed(peers_root, &slug) {
        return Err(format!("peer '{slug}' is closed"));
    }

    // #P1-1 SECURITY — the peer's TRUSTED session key comes ONLY from the wire
    // registry (server-captured at `session/open`), so a resolution can reach
    // exactly THIS peer's oneshots and no other's.
    let Some(peer_session) = peer_trusted_session(profile_id, &slug) else {
        return Err(format!(
            "peer '{slug}' is not open — the user must open the staged peer session before it can be answered"
        ));
    };

    // The AUTHORITATIVE parked set for this peer, straight from the store.
    let all_pendings = peer_pending_summaries(contracts, &peer_session);
    // ADR 0007 — a host-owned app peer's approvals are the person's, answered
    // in the app's own UI. The originator (the system agent) may answer the
    // peer's questions but never approve its tools: refuse a targeted
    // approval, and never select one by default.
    if app_binding::peer_is_host_owned(peers_root, &slug) {
        let targets_approval = match req.id.as_deref() {
            Some(id) => all_pendings
                .iter()
                .any(|p| p.id == id && p.kind == PeerPendingKind::Approval),
            None => {
                req.decision.is_some()
                    && all_pendings
                        .iter()
                        .any(|p| p.kind == PeerPendingKind::Approval)
            }
        };
        if targets_approval {
            return Err(host_owned_approval_refusal(&slug));
        }
    }
    let pendings = peer_pending_answerable_by_originator(peers_root, &slug, all_pendings);
    if pendings.is_empty() {
        return Err(format!(
            "peer '{slug}' is not awaiting input — nothing to respond to \
             (peer_list shows a peer as `awaiting_input` when it is)"
        ));
    }
    let target = match req.id.as_deref() {
        Some(id) => pendings.iter().find(|p| p.id == id).ok_or_else(|| {
            format!(
                "peer '{slug}' has no pending prompt with id '{id}' — check the ids with peer_list"
            )
        })?,
        None if pendings.len() == 1 => &pendings[0],
        None => {
            let ids = pendings
                .iter()
                .map(|p| format!("{} ({})", p.id, p.kind.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "peer '{slug}' has {n} pending prompts — pass the specific id (one of: {ids})",
                n = pendings.len()
            ));
        }
    };

    // #1961 — the human-readable resolution to stamp on the goal-ledger
    // escalation once the answer/decision is delivered below.
    let escalation_resolution: String;
    match target.kind {
        PeerPendingKind::Approval => {
            let Some(decision) = req.decision.as_deref() else {
                return Err(format!(
                    "peer '{slug}' pending '{id}' is an APPROVAL — pass \
                     decision=\"approve\"/\"deny\" (not answer)",
                    id = target.id
                ));
            };
            escalation_resolution = format!("[approval] {decision}");
            let approval_id: ApprovalId =
                serde_json::from_value(serde_json::Value::String(target.id.clone()))
                    .map_err(|_| format!("peer '{slug}' pending id is malformed"))?;
            let params = octos_core::ui_protocol::ApprovalRespondParams {
                session_id: peer_session,
                approval_id,
                decision: ApprovalDecision::from(decision.to_owned()),
                approval_scope: None,
                client_note: Some(format!(
                    "answered by master via peer_respond ({origin_session})"
                )),
            };
            contracts
                .approvals
                .respond_with_context_and_persist(params.clone(), |outcome| {
                    // #P1-5 — publish the canonical `approval/decided` + audit,
                    // attributing the master, before waking the approved runtime.
                    let tool_name = outcome.context.as_ref().map(|ctx| ctx.tool_name.clone());
                    let event = crate::contracts::approvals::build_decided_event(
                        &params,
                        outcome,
                        origin_session,
                        Utc::now(),
                    );
                    on_approval_decided(&event, tool_name.as_deref())
                })
                .map_err(|err| match err {
                    crate::contracts::approvals::ApprovalRespondPersistError::Rpc(err) => {
                        format!("could not resolve peer '{slug}' approval: {}", err.message)
                    }
                    crate::contracts::approvals::ApprovalRespondPersistError::Persist(err) => {
                        format!("could not persist peer '{slug}' approval decision: {err}")
                    }
                })?;
        }
        PeerPendingKind::Question => {
            let Some(req_answers) = req.answers.as_deref() else {
                return Err(format!(
                    "peer '{slug}' pending '{id}' is a QUESTION — pass answer/answers (not decision)",
                    id = target.id
                ));
            };
            escalation_resolution = format!(
                "[answer] {}",
                req_answers
                    .iter()
                    .map(|a| if a.selected_labels.is_empty() {
                        a.free_text.clone().unwrap_or_default()
                    } else {
                        a.selected_labels.join("/")
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            let question_id: octos_core::ui_protocol::QuestionId =
                serde_json::from_value(serde_json::Value::String(target.id.clone()))
                    .map_err(|_| format!("peer '{slug}' pending id is malformed"))?;
            // #new-P2-#2 — map each answer against the STORED request's questions
            // (per-question options), re-read from the authoritative store.
            let questions = contracts
                .user_questions
                .pending_for_session(&peer_session)
                .into_iter()
                .find(|event| event.question_id == question_id)
                .map(|event| event.questions)
                .unwrap_or_default();
            let answers = peer_respond_build_answers(req_answers, &questions);
            let params = UserQuestionRespondParams {
                session_id: peer_session,
                question_id,
                answers,
                client_note: Some(format!(
                    "answered by master via peer_respond ({origin_session})"
                )),
            };
            contracts
                .user_questions
                .respond_with_context(&params)
                .map_err(|err| {
                    format!("could not resolve peer '{slug}' question: {}", err.message)
                })?;
        }
    }
    // #1961 — the answer/decision was delivered above; mark this peer's OPEN
    // escalation resolved in the goal ledger so its durable escalation history
    // stops showing it as open. Best-effort: a goal-less peer, a missing
    // ledger, or no open escalation is a benign no-op, and a ledger write
    // failure must NOT fail the resume the caller already committed to.
    //
    // #1967 codex round — ordering: because delivery happens BEFORE this
    // resolve, a timeout sweep (`sweep_escalation_timeouts`) firing in the
    // gap can flip the row to `[timeout] …` first, making this bulk resolve
    // a no-op — the ledger then shows a timeout for an escalation that was
    // actually answered (the delivered answer itself is unaffected). Dormant
    // while producers write `default_after_secs = None`; the hazard and the
    // deliberate no-amend-API decision are documented on the sweep.
    if let Some(peer_dir) = staged_peer_dir(peers_root, &slug) {
        let goal_id = peer_io::read_peer_file(&peer_dir, "goal", peer_io::PEER_FILE_READ_CAP_SMALL)
            .and_then(|body| body.lines().next().map(|l| l.trim().to_owned()))
            .filter(|s| !s.is_empty());
        if let (Some(goal_id), Some(data_dir)) = (goal_id, peers_root.parent()) {
            if let Err(err) = default_agent_orchestrator().model_goal_resolve_peer_escalation(
                data_dir,
                &goal_id,
                &slug,
                &escalation_resolution,
                origin_session,
            ) {
                tracing::warn!(
                    slug = %slug,
                    goal_id = %goal_id,
                    error = %err,
                    "peer-goal: failed to resolve escalation in goal ledger (answer already delivered)"
                );
            }
        }
    }
    Ok(())
}

/// Count how many `result-<n>.md` version files exist in the peer directory,
/// via the fd-anchored, regular-file-only, scan-capped enumerator so swapping
/// `<slug>` to a symlink can neither redirect the scan into another tree nor
/// inflate the derived version number (#1824).
pub(crate) fn count_peer_result_versions(peer_dir: &std::path::Path) -> u32 {
    peer_io::peer_dir_count_prefixed(peer_dir, "result-", peer_io::PEER_DIR_SCAN_CAP) as u32
}

/// The lock a writer holds while it publishes one round of a staged peer
/// (numbers `result-<n>.md`, writes it, `result.md` and the `turns.txt`
/// line). A host-owned app peer's two lanes (its own session and a request
/// context with shared history, UPCR-2026-034) finish turns concurrently;
/// without it two writers could count the same `n` and one round would
/// overwrite the other, or `turns.txt` and `result.md` could name rounds out
/// of order. Keyed by the peer dir, process-wide (one kernel owns a peers
/// root).
pub(crate) fn peer_round_publish_lock(peer_dir: &Path) -> Arc<std::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<std::sync::Mutex<()>>>>> =
        OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(peer_dir.to_path_buf())
        .or_default()
        .clone()
}

/// Count how many `brief-<n>.md` instruction files exist in the peer directory
/// — the INPUT half of what [`count_peer_result_versions`] counts on the output
/// half, through the same fd-anchored, regular-file-only, scan-capped
/// enumerator (#1824).
///
/// The bare `brief.md` is deliberately NOT counted: its prefix is `brief.`, not
/// `brief-`. A peer staged before brief versioning existed therefore reads as
/// ZERO recorded rounds, and its first recorded instruction becomes round 1 —
/// which is the correct back-compat reading, since its original brief was never
/// captured as a round.
pub(crate) fn count_peer_brief_versions(peer_dir: &std::path::Path) -> u32 {
    peer_io::peer_dir_count_prefixed(peer_dir, "brief-", peer_io::PEER_DIR_SCAN_CAP) as u32
}

/// Record ONE round of master→peer instruction as `brief-<n>.md`, plus a
/// `briefs.txt` index line — the mirror of the `result-<n>.md` / `turns.txt`
/// pair (#435/#1824) for the input half of the exchange.
///
/// Closes the asymmetry #2026 documents: a multi-round peer kept every
/// `result-<n>.md` it produced but only ever the ORIGINAL `brief.md`, so the
/// instructions that drove rounds 2..N were unrecoverable after the fact
/// (`peer_send_input` delivers into the running session, which is not
/// persisted). A live 44-peer fleet had peers with four recorded results and
/// one recorded brief.
///
/// `brief.md` is never rewritten. It is BOTH the staging contract
/// [`staged_peer_dir`] enforces AND the boot prompt `peers::host` replays, so
/// rotating it would silently change what a rehydrated peer starts from.
///
/// Best-effort by design: a failure is logged and swallowed. Losing the audit
/// copy must never fail the instruction it is recording — the same posture the
/// `result-<n>.md` / `turns.txt` writes take.
pub(crate) fn record_peer_brief(peer_dir: &std::path::Path, body: &str) -> u32 {
    let round = count_peer_brief_versions(peer_dir) + 1;
    let updated_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let text = format!("---\nround: {round}\nupdated_unix: {updated_unix}\n---\n\n{body}\n");
    if let Err(err) = peer_io::write_peer_file_atomic(peer_dir, &format!("brief-{round}.md"), &text)
    {
        tracing::warn!(?err, round, "failed to write versioned peer brief");
    }
    if let Err(err) =
        peer_io::append_peer_line(peer_dir, "briefs.txt", &format!("{round} {updated_unix}\n"))
    {
        tracing::warn!(?err, round, "failed to append to briefs.txt");
    }
    round
}

/// Parse `turns.txt` into `[(turn_count, outcome, updated_unix)]`.
/// Returns `None` when the file doesn't exist.
pub(crate) fn parse_peer_turns_index(
    peer_dir: &std::path::Path,
) -> Option<Vec<(u32, String, u64)>> {
    let text = peer_io::read_peer_file(peer_dir, "turns.txt", peer_io::PEER_FILE_READ_CAP_SMALL)?;
    if text.trim().is_empty() {
        return Some(Vec::new());
    }
    let entries: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let count: u32 = parts.next()?.parse().ok()?;
            let outcome = parts.next()?.to_string();
            let ts: u64 = parts.next()?.parse().ok()?;
            Some((count, outcome, ts))
        })
        .collect();
    Some(entries)
}

// ---------------------------------------------------------------------------
// task-evo-peer-turn-status — lifetime read-only projection & execution
// derivation (v3.1 frozen contract).
//
// Design (spec Decisions, outer-loop + GLM/k3 reviewed):
//   * execution's ONLY current-state authorities are (a) the `closed` marker
//     and (b) a TRUSTED read-only projection of `lifetime.json`. There is NO
//     parallel state file and NO derivation from `turns.txt` — an old
//     terminal outcome proves the PAST, never the present (that inference is
//     precisely the stale-done bug this task fixes, in the other direction).
//   * `last_outcome` comes from a STRICT cross-check of the last `turns.txt`
//     entry against the highest-numbered `result-<n>.md` frontmatter
//     (independent reader — the lenient legacy parser stays untouched for
//     its existing consumers). Mismatch ⇒ null: the two native terminal
//     records disagree, so no outcome is asserted.
//   * Trust is fail-closed and typed (see recovery.rs
//     `trusted_lifetime_projection`): reuses PeerLifetime's strict enum and
//     Option field types instead of a hand-rolled serde_json validator, adds
//     the registry_key check and the Idle digest re-computation. Any failure
//     degrades the peer to execution=unknown AND nulls the identity fields.
// ---------------------------------------------------------------------------

/// The execution facet of one peer, derived per the v3.1 contract. Field
/// names are the machine contract (docs/peer-status-interface.json).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerExecutionFacet {
    /// queued | running | idle | failed | closed | unknown
    pub(crate) execution: &'static str,
    /// completed | errored | interrupted | rate_limited | None — from the
    /// STRICT terminal-evidence reader (turns.txt tail cross-checked against
    /// the highest result-<n>.md), never from lifetime.
    pub(crate) last_outcome: Option<String>,
    /// Current round: delivered+1 while queued/running; the just-terminated
    /// round for idle/failed; the delivered count when unknown.
    pub(crate) round: u32,
    /// Delivered rounds = count(result-<n>.md), floored at 1 when a bare
    /// result.md exists with no versioned files (#2024 floor semantics).
    pub(crate) rounds_delivered: u32,
    /// Trusted-lifetime identity fields (anti-cross-runtime / same-slug
    /// fencing). All `None` when the projection is not trusted.
    pub(crate) master_session_id: Option<String>,
    pub(crate) task_id: Option<String>,
    pub(crate) generation: Option<u64>,
    pub(crate) turn_id: Option<String>,
}

/// STRICT recent-terminal-evidence reader (task-evo-peer-turn-status).
///
/// Unlike the lenient legacy [`parse_peer_turns_index`] (which silently drops
/// malformed lines and feeds the display history), this validates that the
/// LAST turns.txt entry and the HIGHEST-numbered `result-<n>.md` frontmatter
/// agree on `(round, outcome)`. Any disagreement, corruption, or absence on
/// either side ⇒ `None` — no outcome is asserted from a single uncorroborated
/// source.
///
/// Deliberately does NOT touch `parse_peer_turns_index` itself: its lenient
/// semantics are load-bearing for existing consumers.
fn read_last_terminal_evidence(peer_dir: &Path, slug: &str) -> Option<(u32, String)> {
    // Side A: the last turns.txt line must parse STRICTLY (a corrupt tail is
    // not allowed to fall back to an older entry — that would resurrect a
    // stale outcome).
    let turns_text =
        peer_io::read_peer_file(peer_dir, "turns.txt", peer_io::PEER_FILE_READ_CAP_SMALL)?;
    let last_line = turns_text.lines().rev().find(|l| !l.trim().is_empty())?;
    let mut parts = last_line.split_whitespace();
    let round: u32 = parts.next()?.parse().ok()?;
    let outcome = parts.next()?;
    let updated_unix: u64 = parts.next()?.parse().ok()?;
    if parts.next().is_some()
        || !matches!(
            outcome,
            "completed" | "errored" | "interrupted" | "rate_limited"
        )
    {
        return None;
    }
    // Side B: the highest-numbered result-<n>.md must exist and its
    // frontmatter must state the SAME (turn, outcome). The versioned result
    // is the terminal payload; turns.txt is the index — both are written by
    // the same single terminal path, so a mismatch means corruption or a
    // torn write.
    let versions = enumerate_peer_result_versions(peer_dir)?;
    let (highest_round, highest_leaf) = versions.last()?;
    if highest_round != &round {
        return None;
    }
    let body = peer_io::read_peer_file(peer_dir, highest_leaf, peer_io::PEER_FILE_READ_CAP_LARGE)?;
    let (header, _) = body.split_once("\n---\n\n")?;
    let mut lines = header.lines();
    if lines.next()? != "---" {
        return None;
    }
    // The frontmatter must name THIS peer — a foreign slug's result file
    // (copied/moved in) must not become this peer's outcome evidence.
    let mut saw_slug = false;
    let mut saw_turn = None;
    let mut saw_outcome = None;
    for line in lines {
        if let Some(slug_line) = line.strip_prefix("slug: ") {
            saw_slug = slug_line == slug;
        } else if let Some(turn) = line.strip_prefix("turn: ") {
            saw_turn = turn.parse::<u32>().ok();
        } else if let Some(outcome_line) = line.strip_prefix("outcome: ") {
            saw_outcome = Some(outcome_line.to_owned());
        }
    }
    if !saw_slug || saw_turn != Some(round) || saw_outcome.as_deref() != Some(outcome) {
        return None;
    }
    let _ = updated_unix;
    Some((round, outcome.to_owned()))
}

/// Enumerate `result-<n>.md` leaves with their PARSED round numbers, sorted
/// ascending. Only well-formed regular files count (fd-anchored scan, #1824);
/// a malformed number skips that leaf (legacy scan semantics). `None` when
/// the scan hit its cap (a TRUNCATED list must never pose as complete —
/// its "highest" would be an arbitrary cutoff, not the newest round).
fn enumerate_peer_result_versions(peer_dir: &Path) -> Option<Vec<(u32, String)>> {
    // The scanner itself reports truncation (Err → None here): its budget
    // counts ALL scanned entries, not just result- hits, so a dir full of
    // unrelated files is also caught — no second-guessing by hit count.
    let names = peer_io::peer_dir_list_prefixed(peer_dir, "result-", peer_io::PEER_DIR_SCAN_CAP)?;
    let mut parsed: Vec<(u32, String)> = names
        .into_iter()
        .filter_map(|name| {
            let stem = name.strip_prefix("result-")?;
            let stem = stem.strip_suffix(".md")?;
            let round: u32 = stem.parse().ok()?;
            Some((round, name))
        })
        .collect();
    parsed.sort_by_key(|(round, _)| *round);
    Some(parsed)
}

/// Derive the execution facet for one staged peer dir. `closed` short-
/// circuits everything (lifecycle terminal state; not a success claim).
/// `profile_id` comes from the caller's explicit context (CLI `--profile`,
/// serve's known profile) — NEVER derived from the peers_root path.
pub(crate) fn derive_peer_execution_facet(
    peer_dir: &Path,
    profile_id: &str,
    slug: &str,
    closed: bool,
) -> PeerExecutionFacet {
    let versions = count_peer_result_versions(peer_dir);
    let has_bare_result = peer_io::peer_regular_file_exists(peer_dir, "result.md");
    // #2024 floor: pre-#435 peers wrote only the bare result.md; their
    // delivered count reads 1, never 0.
    let rounds_delivered = if versions > 0 {
        versions
    } else if has_bare_result {
        1
    } else {
        0
    };
    // last_outcome: STRICT terminal evidence (turns tail × highest result-N
    // cross-check). Independent of the execution authority so an errored
    // round stays visible even when the lifetime projection is untrusted.
    let terminal = read_last_terminal_evidence(peer_dir, slug);
    let last_outcome = terminal.as_ref().map(|(_, outcome)| outcome.clone());
    if closed {
        // The closed marker wins the EXECUTION label (lifecycle terminal),
        // but it does NOT bypass the trust check: identity is retained only
        // when the SAME full projection validation as the open branch passes
        // (registry_key, writer shape, non-empty identity, Idle digest
        // re-bind). Missing/forged/foreign/corrupt ⇒ all-null, exactly like
        // an untrusted open peer (merged-review audit 2026-09-10, PR #2272).
        // Destructure ONCE (ROOT review 2: successive `identity.map` calls
        // move the Option after the first consumption — compile error).
        let (master, task_id, generation, turn_id) =
            match trusted_lifetime_projection(peer_dir, profile_id, slug) {
                Some(p) => (
                    Some(p.master),
                    Some(p.task_id),
                    Some(p.generation),
                    p.turn_id,
                ),
                None => (None, None, None, None),
            };
        return PeerExecutionFacet {
            execution: "closed",
            last_outcome,
            round: rounds_delivered,
            rounds_delivered,
            master_session_id: master,
            task_id,
            generation,
            turn_id,
        };
    }
    match trusted_lifetime_projection(peer_dir, profile_id, slug) {
        Some(projection) => {
            let execution = match projection.phase {
                LifetimePhase::Pending => "queued",
                LifetimePhase::Running => "running",
                LifetimePhase::Idle => "idle",
                LifetimePhase::Failed => "failed",
            };
            let round = match execution {
                // A queued/running round is the one ABOUT to run / running.
                "queued" | "running" => rounds_delivered.saturating_add(1),
                // idle/failed report the round that just terminated; the
                // strict terminal evidence pins it, else the delivered count.
                _ => terminal.map(|(round, _)| round).unwrap_or(rounds_delivered),
            };
            PeerExecutionFacet {
                execution,
                last_outcome,
                round,
                rounds_delivered,
                master_session_id: Some(projection.master),
                task_id: Some(projection.task_id),
                generation: Some(projection.generation),
                turn_id: projection.turn_id,
            }
        }
        // No trusted current-state authority (missing, corrupt, mismatched,
        // or legacy): execution=unknown AND identity all-null. The strict
        // terminal evidence still carries last_outcome — the PAST round's
        // outcome — which is evidence, not a claim about the present.
        None => PeerExecutionFacet {
            execution: "unknown",
            last_outcome,
            round: rounds_delivered,
            rounds_delivered,
            master_session_id: None,
            task_id: None,
            generation: None,
            turn_id: None,
        },
    }
}

pub(crate) const PEER_GATHER_BRIEF_CAP: usize = 16 * 1024;

pub(crate) const PEER_GATHER_RESULT_CAP: usize = 48 * 1024;

/// One peer blackboard row as read off disk (`peers/<slug>/`), per-field
/// caps already applied. The shared currency of the `peer/gather` RPC and
/// the `peer_gather` tool callback — both are views over the SAME read.
pub(crate) struct PeerBlackboardRow {
    pub(crate) slug: String,
    /// Display NAME from `peers/<slug>/name` (the peer's primary address),
    /// falling back to the slug for legacy peers that have no `name` file.
    pub(crate) name: String,
    /// `brief.md`, capped at [`PEER_GATHER_BRIEF_CAP`]. Read only on the
    /// api `peer/gather` path — non-api consumers (peer_list renderer) use
    /// the slug/status columns only. (No entry-level allow needed: the
    /// module-level `allow(dead_code)` on `mod peers` covers no-api builds.)
    pub(crate) brief: String,
    pub(crate) brief_truncated: bool,
    /// `result.md` when any peer turn has terminated, capped at
    /// [`PEER_GATHER_RESULT_CAP`]; `None` = still running.
    pub(crate) result: Option<String>,
    pub(crate) result_truncated: bool,
    pub(crate) result_updated_unix: Option<u64>,
    pub(crate) has_worktree: bool,
    /// `true` when `peers/<slug>/closed` exists — the durable marker written
    /// by `peer_close` retiring the peer. A closed peer receives no further
    /// input; its result files stay readable.
    pub(crate) closed: bool,
    /// #435: parsed `turns.txt` entries: `[(turn_count, outcome, updated_unix)]`.
    /// `None` when the file doesn't exist (single-turn-or-less peer).
    pub(crate) turn_history: Option<Vec<(u32, String, u64)>>,
    /// #peer-model — the model LANE key from `peers/<slug>/model` (a configured
    /// `sub_provider` this peer runs its turns on), trimmed; `None` for a peer
    /// on the profile's primary model.
    pub(crate) model_lane: Option<String>,
    /// task-evo-peer-turn-status — the execution facet (execution /
    /// last_outcome / round / rounds_delivered + identity), derived per the
    /// v3.1 contract. `profile_id` for the lifetime projection is supplied by
    /// the CALLER (serve knows its own; the CLI takes `--profile`) — never
    /// derived from the peers_root path.
    pub(crate) execution_facet: PeerExecutionFacet,
    /// T-0454: the staging session that originated this peer, read from
    /// `peers/<slug>/originator` with the SAME no-follow/capped reader the
    /// adopt cross-check uses. `Some` only when the record is Valid
    /// (trimmed, non-empty); `None` for a missing or invalid record. This is
    /// a best-effort attribution for UI regrouping — it does NOT feed the
    /// trusted `master_session_id` lifetime projection.
    pub(crate) originator: Option<String>,
}

/// #1801: row-reading core of the peer blackboard — every staged peer dir
/// under `peers_root` (a `brief.md` is the staging contract; stray dirs are
/// skipped), optionally narrowed to `slugs`, sorted by slug, with the
/// per-field caps applied. Extracted from `raw_peer_gather` verbatim so the
/// RPC's behavior is unchanged and the `peer_gather` tool reads the exact
/// same rows.
pub(crate) fn read_peer_blackboard(
    peers_root: &Path,
    slugs: Option<&[String]>,
) -> Vec<PeerBlackboardRow> {
    read_peer_blackboard_with_profile(peers_root, slugs, DEFAULT_PEER_LIST_PROFILE)
}

/// The profile id the blackboard reader uses for lifetime projection checks
/// when the caller supplies none — the CLI default profile (`octos`). A
/// caller that knows better (serve) always passes its real profile_id.
pub(crate) const DEFAULT_PEER_LIST_PROFILE: &str = "octos";

/// Profile-aware blackboard read (task-evo-peer-turn-status): identical to
/// [`read_peer_blackboard`] but threads the caller's EXPLICIT profile id into
/// the lifetime projection's registry_key check (spec: profile is never
/// derived from the peers_root path).
pub(crate) fn read_peer_blackboard_with_profile(
    peers_root: &Path,
    slugs: Option<&[String]>,
    profile_id: &str,
) -> Vec<PeerBlackboardRow> {
    let mut rows: Vec<PeerBlackboardRow> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(peers_root) {
        // No `is_dir()` pre-filter — it follows symlinks. `staged_peer_dir`
        // below is the sole gate (safe slug + real non-symlink dir + brief.md).
        let mut dirs: Vec<_> = read_dir.flatten().collect();
        dirs.sort_by_key(|entry| entry.file_name());
        for entry in dirs {
            let slug = entry.file_name().to_string_lossy().into_owned();
            if let Some(filter) = slugs {
                if !filter.iter().any(|wanted| wanted == &slug) {
                    continue;
                }
            }
            // Only REAL, staged (non-symlink) peer dirs — a symlinked entry
            // could redirect reads outside `peers/`; `staged_peer_dir` also
            // enforces the `brief.md` staging contract.
            let Some(dir) = staged_peer_dir(peers_root, &slug) else {
                continue;
            };
            let Some(brief) =
                peer_io::read_peer_file(&dir, "brief.md", peer_io::PEER_FILE_READ_CAP_LARGE)
            else {
                continue;
            };
            let result =
                peer_io::read_peer_file(&dir, "result.md", peer_io::PEER_FILE_READ_CAP_LARGE);
            let result_updated_unix = peer_io::peer_file_mtime(&dir, "result.md")
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_secs());
            let (brief, brief_truncated) = capped_utf8(brief, PEER_GATHER_BRIEF_CAP);
            let (result, result_truncated) = match result {
                Some(result) => {
                    let (capped, truncated) = capped_utf8(result, PEER_GATHER_RESULT_CAP);
                    (Some(capped), truncated)
                }
                None => (None, false),
            };
            // Display name: `peers/<slug>/name`, trimmed; legacy peers with no
            // `name` file fall back to the slug so the row always has an address.
            let name = peer_io::read_peer_file(&dir, "name", peer_io::PEER_FILE_READ_CAP_SMALL)
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| slug.clone());
            // task-evo-peer-turn-status — derive the execution facet BEFORE
            // the struct literal borrows `slug` (the literal moves it).
            let closed = peer_io::peer_regular_file_exists(&dir, "closed");
            let execution_facet =
                derive_peer_execution_facet(&dir, profile_id, slug.as_str(), closed);
            // T-0454: surface the recorded originator for UI regrouping,
            // using the same fail-safe reader as the adopt cross-check.
            let originator = match peer_io::read_peer_identity_file(&dir, "originator") {
                peer_io::PeerFileRead::Valid(value) => {
                    let trimmed = value.trim().to_owned();
                    (!trimmed.is_empty()).then_some(trimmed)
                }
                peer_io::PeerFileRead::Missing | peer_io::PeerFileRead::Invalid(_) => None,
            };
            rows.push(PeerBlackboardRow {
                slug,
                name,
                brief,
                brief_truncated,
                result,
                result_truncated,
                result_updated_unix,
                has_worktree: dir.join("wt").is_dir(),
                closed,
                turn_history: parse_peer_turns_index(&dir),
                execution_facet,
                originator,
                // #peer-model — the recorded model lane, if any (fd-anchored
                // no-follow read so a symlinked/FIFO `model` leaf is refused;
                // trimmed, empty treated as absent).
                model_lane: peer_io::read_peer_file(
                    &dir,
                    "model",
                    peer_io::PEER_FILE_READ_CAP_SMALL,
                )
                .map(|lane| lane.trim().to_owned())
                .filter(|lane| !lane.is_empty()),
            });
        }
    }
    rows
}

/// Compose the compact one-line-per-peer INDEX the `peer_list` tool returns:
/// slug, status (`closed` if retired via `peer_close`, else `done` when a
/// result file exists, else `running`), last-updated unix (or "—"), turn
/// count, and `worktree` when the peer has its own fence. Deliberate contrast
/// with [`compose_peer_gather_text`], which reads each peer's full brief +
/// result — this is the index, that is the payload.
/// Cap on peer rows the `peer_list` index emits inline — a runaway fleet
/// cannot flood the model's context; the overflow folds into a trailing
/// "… and N more" line (read specific peers with peer_gather slugs).
pub(crate) const PEER_LIST_MAX_ROWS: usize = 200;

/// `awaiting_by_slug`: the AUTHORITATIVE parked-prompt set per peer slug,
/// projected from the process-global store by the caller (`build_peer_list_callback`).
/// A slug absent from the map (or closed) is not awaiting input.
pub(crate) fn compose_peer_list_text(
    rows: &[PeerBlackboardRow],
    available_lanes: &[String],
    awaiting_by_slug: &std::collections::HashMap<String, Vec<PeerPendingSummary>>,
) -> String {
    if rows.is_empty() {
        return "(no peers staged)".to_owned();
    }
    let mut lines: Vec<String> = Vec::with_capacity(rows.len().min(PEER_LIST_MAX_ROWS) + 2);
    lines.push(format!("peers ({}):", rows.len()));
    for row in rows.iter().take(PEER_LIST_MAX_ROWS) {
        // Precedence: a retired peer is `closed`; else a peer PARKED on one or
        // more interactive prompts is `awaiting_input` (the master can answer
        // each via peer_respond) — this beats `done` so a persistent peer that
        // finished an earlier turn and is now blocked mid-turn still surfaces as
        // blocked; else `done` when a result exists; else `running`.
        let awaiting: &[PeerPendingSummary] = if row.closed {
            &[]
        } else {
            awaiting_by_slug
                .get(&row.slug)
                .map_or(&[][..], Vec::as_slice)
        };
        let status = if row.closed {
            "closed"
        } else if !awaiting.is_empty() {
            "awaiting_input"
        } else if row.result.is_some() {
            "done"
        } else {
            "running"
        };
        // List each parked prompt (id + kind + short prompt + any offered
        // options) so the master sees WHAT the peer waits on and which `id` to
        // pass to peer_respond — without a peer_gather. Capped so a peer with a
        // flood of prompts can't dominate the index.
        const PEER_LIST_MAX_PENDING: usize = 8;
        let awaiting_note = if awaiting.is_empty() {
            String::new()
        } else {
            let mut items = awaiting
                .iter()
                .take(PEER_LIST_MAX_PENDING)
                .map(|pending| {
                    let (prompt, truncated) = capped_utf8(pending.prompt.trim().to_owned(), 80);
                    let ellipsis = if truncated { "…" } else { "" };
                    let opts = if pending.options.is_empty() {
                        String::new()
                    } else {
                        format!(" options=[{}]", pending.options.join(", "))
                    };
                    format!(
                        "[id={id} {kind}: {prompt}{ellipsis}{opts}]",
                        id = pending.id,
                        kind = pending.kind.as_str()
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            if awaiting.len() > PEER_LIST_MAX_PENDING {
                items.push_str(&format!(
                    " (+{} more)",
                    awaiting.len() - PEER_LIST_MAX_PENDING
                ));
            }
            format!("  · awaiting: {items}")
        };
        let updated = row
            .result_updated_unix
            .map_or_else(|| "—".to_owned(), |ts| ts.to_string());
        let turns = row.turn_history.as_ref().map_or(0, Vec::len);
        let worktree = if row.has_worktree { "  worktree" } else { "" };
        // #peer-model — annotate the peer's model lane, resolved against the
        // CURRENT `sub_providers` so the index matches what the turn actually
        // does: a lane whose key no longer exists is flagged as falling back to
        // the primary model, not printed as if it were live.
        let model = match row.model_lane.as_deref() {
            Some(lane) if available_lanes.iter().any(|key| key == lane) => {
                format!("  · model={lane}")
            }
            Some(lane) => format!("  · model={lane} (unavailable→primary)"),
            None => String::new(),
        };
        // Address by NAME; show the slug in parens when it differs.
        let addr = if row.name == row.slug {
            row.slug.clone()
        } else {
            format!("{} ({})", row.name, row.slug)
        };
        lines.push(format!(
            "- {addr}  {status}  updated {updated}  turns {turns}{worktree}{model}{awaiting_note}"
        ));
        // task-evo-peer-turn-status — surface the CURRENT execution state
        // and the most recent terminal outcome on the index line so a peer
        // mid-round2 (queued/running) with a round1 result on disk no longer
        // reads as settled. Rendered as a suffix (outcome=…/exec=…) to keep
        // the line shape stable for existing readers.
        if status != "closed" || row.execution_facet.last_outcome.is_some() {
            let exec = row.execution_facet.execution;
            if exec != "unknown" {
                lines.push(format!("  · exec={exec}"));
            } else {
                lines.push("  · exec=?".to_owned());
            }
            if let Some(outcome) = row.execution_facet.last_outcome.as_deref() {
                lines.push(format!("  · outcome={outcome}"));
            }
        }
    }
    if rows.len() > PEER_LIST_MAX_ROWS {
        lines.push(format!("… and {} more", rows.len() - PEER_LIST_MAX_ROWS));
    }
    lines.join("\n")
}

/// Build the `peer_list` read callback for ONE turn of the serve/WS path.
/// Mirrors [`build_peer_gather_callback`] but composes the compact status
/// index ([`compose_peer_list_text`]) over the SAME row reader
/// ([`read_peer_blackboard`]); it takes no slugs — it always lists every peer.
/// `available_lanes` (the profile's CURRENT `sub_provider` keys) lets the index
/// flag a peer whose recorded model lane no longer resolves (#peer-model).
pub(crate) fn build_peer_list_callback(
    peers_root: PathBuf,
    available_lanes: Vec<String>,
    contracts: Arc<UiProtocolContractStores>,
    profile_id: String,
) -> octos_agent::PeerListCallback {
    Arc::new(move || {
        // task-evo-peer-turn-status — serve KNOWS its profile; pass it so the
        // lifetime projection validates registry_key against the real one.
        let rows = read_peer_blackboard_with_profile(&peers_root, None, &profile_id);
        // #peer-respond — the AUTHORITATIVE awaiting-input set comes from the
        // process-global store, joined to each open peer by its TRUSTED wire
        // session (never a filesystem marker). A peer with no wire (not open) or
        // no store entries simply isn't awaiting.
        let awaiting_by_slug: std::collections::HashMap<String, Vec<PeerPendingSummary>> = rows
            .iter()
            .filter(|row| !row.closed)
            .filter_map(|row| {
                let session = peer_trusted_session(&profile_id, &row.slug)?;
                // ADR 0007 — a host-owned app peer's approvals are the
                // person's; they are not shown to the originator as input to give.
                let pending = peer_pending_answerable_by_originator(
                    &peers_root,
                    &row.slug,
                    peer_pending_summaries(&contracts, &session),
                );
                (!pending.is_empty()).then(|| (row.slug.clone(), pending))
            })
            .collect();
        Ok(compose_peer_list_text(
            &rows,
            &available_lanes,
            &awaiting_by_slug,
        ))
    })
}
