//! Native ARC compiled-task contract for the MCP session boundary.
//!
//! ARC owns requirement compilation and sends a versioned execution package.
//! Octos validates that package before constructing an agent task, so malformed
//! or workspace-escaping inputs never fall back to the legacy prompt path.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// First native ARC compiled-task schema understood by Octos.
pub const ARC_AGENT_TASK_SCHEMA_V1: &str = "arc.agent-task.v1";

/// Portable task payload produced by ARC's compilation front-end.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArcAgentTaskV1 {
    pub schema: String,
    pub task_id: String,
    pub stage: String,
    pub backend_agent_name: String,
    pub node_id: String,
    pub phase: String,
    pub app_type: String,
    pub workspace_root: String,
    pub requirement_path: String,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub test_type: String,
    pub system_prompt: String,
    pub message: String,
    pub response_schema: Option<Value>,
    pub inputs: Value,
    pub acceptance: Value,
    #[serde(default)]
    pub skills: Vec<String>,
}

/// Validated native ARC request extracted from `run_octos_session.input`.
#[derive(Debug, Clone)]
pub struct ArcAgentTaskRequest {
    pub task: ArcAgentTaskV1,
    pub expected_artifact: PathBuf,
    pub artifact_name: String,
}

impl ArcAgentTaskRequest {
    /// Build the structured `TaskKind::Custom` parameters shown to the agent.
    ///
    /// The ARC system prompt is deliberately absent: the dispatch installs it
    /// through `Agent::with_system_prompt`, never as user-authored content.
    pub fn execution_params(&self) -> Value {
        json!({
            "schema": self.task.schema,
            "task_id": self.task.task_id,
            "stage": self.task.stage,
            "backend_agent_name": self.task.backend_agent_name,
            "node_id": self.task.node_id,
            "phase": self.task.phase,
            "app_type": self.task.app_type,
            "requirement_path": self.task.requirement_path,
            "thread_id": self.task.thread_id,
            "test_type": self.task.test_type,
            "instruction": self.task.message,
            "inputs": self.task.inputs,
            "acceptance": self.task.acceptance,
            "response_schema": self.task.response_schema,
            "requested_skills": self.task.skills,
            "delivery": {
                "expected_artifact": self.expected_artifact,
                "artifact_name": self.artifact_name,
                "format": "json"
            }
        })
    }
}

/// Error at the native ARC boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArcTaskError {
    prefix: &'static str,
    message: String,
}

impl ArcTaskError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            prefix: "arc_task_invalid:",
            message: message.into(),
        }
    }

    fn artifact(message: impl Into<String>) -> Self {
        Self {
            prefix: "artifact_schema_invalid:",
            message: message.into(),
        }
    }
}

impl fmt::Display for ArcTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.prefix, self.message)
    }
}

impl std::error::Error for ArcTaskError {}

/// JSON Schema fragment advertised below `input.arc_task`.
pub fn arc_agent_task_v1_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "schema": {
                "type": "string",
                "const": ARC_AGENT_TASK_SCHEMA_V1,
                "description": "Versioned ARC compiled-task schema."
            },
            "task_id": {"type": "string"},
            "stage": {"type": "string"},
            "backend_agent_name": {"type": "string"},
            "node_id": {"type": "string"},
            "phase": {"type": "string"},
            "app_type": {"type": "string"},
            "workspace_root": {"type": "string"},
            "requirement_path": {"type": "string"},
            "thread_id": {"type": "string"},
            "test_type": {"type": "string"},
            "system_prompt": {"type": "string"},
            "message": {"type": "string"},
            "response_schema": {
                "anyOf": [
                    {"type": "object"},
                    {"type": "null"}
                ]
            },
            "inputs": {"type": "object"},
            "acceptance": {"type": "object"},
            "skills": {
                "type": "array",
                "items": {"type": "string"}
            }
        },
        "required": [
            "schema",
            "task_id",
            "stage",
            "backend_agent_name",
            "node_id",
            "phase",
            "app_type",
            "workspace_root",
            "requirement_path",
            "system_prompt",
            "message",
            "response_schema",
            "inputs",
            "acceptance"
        ]
    })
}

