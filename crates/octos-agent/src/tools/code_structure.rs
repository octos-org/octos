//! Code structure analysis tool using tree-sitter for AST parsing.
//!
//! Extracts functions, structs/classes, imports, and constants from source files.
//! Supports Rust, Python, JavaScript, and TypeScript.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{Tool, ToolResult};

/// Tool that extracts code structure (symbols) from source files via AST parsing.
pub struct CodeStructureTool {
    working_dir: PathBuf,
}

impl CodeStructureTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: cwd.into(),
        }
    }
}

#[derive(Deserialize)]
struct CodeStructureArgs {
    path: String,
    #[serde(default)]
    language: Option<String>,
}

#[async_trait]
impl Tool for CodeStructureTool {
    fn name(&self) -> &str {
        "code_structure"
    }

    fn description(&self) -> &str {
        "Analyze code structure: extract functions, structs/classes, imports, and constants from a source file using AST parsing. Supports Rust, Python, JavaScript, TypeScript."
    }

    fn tags(&self) -> &[&str] {
        &["code", "search"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the source file (relative to working directory)"
                },
                "language": {
                    "type": "string",
                    "enum": ["rust", "python", "javascript", "typescript"],
                    "description": "Language (auto-detected from extension if omitted)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let args: CodeStructureArgs = serde_json::from_value(args.clone())
            .map_err(|e| eyre::eyre!("invalid arguments: {e}"))?;

        let resolved = super::resolve_path(&self.working_dir, &args.path)?;

        // Reject files too large for parsing (1 MB limit)
        const MAX_PARSE_SIZE: u64 = 1_048_576;
        let meta = tokio::fs::metadata(&resolved)
            .await
            .map_err(|e| eyre::eyre!("failed to stat file '{}': {e}", args.path))?;
        if meta.len() > MAX_PARSE_SIZE {
            return Ok(ToolResult {
                output: format!(
                    "file too large for parsing: {} bytes (max 1 MB)",
                    meta.len()
                ),
                success: false,
                ..Default::default()
            });
        }

        // Read file (O_NOFOLLOW atomically rejects symlinks)
        let content = match super::read_no_follow(&resolved).await {
            Ok(c) => c,
            Err(e) => return Ok(super::file_io_error(e, &args.path)),
        };

        let lang = args
            .language
            .as_deref()
            .or_else(|| detect_language(&args.path))
            .ok_or_else(|| {
                eyre::eyre!(
                    "cannot detect language for '{}'. Specify 'language' explicitly.",
                    args.path
                )
            })?
            .to_string();

        match parse_structure(&content, &lang) {
            Ok(structure) => Ok(ToolResult {
                output: serde_json::to_string_pretty(&structure)?,
                success: true,
                ..Default::default()
            }),
            Err(e) => Ok(ToolResult {
                output: format!("parse error: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

/// Detect language from file extension.
fn detect_language(path: &str) -> Option<&str> {
    let ext = path.rsplit('.').next()?;
    match ext {
        "rs" => Some("rust"),
        "py" | "pyi" => Some("python"),
        "js" | "jsx" | "mjs" => Some("javascript"),
        "ts" => Some("typescript"),
        "tsx" => Some("tsx"),
        _ => None,
    }
}

/// Parse source code and extract structural information.
fn parse_structure(source: &str, language: &str) -> Result<serde_json::Value> {
    let mut parser = tree_sitter::Parser::new();

    let ts_language = match language {
        "rust" => tree_sitter_rust::LANGUAGE,
        "python" => tree_sitter_python::LANGUAGE,
        "javascript" => tree_sitter_javascript::LANGUAGE,
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX,
        other => eyre::bail!("unsupported language: {other}"),
    };

    parser
        .set_language(&ts_language.into())
        .map_err(|e| eyre::eyre!("failed to set language: {e}"))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| eyre::eyre!("failed to parse source"))?;

    let root = tree.root_node();
    let bytes = source.as_bytes();

    let mut functions = Vec::new();
    let mut structs = Vec::new();
    let mut imports = Vec::new();
    let mut classes = Vec::new();
    let mut constants = Vec::new();

    collect_symbols(
        root,
        bytes,
        language,
        &mut functions,
        &mut structs,
        &mut imports,
        &mut classes,
        &mut constants,
    );

    Ok(serde_json::json!({
        "language": language,
        "functions": functions,
        "structs": structs,
        "imports": imports,
        "classes": classes,
        "constants": constants,
    }))
}

#[allow(clippy::too_many_arguments)]
fn collect_symbols(
    node: tree_sitter::Node,
    source: &[u8],
    language: &str,
    functions: &mut Vec<serde_json::Value>,
    structs: &mut Vec<serde_json::Value>,
    imports: &mut Vec<serde_json::Value>,
    classes: &mut Vec<serde_json::Value>,
    constants: &mut Vec<serde_json::Value>,
) {
    let kind = node.kind();
    let line = node.start_position().row + 1;

    match language {
        "rust" => match kind {
            "function_item" | "function_signature_item" => {
                if let Some(name) = child_text(&node, "name", source) {
                    let params = child_text(&node, "parameters", source).unwrap_or_default();
                    let ret = child_by_kind(&node, "type")
                        .and_then(|n| node_text(&n, source))
                        .unwrap_or_default();
                    functions.push(serde_json::json!({
                        "name": name, "line": line, "params": params, "return": ret
                    }));
                }
            }
            "struct_item" => {
                if let Some(name) = child_text(&node, "name", source) {
                    let fields = collect_field_names(&node, source);
                    structs.push(serde_json::json!({
                        "name": name, "line": line, "fields": fields
                    }));
                }
            }
            "enum_item" => {
                if let Some(name) = child_text(&node, "name", source) {
                    structs.push(serde_json::json!({
                        "name": name, "line": line, "kind": "enum"
                    }));
                }
            }
            "use_declaration" => {
                if let Some(text) = node_text(&node, source) {
                    imports.push(serde_json::json!(text));
                }
            }
            "const_item" | "static_item" => {
                if let Some(name) = child_text(&node, "name", source) {
                    constants.push(serde_json::json!({"name": name, "line": line}));
                }
            }
            "impl_item" => {
                if let Some(name) =
                    child_by_kind(&node, "type_identifier").and_then(|n| node_text(&n, source))
                {
                    structs.push(serde_json::json!({
                        "name": name, "line": line, "kind": "impl"
                    }));
                }
            }
            _ => {}
        },
        "python" => match kind {
            "function_definition" => {
                if let Some(name) = child_text(&node, "name", source) {
                    let params = child_text(&node, "parameters", source).unwrap_or_default();
                    functions.push(serde_json::json!({
                        "name": name, "line": line, "params": params
                    }));
                }
            }
            "class_definition" => {
                if let Some(name) = child_text(&node, "name", source) {
                    classes.push(serde_json::json!({
                        "name": name, "line": line
                    }));
                }
            }
            "import_statement" | "import_from_statement" => {
                if let Some(text) = node_text(&node, source) {
                    imports.push(serde_json::json!(text));
                }
            }
            _ => {}
        },
        "javascript" | "typescript" | "tsx" => match kind {
            "function_declaration" | "method_definition" | "arrow_function" => {
                // Arrow function name extraction: handles `const f = () => {}`
                // via variable_declarator parent. Does not cover property
                // assignments like `obj.method = () => {}`.
                let name = child_text(&node, "name", source)
                    .or_else(|| {
                        node.parent()
                            .filter(|p| p.kind() == "variable_declarator")
                            .and_then(|p| child_text(&p, "name", source))
                    })
                    .unwrap_or_else(|| "<anonymous>".to_string());
                let params = child_text(&node, "parameters", source)
                    .or_else(|| child_text(&node, "formal_parameters", source))
                    .unwrap_or_default();
                functions.push(serde_json::json!({
                    "name": name, "line": line, "params": params
                }));
            }
            "class_declaration" => {
                if let Some(name) = child_text(&node, "name", source) {
                    classes.push(serde_json::json!({
                        "name": name, "line": line
                    }));
                }
            }
            // TypeScript-specific: interfaces, type aliases, enums
            "interface_declaration" => {
                if let Some(name) = child_text(&node, "name", source) {
                    structs.push(serde_json::json!({
                        "name": name, "line": line, "kind": "interface"
                    }));
                }
            }
            "type_alias_declaration" => {
                if let Some(name) = child_text(&node, "name", source) {
                    structs.push(serde_json::json!({
                        "name": name, "line": line, "kind": "type"
                    }));
                }
            }
            "enum_declaration" => {
                if let Some(name) = child_text(&node, "name", source) {
                    structs.push(serde_json::json!({
                        "name": name, "line": line, "kind": "enum"
                    }));
                }
            }
            "import_statement" => {
                if let Some(text) = node_text(&node, source) {
                    imports.push(serde_json::json!(text));
                }
            }
            _ => {}
        },
        _ => {}
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols(
            child, source, language, functions, structs, imports, classes, constants,
        );
    }
}

/// Get text of a named child field.
fn child_text(node: &tree_sitter::Node, field: &str, source: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(source).ok().map(String::from))
}

/// Get text of a node.
fn node_text(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    node.utf8_text(source).ok().map(String::from)
}

/// Find first child with given kind.
fn child_by_kind<'a>(node: &tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|c| c.kind() == kind)
}

/// Collect field names from a struct body.
fn collect_field_names(node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "field_declaration_list" {
            let mut inner = child.walk();
            for field_node in child.children(&mut inner) {
                if field_node.kind() == "field_declaration" {
                    if let Some(name) = child_text(&field_node, "name", source) {
                        fields.push(name);
                    }
                }
            }
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language("src/main.rs"), Some("rust"));
        assert_eq!(detect_language("script.py"), Some("python"));
        assert_eq!(detect_language("app.js"), Some("javascript"));
        assert_eq!(detect_language("app.ts"), Some("typescript"));
        assert_eq!(detect_language("app.tsx"), Some("tsx"));
        assert_eq!(detect_language("data.json"), None);
    }

    #[test]
    fn test_parse_rust_functions_and_structs() {
        let source = r#"
use std::io;

const MAX: usize = 100;

pub struct Point {
    x: f64,
    y: f64,
}

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn helper() {
    // ...
}
"#;
        let result = parse_structure(source, "rust").unwrap();
        let functions = result["functions"].as_array().unwrap();
        assert_eq!(functions.len(), 2);
        assert_eq!(functions[0]["name"], "add");
        assert_eq!(functions[1]["name"], "helper");

        let structs = result["structs"].as_array().unwrap();
        assert!(structs.iter().any(|s| s["name"] == "Point"));

        let imports = result["imports"].as_array().unwrap();
        assert!(!imports.is_empty());

        let constants = result["constants"].as_array().unwrap();
        assert!(constants.iter().any(|c| c["name"] == "MAX"));
    }

    #[test]
    fn test_parse_python_classes_and_functions() {
        let source = r#"
import os
from pathlib import Path

class MyClass:
    def method(self):
        pass

def standalone(x, y):
    return x + y
"#;
        let result = parse_structure(source, "python").unwrap();
        let functions = result["functions"].as_array().unwrap();
        assert!(functions.iter().any(|f| f["name"] == "standalone"));
        assert!(functions.iter().any(|f| f["name"] == "method"));

        let classes = result["classes"].as_array().unwrap();
        assert!(classes.iter().any(|c| c["name"] == "MyClass"));

        let imports = result["imports"].as_array().unwrap();
        assert!(imports.len() >= 2);
    }

    #[test]
    fn test_parse_javascript_functions() {
        let source = r#"
import { foo } from './bar';

class Widget {
    render() {}
}

function hello(name) {
    return `Hello ${name}`;
}
"#;
        let result = parse_structure(source, "javascript").unwrap();
        let functions = result["functions"].as_array().unwrap();
        assert!(functions.iter().any(|f| f["name"] == "hello"));

        let classes = result["classes"].as_array().unwrap();
        assert!(classes.iter().any(|c| c["name"] == "Widget"));
    }

    #[test]
    fn test_parse_typescript_interfaces_and_types() {
        let source = r#"
import { Component } from 'react';

interface User {
    name: string;
    age: number;
}

type Status = 'active' | 'inactive';

enum Direction {
    Up,
    Down,
}

class UserService {
    getUser(id: number): User {
        return { name: "test", age: 0 };
    }
}

function greet(user: User): string {
    return `Hello ${user.name}`;
}
"#;
        let result = parse_structure(source, "typescript").unwrap();

        let functions = result["functions"].as_array().unwrap();
        assert!(functions.iter().any(|f| f["name"] == "greet"));
        assert!(functions.iter().any(|f| f["name"] == "getUser"));

        let structs = result["structs"].as_array().unwrap();
        assert!(
            structs
                .iter()
                .any(|s| s["name"] == "User" && s["kind"] == "interface")
        );
        assert!(
            structs
                .iter()
                .any(|s| s["name"] == "Status" && s["kind"] == "type")
        );
        assert!(
            structs
                .iter()
                .any(|s| s["name"] == "Direction" && s["kind"] == "enum")
        );

        let classes = result["classes"].as_array().unwrap();
        assert!(classes.iter().any(|c| c["name"] == "UserService"));

        let imports = result["imports"].as_array().unwrap();
        assert!(!imports.is_empty());
    }

    #[test]
    fn test_unsupported_language() {
        let result = parse_structure("", "haskell");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rejects_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let large_file = dir.path().join("big.rs");
        std::fs::write(&large_file, "x".repeat(1_048_577)).unwrap();

        let tool = CodeStructureTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "big.rs"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("file too large"));
    }
}
