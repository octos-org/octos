//! Read-only inspection of workspace contract state.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::json;

use crate::workspace_git::inspect_workspace_contracts;

use super::{Tool, ToolResult};

/// Tool that exposes workspace contract truth for the current workspace root.
///
/// Unlike `check_background_tasks`, this answers whether the deliverable is
/// actually ready according to the declared workspace contract.
pub struct CheckWorkspaceContractTool {
    base_dir: PathBuf,
}

impl CheckWorkspaceContractTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct Input {
    /// Optional project selector, e.g. "slides/my-deck" or just "my-deck".
    #[serde(default)]
    project: Option<String>,
    /// When true, omit non-policy-managed repos. Defaults to true.
    #[serde(default = "default_only_policy_managed")]
    only_policy_managed: bool,
    /// When true, only return repos whose contract is not ready.
    #[serde(default)]
    only_not_ready: bool,
}

fn default_only_policy_managed() -> bool {
    true
}

fn normalize_project_selector(value: &str) -> String {
    value.trim().trim_matches('/').replace('\\', "/")
}

#[async_trait]
impl Tool for CheckWorkspaceContractTool {
    fn name(&self) -> &str {
        "check_workspace_contract"
    }

    fn description(&self) -> &str {
        "Inspect workspace contract state for the current workspace. Use this to answer whether a slides/site deliverable is actually ready, which required checks failed, which artifacts exist, and what revision is currently present. Task state tells you what happened in execution; workspace state tells you what is true about the deliverable. The output includes a top-level `all_ready: bool` — when true, the deliverable is COMPLETE and you should finish the turn rather than re-poll.\n\nDOES NOT answer: whether file CONTENT is correct (manifest layout, script.js logic, image quality, text rendering). This tool only verifies artifact PRESENCE and validator pass/fail — it does not read or interpret file contents. If the user asks about file content (`manifest`, `script`, what's INSIDE a file, why an image looks wrong, etc.), use `read_file` / `list_dir` / `view_image` instead. Re-calling this tool will return the same answer."
    }

    fn tags(&self) -> &[&str] {
        &["gateway", "workspace"]
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Optional workspace project selector, e.g. 'slides/my-deck' or 'my-deck'. When omitted, returns all workspace contracts under the current workspace root."
                },
                "only_policy_managed": {
                    "type": "boolean",
                    "description": "Whether to omit repos without a workspace policy. Defaults to true."
                },
                "only_not_ready": {
                    "type": "boolean",
                    "description": "Whether to return only repos whose workspace contract is not ready. Defaults to false."
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input = serde_json::from_value(args.clone())
            .wrap_err("invalid check_workspace_contract input")?;
        let mut contracts = inspect_workspace_contracts(&self.base_dir)
            .wrap_err("inspect workspace contracts failed")?;
        contracts.sort_by(|left, right| left.repo_label.cmp(&right.repo_label));

        if input.only_policy_managed {
            contracts.retain(|status| status.policy_managed);
        }

        if let Some(project) = input.project.as_deref() {
            let selector = normalize_project_selector(project);
            contracts.retain(|status| {
                status.repo_label == selector
                    || status.slug == selector
                    || format!("{}/{}", status.kind, status.slug) == selector
            });
        }

        if input.only_not_ready {
            contracts.retain(|status| !status.ready);
        }

        let repo_count = contracts.len();
        let ready_count = contracts.iter().filter(|status| status.ready).count();
        // `all_ready` is the explicit termination signal for the LLM. When this
        // is `true`, the deliverable is satisfied and there is no reason to
        // re-poll — every artifact under the contract is present and every
        // required validator passed. Without this flag the LLM had to derive
        // the answer from a 4 KB nested artifact tree, which on degraded
        // models (kimi @ 44 tools, ~110 KB prompt) collapsed into a tight
        // re-call loop the runtime had to break with LOOP DETECTED.
        let all_ready = repo_count > 0 && ready_count == repo_count;
        let summary = if repo_count == 0 {
            "No workspace contracts match the selector.".to_string()
        } else if all_ready {
            format!(
                "All {repo_count} contract(s) satisfied — every required artifact is present and every validator passed. The deliverable is COMPLETE; do not re-poll, finish the turn."
            )
        } else {
            let not_ready = repo_count - ready_count;
            format!(
                "{ready_count}/{repo_count} contract(s) satisfied; {not_ready} not yet ready — inspect `contracts[].ready_state` for what's missing."
            )
        };

        let output = json!({
            "workspace_root": self.base_dir,
            "requested_project": input.project,
            "all_ready": all_ready,
            "summary": summary,
            "repo_count": repo_count,
            "ready_count": ready_count,
            "contracts": contracts,
        });

        Ok(ToolResult {
            output: serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string()),
            success: true,
            ..Default::default()
        })
    }
}