/// Parse and validate an optional native ARC task.
///
/// `Ok(None)` is reserved for callers that omitted `input.arc_task`; malformed
/// native input always returns `Err` and must never enter the legacy path.
pub fn parse_arc_agent_task_input(
    input: &Value,
    workspace_root: &Path,
) -> Result<Option<ArcAgentTaskRequest>, ArcTaskError> {
    let Some(raw_task) = input.get("arc_task") else {
        return Ok(None);
    };
    let task: ArcAgentTaskV1 = serde_json::from_value(raw_task.clone())
        .map_err(|error| ArcTaskError::invalid(format!("cannot decode arc_task: {error}")))?;
    validate_task(&task, workspace_root)?;

    let artifact = input
        .get("expected_artifact")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ArcTaskError::invalid("native ARC execution requires non-empty input.expected_artifact")
        })?;
    let expected_artifact = validate_relative_artifact_path(artifact)?;
    let artifact_name = input
        .get("artifact_name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("arc-stage-result")
        .to_string();

    Ok(Some(ArcAgentTaskRequest {
        task,
        expected_artifact,
        artifact_name,
    }))
}

fn validate_task(task: &ArcAgentTaskV1, workspace_root: &Path) -> Result<(), ArcTaskError> {
    if task.schema != ARC_AGENT_TASK_SCHEMA_V1 {
        return Err(ArcTaskError::invalid(format!(
            "unsupported schema {:?}; expected {ARC_AGENT_TASK_SCHEMA_V1:?}",
            task.schema
        )));
    }
    for (name, value) in [
        ("task_id", task.task_id.as_str()),
        ("stage", task.stage.as_str()),
        ("backend_agent_name", task.backend_agent_name.as_str()),
        ("node_id", task.node_id.as_str()),
        ("phase", task.phase.as_str()),
        ("app_type", task.app_type.as_str()),
        ("workspace_root", task.workspace_root.as_str()),
        ("requirement_path", task.requirement_path.as_str()),
        ("system_prompt", task.system_prompt.as_str()),
        ("message", task.message.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(ArcTaskError::invalid(format!(
                "arc_task.{name} must not be empty"
            )));
        }
    }
    if !task.inputs.is_object() {
        return Err(ArcTaskError::invalid("arc_task.inputs must be an object"));
    }
    if !task.acceptance.is_object() {
        return Err(ArcTaskError::invalid(
            "arc_task.acceptance must be an object",
        ));
    }
    if let Some(response_schema_required) = task.acceptance.get("response_schema_required") {
        let response_schema_required = response_schema_required.as_bool().ok_or_else(|| {
            ArcTaskError::invalid("arc_task.acceptance.response_schema_required must be a boolean")
        })?;
        if response_schema_required && task.response_schema.is_none() {
            return Err(ArcTaskError::invalid(
                "arc_task.response_schema must be an object when acceptance.response_schema_required is true",
            ));
        }
    }
    if task
        .response_schema
        .as_ref()
        .is_some_and(|schema| !schema.is_object())
    {
        return Err(ArcTaskError::invalid(
            "arc_task.response_schema must be an object or null",
        ));
    }

    let configured = std::fs::canonicalize(workspace_root).map_err(|error| {
        ArcTaskError::invalid(format!(
            "cannot resolve mcp-serve workspace_root {}: {error}",
            workspace_root.display()
        ))
    })?;
    let requested = std::fs::canonicalize(Path::new(&task.workspace_root)).map_err(|error| {
        ArcTaskError::invalid(format!(
            "cannot resolve arc_task.workspace_root {:?}: {error}",
            task.workspace_root
        ))
    })?;
    if requested != configured {
        return Err(ArcTaskError::invalid(format!(
            "arc_task.workspace_root {} does not match mcp-serve workspace {}",
            requested.display(),
            configured.display()
        )));
    }
    Ok(())
}

