//! Owner-side inspection and acknowledgement of durable checkpoint notifications.
use super::{ConcurrencyClass, Tool, ToolResult};
use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{Value, json};

type Inbox = dyn Fn(Vec<String>, Option<String>) -> Result<Value, String> + Send + Sync;

pub struct PeerCheckpointInboxTool(Box<Inbox>);

impl PeerCheckpointInboxTool {
    pub fn new(
        callback: impl Fn(Vec<String>, Option<String>) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        Self(Box::new(callback))
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Input {
    acknowledge: Vec<String>,
    after: Option<String>,
}

#[async_trait]
impl Tool for PeerCheckpointInboxTool {
    fn name(&self) -> &str {
        "peer_checkpoint_inbox"
    }
    fn description(&self) -> &str {
        "List queued peer checkpoints for this supervisor and workspace. Inspect their messages and reconcile the referenced work. Pass exact receipt IDs in acknowledge for checkpoints already handled in this turn. Acknowledged messages will not start another turn. Use next_after for pagination."
    }
    fn tags(&self) -> &[&str] {
        &["gateway"]
    }
    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","additionalProperties":false,"properties":{
            "acknowledge":{"type":"array","maxItems":64,"items":{"type":"string","maxLength":256}},
            "after":{"type":"string","maxLength":256}
        }})
    }
    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let result = serde_json::from_value::<Input>(args.clone())
            .map_err(|e| e.to_string())
            .and_then(|input| {
                if input.acknowledge.len() > 64
                    || input
                        .acknowledge
                        .iter()
                        .chain(input.after.iter())
                        .any(|id| id.is_empty() || id.len() > 256)
                {
                    return Err("Invalid checkpoint receipt list".to_owned());
                }
                (self.0)(input.acknowledge, input.after)
            });
        Ok(match result {
            Ok(value) => ToolResult {
                output: value.to_string(),
                success: true,
                ..Default::default()
            },
            Err(error) => ToolResult {
                output: error,
                success: false,
                ..Default::default()
            },
        })
    }
}
