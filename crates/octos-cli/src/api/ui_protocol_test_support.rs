use super::*;
use axum::extract::ws::Message as WsMessage;
use serde_json::Value;
use tokio::sync::mpsc;

pub(crate) async fn recv_rpc_json(rx: &mut mpsc::Receiver<WsMessage>) -> Value {
    let text = match rx.recv().await.expect("rpc frame") {
        WsMessage::Text(text) => Ok(text),
        other => Err(other),
    }
    .expect("expected text frame");
    serde_json::from_str(text.as_str()).expect("json frame")
}

/// One connection's raw App UI RPC path, with its own ledger, contract
/// stores and turn maps. The replies go to the connection's receiver.
pub(crate) struct RawRpc {
    state: Arc<AppState>,
    ws: WsConnection,
    ledger: Arc<UiProtocolLedger>,
    contracts: Arc<UiProtocolContractStores>,
    active_turns: SharedActiveTurns,
    connection_turns: SharedConnectionTurns,
}

impl RawRpc {
    pub(crate) fn new(state: &Arc<AppState>, ws: &WsConnection) -> Self {
        Self {
            state: Arc::clone(state),
            ws: ws.clone(),
            ledger: Arc::new(UiProtocolLedger::new(16)),
            contracts: Arc::new(UiProtocolContractStores::default()),
            active_turns: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            connection_turns: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Whether the raw path handled `method`.
    pub(crate) async fn call(
        &self,
        method: &str,
        features: ConnectionUiFeatures,
        id: &str,
        params: Value,
    ) -> bool {
        let request = RpcRequest::new(id.to_owned(), method, params);
        let (ledger, contracts) = (&self.ledger, &self.contracts);
        let (active_turns, connection_turns) = (&self.active_turns, &self.connection_turns);
        handle_raw_appui_rpc(
            &self.ws,
            &self.state,
            ledger,
            contracts,
            active_turns,
            connection_turns,
            features,
            None,
            id.to_owned(),
            &request,
        )
        .await
    }
}

/// Builds a `WsConnection` whose writer side feeds the retained tests' mpsc.
pub(crate) fn ws_connection_for_test(capacity: usize) -> (WsConnection, mpsc::Receiver<WsMessage>) {
    let (tx, rx) = mpsc::channel(capacity);
    (WsConnection::new(tx), rx)
}
