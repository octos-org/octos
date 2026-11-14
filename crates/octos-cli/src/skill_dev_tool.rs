//! `skill_dev`: lets the agent write, check, run and register a skill from
//! its Rust source through the Relax JIT. Experimental; see
//! `docs/JIT_DEV_MODE.md`.
//!
//! The tool exists only on a server whose operator opted in
//! ([`agent_dev_enabled`]). `check`, `run` and `register` compile, execute or
//! persist code the model wrote, outside the shell sandbox, so each one blocks
//! on the interactive approval and fails closed without one, as
//! `manage_skills` does for install and remove.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_agent::plugins::source_runtime::{self, DevSkill};
use octos_agent::tools::{
    ConcurrencyClass, TOOL_APPROVAL_CTX, TOOL_CTX, Tool, ToolApprovalDecision, ToolApprovalRequest,
    ToolResult,
};
use octos_core::session_scope::{PathClassification, SessionScope};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::commands::skills_dev;

/// Operator opt-in: `1` gives the agent the `skill_dev` tool on a machine
/// where Relax is set up. Follows `OCTOS_SKILLS_CARGO_BUILD`.
pub const AGENT_DEV_ENV: &str = "OCTOS_SKILLS_AGENT_DEV";

/// A first run compiles the crate and its dependencies.
const CHILD_TIMEOUT: Duration = Duration::from_secs(300);
/// What is kept of each output stream of a child.
const MAX_CAPTURE: usize = 256 * 1024;
const MAX_INPUT: usize = 64 * 1024;

/// Whether this server offers `skill_dev`: the operator asked for it and a
/// Relax binary is there to run the source.
pub fn agent_dev_enabled() -> bool {
    cfg!(unix)
        && std::env::var(AGENT_DEV_ENV).as_deref() == Ok("1")
        && source_runtime::relax_binary().is_some()
}

pub struct SkillDevTool {
    skills_dir: PathBuf,
}

impl SkillDevTool {
    /// `skills_dir` is the profile's own skills directory, where `register`
    /// puts a dev skill.
    pub fn new(skills_dir: impl Into<PathBuf>) -> Self {
        Self {
            skills_dir: skills_dir.into(),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    input: Option<Value>,
}

fn fail(output: impl Into<String>) -> ToolResult {
    ToolResult {
        output: output.into(),
        success: false,
        ..Default::default()
    }
}

fn done(output: impl Into<String>) -> ToolResult {
    ToolResult {
        output: output.into(),
        success: true,
        ..Default::default()
    }
}

/// An existing folder the session may write to. `raw` is absolute or relative
/// to the workspace; the folder must exist, so that symbolic links and `..`
/// are resolved before the scope check.
fn folder_in_scope(scope: &SessionScope, raw: Option<&str>) -> Result<PathBuf, String> {
    let path = match raw.map(str::trim).filter(|raw| !raw.is_empty()) {
        Some(raw) if Path::new(raw).is_absolute() => PathBuf::from(raw),
        Some(raw) => scope.workspace().join(raw),
        None => scope.workspace().to_path_buf(),
    };
    let folder = path
        .canonicalize()
        .ok()
        .filter(|folder| folder.is_dir())
        .ok_or_else(|| format!("no folder at {}", path.display()))?;
    match scope.classify_canonical_path(&folder) {
        PathClassification::InWorkspace | PathClassification::InGrantedDir { .. } => Ok(folder),
        _ => Err(format!(
            "{} is outside the session workspace",
            folder.display()
        )),
    }
}

struct ChildOutput {
    /// `None` when the child was stopped at the time limit.
    status: Option<std::process::ExitStatus>,
    stdout: String,
    stderr: String,
}

async fn read_capped(pipe: Option<impl AsyncRead + Unpin>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    // Read to the end even past the cap, so the child never blocks on a full
    // pipe.
    while let Ok(read) = pipe.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        let room = MAX_CAPTURE.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..read.min(room)]);
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Run a Relax command of a skill crate to its end or to the time limit,
/// without the server's secrets in its environment.
async fn run_bounded(
    command: std::process::Command,
    cwd: &Path,
    stdin: Option<Vec<u8>>,
    limit: Duration,
) -> Result<ChildOutput, String> {
    let mut command = tokio::process::Command::from(command);
    octos_agent::sanitize_default_subprocess_env(&mut command);
    command
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start Relax: {error}"))?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A tool that never reads its input is not an error of the call.
        tokio::spawn(async move {
            let _ = pipe.write_all(&bytes).await;
            let _ = pipe.shutdown().await;
        });
    }
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let run = async {
        let (stdout, stderr, status) =
            tokio::join!(read_capped(stdout), read_capped(stderr), child.wait());
        (stdout, stderr, status)
    };
    match tokio::time::timeout(limit, run).await {
        Ok((stdout, stderr, Ok(status))) => Ok(ChildOutput {
            status: Some(status),
            stdout,
            stderr,
        }),
        Ok((_, _, Err(error))) => Err(format!("Relax did not finish: {error}")),
        // Dropping the child stops it.
        Err(_) => Ok(ChildOutput {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
        }),
    }
}

