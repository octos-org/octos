//! UPCR-2026-034 `peer/purge` (#2604): erase a host-owned app peer and free
//! its (app, account) binding.
//!
//! The host (OctoSense) calls it when a person removes an account or
//! uninstalls an app. It closes the peer if it is open, stops everything
//! still running for it, then erases what the kernel keeps for it: its
//! transcripts (the peer's own session and every request context), its memory
//! namespace (the child context namespaces included), its blackboard and
//! control files (`peers/<slug>/`: brief, results, bindings, host tool set,
//! tool audit), and its workspace when the kernel provisioned it. A
//! host-supplied workspace belongs to the host: only the kernel-made
//! `contexts/<id>/` folders inside it are removed. Tombstones and the audit
//! row live outside `peers/` ([`crate::peers::purge`]).

use super::*;

use crate::peers::purge::{
    PurgeTombstone, append_audit, is_real_path, remove_tree_within, tombstone_for_token,
    write_tombstone,
};

/// `approval/cancelled` reason of a prompt cancelled by `peer/purge`.
const APPROVAL_CANCELLED_REASON_PEER_PURGED: &str = "peer_purged";

/// How long `peer/purge` waits for the peer's running turns to stop before it
/// gives up (`peer_purge_busy`; the peer stays closed and a retry finishes).
const PURGE_TURN_STOP_WAIT: Duration = Duration::from_secs(10);

/// Settle time after a stopped turn's terminal, so its last writes land
/// before the transcripts are erased.
const PURGE_TURN_SETTLE: Duration = Duration::from_millis(200);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPeerPurgeParams {
    /// The owning peer's originator session (the host's system agent).
    session_id: SessionKey,
    /// Peer name or slug.
    peer: String,
    /// The host token the peer was created with.
    #[serde(default)]
    host_token: Option<String>,
    #[serde(default)]
    profile_id: Option<String>,
}

/// Slugs (per peers root) being purged right now.
fn purges_in_flight() -> &'static StdMutex<HashSet<String>> {
    static IN_FLIGHT: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

struct InFlight(String);