fn validate_relative_artifact_path(path: &str) -> Result<PathBuf, ArcTaskError> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(ArcTaskError::invalid(
            "input.expected_artifact must be workspace-relative",
        ));
    }
    let mut has_normal_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_normal_component = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ArcTaskError::invalid(
                    "input.expected_artifact must not escape the workspace",
                ));
            }
        }
    }
    if !has_normal_component {
        return Err(ArcTaskError::invalid(
            "input.expected_artifact must name a file",
        ));
    }
    Ok(path.to_path_buf())
}

/// Validate an ARC JSON artifact against the supported response-schema subset.
pub fn validate_arc_response(
    response_schema: &Value,
    artifact: &Value,
) -> Result<(), ArcTaskError> {
    let mut errors = Vec::new();
    validate_schema_node(
        response_schema,
        response_schema,
        artifact,
        "$",
        0,
        &mut errors,
    );
    if errors.is_empty() {
        Ok(())
    } else {
        errors.truncate(8);
        Err(ArcTaskError::artifact(errors.join("; ")))
    }
}

/// Ensure a resolved ARC artifact does not escape through a workspace symlink.
pub fn validate_arc_artifact_location(
    workspace_root: &Path,
    artifact_path: &Path,
) -> Result<(), ArcTaskError> {
    let workspace = std::fs::canonicalize(workspace_root).map_err(|error| {
        ArcTaskError::artifact(format!(
            "cannot resolve workspace {}: {error}",
            workspace_root.display()
        ))
    })?;
    let artifact = std::fs::canonicalize(artifact_path).map_err(|error| {
        ArcTaskError::artifact(format!(
            "cannot resolve artifact {}: {error}",
            artifact_path.display()
        ))
    })?;
    if !artifact.starts_with(&workspace) {
        return Err(ArcTaskError::artifact(format!(
            "artifact {} resolves outside workspace {}",
            artifact.display(),
            workspace.display()
        )));
    }
    Ok(())
}

fn validate_schema_node(
    root: &Value,
    schema: &Value,
    instance: &Value,
    path: &str,
    depth: usize,
    errors: &mut Vec<String>,
) {
    if depth > 64 {
        errors.push(format!("{path}: schema nesting exceeds 64 levels"));
        return;
    }
    if schema == &Value::Bool(true) {
        return;
    }
    if schema == &Value::Bool(false) {
        errors.push(format!("{path}: rejected by false schema"));
        return;
    }
    let Some(schema_object) = schema.as_object() else {
        errors.push(format!("{path}: schema must be an object or boolean"));
        return;
    };

    if let Some(reference) = schema_object.get("$ref").and_then(Value::as_str) {
        match resolve_local_reference(root, reference) {
            Some(target) => {
                validate_schema_node(root, target, instance, path, depth + 1, errors);
            }
            None => errors.push(format!("{path}: unresolved schema reference {reference:?}")),
        }
        return;
    }

    if let Some(branches) = schema_object.get("anyOf").and_then(Value::as_array) {
        if !branches
            .iter()
            .any(|branch| schema_matches(root, branch, instance, depth + 1))
        {
            errors.push(format!("{path}: value does not satisfy anyOf"));
        }
        return;
    }
    if let Some(branches) = schema_object.get("oneOf").and_then(Value::as_array) {
        let matching = branches
            .iter()
            .filter(|branch| schema_matches(root, branch, instance, depth + 1))
            .count();
        if matching != 1 {
            errors.push(format!(
                "{path}: value must satisfy exactly one oneOf branch, matched {matching}"
            ));
        }
        return;
    }

    if let Some(constant) = schema_object.get("const")
        && constant != instance
    {
        errors.push(format!("{path}: value does not match const"));
    }
    if let Some(allowed) = schema_object.get("enum").and_then(Value::as_array)
        && !allowed.contains(instance)
    {
        errors.push(format!("{path}: value is not in enum"));
    }
    if let Some(expected_type) = schema_object.get("type")
        && !matches_type(instance, expected_type)
    {
        errors.push(format!(
            "{path}: expected {}, got {}",
            schema_type_label(expected_type),
            instance_type(instance)
        ));
        return;
    }

    if let Some(object) = instance.as_object() {
        let required = schema_object
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str);
        for name in required {
            if !object.contains_key(name) {
                errors.push(format!("{path}.{name}: missing required property"));
            }
        }
        let properties = schema_object.get("properties").and_then(Value::as_object);
        if let Some(properties) = properties {
            for (name, child_schema) in properties {
                if let Some(child) = object.get(name) {
                    validate_schema_node(
                        root,
                        child_schema,
                        child,
                        &format!("{path}.{name}"),
                        depth + 1,
                        errors,
                    );
                }
            }
        }
        if schema_object.get("additionalProperties") == Some(&Value::Bool(false)) {
            for name in object.keys() {
                if properties.is_none_or(|properties| !properties.contains_key(name)) {
                    errors.push(format!("{path}.{name}: additional property is not allowed"));
                }
            }
        }
    }

    if let Some(items) = schema_object.get("items")
        && let Some(array) = instance.as_array()
    {
        for (index, child) in array.iter().enumerate() {
            validate_schema_node(
                root,
                items,
                child,
                &format!("{path}[{index}]"),
                depth + 1,
                errors,
            );
        }
    }
}

