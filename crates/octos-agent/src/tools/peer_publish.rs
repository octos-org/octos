//! Explicit mid-turn checkpoint on the caller's own peer blackboard.
//! This does not finish the turn, send sibling instructions, or imply approval.
use super::{Tool, ToolResult};
use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub type PeerPublishCallback = Arc<dyn Fn(String) -> Result<String, String> + Send + Sync>;
pub struct PeerPublishTool {
    publish: PeerPublishCallback,
}
impl PeerPublishTool {
    pub fn new(publish: PeerPublishCallback) -> Self {
        Self { publish }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    message: String,
}
#[async_trait]
impl Tool for PeerPublishTool {
    fn name(&self) -> &str {
        "peer_publish"
    }
    fn description(&self) -> &str {
        "Publish an intermediate checkpoint to your own peer blackboard, including exact artifact paths and what is ready or unresolved. Supervisor and peers can read it through peer_gather while you keep working. This does not send instructions, mark work complete, or wake an idle peer; the supervisor routes follow-up tasks."
    }
    fn tags(&self) -> &[&str] {
        &["gateway"]
    }
    fn concurrency_class(&self) -> super::ConcurrencyClass {
        super::ConcurrencyClass::Exclusive
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","required":["message"],"additionalProperties":false,"properties":{"message":{"type":"string","minLength":1,"maxLength":8192}}})
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
        Ok(match (self.publish)(input.message) {
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
        for args in [
            json!({"message":""}),
            json!({"message":"x".repeat(8193)}),
            json!({"message":"done","slug":"sibling"}),
        ] {
            assert!(!tool.execute(&args).await.unwrap().success);
        }
    }
}
