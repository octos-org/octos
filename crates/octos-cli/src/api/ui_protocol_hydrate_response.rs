// Keep an authoritative Changes snapshot out of generic string/array trimming,
// while preserving the existing record previews for long conversations.
fn send_hydrate_result(ws: &WsConnection, id: String, result: SessionHydrateResult) {
    if result.file_mutations.as_ref().is_none_or(Vec::is_empty) {
        send_serialized_rpc_result(
            ws,
            id,
            octos_core::ui_protocol::methods::SESSION_HYDRATE,
            result,
        );
        return;
    }
    match hydrate_frame_with_complete_mutations(&id, result) {
        Ok(text) => {
            if let Err(error) = ws.send_lifecycle(WsMessage::text(text)) {
                warn!(?error, "failed to deliver complete hydrate response");
            }
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

fn hydrate_frame_with_complete_mutations(
    id: &str,
    result: SessionHydrateResult,
) -> Result<String, RpcError> {
    let encoding_error = |error: serde_json::Error| {
        warn!(%error, "failed to encode hydrate response");
        RpcError::internal_error("failed to encode session/hydrate")
    };
    let budget_error = || {
        super::ui_protocol_ledger::hydrate_references_error(
            "response_bytes_limit",
            Some(MAX_TEXT_FRAME_BYTES),
        )
    };
    let mut value = serde_json::to_value(result).map_err(encoding_error)?;
    let original =
        app_ui_codec::to_compact_json(&RpcResponse::success(id, &value)).map_err(encoding_error)?;
    if original.len() <= MAX_TEXT_FRAME_BYTES {
        return Ok(original);
    }

    // The empty-array placeholder keeps its key and delimiters in the record
    // envelope. Reserve the exact additional encoded bytes, including escaping.
    let mutations = value["file_mutations"].take();
    let reserved = app_ui_codec::to_compact_json(&mutations)
        .map_err(encoding_error)?
        .len()
        - 2;
    let record_cap = MAX_TEXT_FRAME_BYTES
        .checked_sub(reserved)
        .ok_or_else(budget_error)?;
    value["file_mutations"] = serde_json::json!([]);
    let records =
        app_ui_codec::to_compact_json(&RpcResponse::success(id, value)).map_err(encoding_error)?;
    let preview = preview_frame_with_budget(
        records,
        record_cap,
        TRUNCATED_FRAME_TARGET_BYTES.saturating_sub(reserved),
    );
    let mut response: RpcResponse<Value> =
        serde_json::from_str(&preview).map_err(encoding_error)?;
    response.result["file_mutations"] = mutations;
    // Restore the exact request id too: it is not previewable response content.
    let text = app_ui_codec::to_compact_json(&RpcResponse::success(id, response.result))
        .map_err(encoding_error)?;
    if text.len() > MAX_TEXT_FRAME_BYTES {
        return Err(budget_error());
    }
    Ok(text)
}