fn report(what: &str, output: &ChildOutput) -> ToolResult {
    let Some(status) = output.status else {
        return fail(format!(
            "{what} was stopped after {} s.",
            CHILD_TIMEOUT.as_secs()
        ));
    };
    let mut text = String::new();
    if !status.success() {
        text.push_str(&format!("{what} failed ({status}).\n"));
    }
    let stdout = output.stdout.trim();
    let stderr = output.stderr.trim();
    if !stdout.is_empty() {
        text.push_str(stdout);
        text.push('\n');
    }
    if !stderr.is_empty() {
        if !stdout.is_empty() {
            text.push_str("\nstderr:\n");
        }
        text.push_str(stderr);
        text.push('\n');
    }
    if text.is_empty() {
        text.push_str(&format!("{what} passed with no output.\n"));
    }
    ToolResult {
        output: text,
        success: status.success(),
        ..Default::default()
    }
}

impl SkillDevTool {
    /// Ask the user before code the model wrote is compiled, run or
    /// registered. `Some` is the refusal to return.
    async fn refused(&self, action: &str, body: String, source: &Path) -> Option<ToolResult> {
        let Ok(requester) = TOOL_APPROVAL_CTX.try_with(Clone::clone) else {
            tracing::warn!(
                action,
                "skill_dev requires approval: denied (no interactive approval available)"
            );
            return Some(fail(format!(
                "skill_dev {action} requires interactive approval and was denied: no interactive approval channel is available. The user can run `octos skills {action}` themselves instead.",
                action = if action == "register" { "dev" } else { action }
            )));
        };
        let tool_id = TOOL_CTX
            .try_with(|inner| inner.tool_id.clone())
            .unwrap_or_default();
        let decision = requester
            .request_approval(ToolApprovalRequest {
                tool_id,
                tool_name: self.name().to_owned(),
                title: format!("Approve skill {action} from source"),
                body,
                command: None,
                cwd: Some(source.display().to_string()),
                once_only: false,
                host_tool: None,
            })
            .await;
        if matches!(decision, ToolApprovalDecision::Deny) {
            tracing::warn!(action, "skill_dev denied by interactive approval");
            return Some(fail(format!(
                "skill_dev {action} was denied by interactive approval. Do not retry without the user asking again."
            )));
        }
        None
    }

    fn new_skill(scope: &SessionScope, input: &Input) -> ToolResult {
        let Some(name) = input.name.as_deref().map(str::trim) else {
            return fail("`name` is required for new");
        };
        let parent = match folder_in_scope(scope, input.path.as_deref()) {
            Ok(parent) => parent,
            Err(error) => {
                return fail(format!(
                    "{error}. For new, `path` is the existing parent folder: the crate is created at <path>/{name}. Leave `path` out to create it in the workspace."
                ));
            }
        };
        match skills_dev::scaffold_skill(name, &parent) {
            Ok((dir, tool)) => done(format!(
                "Created skill crate {dir} with one tool `{tool}`: Cargo.toml, src/main.rs, manifest.json, SKILL.md.\n\
                 Edit src/main.rs with the file tools and keep manifest.json in step with the tools it serves. \
                 Then call skill_dev with action check, action run (tool and input), and action register.",
                dir = dir.display()
            )),
            Err(error) => fail(format!("{error:#}")),
        }
    }

