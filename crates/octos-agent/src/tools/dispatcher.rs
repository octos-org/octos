//! Stable model-facing discovery and invocation. Resolution precedes the normal
//! agent execution gates; this module never executes a nested agent or program.
use async_trait::async_trait;
use eyre::{Result, bail};
use octos_core::ToolCall;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolPermissions, ToolRegistry, ToolResult};

pub(crate) const DIRECT: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "exec_command",
    "discover_tools",
    "run_tool",
];
const MAX_DISCOVERY_BYTES: usize = 24_000;

pub(crate) struct DiscoverTools;
pub(crate) struct RunTool;

#[async_trait]
impl Tool for DiscoverTools {
    fn name(&self) -> &str {
        "discover_tools"
    }
    fn description(&self) -> &str {
        "Find optional tools without changing the tool list. Search query returns names and descriptions; name returns the exact parameter schema. Use run_tool with that name and arguments. Supports registered shell sessions, search, web, browser, agent, plugin and MCP tools when authorized."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "query":{"type":"string","description":"Search names and descriptions; omit to browse."},
            "name":{"type":"string","description":"Get one tool's full schema; exclusive with query."},
            "limit":{"type":"integer","minimum":1,"maximum":10,"default":5},
            "offset":{"type":"integer","minimum":0,"default":0},
            "schema_pointer":{"type":"string","description":"JSON pointer within input_schema; requires name. Use for oversized schemas."}
        },"additionalProperties":false})
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn execute(&self, _: &Value) -> Result<ToolResult> {
        bail!("discover_tools requires the tool registry")
    }
}

