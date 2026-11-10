//! Synchronous long-term-memory entry RPCs (T-0548, UPCR-2026-RD1002E7E):
//! `memory/entries`, `memory/edit`, `memory/delete`.
//!
//! Unlike `memory/request` (durable admission for the background
//! consolidator), these methods read, mutate and republish MEMORY.md in the
//! caller's request: no model call, and no dependency on the
//! memory-refresh toggle. Every mutation holds the profile's
//! `memory/.write.lock` flock — the same lock `run_consolidation` holds for
//! its whole run — so a direct edit/delete can never be clobbered by an
//! in-progress consolidation (see `memory_consolidate/direct.rs`).

use super::*;
use crate::memory_consolidate::direct::{self, DirectError};
use octos_core::ui_protocol::{
    MEMORY_REQUEST_MAX_CONTENT_BYTES, MemoryDeleteParams, MemoryDeleteResult, MemoryEditParams,
    MemoryEditResult, MemoryEntriesParams, MemoryEntriesResult, MemoryEntryItem,
};

/// Frame budget for the entry list: the same budget `memory/overview`
/// applies to the whole `long_term` markdown — the entries carry the same
/// bytes plus small per-entry overhead.
const MEMORY_ENTRIES_BUDGET: usize = MEMORY_RPC_LONG_TERM_BUDGET;

/// Resolve the caller's authorized profile to its on-disk memory dir
/// WITHOUT bootstrapping a profile runtime: these methods never call a
/// model, so a profile without an LLM selection (or with memory refresh
/// disabled) must still work.
fn resolve_memory_dir(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: &AuthIdentity,
    session_id: Option<&str>,
    method: &str,
) -> Result<PathBuf, RpcError> {
    let profile_id = resolve_memory_profile_id(state, headers, identity, session_id, method)?;
    let profile_store = state.profile_store.as_ref().ok_or_else(|| {
        RpcError::runtime_not_ready(format!(
            "{method}: profile store not configured on this server"
        ))
    })?;
    let profile = profile_store
        .get(&profile_id)
        .map_err(|_| RpcError::internal_error("could not read profile"))?
        .ok_or_else(|| RpcError::not_found("profile", &profile_id))?;
    Ok(profile_store.resolve_data_dir(&profile).join("memory"))
}

/// Validate the shared `entry_id` + `version` precondition pair.
fn validate_entry_ref(entry_id: &str, version: &str) -> Result<(), RpcError> {
    if !octos_memory::is_valid_entry_id(entry_id) {
        return Err(RpcError::invalid_params(
            "entry_id must be ^m followed by 6 of [a-z2-7]",
        ));
    }
    // Versions are minted by the kernel as lowercase SHA-256 hex; anything
    // else is malformed (not a conflict — the caller never saw a real one).
    if version.len() != 64
        || !version
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(RpcError::invalid_params(
            "version must be the 64-char lowercase sha256 hex reported by memory/entries",
        ));
    }
    Ok(())
}

/// Map an engine outcome onto the honest wire error. Engine `Err` (IO)
/// means the mutation may not have happened; the message says what is
/// known, without echoing local paths or memory contents.
fn direct_outcome_to_rpc<T>(
    method: &str,
    verb: &str,
    entry_id: &str,
    outcome: Result<Result<T, DirectError>, eyre::Report>,
) -> Result<T, RpcError> {
    match outcome {
        Ok(Ok(receipt)) => Ok(receipt),
        Ok(Err(DirectError::NotFound)) => Err(RpcError::not_found("memory_entry", entry_id)),
        Ok(Err(DirectError::VersionConflict { current_version })) => Err(
            RpcError::version_conflict("memory_entry", entry_id, current_version),
        ),
        Ok(Err(DirectError::InvalidContent(reason))) => Err(RpcError::invalid_params(reason)),
        Err(error) => {
            tracing::warn!(%method, error = %format!("{error:#}"), "direct memory {verb} failed");
            Err(RpcError::internal_error(match verb {
                // The edit publish is atomic and follows all validation, so
                // a failure left the stored entry untouched.
                "edit" => "memory entry edit failed; the stored entry was not changed",
                // The delete scrub touches archive files BEFORE MEMORY.md is
                // republished; a mid-apply failure can leave the live entry
                // present with some archived copies already removed. Report
                // that honestly instead of claiming all-or-nothing.
                _ => {
                    "memory entry delete failed partway; the live entry may still be present while some archived copies were already removed — inspect and retry"
                }
            }))
        }
    }
}

fn auth_or_error(
    ws: &WsConnection,
    identity: Option<&AuthIdentity>,
    close_on_auth_unavailable: bool,
    id: &str,
    method: &str,
) -> Option<AuthIdentity> {
    let Some(identity) = identity.cloned() else {
        if close_on_auth_unavailable {
            let _ = close_ws_with_code(ws, 1008, "auth_expired");
        }
        let _ = send_rpc_error(ws, Some(id.to_string()), auth_unavailable_error(method));
        return None;
    };
    Some(identity)
}

