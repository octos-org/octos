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
        "Inspect workspace contract state for the current workspace. Use this to answer whether a slides/site deliverable is actually ready, which required checks failed, which artifacts exist, and what revision is currently present. Task state tells you what happened in execution; workspace state tells you what is true about the deliverable."
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

        let output = json!({
            "workspace_root": self.base_dir,
            "requested_project": input.project,
            "repo_count": contracts.len(),
            "ready_count": contracts.iter().filter(|status| status.ready).count(),
            "contracts": contracts,
        });

        Ok(ToolResult {
            output: serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string()),
            success: true,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_git::WorkspaceProjectKind;
    use crate::workspace_policy::{WorkspacePolicy, write_workspace_policy};

    fn write_file(path: impl AsRef<std::path::Path>, contents: &str) {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn returns_ready_contract_for_matching_slides_project() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("slides/demo");
        std::fs::create_dir_all(&repo_root).unwrap();
        write_workspace_policy(
            &repo_root,
            &WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides),
        )
        .unwrap();
        write_file(repo_root.join("script.js"), "// slides");
        write_file(repo_root.join("memory.md"), "# memory");
        write_file(repo_root.join("changelog.md"), "# changelog");
        write_file(repo_root.join("output/deck.pptx"), "deck");
        write_file(repo_root.join("output/imgs/manifest.json"), "{}");
        write_file(repo_root.join("output/imgs/slide-01.png"), "png");

        let tool = CheckWorkspaceContractTool::new(tmp.path());
        let result = tool
            .execute(&json!({"project": "slides/demo"}))
            .await
            .unwrap();

        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["repo_count"], 1);
        assert_eq!(payload["ready_count"], 1);
        assert_eq!(payload["contracts"][0]["repo_label"], "slides/demo");
        assert_eq!(payload["contracts"][0]["ready"], true);
        assert_eq!(payload["contracts"][0]["artifacts"][0]["name"], "deck");
    }

    #[tokio::test]
    async fn can_filter_to_only_not_ready_contracts() {
        let tmp = tempfile::tempdir().unwrap();
        let ready_root = tmp.path().join("slides/ready");
        let broken_root = tmp.path().join("slides/broken");
        for root in [&ready_root, &broken_root] {
            std::fs::create_dir_all(root).unwrap();
            write_workspace_policy(
                root,
                &WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides),
            )
            .unwrap();
            write_file(root.join("script.js"), "// slides");
            write_file(root.join("memory.md"), "# memory");
            write_file(root.join("changelog.md"), "# changelog");
        }
        write_file(ready_root.join("output/deck.pptx"), "deck");
        write_file(ready_root.join("output/imgs/manifest.json"), "{}");
        write_file(ready_root.join("output/imgs/slide-01.png"), "png");

        let tool = CheckWorkspaceContractTool::new(tmp.path());
        let result = tool
            .execute(&json!({"only_not_ready": true}))
            .await
            .unwrap();

        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        let contracts = payload["contracts"].as_array().unwrap();
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0]["repo_label"], "slides/broken");
        assert_eq!(contracts[0]["ready"], false);
    }

    #[tokio::test]
    async fn returns_ready_contract_for_matching_site_project_template() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("sites/news");
        std::fs::create_dir_all(&repo_root).unwrap();
        write_workspace_policy(
            &repo_root,
            &WorkspacePolicy::for_site_build_output("out"),
        )
        .unwrap();
        write_file(repo_root.join("mofa-site-session.json"), "{}");
        write_file(repo_root.join("site-plan.json"), "{}");
        write_file(repo_root.join("optimized-prompt.md"), "# prompt");
        write_file(repo_root.join("out/index.html"), "<html></html>");

        let tool = CheckWorkspaceContractTool::new(tmp.path());
        let result = tool
            .execute(&json!({"project": "sites/news"}))
            .await
            .unwrap();

        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["repo_count"], 1);
        assert_eq!(payload["ready_count"], 1);
        assert_eq!(payload["contracts"][0]["repo_label"], "sites/news");
        assert_eq!(payload["contracts"][0]["ready"], true);
        assert_eq!(payload["contracts"][0]["artifacts"][0]["name"], "entrypoint");
        assert_eq!(payload["contracts"][0]["artifacts"][0]["pattern"], "out/index.html");
    }
}