    async fn source_action(&self, scope: &SessionScope, input: Input) -> ToolResult {
        let action = input.action.as_str();
        if input.path.as_deref().is_none_or(|p| p.trim().is_empty()) {
            return fail(format!(
                "`path` (the skill's folder) is required for {action}"
            ));
        }
        let source = match folder_in_scope(scope, input.path.as_deref()) {
            Ok(source) => source,
            Err(error) => return fail(error),
        };
        let stdin = match action {
            "run" => {
                if input.tool.as_deref().is_none_or(|t| t.trim().is_empty()) {
                    return fail("`tool` is required for run");
                }
                let value = match input.input.clone() {
                    // Some models send the object as a string.
                    Some(Value::String(text)) => match serde_json::from_str::<Value>(&text) {
                        Ok(value) => value,
                        Err(error) => return fail(format!("`input` is not JSON: {error}")),
                    },
                    Some(value) => value,
                    None => json!({}),
                };
                let bytes = value.to_string().into_bytes();
                if bytes.len() > MAX_INPUT {
                    return fail(format!("`input` is larger than {MAX_INPUT} bytes"));
                }
                Some(bytes)
            }
            _ => None,
        };
        let tool = input.tool.as_deref().unwrap_or_default().trim().to_owned();

        let body = match action {
            "check" => format!(
                "Type-check the skill crate at {}. Its build scripts and procedural macros run on this machine, outside the sandbox.",
                source.display()
            ),
            "run" => format!(
                "Compile the skill crate at {} from source and run its tool `{tool}` on this machine, outside the sandbox.",
                source.display()
            ),
            _ => format!(
                "Register the skill crate at {} as a dev skill of this profile. From the next server start its tools are regular agent tools that run from this source, outside the sandbox.",
                source.display()
            ),
        };
        if let Some(refusal) = self.refused(action, body, &source).await {
            return refusal;
        }

        if action == "register" {
            let skills_dir = self.skills_dir.clone();
            let registered = tokio::task::spawn_blocking(move || {
                skills_dev::register_source_skill(&skills_dir, &source, &BTreeMap::new(), false)
            })
            .await;
            return match registered {
                Ok(Ok((dev, dest))) => done(format!(
                    "Registered '{}' as a dev skill that runs from {}.\n\
                     Its tools join the regular tool list the next time the server starts. Until then call them with skill_dev action run. \
                     A source edit needs no further step. A change to manifest.json or SKILL.md is read at the next server start.",
                    dest.file_name().unwrap_or_default().to_string_lossy(),
                    dev.source.display()
                )),
                Ok(Err(error)) => fail(format!("{error:#}")),
                Err(error) => fail(format!("register did not finish: {error}")),
            };
        }

        // Resolving the crate asks cargo for the workspace root.
        let resolve_from = source.clone();
        let dev = match tokio::task::spawn_blocking(move || {
            DevSkill::resolve(&resolve_from, &BTreeMap::new())
        })
        .await
        {
            Ok(Ok(dev)) => dev,
            Ok(Err(error)) => return fail(format!("{error:#}")),
            Err(error) => return fail(format!("{action} did not finish: {error}")),
        };
        let (what, command) = match action {
            "check" => ("The check".to_owned(), dev.check_command()),
            _ => (format!("Tool `{tool}`"), dev.run_command(&tool)),
        };
        match run_bounded(command, scope.workspace(), stdin, CHILD_TIMEOUT).await {
            Ok(output) => report(&what, &output),
            Err(error) => fail(error),
        }
    }
}

#[async_trait]
impl Tool for SkillDevTool {
    fn name(&self) -> &str {
        "skill_dev"
    }

    fn description(&self) -> &str {
        "Develop a skill (a plugin that gives this agent new tools) from Rust source, with no build step, through the Relax JIT. Experimental. Actions: `new` writes a starter skill crate into the workspace; `check` type-checks a skill crate and returns the compiler's errors; `run` calls one tool of the crate with a JSON input and returns what it printed; `register` adds the crate to this profile as a dev skill, whose tools are regular tools after the next server start. Use this tool, not cargo in the shell: the shell sandbox cannot run the Rust toolchain. Edit the source with the file tools between calls: the next `check` or `run` uses the edit. `check`, `run` and `register` ask the user for approval. The first `check` or `run` of a crate compiles it and can take a minute."
    }

