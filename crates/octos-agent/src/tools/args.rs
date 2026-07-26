//! Structured, model-facing tool-argument validation (#1770).
//!
//! When a tool call fails to deserialize, the model used to see a terse
//! serde message ("missing field `path`") that stops at the FIRST
//! problem and never explains unknown/misspelled parameters (serde
//! ignores them unless the input struct opts into
//! `#[serde(deny_unknown_fields)]`). The model then either retries the
//! identical call (doom loop, #1765) or hallucinates a fix.
//!
//! [`parse_tool_args`] keeps serde as the source of truth for parsing
//! but, on failure, walks the raw JSON against the tool's own
//! `input_schema()` and reports EVERY problem in one model-facing
//! message:
//!
//! ```text
//! Invalid arguments for tool 'read_file':
//! - path: missing required parameter
//! - file_path: unknown parameter (did you mean 'path'?)
//! - start_line: expected integer, got string
//! Fix the arguments and call the tool again.
//! ```
//!
//! The error is a [`super::ToolInputError`] wrapped in `eyre`, so the
//! existing plumbing applies unchanged: the text reaches the model
//! verbatim via the `Error: {e}` tool result, and the #1690 marker
//! keeps a malformed call from cascade-cancelling well-formed siblings
//! in a serial batch.

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::ToolInputError;

/// One problem found while validating tool arguments. Mirrors the
/// opencode `InvalidArgumentsError.issues` shape
/// (`packages/opencode/src/tool/tool.ts:24-37`): a parameter path, a
/// human-readable message, and an optional "did you mean" suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    /// Parameter the issue is about (e.g. `path`, `start_line`), or
    /// `(input)` for issues with the argument object itself.
    pub path: String,
    /// What is wrong (e.g. "missing required parameter").
    pub message: String,
    /// Optional correction hint (e.g. "did you mean 'path'?").
    pub suggestion: Option<String>,
}

impl ValidationIssue {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
            suggestion: None,
        }
    }

    fn with_suggestion(mut self, suggestion: Option<String>) -> Self {
        self.suggestion = suggestion;
        self
    }
}

/// Parse tool arguments via serde, producing a structured, model-facing
/// error on failure.
///
/// On success this is exactly `serde_json::from_value`. On failure it
/// collects ALL issues (missing required parameters, unknown parameters
/// with did-you-mean suggestions, shallow type mismatches) by
/// validating `args` against the tool's JSON `schema` — the same object
/// the LLM was shown — and returns a [`ToolInputError`] whose `Display`
/// lists each one. When the schema walk finds nothing (e.g. a
/// constraint the shallow walk cannot see), the serde error text is
/// surfaced instead so the model still learns something actionable.
pub fn parse_tool_args<T: DeserializeOwned>(
    tool_name: &str,
    schema: &Value,
    args: &Value,
) -> eyre::Result<T> {
    match serde_json::from_value::<T>(args.clone()) {
        Ok(input) => Ok(input),
        Err(serde_err) => {
            let mut issues = validate_args_against_schema(schema, args);
            if issues.is_empty() {
                issues.push(ValidationIssue::new("(input)", serde_err.to_string()));
            }
            // Structured log for debugging/analytics (#1770 acceptance):
            // the typed issue list, not just the flattened prose.
            tracing::warn!(
                tool = tool_name,
                issues = ?issues,
                "tool argument validation failed; returning model-facing issue list"
            );
            Err(eyre::Report::new(ToolInputError::new(
                invalid_args_message(tool_name, &issues),
            )))
        }
    }
}

/// Format the collected issues as the model-facing message.
pub fn invalid_args_message(tool_name: &str, issues: &[ValidationIssue]) -> String {
    let mut msg = format!("Invalid arguments for tool '{tool_name}':\n");
    for issue in issues {
        msg.push_str(&format!("- {}: {}", issue.path, issue.message));
        if let Some(suggestion) = &issue.suggestion {
            msg.push_str(&format!(" ({suggestion})"));
        }
        msg.push('\n');
    }
    msg.push_str("Fix the arguments and call the tool again.");
    msg
}