impl Drop for InFlight {
    fn drop(&mut self) {
        purges_in_flight()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

fn purge_error(kind: &str, message: String) -> RpcError {
    host_peer_error(kind, message)
}

/// Stop `session`'s running turn, if any, and wait until it reached its
/// terminal. Returns whether a turn was running; `Err` when it did not stop
/// within [`PURGE_TURN_STOP_WAIT`].
async fn stop_session_turn(
    active_turns: &SharedActiveTurns,
    session: &SessionKey,
) -> Result<bool, ()> {
    let outcome =
        interrupt_active_turn_for_session(active_turns, session, InterruptOrigin::PeerPurge).await;
    let running = matches!(
        outcome,
        InterruptOutcome::Captured { .. } | InterruptOutcome::AlreadyInterrupting
    );
    if !running {
        return Ok(false);
    }
    let deadline = tokio::time::Instant::now() + PURGE_TURN_STOP_WAIT;
    loop {
        let terminal = {
            let registry = active_turns.lock().await;
            match registry.get(session) {
                None => true,
                Some(turn) => matches!(&*turn.state.lock().await, TurnState::Terminal(_)),
            }
        };
        if terminal {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Cancel every approval and question pending on `session`.
fn cancel_session_prompts(
    contracts: &UiProtocolContractStores,
    session: &SessionKey,
    emit_cancelled: &dyn Fn(ApprovalCancelledEvent),
) -> usize {
    let mut cancelled = 0;
    for event in contracts.approvals.pending_for_session(session) {
        if let Some(done) = contracts.approvals.cancel_pending_approval(
            session,
            &event.approval_id,
            &event.turn_id,
            APPROVAL_CANCELLED_REASON_PEER_PURGED,
        ) {
            cancelled += 1;
            emit_cancelled(ApprovalCancelledEvent {
                session_id: session.clone(),
                topic: session.topic().map(ToOwned::to_owned),
                approval_id: done.approval_id,
                turn_id: done.turn_id,
                reason: APPROVAL_CANCELLED_REASON_PEER_PURGED.to_owned(),
            });
        }
    }
    for event in contracts.user_questions.pending_for_session(session) {
        if contracts
            .user_questions
            .cancel_pending_question(
                session,
                &event.question_id,
                APPROVAL_CANCELLED_REASON_PEER_PURGED,
            )
            .is_some()
        {
            cancelled += 1;
        }
    }
    cancelled
}

/// Remove every file of `session`'s transcript under the session store
/// `root`: the JSONL (flat and per-user layouts), its sealed segments, its
/// sidecars (`<topic>.*`), the migration marker and the reasoning-effort
/// sidecar. Returns how many entries were removed.
async fn erase_transcript_in(root: &Path, session: &SessionKey, errors: &mut Vec<String>) -> usize {
    if !root.is_dir() {
        return 0;
    }
    let mut removed = 0;
    match octos_bus::SessionManager::open(root) {
        Ok(mut manager) => {
            let flat = manager.session_path(session);
            if let Err(error) = manager.clear(session).await {
                errors.push(format!("transcript {session}: {error}"));
            }
            if let (Some(dir), Some(stem)) = (
                flat.parent(),
                flat.file_stem().and_then(|stem| stem.to_str()),
            ) {
                removed += remove_entries_with_stem(root, dir, stem, None, errors);
            }
        }
        Err(error) => errors.push(format!("session store {}: {error}", root.display())),
    }
    let topic = session.topic().unwrap_or("default");
    let encoded_topic = octos_bus::session::encode_path_component(topic);
    let user_sessions = root
        .join("users")
        .join(octos_bus::session::encode_path_component(
            session.base_key(),
        ))
        .join("sessions");
    removed += remove_entries_with_stem(
        root,
        &user_sessions,
        &encoded_topic,
        Some(&format!(".migrated.{encoded_topic}")),
        errors,
    );
    // The per-turn context-manager snapshot (verbatim conversation content).
    match remove_tree_within(
        root,
        &crate::context_manager::context_ledger_path(root, &session.0),
    ) {
        Ok(true) => removed += 1,
        Ok(false) => {}
        Err(error) => errors.push(format!("context ledger {session}: {error}")),
    }
    if let Err(error) =
        crate::api::ui_protocol_reasoning_effort::clear_reasoning_effort(root, session)
    {
        errors.push(format!("reasoning effort {session}: {error}"));
    }
    removed
}

/// Remove the entries of `dir` named `<stem>.<anything>` (and `extra`),
/// never outside the store `root`.
fn remove_entries_with_stem(
    root: &Path,
    dir: &Path,
    stem: &str,
    extra: Option<&str>,
    errors: &mut Vec<String>,
) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let prefix = format!("{stem}.");
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) || Some(name.as_str()) == extra {
            match remove_tree_within(root, &entry.path()) {
                Ok(true) => removed += 1,
                Ok(false) => {}
                Err(error) => errors.push(format!("{}: {error}", entry.path().display())),
            }
        }
    }
    removed
}

/// `peer/purge` (UPCR-2026-034, #2604) — see the module docs.
pub(super) async fn raw_peer_purge(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    contracts: &Arc<UiProtocolContractStores>,
    active_turns: &SharedActiveTurns,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    if ws.is_external() {
        return Err(external_host_tools_denied(&request.method));
    }
    let params: RawPeerPurgeParams = parse_raw_params(request)?;
    let profile_id = raw_scoped_llm_profile_id(
        params.profile_id.clone(),
        Some(&params.session_id),
        connection_profile_id,
    )?;
    let (_, data_dir) = resolve_profile_data_dir(state, Some(&profile_id))?;
    let peers_root = data_dir.join("peers");
    let token = params.host_token.as_deref();

    // Authorize like every host control call (originator + host token). A
    // retry after a completed purge finds no peer (or a new one under the same
    // name): its tombstone answers it instead.
    let slug = match authorize_host_peer_call(&peers_root, &params.peer, &params.session_id, token)
    {
        Ok(slug) => slug,
        Err(error) => {
            if let Some(tombstone) = token.and_then(|token| tombstone_for_token(&peers_root, token))
            {
                if tombstone.names(&params.peer) && tombstone.originator == params.session_id.0 {
                    return Ok(json!({
                        "session_id": params.session_id,
                        "profile_id": profile_id,
                        "slug": tombstone.slug,
                        "purged": false,
                        "already_purged": true,
                        "purged_at": tombstone.purged_at,
                        // Surface any residue the original purge recorded
                        // (#2659): the retry is the caller's chance to
                        // finish or escalate the partial erase.
                        "partial": !tombstone.errors.is_empty(),
                        "residual_errors": tombstone.errors,
                    }));
                }
            }
            return Err(error);
        }
    };
    let Some(binding) = crate::peers::app_binding::read_peer_host_binding(&peers_root, &slug)
    else {
        return Err(purge_error(
            "peer_not_host_bound",
            format!("peer '{slug}' is not a host-owned app peer; only those can be purged"),
        ));
    };
    // The peer's tool host, when it has one, is the owning connection: only
    // it may erase the peer. With no live route (the host reconnected and has
    // not registered again) the host token is the credential.
    if let Some(owner) = crate::peers::host_tools::host_route_connection(&peers_root, &slug) {
        if owner != ws.connection_id.0 {
            return Err(RpcError::permission_denied(format!(
                "peer '{slug}' is driven by another connection of its host; purge it there"
            ))
            .with_data(json!({ "kind": "peer_purge_not_owner" })));
        }
    }
    let in_flight_key = format!("{}\u{0}{slug}", peers_root.display());
    if !purges_in_flight()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(in_flight_key.clone())
    {
        return Err(purge_error(
            "peer_purge_in_progress",
            format!("peer '{slug}' is being purged already"),
        ));
    }
    let _in_flight = InFlight(in_flight_key);
    let Some(peer_dir) = staged_peer_dir(&peers_root, &slug) else {
        return Err(purge_error(
            "peer_not_found",
            format!("peer '{slug}' is not staged"),
        ));
    };
    let name = peer_io::read_peer_file(&peer_dir, "name", peer_io::PEER_FILE_READ_CAP_SMALL)
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty());
    let originator = params.session_id.clone();
    let peer_session = SessionKey(format!("{}#peer-{slug}", originator.base_key()));
    let contexts = crate::peers::app_binding::context_bindings(&peers_root, &slug);
    let context_sessions: Vec<(String, SessionKey)> = contexts
        .iter()
        .map(|(id, _)| {
            (
                id.clone(),
                crate::peers::app_binding::context_session_key(&originator, &slug, id),
            )
        })
        .collect();

    let emit_ws = ws.clone();
    let emit_ledger = ledger.clone();
    let emit_cancelled = move |event: ApprovalCancelledEvent| {
        let _ = send_notification_durable(
            &emit_ws,
            &emit_ledger,
            UiNotification::ApprovalCancelled(event),
        );
    };

    // 1. Close: the durable marker refuses every new turn, input and context
    //    from here on (the peer's session and its contexts), and the close
    //    path clears the input queue, the wire and the build-cache slot.
    let was_open = !peer_is_closed(&peers_root, &slug);
    if was_open {
        let closed_ws = ws.clone();
        let closed_ledger = ledger.clone();
        close_authorized_peer(
            &peers_root,
            &peer_dir,
            &slug,
            &originator.0,
            &profile_id,
            contracts,
            &move |event| {
                let _ = send_notification_durable(
                    &closed_ws,
                    &closed_ledger,
                    UiNotification::PeerClosed(event),
                );
            },
            &emit_cancelled,
        )
        .map_err(RpcError::internal_error)?;
    }
    let mut contexts_closed = 0;
    for (id, context) in &contexts {
        if !context.closed {
            let mut closed = context.clone();
            closed.closed = true;
            crate::peers::app_binding::write_context_binding(&peers_root, &slug, id, &closed)
                .map_err(RpcError::internal_error)?;
            contexts_closed += 1;
        }
    }
    // 2. Fail the host tool calls in flight (`peer_purged`) and drop the
    //    route, so nothing reaches the host for this peer any more.
    let host_calls_failed = crate::peers::host_tools::purge_peer_host_state(&peers_root, &slug);
    // 3. Stop every running turn of the peer and its contexts, and cancel
    //    their pending approvals and questions.
    let mut interrupted = Vec::new();
    let mut prompts_cancelled = 0;
    let all_sessions: Vec<SessionKey> = std::iter::once(peer_session.clone())
        .chain(context_sessions.iter().map(|(_, key)| key.clone()))
        .collect();
    for session in &all_sessions {
        prompts_cancelled += cancel_session_prompts(contracts, session, &emit_cancelled);
        match stop_session_turn(active_turns, session).await {
            Ok(true) => interrupted.push(session.clone()),
            Ok(false) => {}
            Err(()) => {
                return Err(purge_error(
                    "peer_purge_busy",
                    format!(
                        "a turn of '{session}' did not stop within {} s; the peer is closed, \
                         retry the purge",
                        PURGE_TURN_STOP_WAIT.as_secs()
                    ),
                ));
            }
        }
        // A prompt raised while the turn was stopping.
        prompts_cancelled += cancel_session_prompts(contracts, session, &emit_cancelled);
    }
    if !interrupted.is_empty() {
        tokio::time::sleep(PURGE_TURN_SETTLE).await;
    }

    // 4. Erase.
    let mut errors: Vec<String> = Vec::new();
    // Transcripts, in every store a session of this peer can have used: the
    // profile's (or the ephemeral override), and the per-project stores of
    // the peer's and the contexts' folders.
    let profile_runtime = resolve_session_profile_runtime(state, Some(&profile_id));
    let mut roots: Vec<PathBuf> = vec![
        profile_runtime
            .as_ref()
            .and_then(|runtime| runtime.session_store_root.clone())
            .unwrap_or_else(|| data_dir.clone()),
    ];
    let project_root =
        |cwd: &Path| crate::runtime::session::project_sessions_root(cwd, &profile_id);
    // A per-project store lives in the host's folder: only through real
    // (symlink-free) folders.
    for cwd in std::iter::once(&binding.cwd).chain(contexts.iter().map(|(_, c)| &c.cwd)) {
        let root = project_root(cwd);
        if is_real_path(cwd) && (!root.exists() || is_real_path(&root)) {
            roots.push(root);
        }
    }
    roots.sort();
    roots.dedup();
    let mut transcript_entries = 0;
    for session in &all_sessions {
        state.session_cache.invalidate_session(session).await;
        for root in &roots {
            transcript_entries += erase_transcript_in(root, session, &mut errors).await;
        }
    }
    // Memory: the peer's namespace, its contexts' nested under it.
    let memory_erased = match crate::runtime::memory_namespace::erase_memory_namespace(
        &data_dir,
        &binding.memory_namespace,
    )
    .await
    {
        Ok(erased) => erased,
        Err(error) => {
            errors.push(format!("memory namespace: {error}"));
            false
        }
    };
    // Workspace: all of it when the kernel provisioned it; otherwise only the
    // kernel-made context folders inside the host's folder.
    // Nothing is erased through a symlink: the bound workspace must still be
    // the real (canonical) folder it was bound as, and every removal stays
    // inside its expected root.
    // A workspace that is gone has nothing left to protect: an earlier
    // attempt of this very purge may have erased it before stopping, and a
    // retry must finish rather than fail forever on our own leftover. Only
    // a path still on disk that is no longer its canonical form (a swapped
    // symlink) is refused.
    let workspace_missing = std::fs::symlink_metadata(&binding.cwd)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    let workspace_is_real = !workspace_missing && is_real_path(&binding.cwd);
    let provisioned_dir = data_dir.join(crate::runtime::memory_namespace::APP_WORKSPACES_DIR);
    let provisioned_root = dunce::canonicalize(&data_dir)
        .unwrap_or_else(|_| data_dir.clone())
        .join(crate::runtime::memory_namespace::APP_WORKSPACES_DIR);
    let workspace = if binding.cwd.starts_with(&provisioned_root) {
        if workspace_is_real {
            if let Err(error) = remove_tree_within(&provisioned_dir, &binding.cwd) {
                errors.push(format!("workspace {}: {error}", binding.cwd.display()));
            }
        } else if !workspace_missing {
            errors.push(format!(
                "workspace {}: no longer its real path; not erased",
                binding.cwd.display()
            ));
        }
        "erased"
    } else {
        let contexts_root = binding.cwd.join("contexts");
        if workspace_is_real {
            for (_, context) in &contexts {
                if context.cwd.parent() == Some(contexts_root.as_path()) {
                    if let Err(error) = remove_tree_within(&contexts_root, &context.cwd) {
                        errors.push(format!("context folder {}: {error}", context.cwd.display()));
                    }
                }
            }
            // Only when nothing else is left in it.
            let _ = std::fs::remove_dir(&contexts_root);
        } else if !workspace_missing && !contexts.is_empty() {
            errors.push(format!(
                "workspace {}: no longer its real path; context folders not erased",
                binding.cwd.display()
            ));
        }
        "kept"
    };
    // A partially-failed erase must not finalize: the tombstone would answer
    // every retry `already_purged`, and the leftovers — files the account
    // still owns — would never be erased. The peer stays closed and staged,
    // so a retry runs the whole (idempotent) erase again.
    if !errors.is_empty() {
        append_audit(
            &peers_root,
            &json!({
                "ts": Utc::now().to_rfc3339(),
                "event": "peer_purge_incomplete",
                "profile_id": profile_id,
                "session_id": originator,
                "slug": &slug,
                "name": &name,
                "memory_namespace": &binding.memory_namespace,
                "cwd": binding.cwd.to_string_lossy(),
                "connection": ws.connection_id.0,
                "was_open": was_open,
                "contexts": context_sessions.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
                "interrupted": interrupted,
                "host_calls_failed": host_calls_failed,
                "erased": {
                    "transcript_entries": transcript_entries,
                    "memory_namespace": &binding.memory_namespace,
                    "memory": memory_erased,
                    "workspace": workspace,
                    "peer_dir": !peer_dir.exists(),
                },
                "errors": &errors,
            }),
        );
        return Err(purge_error(
            "peer_purge_incomplete",
            format!(
                "peer '{slug}': {} of its entries could not be erased; the peer stays \
                 closed, retry the purge",
                errors.len()
            ),
        )
        .with_data(json!({
            "kind": "peer_purge_incomplete",
            "errors": errors,
        })));
    }
    // The tombstones go down BEFORE the peer dir: from here on a retry with
    // the same token is answered `already_purged`, and a stale client's
    // `#peer-<slug>` session is refused.
    let purged_at = Utc::now().to_rfc3339();
    write_tombstone(
        &peers_root,
        &binding.token_sha256,
        &PurgeTombstone {
            slug: slug.clone(),
            name: name.clone(),
            originator: originator.0.clone(),
            memory_namespace: binding.memory_namespace.clone(),
            purged_at: purged_at.clone(),
            // Record partial failures on the tombstone itself: a retry
            // answers `already_purged` with this list, so residue is never
            // silent (#2659).
            errors: errors.clone(),
        },
    )
    .map_err(|error| RpcError::internal_error(format!("failed to record the purge: {error}")))?;
    // The blackboard and every control file; frees the name, the slug, the
    // namespace and the workspace reservation.
    if let Err(error) = remove_tree_within(&peers_root, &peer_dir) {
        errors.push(format!("peer directory: {error}"));
    }
    for session in &all_sessions {
        state.session_cache.invalidate_session(session).await;
    }

    let result = json!({
        "session_id": originator,
        "profile_id": profile_id,
        "slug": slug,
        "name": name,
        "purged": true,
        "partial": !errors.is_empty(),
        "already_purged": false,
        "purged_at": purged_at,
        "was_open": was_open,
        "contexts": context_sessions.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        "contexts_closed": contexts_closed,
        "interrupted": interrupted,
        "host_calls_failed": host_calls_failed,
        "prompts_cancelled": prompts_cancelled,
        "erased": {
            "transcript_entries": transcript_entries,
            "memory_namespace": binding.memory_namespace,
            "memory": memory_erased,
            "workspace": workspace,
            "peer_dir": !peer_dir.exists(),
        },
        "errors": errors,
    });
    append_audit(
        &peers_root,
        &json!({
            "ts": purged_at,
            "event": "peer_purged",
            "profile_id": result["profile_id"],
            "session_id": result["session_id"],
            "slug": result["slug"],
            "name": result["name"],
            "memory_namespace": binding.memory_namespace,
            "cwd": binding.cwd.to_string_lossy(),
            "connection": ws.connection_id.0,
            "was_open": was_open,
            "contexts": result["contexts"],
            "interrupted": result["interrupted"],
            "host_calls_failed": host_calls_failed,
            "erased": result["erased"],
            "errors": result["errors"],
        }),
    );
    Ok(result)
}