    fn tags(&self) -> &[&str] {
        &["code"]
    }

    // check, run and register block on the interactive approval bridge; a
    // human may take longer than any dispatch timeout.
    fn blocks_on_human_input(&self) -> bool {
        true
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["new", "check", "run", "register"],
                    "description": "Action to perform"
                },
                "name": {
                    "type": "string",
                    "description": "Name of the skill to create (for new): lower-case letters, digits and hyphens, starting with a letter"
                },
                "path": {
                    "type": "string",
                    "description": "For new: an existing parent folder; the crate is created at <path>/<name> (default parent: the workspace). For check, run and register: the skill crate's own folder. A relative path starts at the workspace."
                },
                "tool": {
                    "type": "string",
                    "description": "Name of the tool to call (for run)"
                },
                "input": {
                    "type": "object",
                    "description": "JSON input of the tool (for run, default {})"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: Input = match serde_json::from_value(args.clone()) {
            Ok(input) => input,
            Err(error) => return Ok(fail(format!("invalid input: {error}"))),
        };
        let scope: Option<Arc<SessionScope>> = TOOL_CTX
            .try_with(|inner| inner.session_scope.clone())
            .ok()
            .flatten();
        let Some(scope) = scope else {
            return Ok(fail(
                "skill_dev needs a session workspace, and this call has none",
            ));
        };
        Ok(match input.action.as_str() {
            "new" => Self::new_skill(&scope, &input),
            "check" | "run" | "register" => self.source_action(&scope, input).await,
            other => fail(format!(
                "unknown action '{other}': use new, check, run or register"
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_agent::tools::{ToolApprovalRequester, ToolContext};

    struct Deny;

    #[async_trait]
    impl ToolApprovalRequester for Deny {
        async fn request_approval(&self, _request: ToolApprovalRequest) -> ToolApprovalDecision {
            ToolApprovalDecision::Deny
        }
    }

    #[tokio::test]
    async fn skill_dev_stays_in_the_workspace_and_does_nothing_without_an_approval() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("ws");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // A link that leaves the workspace, where the platform has one.
        let link = cfg!(unix).then_some("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, workspace.join("link")).unwrap();
        let skills = root.path().join("skills");
        let tool = SkillDevTool::new(&skills);
        let mut ctx = ToolContext::zero();
        ctx.session_scope = Some(Arc::new(
            SessionScope::solo(workspace.clone(), Vec::new()).unwrap(),
        ));
        let call = async |args: Value| {
            let run = async { tool.execute(&args).await.unwrap() };
            TOOL_CTX.scope(ctx.clone(), run).await
        };

        let made = call(json!({"action": "new", "name": "demo"})).await;
        assert!(made.success, "{}", made.output);
        assert!(workspace.join("demo/src/main.rs").is_file());
        // Not through a link that leaves the workspace, not by an absolute
        // path, and not into the folder above.
        let escapes = [link, outside.to_str(), Some("..")];
        for path in escapes.into_iter().flatten() {
            for action in ["new", "check", "run", "register"] {
                let args = json!({"action": action, "name": "esc", "path": path, "tool": "t"});
                let refused = call(args).await;
                assert!(!refused.success, "{action} {path}");
                assert!(refused.output.contains("outside the session workspace"));
            }
        }
        assert!(!outside.join("esc").exists());

        // No approval channel, and a denied approval: nothing is compiled,
        // run or registered.
        for action in ["check", "run", "register"] {
            let args = json!({"action": action, "path": "demo", "tool": "demo"});
            let headless = call(args.clone()).await;
            assert!(!headless.success);
            assert!(headless.output.contains("no interactive approval channel"));
            let denied = TOOL_APPROVAL_CTX.scope(Arc::new(Deny), call(args)).await;
            assert!(denied.output.contains("denied by interactive approval"));
        }
        assert!(!skills.exists());
        assert!(!workspace.join("demo/target").exists());
    }
}