pub(super) async fn handle_memory_entries(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    close_on_auth_unavailable: bool,
    id: String,
    params: MemoryEntriesParams,
) {
    let method = octos_core::ui_protocol::methods::MEMORY_ENTRIES;
    let Some(identity) = auth_or_error(ws, identity, close_on_auth_unavailable, &id, method) else {
        return;
    };
    let memory_dir = match resolve_memory_dir(
        state,
        headers,
        &identity,
        params.session_id.as_deref(),
        method,
    ) {
        Ok(dir) => dir,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    // Lock-free read: MEMORY.md publishes are atomic (temp + rename), so a
    // reader sees a complete old or new file, never a torn one.
    let entries = match tokio::task::spawn_blocking(move || direct::list_entries(&memory_dir)).await
    {
        Ok(Ok(entries)) => entries,
        _ => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error("could not read memory entries"),
            );
            return;
        }
    };
    // Whole-entry truncation against the escaped-length frame budget: an
    // entry is never cut mid-text; `truncated` + `total` say so honestly.
    let total = entries.len();
    let mut items: Vec<MemoryEntryItem> = Vec::with_capacity(total);
    let mut used = 0usize;
    let mut truncated = false;
    for entry in entries {
        let text_escaped: usize = entry.text.chars().map(json_escaped_len).sum();
        // id (8) + version (64) + JSON keys/quotes/punctuation per item.
        let cost = text_escaped + 96;
        if used + cost > MEMORY_ENTRIES_BUDGET {
            truncated = true;
            break;
        }
        used += cost;
        items.push(MemoryEntryItem {
            version: direct::entry_version(&entry),
            id: entry.id,
            text: entry.text,
        });
    }
    send_aux_rpc_result(
        ws,
        id,
        method,
        json!(MemoryEntriesResult {
            entries: items,
            total,
            truncated,
        }),
    );
}

pub(super) async fn handle_memory_edit(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    close_on_auth_unavailable: bool,
    id: String,
    params: MemoryEditParams,
) {
    let method = octos_core::ui_protocol::methods::MEMORY_EDIT;
    let Some(identity) = auth_or_error(ws, identity, close_on_auth_unavailable, &id, method) else {
        return;
    };
    // Same cap and content guard as `memory/request` content.
    if params.content.trim().is_empty() || params.content.len() > MEMORY_REQUEST_MAX_CONTENT_BYTES {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params("content must be non-blank and at most 16384 UTF-8 bytes"),
        );
        return;
    }
    if octos_memory::guard::first_threat(&params.content).is_some() {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params("memory entry edit rejected by the content guard"),
        );
        return;
    }
    if let Err(error) = validate_entry_ref(&params.entry_id, &params.version) {
        let _ = send_rpc_error(ws, Some(id), error);
        return;
    }
    let memory_dir = match resolve_memory_dir(
        state,
        headers,
        &identity,
        params.session_id.as_deref(),
        method,
    ) {
        Ok(dir) => dir,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    let entry_id = params.entry_id.clone();
    let version = params.version.clone();
    let content = params.content.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let _lock = direct::lock_memory_writes_blocking(&memory_dir)?;
        direct::edit_entry(
            &memory_dir,
            &entry_id,
            &version,
            &content,
            chrono::Local::now().date_naive(),
        )
    })
    .await
    .unwrap_or_else(|join| Err(eyre::eyre!("memory edit task failed: {join}")));
    match direct_outcome_to_rpc(method, "edit", &params.entry_id, outcome) {
        Ok(receipt) => send_aux_rpc_result(
            ws,
            id,
            method,
            json!(MemoryEditResult {
                entry_id: receipt.entry_id,
                version: receipt.version,
            }),
        ),
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

pub(super) async fn handle_memory_delete(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    close_on_auth_unavailable: bool,
    id: String,
    params: MemoryDeleteParams,
) {
    let method = octos_core::ui_protocol::methods::MEMORY_DELETE;
    let Some(identity) = auth_or_error(ws, identity, close_on_auth_unavailable, &id, method) else {
        return;
    };
    if let Err(error) = validate_entry_ref(&params.entry_id, &params.version) {
        let _ = send_rpc_error(ws, Some(id), error);
        return;
    }
    let memory_dir = match resolve_memory_dir(
        state,
        headers,
        &identity,
        params.session_id.as_deref(),
        method,
    ) {
        Ok(dir) => dir,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    let entry_id = params.entry_id.clone();
    let version = params.version.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let _lock = direct::lock_memory_writes_blocking(&memory_dir)?;
        direct::delete_entry(
            &memory_dir,
            &entry_id,
            &version,
            chrono::Local::now().date_naive(),
        )
    })
    .await
    .unwrap_or_else(|join| Err(eyre::eyre!("memory delete task failed: {join}")));
    match direct_outcome_to_rpc(method, "delete", &params.entry_id, outcome) {
        Ok(receipt) => send_aux_rpc_result(
            ws,
            id,
            method,
            json!(MemoryDeleteResult {
                entry_id: receipt.entry_id,
                version: receipt.version,
            }),
        ),
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}
