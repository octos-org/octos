//! Strict validator for plugin manifest tool schemas (RFC-2).
//!
//! Background
//! ----------
//! On 2026-05-25 the `mofa-slides v0.5.0` skill shipped a tool with an
//! `input_schema` of the form
//!
//! ```json
//! {
//!   "type": "object",
//!   "anyOf": [
//!     { "required": ["slides"] },
//!     { "required": ["input"] }
//!   ]
//! }
//! ```
//!
//! Each `anyOf` branch lacks a `type` declaration. Strict LLM provider
//! validators (e.g. Moonshot Kimi K2.6, several Deepseek revisions)
//! reject this shape at request time, surfacing as a runtime tool-call
//! failure deep inside the agent loop with no useful pointer back to the
//! manifest. RFC-2 closes that loop by validating every manifest at
//! parse time and at install time so the problem surfaces with a clear
//! error pointing at the offending field.
//!
//! Validation layers
//! -----------------
//! 1. **Draft 07 sanity** — we walk every schema node and reject any
//!    keyword whose declared type is incompatible with Draft 07 (e.g. a
//!    `required` field that isn't an array of strings). This is the
//!    cheap subset of meta-schema validation that catches today's bug
//!    class without pulling in a full meta-schema runtime.
//! 2. **Strict octos rules** — defensive rules tuned to LLM provider
//!    validators. Every `anyOf`/`oneOf`/`allOf` branch must declare a
//!    `type`, every `properties.X` must declare a `type`, `$ref` and
//!    `$dynamicAnchor`/`$dynamicRef` are rejected unless the schema
//!    opts in to Draft 07 explicitly, `required` must be an array of
//!    strings, `enum` values must be unique, and the root must be
//!    `type: "object"`.
//!
//! Tuning
//! ------
//! The strict layer is gated behind `OCTOS_MANIFEST_VALIDATION`:
//!
//! - `strict` (default) — all rules above are enforced.
//! - `lenient` — Draft 07 sanity only; the octos rules are skipped.
//! - `off` — validator returns Ok unconditionally. Reserved for
//!   incident-response unblocks; never set this in production by
//!   default.

use std::collections::HashSet;
use std::env;
use std::fmt;

use serde_json::Value;

use crate::manifest::PluginManifest;

/// Which schema on a tool definition failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaKind {
    /// `tool.input_schema`.
    Input,
    /// `tool.output_schema` (future-proofing — not yet declared on the
    /// `ToolDefinition` struct but parsed via `serde_json::Value` when
    /// extra fields are present in the manifest JSON).
    Output,
}

impl fmt::Display for SchemaKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaKind::Input => f.write_str("input_schema"),
            SchemaKind::Output => f.write_str("output_schema"),
        }
    }
}

/// A single manifest schema validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestSchemaError {
    /// The tool whose schema failed.
    pub tool_name: String,
    /// Whether the failure was on `input_schema` or `output_schema`.
    pub schema_kind: SchemaKind,
    /// JSON-pointer-style path within the schema (e.g.
    /// `/anyOf/0`, `/properties/city`).
    pub path: String,
    /// Human-readable explanation.
    pub message: String,
}

impl fmt::Display for ManifestSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = if self.path.is_empty() {
            "/"
        } else {
            &self.path
        };
        write!(
            f,
            "tool '{tool}' {kind} at {path}: {msg}",
            tool = self.tool_name,
            kind = self.schema_kind,
            path = path,
            msg = self.message
        )
    }
}

/// Profile selecting how strict the validator should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationProfile {
    /// All Draft 07 sanity rules + all strict octos rules.
    Strict,
    /// Draft 07 sanity rules only; strict octos rules are skipped.
    Lenient,
    /// Validator returns Ok unconditionally.
    Off,
}

