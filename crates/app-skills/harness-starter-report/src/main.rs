//! CLI binary for `harness-starter-report`.
//!
//! Protocol: `./harness-starter-report <tool_name>` with JSON on stdin,
//! one JSON object on stdout.

#![deny(unsafe_code)]

use std::io::Read;
use std::path::PathBuf;

use harness_starter_report::{GenerateReportInput, generate_report};
use serde_json::json;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tool_name = args.get(1).map(String::as_str).unwrap_or("");

    let mut raw = String::new();
    if let Err(err) = std::io::stdin().read_to_string(&mut raw) {
        reply_failure(&format!("read stdin failed: {err}"));
        return;
    }

    match tool_name {
        "generate_report" => handle_generate_report(&raw),
        other => reply_failure(&format!(
            "unknown tool '{other}', expected: generate_report"
        )),
    }
}

fn handle_generate_report(raw: &str) {
    let input: GenerateReportInput = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(err) => {
            reply_failure(&format!("invalid input JSON: {err}"));
            return;
        }
    };
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match generate_report(&workspace_root, &input) {
        Ok(out) => {
            println!(
                "{}",
                json!({
                    "success": true,
                    "output": format!(
                        "Wrote {} for topic '{}'",
                        out.artifact_path.display(),
                        input.topic
                    ),
                    "files_to_send": [out.artifact_path.to_string_lossy()]
                })
            );
        }
        Err(err) => reply_failure(&format!("generate_report failed: {err:?}")),
    }
}

fn reply_failure(message: &str) {
    eprintln!("{message}");
    println!(
        "{}",
        json!({
            "success": false,
            "output": message
        })
    );
    std::process::exit(1);
}