#[async_trait]
impl Tool for RunTool {
    fn name(&self) -> &str {
        "run_tool"
    }
    fn description(&self) -> &str {
        "Invoke one authorized tool by name using its exact arguments from discover_tools. Normal approvals, permissions, timeouts and results apply. Separate independent calls may run concurrently; wait for results before submitting dependent work."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "name":{"type":"string","minLength":1},
            "arguments":{"type":"object","additionalProperties":true}
        },"required":["name","arguments"],"additionalProperties":false})
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn execute(&self, _: &Value) -> Result<ToolResult> {
        bail!("run_tool requires the tool registry")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invocation {
    name: String,
    arguments: serde_json::Map<String, Value>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Discovery {
    query: Option<String>,
    name: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    schema_pointer: Option<String>,
}

/// For history analysis only. This does NOT authorize execution.
pub(crate) fn logical_call<'a>(name: &'a str, args: &'a Value) -> (&'a str, &'a Value) {
    if name == "run_tool"
        && let (Some(name), Some(args)) = (
            args.get("name").and_then(Value::as_str),
            args.get("arguments"),
        )
        && args.is_object()
        && !matches!(name, "run_tool" | "discover_tools")
    {
        return (name, args);
    }
    (name, args)
}

impl ToolRegistry {
    pub(crate) fn is_dispatch_gateway(&self, name: &str) -> bool {
        self.get(name).is_some_and(|tool| {
            tool.as_any().is::<RunTool>() || tool.as_any().is::<DiscoverTools>()
        })
    }

    fn dispatcher_allows(&self, name: &str, permissions: &ToolPermissions) -> bool {
        self.is_tool_visible_post_activation(name)
            && permissions.is_tool_allowed(name)
            && self.get(name).is_some_and(|tool| tool.is_available())
    }

    /// Resolve exactly one call, preserving the provider's ID and metadata.
    /// Authorization is checked for BOTH the gateway and the selected tool.
    pub(crate) fn resolve_dispatch_call(
        &self,
        call: &ToolCall,
        permissions: &ToolPermissions,
    ) -> Result<ToolCall> {
        if call.name != "run_tool" || !self.is_dispatch_gateway(&call.name) {
            return Ok(call.clone());
        }
        if !self.dispatcher_allows("run_tool", permissions) {
            bail!("run_tool is not available in this session");
        }
        if serde_json::to_vec(&call.arguments)?.len() > 1_048_576 {
            bail!("run_tool arguments exceed 1 MB");
        }
        let invocation: Invocation = serde_json::from_value(call.arguments.clone())?;
        if matches!(invocation.name.as_str(), "run_tool" | "discover_tools") {
            bail!("recursive dispatch is not supported; call discover_tools directly");
        }
        if !self.dispatcher_allows(&invocation.name, permissions) {
            bail!("requested tool is unknown or unavailable in this session");
        }
        Ok(ToolCall {
            id: call.id.clone(),
            name: invocation.name,
            arguments: Value::Object(invocation.arguments),
            metadata: call.metadata.clone(),
        })
    }

    pub(crate) fn discover_tools(
        &self,
        args: &Value,
        permissions: &ToolPermissions,
    ) -> Result<ToolResult> {
        if !self.dispatcher_allows("discover_tools", permissions) {
            bail!("discover_tools is not available in this session");
        }
        if serde_json::to_vec(args)?.len() > 1_048_576 {
            bail!("discover_tools arguments exceed 1 MB");
        }
        let request: Discovery = serde_json::from_value(args.clone())?;
        if request.name.is_some() && request.query.is_some() {
            bail!("provide name or query, not both");
        }
        let limit = request.limit.unwrap_or(5);
        if !(1..=10).contains(&limit) {
            bail!("limit must be between 1 and 10");
        }
        let value = if let Some(name) = request.name {
            if !self.dispatcher_allows(&name, permissions) || self.is_dispatch_gateway(&name) {
                bail!("requested tool is unknown or unavailable in this session");
            }
            let tool = self.get(&name).expect("authorized registered tool");
            let schema = tool.input_schema();
            if let Some(pointer) = request.schema_pointer {
                let fragment = schema
                    .pointer(&pointer)
                    .ok_or_else(|| eyre::eyre!("unknown schema_pointer"))?;
                json!({"name":name,"schema_pointer":pointer,"schema":fragment})
            } else {
                json!({"name":name,"description":tool.description(),"input_schema":schema})
            }
        } else {
            if request.schema_pointer.is_some() {
                bail!("schema_pointer requires name");
            }
            let query = request.query.unwrap_or_default().to_lowercase();
            let terms: Vec<_> = query.split_whitespace().collect();
            let mut matches: Vec<_> = self
                .dispatch_catalog(permissions)
                .into_iter()
                .filter_map(|(name, description)| {
                    let haystack = format!("{name} {description}").to_lowercase();
                    let score = terms
                        .iter()
                        .filter(|term| haystack.contains(**term))
                        .count();
                    (terms.is_empty() || score > 0).then_some((score, name, description))
                })
                .collect();
            matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let total = matches.len();
            let offset = request.offset.unwrap_or(0);
            let items: Vec<_> = matches.into_iter().skip(offset).take(limit)
                .map(|(_,name,description)| json!({"name":name,"description":description.chars().take(300).collect::<String>()})).collect();
            let next = offset.saturating_add(items.len());
            json!({"tools":items,"total":total,"next_offset":(next < total).then_some(next),
                "hint":"Call discover_tools with name to obtain its typed input_schema."})
        };
        let output = serde_json::to_string(&value)?;
        if output.len() > MAX_DISCOVERY_BYTES {
            if value.get("tools").is_some() {
                bail!("discovery results exceed 24000 bytes; narrow query or reduce limit");
            }
            // Do not hand the model truncated JSON masquerading as a schema.
            let schema = value.get("input_schema").or_else(|| value.get("schema"));
            let keys: Vec<_> = schema
                .and_then(Value::as_object)
                .map(|obj| obj.keys().take(40).collect())
                .unwrap_or_default();
            let output = serde_json::to_string(&json!({
                "name":value["name"], "schema_pointer":value.get("schema_pointer").cloned().unwrap_or(json!("")),
                "schema_omitted":true,"keys":keys,
                "hint":"Schema exceeds 24000 bytes. Request smaller fragments with name and schema_pointer, e.g. /properties or /properties/path. JSON pointer escapes: ~0 for ~, ~1 for /."
            }))?;
            if output.len() > MAX_DISCOVERY_BYTES {
                bail!("schema metadata exceeds 24000 bytes; request a smaller schema_pointer");
            }
            return Ok(ToolResult {
                success: true,
                output,
                ..Default::default()
            });
        }
        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }
}