/// Validate `args` against a tool's JSON schema, collecting every issue
/// instead of failing on the first (opencode-style, #1770).
///
/// Checks performed (shallow, top-level object only — the depth our
/// tool schemas use):
/// - the argument value is a JSON object;
/// - every `required` property is present (with a did-you-mean pointing
///   at a stray near-miss key when one exists);
/// - every supplied key is a known property (with a did-you-mean
///   against the schema's property names);
/// - each supplied non-null value matches the property's declared
///   `type` (null is skipped: optional fields deserialize from null).
pub fn validate_args_against_schema(schema: &Value, args: &Value) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return issues;
    };
    let known: Vec<&str> = properties.keys().map(String::as_str).collect();

    let Some(args_obj) = args.as_object() else {
        issues.push(ValidationIssue::new(
            "(input)",
            format!(
                "expected a JSON object with the tool's parameters, got {}",
                json_type_name(args)
            ),
        ));
        return issues;
    };

    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|entries| entries.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    // Missing required parameters. If the model supplied a near-miss
    // key instead (`file_path` for `path`), point at it.
    for name in &required {
        if !args_obj.contains_key(*name) {
            let stray_near_miss = args_obj
                .keys()
                .filter(|key| !known.contains(&key.as_str()))
                .find(|key| is_near_miss(name, key))
                .map(|key| format!("did you supply it as '{key}'?"));
            issues.push(
                ValidationIssue::new(*name, "missing required parameter")
                    .with_suggestion(stray_near_miss),
            );
        }
    }

    // Unknown parameters, with did-you-mean against the known names.
    for key in args_obj.keys() {
        if !known.contains(&key.as_str()) {
            let suggestion = did_you_mean(key, &known);
            issues.push(
                ValidationIssue::new(key.clone(), "unknown parameter").with_suggestion(suggestion),
            );
        }
    }

    // Shallow type check for known, non-null values.
    for (key, value) in args_obj {
        if value.is_null() {
            continue;
        }
        let Some(expected) = properties.get(key).and_then(|prop| prop.get("type")) else {
            continue;
        };
        if !value_matches_schema_type(value, expected) {
            issues.push(ValidationIssue::new(
                key.clone(),
                format!(
                    "expected {}, got {}",
                    schema_type_label(expected),
                    json_type_name(value)
                ),
            ));
        }
    }

    issues
}

/// Best did-you-mean candidate among `known` parameter names.
pub fn did_you_mean(unknown: &str, known: &[&str]) -> Option<String> {
    known
        .iter()
        .filter(|candidate| is_near_miss(candidate, unknown))
        .min_by_key(|candidate| levenshtein(&candidate.to_lowercase(), &unknown.to_lowercase()))
        .map(|candidate| format!("did you mean '{candidate}'?"))
}

/// Whether `candidate` is close enough to `name` to suggest. Accepts
/// case/underscore variants (`startLine` → `start_line`), affixed
/// variants (`file_path` → `path`), and small typos (edit distance
/// scaled by length, minimum 2).
fn is_near_miss(name: &str, candidate: &str) -> bool {
    let a = name.to_lowercase().replace(['_', '-'], "");
    let b = candidate.to_lowercase().replace(['_', '-'], "");
    if a == b {
        return true;
    }
    // Affixed variant of the same word (`filepath` vs `path`): a common
    // LLM confusion between tools with different parameter spellings.
    if a.len() >= 3
        && b.len() >= 3
        && (a.starts_with(&b) || a.ends_with(&b) || b.starts_with(&a) || b.ends_with(&a))
    {
        return true;
    }
    let max_distance = (name.len().max(candidate.len()) / 3).max(2);
    levenshtein(&name.to_lowercase(), &candidate.to_lowercase()) <= max_distance
}

/// Classic dynamic-programming edit distance (single row). Inputs are
/// parameter names — short ASCII-ish identifiers — so O(a*b) is fine.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut previous_diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous_diagonal + usize::from(ca != cb);
            previous_diagonal = row[j + 1];
            row[j + 1] = substitution.min(row[j] + 1).min(previous_diagonal + 1);
        }
    }
    row[b.len()]
}

/// Human-readable JSON type of a value.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Whether a value satisfies a JSON-schema `type` declaration (a single
/// type string, or an array of alternatives like `["string", "null"]`).
fn value_matches_schema_type(value: &Value, expected: &Value) -> bool {
    match expected {
        Value::String(type_name) => match type_name.as_str() {
            "string" => value.is_string(),
            "integer" => value.as_number().is_some_and(|n| n.is_i64() || n.is_u64()),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            "null" => value.is_null(),
            // Unknown type keyword: don't second-guess serde.
            _ => true,
        },
        Value::Array(alternatives) => alternatives
            .iter()
            .any(|alternative| value_matches_schema_type(value, alternative)),
        // Malformed schema: skip the check rather than false-positive.
        _ => true,
    }
}

