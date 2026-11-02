// Keep an authoritative Changes snapshot out of generic string/array trimming.
fn send_hydrate_result(ws: &WsConnection, id: String, result: SessionHydrateResult) {
    if result.file_mutations.as_ref().is_none_or(Vec::is_empty) {
        send_serialized_rpc_result(ws, id, octos_core::ui_protocol::methods::SESSION_HYDRATE, result);
        return;
    }
    let frame = serde_json::to_value(result).and_then(|value| {
        app_ui_codec::to_compact_json(&RpcResponse::success(id.clone(), value))
    });
    match frame {
        Ok(text) if text.len() <= MAX_TEXT_FRAME_BYTES => {
            // Exact serialization including the request id; do not preview it.
            if let Err(error) = ws.send_lifecycle(WsMessage::text(text)) {
                warn!(?error, "failed to deliver complete hydrate response");
            }
        }
        Ok(_) => {
            let error = super::ui_protocol_ledger::hydrate_references_error(
                "response_bytes_limit", Some(MAX_TEXT_FRAME_BYTES));
            let _ = send_rpc_error(ws, Some(id), error);
        }
        Err(error) => {
            warn!(%error, "failed to serialize hydrate response");
            let _ = send_rpc_error(ws, Some(id), RpcError::internal_error("failed to serialize session/hydrate"));
        }
    }
}
