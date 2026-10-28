//! Explicit user requests and application document nomination. These handlers
//! only admit work; the existing profile consolidator remains the sole writer.

use super::*;
use octos_core::ui_protocol::{
    MEMORY_PROMOTE_DEFAULT_LIMIT, MEMORY_PROMOTE_MAX_LIMIT, MEMORY_PROMOTE_MAX_SOURCES,
    MEMORY_REQUEST_MAX_CONTENT_BYTES, MemoryPromoteParams, MemoryPromoteResult,
    MemoryRequestOperation, MemoryRequestParams, MemoryRequestResult, MemoryRequestStatus,
};
use octos_memory::{MemoryStore, NoteKind, NoteOrigin, StagingNote};

fn validate_request(params: &MemoryRequestParams) -> Result<StagingNote, RpcError> {
    let invalid = RpcError::invalid_params;
    if params.request_id.is_empty()
        || params.request_id.len() > 128
        || !params
            .request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err(invalid(
            "request_id must be 1..=128 ASCII letters, digits or -_.:",
        ));
    }
    use MemoryRequestOperation::*;
    match params.operation {
        Remember if params.entry_id.is_some() => {
            return Err(invalid("remember does not accept entry_id"));
        }
        Correct | Forget
            if !params
                .entry_id
                .as_deref()
                .is_some_and(octos_memory::is_valid_entry_id) =>
        {
            return Err(invalid("entry_id must be ^m followed by 6 of [a-z2-7]"));
        }
        _ => {}
    }
    let (kind, content) = match params.operation {
        Forget => {
            if params.content.is_some() {
                return Err(invalid("forget accepts entry_id only, not content"));
            }
            (
                NoteKind::Forget,
                format!("id:{}", params.entry_id.as_deref().expect("validated id")),
            )
        }
        Remember | Correct => {
            let content = params.content.as_deref().unwrap_or("");
            if content.trim().is_empty() || content.len() > MEMORY_REQUEST_MAX_CONTENT_BYTES {
                return Err(invalid(
                    "content must be non-blank and at most 16384 UTF-8 bytes",
                ));
            }
            if octos_memory::guard::first_threat(content).is_some() {
                return Err(invalid("memory request rejected by the content guard"));
            }
            (
                if params.operation == Correct {
                    NoteKind::Correction
                } else {
                    NoteKind::UserRequest
                },
                content.to_owned(),
            )
        }
    };
    Ok(StagingNote {
        origin: NoteOrigin::Host,
        kind,
        content,
        session_key: None,
        sensitive: false,
        replaces_id: if params.operation == Correct {
            params.entry_id.clone()
        } else {
            None
        },
    })
}

fn admission_error(error: eyre::Report) -> RpcError {
    if error
        .downcast_ref::<octos_memory::MemoryRequestConflict>()
        .is_some()
    {
        RpcError::invalid_params("request_id already used with different content")
            .with_data(json!({"kind": "request_id_conflict"}))
    } else {
        // Do not echo local paths, note contents or storage errors to clients.
        RpcError::internal_error(
            "memory request could not be saved; retry with the same request_id",
        )
    }
}

async fn admit_request(
    store: &MemoryStore,
    params: &MemoryRequestParams,
    note: &StagingNote,
) -> Result<(), RpcError> {
    // An honored forget no longer has its target. Check replay before existence.
    if store
        .has_staging_request(&params.request_id, note)
        .await
        .map_err(admission_error)?
    {
        return Ok(());
    }
    if let Some(entry_id) = &params.entry_id {
        let memory = store
            .read_long_term()
            .await
            .map_err(|_| RpcError::internal_error("could not read memory"))?;
        let entries = crate::memory_consolidate::entry::parse_memory_md(&memory)
            .map_err(|_| RpcError::internal_error("could not parse memory entries"))?;
        let exists = match entries {
            crate::memory_consolidate::entry::ParsedMemory::Entries(entries) => {
                entries.iter().any(|e| &e.id == entry_id)
            }
            crate::memory_consolidate::entry::ParsedMemory::Legacy(_) => false,
        };
        if !exists {
            return Err(RpcError::not_found("memory_entry", entry_id));
        }
    }
    store
        .write_staging_request(&params.request_id, note)
        .await
        .map_err(admission_error)
}

pub(super) async fn handle_memory_request(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    close_on_auth_unavailable: bool,
    id: String,
    params: MemoryRequestParams,
) {
    let method = octos_core::ui_protocol::methods::MEMORY_REQUEST;
    let Some(identity) = identity else {
        if close_on_auth_unavailable {
            let _ = close_ws_with_code(ws, 1008, "auth_expired");
        }
        let _ = send_rpc_error(ws, Some(id), auth_unavailable_error(method));
        return;
    };
    let note = match validate_request(&params) {
        Ok(note) => note,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    let (_, runtime) = match resolve_memory_profile_runtime(state, headers, identity, method).await
    {
        Ok(resolved) => resolved,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    if let Err(error) = admit_request(&runtime.memory_store, &params, &note).await {
        let _ = send_rpc_error(ws, Some(id), error);
        return;
    }
    send_aux_rpc_result(
        ws,
        id,
        method,
        json!(MemoryRequestResult {
            request_id: params.request_id,
            operation: params.operation,
            status: MemoryRequestStatus::Accepted,
            refresh_enabled: runtime.memory_refresh_enabled,
        }),
    );
}

fn validate_promotion(params: &MemoryPromoteParams) -> Result<usize, RpcError> {
    let limit = params.limit.unwrap_or(MEMORY_PROMOTE_DEFAULT_LIMIT);
    if limit == 0 || limit > MEMORY_PROMOTE_MAX_LIMIT {
        return Err(RpcError::invalid_params("limit must be 1..=100"));
    }
    if params.sources.len() > MEMORY_PROMOTE_MAX_SOURCES
        || params.sources.iter().any(|s| {
            s.is_empty() || s.len() > 128 || s.trim() != s || s.chars().any(char::is_control)
        })
    {
        return Err(RpcError::invalid_params(
            "sources must contain at most 32 non-blank labels of at most 128 bytes, without surrounding whitespace or control characters",
        ));
    }
    Ok(limit)
}

pub(super) async fn handle_memory_promote(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    close_on_auth_unavailable: bool,
    id: String,
    params: MemoryPromoteParams,
) {
    let method = octos_core::ui_protocol::methods::MEMORY_PROMOTE;
    let Some(identity) = identity else {
        if close_on_auth_unavailable {
            let _ = close_ws_with_code(ws, 1008, "auth_expired");
        }
        let _ = send_rpc_error(ws, Some(id), auth_unavailable_error(method));
        return;
    };
    let limit = match validate_promotion(&params) {
        Ok(limit) => limit,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    let (_, runtime) = match resolve_memory_profile_runtime(state, headers, identity, method).await
    {
        Ok(resolved) => resolved,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    let promoted = match octos_agent::memory_index::nominate_for_promotion_from_sources(
        &runtime.memory_store,
        &runtime.recall,
        params.min_visits.unwrap_or(2),
        limit,
        None,
        &params.sources,
    )
    .await
    {
        Ok(promoted) => promoted,
        Err(_) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error("could not stage memory records"),
            );
            return;
        }
    };
    let record_ids: Vec<String> = promoted.into_iter().map(|r| r.id).collect();
    send_aux_rpc_result(
        ws,
        id,
        method,
        json!(MemoryPromoteResult {
            count: record_ids.len(),
            record_ids,
            refresh_enabled: runtime.memory_refresh_enabled,
        }),
    );
}
