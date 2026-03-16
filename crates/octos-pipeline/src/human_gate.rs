//! Human-in-the-loop (Interviewer pattern) for pipeline nodes.
//!
//! Allows pipeline execution to pause at designated nodes and wait for
//! human input before continuing. Supports approval gates, questionnaires,
//! and free-form input.

use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// Default timeout for human input requests (5 minutes).
const DEFAULT_INPUT_TIMEOUT: Duration = Duration::from_secs(300);

/// A request for human input during pipeline execution.
#[derive(Debug, Clone, Serialize)]
pub struct HumanRequest {
    /// Unique request ID.
    pub id: String,
    /// Pipeline node that triggered the request.
    pub node_id: String,
    /// What the pipeline is asking for.
    pub prompt: String,
    /// Type of input expected.
    pub input_type: HumanInputType,
    /// Context from previous nodes (for the human's reference).
    pub context: Option<String>,
}

/// Type of human input expected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HumanInputType {
    /// Simple approve/reject decision.
    Approval,
    /// Free-form text input.
    FreeText,
    /// Select from predefined choices.
    Choice { options: Vec<String> },
}

/// The human's response to a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanResponse {
    /// The request ID this responds to.
    pub request_id: String,
    /// Whether the human approved (for Approval type).
    pub approved: Option<bool>,
    /// Text input (for FreeText and Choice types).
    pub input: Option<String>,
}

impl HumanResponse {
    pub fn approve(request_id: &str) -> Self {
        Self {
            request_id: request_id.to_string(),
            approved: Some(true),
            input: None,
        }
    }

    pub fn reject(request_id: &str) -> Self {
        Self {
            request_id: request_id.to_string(),
            approved: Some(false),
            input: None,
        }
    }

    pub fn text(request_id: &str, input: &str) -> Self {
        Self {
            request_id: request_id.to_string(),
            approved: None,
            input: Some(input.to_string()),
        }
    }

    /// Returns true if the response is an approval or has non-empty input.
    pub fn is_positive(&self) -> bool {
        self.approved.unwrap_or(false) || self.input.as_ref().is_some_and(|s| !s.is_empty())
    }
}

/// Trait for providing human responses to pipeline requests.
#[async_trait]
pub trait HumanInputProvider: Send + Sync {
    /// Request human input. Blocks until the human responds.
    async fn request_input(&self, request: HumanRequest) -> Result<HumanResponse>;
}

/// Channel-based human input provider.
/// Requests are sent to a receiver (e.g. CLI, API handler),
/// responses come back via oneshot channels.
pub struct ChannelInputProvider {
    tx: mpsc::Sender<(HumanRequest, oneshot::Sender<HumanResponse>)>,
    timeout: Duration,
}

impl ChannelInputProvider {
    pub fn new() -> (
        Self,
        mpsc::Receiver<(HumanRequest, oneshot::Sender<HumanResponse>)>,
    ) {
        let (tx, rx) = mpsc::channel(8);
        (
            Self {
                tx,
                timeout: DEFAULT_INPUT_TIMEOUT,
            },
            rx,
        )
    }

    /// Create with a custom timeout for human responses.
    pub fn with_timeout(
        timeout: Duration,
    ) -> (
        Self,
        mpsc::Receiver<(HumanRequest, oneshot::Sender<HumanResponse>)>,
    ) {
        let (tx, rx) = mpsc::channel(8);
        (Self { tx, timeout }, rx)
    }
}

#[async_trait]
impl HumanInputProvider for ChannelInputProvider {
    async fn request_input(&self, request: HumanRequest) -> Result<HumanResponse> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx
            .send((request, resp_tx))
            .await
            .map_err(|_| eyre::eyre!("human input channel closed"))?;
        tokio::time::timeout(self.timeout, resp_rx)
            .await
            .map_err(|_| eyre::eyre!("human input timed out after {:?}", self.timeout))?
            .map_err(|_| eyre::eyre!("human response channel dropped"))
    }
}

/// Auto-approve provider for testing — always approves.
pub struct AutoApproveProvider;

#[async_trait]
impl HumanInputProvider for AutoApproveProvider {
    async fn request_input(&self, request: HumanRequest) -> Result<HumanResponse> {
        Ok(HumanResponse::approve(&request.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_create_approval_response() {
        let resp = HumanResponse::approve("req_1");
        assert!(resp.is_positive());
        assert_eq!(resp.approved, Some(true));
    }

    #[test]
    fn should_create_rejection_response() {
        let resp = HumanResponse::reject("req_1");
        assert!(!resp.is_positive());
        assert_eq!(resp.approved, Some(false));
    }

    #[test]
    fn should_create_text_response() {
        let resp = HumanResponse::text("req_1", "looks good");
        assert!(resp.is_positive());
        assert_eq!(resp.input.as_deref(), Some("looks good"));
    }

    #[test]
    fn should_detect_empty_text_as_negative() {
        let resp = HumanResponse::text("req_1", "");
        assert!(!resp.is_positive());
    }

    #[tokio::test]
    async fn should_auto_approve() {
        let provider = AutoApproveProvider;
        let request = HumanRequest {
            id: "req_1".into(),
            node_id: "review".into(),
            prompt: "Approve these changes?".into(),
            input_type: HumanInputType::Approval,
            context: None,
        };
        let resp = provider.request_input(request).await.unwrap();
        assert!(resp.is_positive());
    }

    #[tokio::test]
    async fn should_work_via_channel() {
        let (provider, mut rx) = ChannelInputProvider::new();

        let handle = tokio::spawn(async move {
            let request = HumanRequest {
                id: "req_2".into(),
                node_id: "gate".into(),
                prompt: "Continue?".into(),
                input_type: HumanInputType::Approval,
                context: None,
            };
            provider.request_input(request).await.unwrap()
        });

        // Simulate human responding
        let (req, resp_tx) = rx.recv().await.unwrap();
        assert_eq!(req.id, "req_2");
        resp_tx.send(HumanResponse::approve(&req.id)).unwrap();

        let resp = handle.await.unwrap();
        assert!(resp.is_positive());
    }

    #[test]
    fn should_serialize_input_types() {
        let json = serde_json::to_value(&HumanInputType::Approval).unwrap();
        assert_eq!(json, "approval");

        let json = serde_json::to_value(&HumanInputType::Choice {
            options: vec!["a".into(), "b".into()],
        })
        .unwrap();
        assert_eq!(json["choice"]["options"][0], "a");
    }
}