fn schema_matches(root: &Value, schema: &Value, instance: &Value, depth: usize) -> bool {
    let mut errors = Vec::new();
    validate_schema_node(root, schema, instance, "$", depth, &mut errors);
    errors.is_empty()
}

fn resolve_local_reference<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    root.pointer(pointer)
}

fn matches_type(instance: &Value, expected: &Value) -> bool {
    match expected {
        Value::String(kind) => matches_single_type(instance, kind),
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| matches_single_type(instance, kind)),
        _ => false,
    }
}

fn matches_single_type(instance: &Value, expected: &str) -> bool {
    match expected {
        "null" => instance.is_null(),
        "boolean" => instance.is_boolean(),
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "integer" => instance.as_i64().is_some() || instance.as_u64().is_some(),
        "number" => instance.is_number(),
        _ => false,
    }
}

fn schema_type_label(expected: &Value) -> String {
    match expected {
        Value::String(kind) => kind.clone(),
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "a valid JSON Schema type".to_string(),
    }
}

fn instance_type(instance: &Value) -> &'static str {
    match instance {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_validator_supports_pydantic_refs_and_nullable_any_of() {
        let schema = json!({
            "type": "object",
            "required": ["item", "note"],
            "$defs": {
                "Item": {
                    "type": "object",
                    "required": ["id"],
                    "properties": {"id": {"type": "string"}},
                    "additionalProperties": false
                }
            },
            "properties": {
                "item": {"$ref": "#/$defs/Item"},
                "note": {
                    "anyOf": [
                        {"type": "string"},
                        {"type": "null"}
                    ]
                }
            }
        });

        validate_arc_response(&schema, &json!({"item": {"id": "one"}, "note": null}))
            .expect("valid Pydantic-style response");
    }

    #[test]
    fn response_validator_rejects_nested_additional_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "item": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }
        });

        let error = validate_arc_response(&schema, &json!({"item": {"unexpected": true}}))
            .expect_err("additional property must fail");
        assert!(error.to_string().contains("$.item.unexpected"));
    }

    #[cfg(unix)]
    #[test]
    fn artifact_location_rejects_symlink_outside_workspace() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_artifact = outside.path().join("result.json");
        std::fs::write(&outside_artifact, "{}").unwrap();
        let linked_artifact = workspace.path().join("result.json");
        symlink(&outside_artifact, &linked_artifact).unwrap();

        let error = validate_arc_artifact_location(workspace.path(), &linked_artifact)
            .expect_err("symlink escape must fail");
        assert!(error.to_string().contains("outside workspace"));
    }
}