/// Label for the expected type in the issue message.
fn schema_type_label(expected: &Value) -> String {
    match expected {
        Value::String(type_name) => type_name.clone(),
        Value::Array(alternatives) => alternatives
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "the declared type".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DemoInput {
        path: String,
        #[serde(default)]
        start_line: Option<usize>,
    }

    fn demo_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": { "type": "integer" }
            },
            "required": ["path"]
        })
    }

    #[test]
    fn should_parse_valid_args_unchanged() {
        let input: DemoInput = parse_tool_args(
            "demo",
            &demo_schema(),
            &json!({"path": "a.txt", "start_line": 3}),
        )
        .expect("valid args parse");
        assert_eq!(input.path, "a.txt");
        assert_eq!(input.start_line, Some(3));
    }

    #[test]
    fn should_collect_all_issues_not_just_first() {
        // Missing required + unknown key + type mismatch in ONE report.
        let issues = validate_args_against_schema(
            &demo_schema(),
            &json!({"file_path": "a.txt", "start_line": "abc"}),
        );
        let paths: Vec<&str> = issues.iter().map(|issue| issue.path.as_str()).collect();
        assert!(paths.contains(&"path"), "missing required: {issues:?}");
        assert!(paths.contains(&"file_path"), "unknown key: {issues:?}");
        assert!(
            issues.iter().any(|issue| issue.path == "start_line"
                && issue.message.contains("expected integer, got string")),
            "type mismatch: {issues:?}"
        );
        assert_eq!(issues.len(), 3, "exactly the three issues: {issues:?}");
    }

    #[test]
    fn should_suggest_near_miss_for_unknown_parameter() {
        let issues =
            validate_args_against_schema(&demo_schema(), &json!({"path": "a.txt", "startline": 2}));
        let unknown = issues
            .iter()
            .find(|issue| issue.path == "startline")
            .expect("unknown param issue");
        assert_eq!(
            unknown.suggestion.as_deref(),
            Some("did you mean 'start_line'?")
        );
    }

    #[test]
    fn should_suggest_camel_case_variant() {
        // `filePath` → `path`? No: near-miss must map camelCase to the
        // snake_case KNOWN name when the letters match.
        let issues =
            validate_args_against_schema(&demo_schema(), &json!({"path": "x", "startLine": 2}));
        let unknown = issues
            .iter()
            .find(|issue| issue.path == "startLine")
            .expect("unknown param issue");
        assert_eq!(
            unknown.suggestion.as_deref(),
            Some("did you mean 'start_line'?")
        );
    }

    #[test]
    fn should_report_non_object_input() {
        let issues = validate_args_against_schema(&demo_schema(), &json!("just a string"));
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("expected a JSON object"));
        assert!(issues[0].message.contains("got string"));
    }

    #[test]
    fn should_skip_type_check_for_null_optionals() {
        // Optional fields deserialize from null; the walk must not
        // report a mismatch serde would accept.
        let issues = validate_args_against_schema(
            &demo_schema(),
            &json!({"path": "a.txt", "start_line": null}),
        );
        assert!(issues.is_empty(), "null optional is fine: {issues:?}");
    }

    #[test]
    fn should_accept_type_alternatives() {
        let schema = json!({
            "type": "object",
            "properties": { "limit": { "type": ["integer", "null"] } },
            "required": []
        });
        assert!(validate_args_against_schema(&schema, &json!({"limit": 5})).is_empty());
        assert!(!validate_args_against_schema(&schema, &json!({"limit": "5"})).is_empty());
    }

    #[test]
    fn should_wrap_error_as_tool_input_error_with_full_message() {
        let err = parse_tool_args::<DemoInput>("demo", &demo_schema(), &json!({"pth": "a.txt"}))
            .expect_err("must fail");
        assert!(
            err.chain().any(|src| src.is::<ToolInputError>()),
            "carries the #1690 non-cascading marker"
        );
        let msg = format!("{err}");
        assert!(msg.contains("Invalid arguments for tool 'demo'"), "{msg}");
        assert!(msg.contains("- path: missing required parameter"), "{msg}");
        assert!(msg.contains("did you supply it as 'pth'?"), "{msg}");
        assert!(
            msg.contains("- pth: unknown parameter (did you mean 'path'?)"),
            "{msg}"
        );
        assert!(
            msg.contains("Fix the arguments and call the tool again."),
            "{msg}"
        );
    }

    #[test]
    fn should_fall_back_to_serde_error_when_schema_walk_finds_nothing() {
        // A constraint the shallow walk cannot see: negative value for
        // usize. Schema type is integer (matches), serde still fails.
        let err = parse_tool_args::<DemoInput>(
            "demo",
            &demo_schema(),
            &json!({"path": "a.txt", "start_line": -4}),
        )
        .expect_err("must fail");
        let msg = format!("{err}");
        assert!(msg.contains("Invalid arguments for tool 'demo'"), "{msg}");
        assert!(msg.contains("(input)"), "falls back to serde text: {msg}");
        assert!(
            msg.contains("invalid value"),
            "serde detail retained: {msg}"
        );
    }

    #[test]
    fn should_not_suggest_for_totally_unrelated_name() {
        let issues =
            validate_args_against_schema(&demo_schema(), &json!({"path": "a.txt", "zzzzqqqq": 1}));
        let unknown = issues
            .iter()
            .find(|issue| issue.path == "zzzzqqqq")
            .expect("unknown param issue");
        assert_eq!(unknown.suggestion, None, "no bogus suggestion");
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("path", "pth"), 1);
    }
}
