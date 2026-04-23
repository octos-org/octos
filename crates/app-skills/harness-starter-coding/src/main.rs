//! CLI binary for `harness-starter-coding`.

#![deny(unsafe_code)]

use std::io::Read;
use std::path::PathBuf;

use harness_starter_coding::{ProposePatchInput, propose_patch};
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
        "propose_patch" => handle_propose_patch(&raw),
        other => reply_failure(&format!("unknown tool '{other}', expected: propose_patch")),
    }
}

fn handle_propose_patch(raw: &str) {
    let input: ProposePatchInput = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(err) => {
            reply_failure(&format!("invalid input JSON: {err}"));
            return;
        }
    };
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match propose_patch(&workspace_root, &input) {
        Ok(out) => {
            println!(
                "{}",
                json!({
                    "success": true,
                    "output": format!(
                        "Wrote diff {} ({} files)",
                        out.diff_path.display(),
                        out.changed_files.len()
                    ),
                    "files_to_send": [
                        out.diff_path.to_string_lossy(),
                        out.preview_path.to_string_lossy()
                    ]
                })
            );
        }
        Err(err) => reply_failure(&format!("propose_patch failed: {err:?}")),
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