impl ValidationProfile {
    /// Resolve the profile from the `OCTOS_MANIFEST_VALIDATION`
    /// environment variable. Unknown values fall back to `Strict`
    /// (fail-closed) and a single `warn!` is emitted via `tracing`.
    pub fn from_env() -> Self {
        match env::var("OCTOS_MANIFEST_VALIDATION").ok().as_deref() {
            Some("strict") | None | Some("") => ValidationProfile::Strict,
            Some("lenient") => ValidationProfile::Lenient,
            Some("off") => ValidationProfile::Off,
            Some(other) => {
                tracing::warn!(
                    value = %other,
                    "OCTOS_MANIFEST_VALIDATION has unknown value; defaulting to 'strict'"
                );
                ValidationProfile::Strict
            }
        }
    }
}

/// Validate every tool schema on the manifest using the env-driven
/// profile. Equivalent to `validate_manifest_schemas_with(manifest,
/// ValidationProfile::from_env())`.
pub fn validate_manifest_schemas(
    manifest: &PluginManifest,
) -> Result<(), Vec<ManifestSchemaError>> {
    validate_manifest_schemas_with(manifest, ValidationProfile::from_env())
}

/// Validate every tool schema with an explicit profile. Tests use this
/// to exercise both modes without mutating the process environment.
pub fn validate_manifest_schemas_with(
    manifest: &PluginManifest,
    profile: ValidationProfile,
) -> Result<(), Vec<ManifestSchemaError>> {
    if matches!(profile, ValidationProfile::Off) {
        return Ok(());
    }
    let mut errors = Vec::new();

    for tool in &manifest.tools {
        // `input_schema` is `serde_json::Value` on `ToolDefinition`.
        // An empty object is the legacy default — we treat it as
        // "schema absent" and require a real schema.
        validate_one_schema(
            &tool.name,
            SchemaKind::Input,
            &tool.input_schema,
            profile,
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validate a single schema with the given profile. Public for the
/// `validate_manifests` CI bin to use without going through a
/// `PluginManifest`.
pub fn validate_schema(
    tool_name: &str,
    schema_kind: SchemaKind,
    schema: &Value,
    profile: ValidationProfile,
) -> Vec<ManifestSchemaError> {
    let mut errors = Vec::new();
    validate_one_schema(tool_name, schema_kind, schema, profile, &mut errors);
    errors
}

fn validate_one_schema(
    tool_name: &str,
    schema_kind: SchemaKind,
    schema: &Value,
    profile: ValidationProfile,
    errors: &mut Vec<ManifestSchemaError>,
) {
    // The schema must at least be a JSON object — Draft 07 says a
    // schema can also be `true`/`false`, but tool input schemas are
    // never one of those in practice; rejecting now gives a useful
    // pointer rather than letting the walker silently no-op.
    //
    // This check fires in every profile (including lenient) because a
    // non-object root is not a valid Draft 07 schema for the tool-input
    // role; the `Off` short-circuit higher up still bypasses it.
    if !schema.is_object() {
        errors.push(ManifestSchemaError {
            tool_name: tool_name.to_string(),
            schema_kind,
            path: String::new(),
            message: format!("schema must be a JSON object, found {}", kind_label(schema)),
        });
        return;
    }

    // Detect "explicit Draft 07 opt-in" at the root once and thread it
    // down. Only the root `$schema` counts — sub-schemas don't get to
    // opt themselves into `$ref`/etc on their own.
    let explicit_draft07 = schema
        .get("$schema")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.contains("draft-07"));

    // Layer 1: Draft 07 sanity — always run.
    walk_draft07(tool_name, schema_kind, schema, "", errors);

    // Layer 2: octos strict rules — run unless explicitly relaxed.
    if matches!(profile, ValidationProfile::Strict) {
        // The empty-schema rule belongs to the strict octos profile —
        // `{}` is a valid Draft 07 schema, so accepting it in lenient
        // mode matches the documented "Draft 07 sanity only" contract
        // (codex P2 review, 2026-05-25). Operators who genuinely want
        // an "accept anything" tool can opt out with =lenient or =off.
        if schema.as_object().is_some_and(|o| o.is_empty()) {
            errors.push(ManifestSchemaError {
                tool_name: tool_name.to_string(),
                schema_kind,
                path: String::new(),
                message: "schema is empty — tools must declare `\"type\": \"object\"` and at least an empty `properties` map".to_string(),
            });
            return;
        }
        // Root-schema-only rules.
        check_root(tool_name, schema_kind, schema, errors);
        walk_strict(tool_name, schema_kind, schema, "", explicit_draft07, errors);
    }
}

// ── Layer 1: Draft 07 sanity ─────────────────────────────────────────

fn walk_draft07(
    tool_name: &str,
    schema_kind: SchemaKind,
    schema: &Value,
    path: &str,
    errors: &mut Vec<ManifestSchemaError>,
) {
    let Some(obj) = schema.as_object() else {
        return;
    };

    // Type-level sanity for known Draft 07 keywords. We pick the ones
    // whose mis-typed declarations are easy to ship and hard to spot.
    if let Some(v) = obj.get("required") {
        if !v.is_array() {
            errors.push(err(
                tool_name,
                schema_kind,
                path,
                "`required` must be an array",
            ));
        } else {
            for (i, item) in v.as_array().unwrap().iter().enumerate() {
                if !item.is_string() {
                    errors.push(err(
                        tool_name,
                        schema_kind,
                        &join(path, &format!("required/{i}")),
                        &format!("`required[{i}]` must be a string"),
                    ));
                }
            }
        }
    }

    if let Some(v) = obj.get("enum") {
        if !v.is_array() {
            errors.push(err(tool_name, schema_kind, path, "`enum` must be an array"));
        }
    }

    if let Some(v) = obj.get("type") {
        match v {
            // String form — must be one of the Draft 07 type names.
            // Catches typos like `"strng"` that satisfy the strict
            // "has type" check but are rejected by JSON Schema and
            // provider validators (codex P2 review, 2026-05-25).
            Value::String(s) => {
                if !is_valid_draft07_type(s) {
                    errors.push(err(
                        tool_name,
                        schema_kind,
                        path,
                        &format!(
                            "`type: \"{s}\"` is not a valid JSON Schema Draft 07 type (must be one of: null, boolean, object, array, number, string, integer)"
                        ),
                    ));
                }
            }
            Value::Array(arr) => {
                for (i, item) in arr.iter().enumerate() {
                    match item.as_str() {
                        Some(s) if is_valid_draft07_type(s) => {}
                        Some(s) => errors.push(err(
                            tool_name,
                            schema_kind,
                            &join(path, &format!("type/{i}")),
                            &format!(
                                "`type[{i}]: \"{s}\"` is not a valid JSON Schema Draft 07 type"
                            ),
                        )),
                        None => errors.push(err(
                            tool_name,
                            schema_kind,
                            &join(path, &format!("type/{i}")),
                            "`type` array items must be strings",
                        )),
                    }
                }
            }
            _ => errors.push(err(
                tool_name,
                schema_kind,
                path,
                "`type` must be a string or array of strings",
            )),
        }
    }

    if let Some(v) = obj.get("properties") {
        if !v.is_object() {
            errors.push(err(
                tool_name,
                schema_kind,
                path,
                "`properties` must be an object",
            ));
        }
    }

    if let Some(v) = obj.get("items") {
        if !(v.is_object() || v.is_array()) {
            errors.push(err(
                tool_name,
                schema_kind,
                path,
                "`items` must be a schema object or array of schemas",
            ));
        }
    }

    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(v) = obj.get(combinator) {
            if !v.is_array() {
                errors.push(err(
                    tool_name,
                    schema_kind,
                    path,
                    &format!("`{combinator}` must be an array"),
                ));
            }
        }
    }

    // Recurse into every schema-bearing keyword. We use the shared
    // `for_each_subschema` helper so the Draft 07 walker, the strict
    // walker, and any future layer see the same recursion shape; if
    // somebody adds a new sub-schema-bearing keyword they only need
    // to update one place.
    for_each_subschema(obj, path, |sub, sub_path| {
        walk_draft07(tool_name, schema_kind, sub, &sub_path, errors);
    });
}

// ── Layer 2: strict octos rules ──────────────────────────────────────

fn check_root(
    tool_name: &str,
    schema_kind: SchemaKind,
    schema: &Value,
    errors: &mut Vec<ManifestSchemaError>,
) {
    let Some(obj) = schema.as_object() else {
        return;
    };
    match obj.get("type") {
        Some(Value::String(s)) if s == "object" => {}
        Some(Value::String(s)) => {
            errors.push(err(
                tool_name,
                schema_kind,
                "",
                &format!(
                    "root schema must have `type: \"object\"` (found `{s}`); tools always receive a JSON object payload"
                ),
            ));
        }
        Some(_) => {
            errors.push(err(
                tool_name,
                schema_kind,
                "",
                "root schema `type` must be the string `\"object\"`",
            ));
        }
        None => {
            errors.push(err(
                tool_name,
                schema_kind,
                "",
                "root schema must declare `type: \"object\"`",
            ));
        }
    }
}

fn walk_strict(
    tool_name: &str,
    schema_kind: SchemaKind,
    schema: &Value,
    path: &str,
    draft07_opt_in: bool,
    errors: &mut Vec<ManifestSchemaError>,
) {
    let Some(obj) = schema.as_object() else {
        return;
    };

    // `$ref` / `$dynamicRef` / `$dynamicAnchor` are rejected unless the
    // root schema declared `"$schema": "...draft-07..."`. The opt-in is
    // computed once at entry and threaded down so a nested `$ref` is
    // still allowed when the root opted in, but a manifest that never
    // declared Draft 07 cannot sneak `$ref` in via a sub-schema.
    for forbidden in ["$ref", "$dynamicRef", "$dynamicAnchor"] {
        if obj.contains_key(forbidden) && !draft07_opt_in {
            errors.push(err(
                tool_name,
                schema_kind,
                path,
                &format!(
                    "`{forbidden}` is not allowed unless the root schema sets `\"$schema\": \"http://json-schema.org/draft-07/schema#\"`"
                ),
            ));
        }
    }

    // `enum` values must be unique.
    if let Some(Value::Array(values)) = obj.get("enum") {
        let mut seen: HashSet<String> = HashSet::new();
        for (i, v) in values.iter().enumerate() {
            // We compare via canonical JSON. `serde_json::to_string`
            // is sufficient because the comparator is purely
            // structural — no float fuzzing needed for enum values.
            let key = serde_json::to_string(v).unwrap_or_default();
            if !seen.insert(key) {
                errors.push(err(
                    tool_name,
                    schema_kind,
                    &join(path, &format!("enum/{i}")),
                    &format!("`enum` values must be unique (duplicate: {v})"),
                ));
            }
        }
    }

    // Node-local checks: every `properties.X` must declare a `type`.
    if let Some(Value::Object(props)) = obj.get("properties") {
        for (k, sub) in props {
            let sub_path = join(path, &format!("properties/{k}"));
            if let Some(sub_obj) = sub.as_object() {
                let has_type_or_combinator = sub_obj.contains_key("type")
                    || sub_obj.contains_key("anyOf")
                    || sub_obj.contains_key("oneOf")
                    || sub_obj.contains_key("allOf")
                    || sub_obj.contains_key("$ref")
                    || sub_obj.contains_key("const")
                    || sub_obj.contains_key("enum");
                if !has_type_or_combinator {
                    errors.push(err(
                        tool_name,
                        schema_kind,
                        &sub_path,
                        &format!(
                            "property `{k}` must declare a `type` (or `anyOf`/`oneOf`/`allOf`/`enum`/`const`); strict provider validators reject untyped properties"
                        ),
                    ));
                }
            } else {
                errors.push(err(
                    tool_name,
                    schema_kind,
                    &sub_path,
                    &format!("property `{k}` must be a schema object"),
                ));
            }
        }
    }

    // Node-local: every `anyOf`/`oneOf`/`allOf` branch must declare a `type`.
    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(branches)) = obj.get(combinator) {
            for (i, branch) in branches.iter().enumerate() {
                let branch_path = join(path, &format!("{combinator}/{i}"));
                if let Some(b_obj) = branch.as_object() {
                    let has_type = b_obj.contains_key("type")
                        || b_obj.contains_key("$ref")
                        || b_obj.contains_key("const")
                        || b_obj.contains_key("enum");
                    if !has_type {
                        errors.push(err(
                            tool_name,
                            schema_kind,
                            &branch_path,
                            &format!(
                                "`{combinator}` branch must declare a `type` (or `enum`/`const`); strict provider validators reject untyped branches — this is today's mofa-slides v0.5.0 bug shape"
                            ),
                        ));
                    }
                }
            }
        }
    }

    // Recurse into every schema-bearing keyword. Shared between
    // `walk_draft07` and `walk_strict` so adding a new sub-schema
    // keyword (e.g. a future Draft 07 extension) updates both layers
    // automatically.
    for_each_subschema(obj, path, |sub, sub_path| {
        walk_strict(
            tool_name,
            schema_kind,
            sub,
            &sub_path,
            draft07_opt_in,
            errors,
        );
    });
}

