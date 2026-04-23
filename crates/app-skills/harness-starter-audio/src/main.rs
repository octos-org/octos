//! CLI binary for `harness-starter-audio`.
//!
//! Protocol: `./harness-starter-audio <tool_name>` with JSON on stdin,
//! one JSON object on stdout.

#![deny(unsafe_code)]

use std::io::Read;
use std::path::PathBuf;

use harness_starter_audio::{SynthesizeClipInput, synthesize_clip};
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
        "synthesize_clip" => handle_synthesize_clip(&raw),
        other => reply_failure(&format!(
            "unknown tool '{other}', expected: synthesize_clip"
        )),
    }
}

fn handle_synthesize_clip(raw: &str) {
    let input: SynthesizeClipInput = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(err) => {
            reply_failure(&format!("invalid input JSON: {err}"));
            return;
        }
    };
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match synthesize_clip(&workspace_root, &input) {
        Ok(out) => {
            println!(
                "{}",
                json!({
                    "success": true,
                    "output": format!(
                        "Wrote {} ({} bytes) for label '{}'",
                        out.artifact_path.display(),
                        out.byte_len,
                        input.label
                    ),
                    "files_to_send": [out.artifact_path.to_string_lossy()]
                })
            );
        }
        Err(err) => reply_failure(&format!("synthesize_clip failed: {err:?}")),
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
