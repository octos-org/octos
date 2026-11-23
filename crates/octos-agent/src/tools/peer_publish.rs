//! Explicit mid-turn checkpoint on the caller's own peer blackboard.
//! This does not finish the turn, send sibling instructions, or imply approval.
use super::{Tool, ToolResult};
use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub type PeerPublishCallback = Arc<dyn Fn(String) -> Result<String, String> + Send + Sync>;
type PeerPublishHandoff = Arc<dyn Fn(String, String) -> Result<String, String> + Send + Sync>;
pub struct PeerPublishTool {
    publish: PeerPublishCallback,
    handoff: Option<PeerPublishHandoff>,
}
impl PeerPublishTool {
    pub fn new(publish: PeerPublishCallback) -> Self {
        Self {
            publish,
            handoff: None,
        }
    }

    pub fn with_handoff(
        mut self,
        handoff: impl Fn(String, String) -> Result<String, String> + Send + Sync + 'static,
    ) -> Self {
        self.handoff = Some(Arc::new(handoff));
        self
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    message: String,
    #[serde(default, deserialize_with = "handoff_id_if_present")]
    handoff_id: Option<String>,
}
fn handoff_id_if_present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

#[async_trait]
impl Tool for PeerPublishTool {
    fn name(&self) -> &str {
        "peer_publish"
    }
    fn description(&self) -> &str {
        if self.handoff.is_some() {
            return "Publish a checkpoint to your own peer blackboard. Include exact artifact paths and anything unresolved. For a handoff ready for supervisor action, include a stable handoff_id to queue one turn on your owning supervisor. Reuse the ID for retries. Omit it for ordinary progress. The supervisor verifies the artifacts and decides the next action.";
        }
        "Publish an intermediate checkpoint to your own peer blackboard, including exact artifact paths and what is ready or unresolved. Supervisor and peers can read it through peer_gather while you keep working. This does not send instructions, mark work complete, or wake an idle peer; the supervisor routes follow-up tasks."
    }
    fn tags(&self) -> &[&str] {
        &["gateway"]
    }
    fn concurrency_class(&self) -> super::ConcurrencyClass {
        super::ConcurrencyClass::Exclusive
    }
    fn input_schema(&self) -> Value {
        let mut schema = json!({"type":"object","required":["message"],"additionalProperties":false,"properties":{"message":{"type":"string","minLength":1,"maxLength":8192}}});
        if self.handoff.is_some() {
            schema["properties"]["handoff_id"] = json!({"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9._-]+$","description":"Stable identifier for this publication, e.g. slide-03-cycle-01-ready. Choose a new ID for a new revision."});
        }
        schema
    }
    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input = match serde_json::from_value::<Input>(args.clone()) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("invalid peer_publish arguments: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };
        if input.message.trim().is_empty() || input.message.len() > 8192 {
            return Ok(ToolResult {
                output: "message must contain 1..8192 UTF-8 bytes".into(),
                success: false,
                ..Default::default()
            });
        }
        let result = match input.handoff_id {
            None => (self.publish)(input.message),
            Some(id) if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) => Err("handoff_id must contain 1..128 ASCII letters, digits, dots, underscores or hyphens".into()),
            Some(id) => match &self.handoff {
                Some(handoff) => handoff(input.message, id),
                None => Err("Supervisor handoff is unavailable for this peer".into()),
            }
        };
        Ok(match result {
            Ok(output) => ToolResult {
                output,
                success: true,
                ..Default::default()
            },
            Err(output) => ToolResult {
                output,
                success: false,
                ..Default::default()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn only_publishes_bounded_checkpoint_without_caller_chosen_identity() {
        let tool = PeerPublishTool::new(Arc::new(|m| Ok(format!("checkpoint: {m}"))));
        assert!(
            tool.execute(&json!({"message":"review/a/cycle.json ready"}))
                .await
                .unwrap()
                .success
        );
        assert!(
            tool.input_schema()["properties"]
                .get("handoff_id")
                .is_none()
        );
        for args in [
            json!({"message":""}),
            json!({"message":"x".repeat(8193)}),
            json!({"message":"done","slug":"sibling"}),
            json!({"message":"done","handoff_id":"page-1"}),
            json!({"message":"done","handoff_id":null}),
        ] {
            assert!(!tool.execute(&args).await.unwrap().success);
        }
    }

    #[tokio::test]
    async fn peer_publish_handoff_is_explicit_bounded_and_has_no_recipient_argument() {
        let tool =
            PeerPublishTool::new(Arc::new(|m| Ok(m))).with_handoff(|m, id| Ok(format!("{id} {m}")));
        assert_eq!(
            tool.execute(&json!({"message":"working"}))
                .await
                .unwrap()
                .output,
            "working"
        );
        assert_eq!(
            tool.execute(&json!({"message":"artifact ready","handoff_id":"s03-r1"}))
                .await
                .unwrap()
                .output,
            "s03-r1 artifact ready"
        );
        for id in ["", "../bad/id", "带空格", &"x".repeat(129)] {
            assert!(
                !tool
                    .execute(&json!({"message":"ready","handoff_id":id}))
                    .await
                    .unwrap()
                    .success
            );
        }
        assert!(
            !tool
                .execute(&json!({"message":"ready","handoff_id":"x","target":"sibling"}))
                .await
                .unwrap()
                .success
        );
    }
}