// ── Helpers ──────────────────────────────────────────────────────────

/// The seven type names recognised by JSON Schema Draft 07.
const DRAFT07_TYPES: &[&str] = &[
    "null", "boolean", "object", "array", "number", "string", "integer",
];

/// Whether `name` is a Draft 07 primitive type. Used to catch typos
/// (e.g. `"strng"`) that would slip past the strict "has type" rule
/// but fail at provider validation time.
fn is_valid_draft07_type(name: &str) -> bool {
    DRAFT07_TYPES.contains(&name)
}

/// Invoke `visit(sub, sub_path)` for every schema-bearing sub-schema
/// of `obj`. Centralising the recursion shape means both validation
/// layers descend into the same set of keywords, and a future
/// schema-bearing keyword (e.g. an extension we adopt) only needs to
/// be added once.
///
/// Keyword coverage matches Draft 07's "applicator" vocabulary:
/// `properties`, `patternProperties`, `additionalProperties`,
/// `definitions` / `$defs`, `propertyNames`, `items`,
/// `additionalItems`, `contains`, `anyOf`/`oneOf`/`allOf`, `not`,
/// `if`/`then`/`else`, `dependencies` (schema form). The walker is
/// conservative: keywords whose value is not a schema in Draft 07
/// (e.g. `dependencies` with an array form, `enum` values) are
/// ignored. That matches what JSON Schema validators do.
fn for_each_subschema<F>(obj: &serde_json::Map<String, Value>, path: &str, mut visit: F)
where
    F: FnMut(&Value, String),
{
    // Object-valued shorthand: `properties` and `patternProperties`
    // both map to a `name -> sub-schema` table.
    for keyword in ["properties", "patternProperties", "definitions", "$defs"] {
        if let Some(Value::Object(map)) = obj.get(keyword) {
            for (k, sub) in map {
                visit(sub, join(path, &format!("{keyword}/{k}")));
            }
        }
    }

    // Array-valued: every combinator's items are sub-schemas.
    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(arr)) = obj.get(combinator) {
            for (i, sub) in arr.iter().enumerate() {
                visit(sub, join(path, &format!("{combinator}/{i}")));
            }
        }
    }

    // `items` is either a single schema or an array of schemas.
    if let Some(items) = obj.get("items") {
        match items {
            Value::Object(_) => visit(items, join(path, "items")),
            Value::Array(arr) => {
                for (i, sub) in arr.iter().enumerate() {
                    visit(sub, join(path, &format!("items/{i}")));
                }
            }
            _ => {}
        }
    }

    // Single-schema applicators. `additionalProperties` and
    // `additionalItems` can also be booleans — those aren't schemas
    // and we skip them.
    for keyword in [
        "additionalProperties",
        "additionalItems",
        "propertyNames",
        "contains",
        "not",
        "if",
        "then",
        "else",
    ] {
        if let Some(sub) = obj.get(keyword) {
            if sub.is_object() {
                visit(sub, join(path, keyword));
            }
        }
    }

    // `dependencies` in Draft 07 is either `{name: [required-list]}`
    // (skip — not a sub-schema) or `{name: sub-schema}` (recurse).
    if let Some(Value::Object(deps)) = obj.get("dependencies") {
        for (k, v) in deps {
            if v.is_object() {
                visit(v, join(path, &format!("dependencies/{k}")));
            }
        }
    }
}

