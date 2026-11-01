use super::WsConnection;
use axum::extract::ws::Message as WsMessage;
use octos_core::Message;
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

/// Builds a `WsConnection` whose writer side feeds an in-test `mpsc`. The
/// returned receiver is the "dedicated writer task" stand-in; drain it to
/// unblock further sends, leave it alone to simulate a slow client.
pub(crate) fn ws_connection_for_test(capacity: usize) -> (WsConnection, mpsc::Receiver<WsMessage>) {
    let (tx, rx) = mpsc::channel(capacity);
    (WsConnection::new(tx), rx)
}

/// Mock provider for router-emission tests: never called, just lets
/// `AdaptiveRouter::new` accept two slots so the lane-scoring + breaker
/// snapshot paths fire end-to-end.
pub(crate) struct Wave4AStubProvider {
    pub(crate) name: &'static str,
    pub(crate) model: &'static str,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for Wave4AStubProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        Err(eyre::eyre!("stub not callable in tests"))
    }
    fn model_id(&self) -> &str {
        self.model
    }
    fn provider_name(&self) -> &str {
        self.name
    }
}