fn err(tool_name: &str, schema_kind: SchemaKind, path: &str, message: &str) -> ManifestSchemaError {
    ManifestSchemaError {
        tool_name: tool_name.to_string(),
        schema_kind,
        path: path.to_string(),
        message: message.to_string(),
    }
}

fn join(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        format!("/{child}")
    } else {
        format!("{parent}/{child}")
    }
}

fn kind_label(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the strict validator.
    //!
    //! These tests intentionally do **not** mutate `OCTOS_MANIFEST_VALIDATION`.
    //! Cargo runs tests in parallel threads inside one process; an env-var
    //! mutation in one test leaks into siblings and yields flakey runs.
    //! Each test passes an explicit [`ValidationProfile`] instead — that's the
    //! same code path the env var would have hit.

    use super::*;
    use crate::manifest::PluginManifest;
    use serde_json::json;

    fn schema_of(json: serde_json::Value) -> Value {
        json
    }

    /// Reproduces the 2026-05-25 mofa-slides v0.5.0 incident: every
    /// `anyOf` branch is bare `{required:[…]}` with no `type`. Strict
    /// must reject this so the agent never asks the provider with a
    /// schema that will be refused mid-request.
    #[test]
    fn anyof_branch_missing_type_rejected() {
        let schema = schema_of(json!({
            "type": "object",
            "anyOf": [
                { "required": ["slides"] },
                { "required": ["input"] }
            ]
        }));
        let errors = validate_schema(
            "mofa_slides",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path.starts_with("/anyOf/0") && e.message.contains("type")),
            "expected anyOf/0 type-missing error, got {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path.starts_with("/anyOf/1") && e.message.contains("type")),
            "expected anyOf/1 type-missing error, got {errors:?}"
        );
    }

    /// `properties.foo = {description: ""}` is the second-most common
    /// shape strict provider validators reject — they want a `type`.
    #[test]
    fn properties_field_missing_type_rejected() {
        let schema = schema_of(json!({
            "type": "object",
            "properties": {
                "foo": { "description": "no type" }
            }
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert_eq!(errors.len(), 1, "errors = {errors:?}");
        assert_eq!(errors[0].path, "/properties/foo");
        assert!(errors[0].message.contains("must declare a `type`"));
    }

    /// `$ref` is rejected unless the schema explicitly opts in to
    /// Draft 07 at the root. This is RFC-2's lock-down on resolver
    /// surface area — every provider parses `$ref` differently.
    #[test]
    fn dollar_ref_rejected_without_explicit_allow() {
        let schema = schema_of(json!({
            "type": "object",
            "properties": {
                "x": { "$ref": "#/definitions/X" }
            }
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path == "/properties/x" && e.message.contains("$ref")),
            "expected $ref rejection, got {errors:?}"
        );
    }

    /// Same schema becomes acceptable (for the `$ref` rule) once the
    /// root opts into Draft 07. The Draft 07 sanity layer still
    /// produces no errors for this shape.
    #[test]
    fn dollar_ref_allowed_with_explicit_draft07() {
        let schema = schema_of(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "x": { "$ref": "#/definitions/X", "type": "string" }
            }
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        // Property `x` now has both `$ref` and `type` — the explicit
        // Draft 07 opt-in disables the $ref rejection, and the `type`
        // satisfies the property-must-have-type rule.
        assert!(
            errors.is_empty(),
            "explicit draft-07 opt-in should clear $ref rejection, got {errors:?}"
        );
    }

    /// `enum` duplicates are detected; same canonical JSON form counts.
    #[test]
    fn non_unique_enum_rejected() {
        let schema = schema_of(json!({
            "type": "object",
            "properties": {
                "color": { "type": "string", "enum": ["red", "blue", "red"] }
            }
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert!(
            errors.iter().any(|e| e.message.contains("unique")),
            "expected duplicate-enum error, got {errors:?}"
        );
    }

    /// The root MUST be `type: "object"` — every tool receives a JSON
    /// object payload, and accepting `type: "array"` at the root would
    /// confuse every provider on the wire.
    #[test]
    fn root_schema_must_be_object_type() {
        let schema = schema_of(json!({
            "type": "string"
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path.is_empty() && e.message.contains("object")),
            "expected root-must-be-object error, got {errors:?}"
        );
    }

    /// The root MUST be `type: "object"` — root with no type at all
    /// also fails, because untyped roots are exactly what shipped in
    /// the mofa-slides incident.
    #[test]
    fn root_schema_with_no_type_rejected() {
        let schema = schema_of(json!({
            "properties": { "x": { "type": "string" } }
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path.is_empty() && e.message.contains("must declare")),
            "expected root-no-type error, got {errors:?}"
        );
    }

    /// Sanity test: a known-good bundled manifest passes strict mode.
    /// We use the weather skill — clean `type: "object"` root, every
    /// property has a `type`, `required` is well-formed. If this
    /// regresses we've introduced a false positive.
    #[test]
    fn valid_manifest_accepted() {
        let raw = include_str!("../../app-skills/weather/manifest.json");
        let manifest = PluginManifest::from_json(raw).expect("bundled weather manifest must parse");
        assert!(
            validate_manifest_schemas_with(&manifest, ValidationProfile::Strict).is_ok(),
            "bundled weather manifest should pass strict validation"
        );
    }

    /// Lenient profile must accept the strict-failing mofa-slides
    /// shape so operators can unblock prod immediately by setting
    /// `OCTOS_MANIFEST_VALIDATION=lenient`.
    #[test]
    fn lenient_profile_accepts_bare_anyof_branch() {
        let schema = schema_of(json!({
            "type": "object",
            "anyOf": [
                { "required": ["slides"] },
                { "required": ["input"] }
            ]
        }));
        let errors = validate_schema(
            "mofa_slides",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Lenient,
        );
        assert!(
            errors.is_empty(),
            "lenient should accept bare anyOf branches, got {errors:?}"
        );
    }

    /// `off` profile is a panic-button bypass: even Draft 07 sanity
    /// is skipped (e.g. ship-now incident response).
    #[test]
    fn off_profile_accepts_everything() {
        let schema = schema_of(json!({
            "type": "string",
            "required": "not-an-array",
            "properties": "not-an-object"
        }));
        let errors = validate_schema("tool_x", SchemaKind::Input, &schema, ValidationProfile::Off);
        // `validate_schema` runs `validate_one_schema` regardless, so
        // we expect the layer-1 errors to surface here. The full
        // entrypoint `validate_manifest_schemas_with` honours `Off`
        // and returns Ok unconditionally — exercise that separately.
        assert!(
            !errors.is_empty(),
            "validate_schema runs layer 1 regardless of profile"
        );

        let tool = crate::manifest::ToolDefinition {
            name: "tool_x".into(),
            description: "x".into(),
            input_schema: schema,
            entrypoint: None,
            concurrency_class: None,
        };
        let manifest = PluginManifest {
            id: "p".into(),
            version: "0".into(),
            plugin_type: None,
            description: None,
            author: None,
            homepage: None,
            license: None,
            binary: None,
            timeout_secs: None,
            tools: vec![tool],
            hooks: vec![],
            requires: None,
            config_schema: None,
            install: vec![],
            requires_network: None,
            hardware_lifecycle: None,
        };
        assert!(
            validate_manifest_schemas_with(&manifest, ValidationProfile::Off).is_ok(),
            "Off profile must short-circuit to Ok"
        );
    }

    /// Layer 1 catches non-array `required` regardless of profile.
    /// This is a Draft 07 sanity rule that mirrors the meta-schema.
    #[test]
    fn draft07_required_must_be_array() {
        let schema = schema_of(json!({
            "type": "object",
            "required": "city"  // should be ["city"]
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Lenient,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("required") && e.message.contains("array")),
            "lenient must still catch non-array `required`, got {errors:?}"
        );
    }

    /// Empty `input_schema: {}` (the legacy default) is rejected with
    /// a clear error rather than silently passing. Without this the
    /// validator would accept manifests that effectively declare "no
    /// parameters" while their tool expects them.
    #[test]
    fn empty_input_schema_rejected() {
        let schema = schema_of(json!({}));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("empty"));
    }

    /// The path encoded in errors must point at the offending field
    /// so authors don't have to grep the manifest by hand.
    #[test]
    fn error_path_points_at_offending_field() {
        let schema = schema_of(json!({
            "type": "object",
            "properties": {
                "nested": {
                    "type": "object",
                    "anyOf": [
                        { "required": ["a"] }
                    ]
                }
            }
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path == "/properties/nested/anyOf/0"),
            "expected /properties/nested/anyOf/0 path, got {errors:?}"
        );
    }

    // ── Codex P2 review follow-ups (2026-05-25) ─────────────────────

    /// Codex P2 #2: `{}` is a valid Draft 07 schema, so lenient mode
    /// must accept it. The empty-schema rule is part of the strict
    /// octos profile only.
    #[test]
    fn lenient_profile_accepts_empty_schema() {
        let schema = schema_of(json!({}));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Lenient,
        );
        assert!(
            errors.is_empty(),
            "lenient must accept empty `{{}}` (valid Draft 07), got {errors:?}"
        );
    }

    /// Codex P2 #2 (continued): strict still rejects empty `{}`.
    #[test]
    fn strict_profile_still_rejects_empty_schema() {
        let schema = schema_of(json!({}));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert!(
            errors.iter().any(|e| e.message.contains("empty")),
            "strict must reject empty `{{}}`, got {errors:?}"
        );
    }

    /// Codex P2 #3: `{"type": "strng"}` satisfied the strict "has
    /// type" check but is rejected by Draft 07 + provider validators.
    /// The Draft 07 sanity pass now catches this typo.
    #[test]
    fn invalid_draft07_type_name_rejected() {
        let schema = schema_of(json!({
            "type": "object",
            "properties": {
                "field": { "type": "strng" }  // typo for "string"
            }
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Lenient,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path == "/properties/field" && e.message.contains("\"strng\"")),
            "expected invalid-type rejection, got {errors:?}"
        );
    }

    /// Codex P2 #3 (continued): array-form `type` with a bogus entry
    /// is also caught.
    #[test]
    fn invalid_draft07_type_name_in_array_rejected() {
        let schema = schema_of(json!({
            "type": "object",
            "properties": {
                "field": { "type": ["string", "bogus"] }
            }
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Lenient,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path == "/properties/field/type/1" && e.message.contains("\"bogus\"")),
            "expected invalid-type-in-array rejection, got {errors:?}"
        );
    }

    /// Codex P2 #4: a bare-`anyOf`-branch shape hidden under
    /// `additionalProperties` must be caught. Without the recursion
    /// fix this would silently pass.
    #[test]
    fn additional_properties_recursion_catches_invalid_anyof() {
        let schema = schema_of(json!({
            "type": "object",
            "additionalProperties": {
                "type": "object",
                "anyOf": [
                    { "required": ["a"] },
                    { "required": ["b"] }
                ]
            }
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path.starts_with("/additionalProperties/anyOf/")),
            "expected anyOf-under-additionalProperties rejection, got {errors:?}"
        );
    }

    /// Codex P2 #4 (continued): `$defs` / `definitions` schemas also
    /// recurse so a malformed shared definition can't sneak past.
    #[test]
    fn defs_recursion_catches_invalid_anyof() {
        let schema = schema_of(json!({
            "type": "object",
            "$defs": {
                "Inner": {
                    "type": "object",
                    "anyOf": [
                        { "required": ["x"] }
                    ]
                }
            }
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.path.starts_with("/$defs/Inner/anyOf/")),
            "expected anyOf-under-$defs rejection, got {errors:?}"
        );
    }

    /// Codex P2 #4 (continued): `if`/`then`/`else` are sub-schemas in
    /// Draft 07 and must recurse so the same bug class doesn't hide
    /// inside conditional branches.
    #[test]
    fn if_then_else_recursion_catches_invalid_anyof() {
        let schema = schema_of(json!({
            "type": "object",
            "if": {
                "type": "object",
                "anyOf": [
                    { "required": ["mode"] }
                ]
            },
            "then": { "type": "object" }
        }));
        let errors = validate_schema(
            "tool_x",
            SchemaKind::Input,
            &schema,
            ValidationProfile::Strict,
        );
        assert!(
            errors.iter().any(|e| e.path.starts_with("/if/anyOf/")),
            "expected anyOf-under-if rejection, got {errors:?}"
        );
    }
}
