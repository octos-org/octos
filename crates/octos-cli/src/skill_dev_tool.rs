//! `skill_dev`: lets the agent write, check, run and register a skill from
//! its Rust source through the Relax JIT. Experimental; see
//! `docs/JIT_DEV_MODE.md`.
//!
//! The tool exists only on a server whose operator opted in
//! ([`SkillDevTool::register`]).
//!
//! `check` and `run` compile and execute code the model wrote. They do it
//! inside the session's sandbox, the one the session's shell runs its
//! commands in, with the whole process tree bound to the call. Without a
//! session sandbox, or with one that refuses, they do nothing. `register`
//! makes such code a tool that later runs as any installed skill does,
//! outside the sandbox, so it copies what it registers and asks every time.
//! All three block on the interactive approval and fail closed without one,
//! as `manage_skills` does for install and remove.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_agent::ToolRegistry;
use octos_agent::plugins::source_runtime::{self, DevSkill};
use octos_agent::sandbox::Sandbox;
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

pub struct SkillDevTool {
    skills_dir: PathBuf,
    relax: PathBuf,
}

impl SkillDevTool {
    /// Give `tools` the `skill_dev` tool when this server offers it: the
    /// operator set [`AGENT_DEV_ENV`] to `1`, a Relax binary is there to run
    /// the source, and the server is not host-scoped. `skills_dir` is the
    /// profile's own skills directory, where `register` puts a dev skill.
    /// Returns whether the tool was added.
    pub fn register(tools: &mut ToolRegistry, skills_dir: PathBuf, host_scoped: bool) -> bool {
        let flag = std::env::var(AGENT_DEV_ENV).ok();
        let relax = source_runtime::relax_binary();
        Self::register_when(tools, skills_dir, host_scoped, flag.as_deref(), relax)
    }

    fn register_when(
        tools: &mut ToolRegistry,
        skills_dir: PathBuf,
        host_scoped: bool,
        flag: Option<&str>,
        relax: Option<PathBuf>,
    ) -> bool {
        let offered = cfg!(unix) && flag == Some("1") && !host_scoped;
        let Some(relax) = relax.filter(|_| offered) else {
            return false;
        };
        tools.register(Self { skills_dir, relax });
        true
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

/// How a child ended.
enum Ended {
    Exited(std::process::ExitStatus),
    TimedOut,
    Interrupted,
}

struct ChildOutput {
    ended: Ended,
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

/// Run `command` to its end, to the time limit, or until `interrupted`
/// resolves, without the server's secrets in its environment.
///
/// The child leads a process group of its own. When the call ends early (the
/// time limit, an interrupt, or the caller dropping this future) the whole
/// group is killed, so that cargo, a build script or a process the tool
/// started does not outlive the call. This is the lifecycle of a plugin tool.
async fn run_bounded(
    mut command: tokio::process::Command,
    stdin: Option<Vec<u8>>,
    limit: Duration,
    interrupted: impl Future<Output = ()>,
) -> Result<ChildOutput, String> {
    octos_agent::sanitize_default_subprocess_env(&mut command);
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start the command: {error}"))?;
    // Dropped armed on every early end, which kills the group.
    #[cfg(unix)]
    let mut group = child
        .id()
        .map(octos_agent::plugins::ProcessGroupKillGuard::new);
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
    let early = |ended| ChildOutput {
        ended,
        stdout: String::new(),
        stderr: String::new(),
    };
    tokio::select! {
        (stdout, stderr, status) = run => {
            let status = status.map_err(|error| format!("the command did not finish: {error}"))?;
            // It exited and was reaped: there is no group left to kill.
            #[cfg(unix)]
            if let Some(group) = group.as_mut() {
                group.disarm();
            }
            Ok(ChildOutput { ended: Ended::Exited(status), stdout, stderr })
        }
        () = tokio::time::sleep(limit) => Ok(early(Ended::TimedOut)),
        () = interrupted => Ok(early(Ended::Interrupted)),
    }
}

/// Two lines Relax prints on every run inside a sandbox, where it can
/// neither open its inspection socket nor hard-link its own executable. They
/// say nothing about the skill.
fn is_sandbox_noise(line: &str) -> bool {
    line.starts_with("relax-remote: ")
        || line.starts_with("relax run: the program's own executable could not be made")
}

/// What to tell the model when the sandbox, not the skill, stopped a run.
fn sandbox_hint(relax: &Path) -> String {
    let paths: Vec<String> = source_runtime::sandbox_read_paths(relax)
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    format!(
        "[sandbox] skill_dev runs inside the session sandbox, as the shell does, and the sandbox stopped this. \
         The JIT must be able to read: {}. An operator adds these to the profile's sandbox.read_allow_paths (`octos skills jit status` prints them). \
         Dependencies come from cargo's cache: there is no network in the sandbox unless the profile allows it. \
         In a container sandbox, Relax and the Rust toolchain must be in the image. This cannot be fixed from inside the session.",
        paths.join(", ")
    )
}

/// `confined` is the Relax binary when the run was inside a real sandbox.
fn report(what: &str, output: &ChildOutput, confined: Option<&Path>) -> ToolResult {
    let status = match output.ended {
        Ended::Exited(status) => status,
        Ended::TimedOut => {
            return fail(format!(
                "{what} was stopped after {} s, with every process it started.",
                CHILD_TIMEOUT.as_secs()
            ));
        }
        Ended::Interrupted => {
            return fail(format!(
                "{what} was interrupted and stopped, with every process it started."
            ));
        }
    };
    let mut text = String::new();
    if !status.success() {
        text.push_str(&format!("{what} failed ({status}).\n"));
    }
    let stdout = output.stdout.trim();
    let stderr: Vec<&str> = output
        .stderr
        .lines()
        .filter(|line| confined.is_none() || !is_sandbox_noise(line))
        .collect();
    let stderr = stderr.join("\n");
    let stderr = stderr.trim();
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
    let denied = ["Operation not permitted", "Permission denied", "not found"];
    if let Some(relax) = confined
        && !status.success()
        && denied.iter().any(|sign| stderr.contains(sign))
    {
        text.push('\n');
        text.push_str(&sandbox_hint(relax));
        text.push('\n');
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
                // An approval remembered for the session, perhaps given at
                // a `check`, must not answer a `register`: each one decides
                // what runs unsandboxed from then on.
                once_only: action == "register",
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
        // The registered copy must be out of reach of the file tools.
        if action == "register"
            && matches!(
                scope.classify_canonical_path(&self.skills_dir),
                PathClassification::InWorkspace | PathClassification::InGrantedDir { .. }
            )
        {
            return fail(
                "register is not available in this session: the profile's skills folder lies inside the session's workspace, so a registered copy could still be edited from here. The user can run `octos skills dev` instead.",
            );
        }
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

        // check and run happen inside the session's sandbox or not at all.
        let sandbox: Option<Arc<dyn Sandbox>> = match action {
            "register" => None,
            _ => {
                let sandbox = TOOL_CTX
                    .try_with(|inner| inner.sandbox.clone())
                    .ok()
                    .flatten();
                let Some(sandbox) = sandbox else {
                    return fail(format!(
                        "skill_dev {action} runs inside the session sandbox, and this call has none. The user can run `octos skills {action}` themselves instead."
                    ));
                };
                if let Some(refusal) = sandbox.refusal() {
                    return fail(refusal.to_string());
                }
                Some(sandbox)
            }
        };
        let place = match &sandbox {
            Some(sandbox) if sandbox.is_noop() => {
                "on this machine, unconfined: this server runs its sessions without a sandbox"
            }
            _ => "inside the session sandbox, as a shell command of this session would",
        };
        let body = match action {
            "check" => format!(
                "Type-check the skill crate at {}. Its build scripts and procedural macros run {place}.",
                source.display()
            ),
            "run" => format!(
                "Compile the skill crate at {} from source and run its tool `{tool}` {place}.",
                source.display()
            ),
            _ => format!(
                "Register the skill crate at {} as a dev skill of this profile. Octos copies the crate as it is now. From the next server start its tools are regular agent tools in every session of the profile, and they run from that copy, outside the sandbox, with no further approval. A later edit of the crate does not reach them until it is registered, and approved, again. `octos skills remove` undoes this.",
                source.display()
            ),
        };
        if let Some(refusal) = self.refused(action, body, &source).await {
            return refusal;
        }

        let Some(sandbox) = sandbox else {
            let skills_dir = self.skills_dir.clone();
            let registered = tokio::task::spawn_blocking(move || {
                skills_dev::register_pinned_skill(&skills_dir, &source)
            })
            .await;
            return match registered {
                Ok(Ok((dev, dest))) => done(format!(
                    "Registered '{}' as a dev skill, from a copy of {} as it is now.\n\
                     Its tools join the regular tool list the next time the server starts, and they run the copy. Until then call them with skill_dev action run. \
                     A later edit of the crate reaches the registered tools only after another skill_dev register, which asks the user again.",
                    dest.file_name().unwrap_or_default().to_string_lossy(),
                    dev.pinned_from.as_deref().unwrap_or(&dev.source).display()
                )),
                Ok(Err(error)) => fail(format!("{error:#}")),
                Err(error) => fail(format!("register did not finish: {error}")),
            };
        };

        // Nothing is started for the crate before the sandbox has it.
        let dev = match DevSkill::resolve_standalone(&source, &self.relax) {
            Ok(dev) => dev,
            Err(error) => return fail(format!("{error:#}")),
        };
        let (what, command) = match action {
            "check" => ("The check".to_owned(), dev.check_command()),
            _ => (format!("Tool `{tool}`"), dev.run_command(&tool)),
        };
        // The same cwd the session's shell gets: the sandbox lets a command
        // write there and nowhere else.
        let mut confined = sandbox.wrap_command(&dev.sandbox_line(&command), scope.workspace());
        for (name, value) in command.get_envs() {
            match value {
                Some(value) => confined.env(name, value),
                None => confined.env_remove(name),
            };
        }
        let turn_cancel = TOOL_CTX
            .try_with(|inner| inner.turn_cancel.clone())
            .ok()
            .flatten();
        let interrupted = async {
            match turn_cancel {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending().await,
            }
        };
        let relax = (!sandbox.is_noop()).then_some(self.relax.as_path());
        match run_bounded(confined, stdin, CHILD_TIMEOUT, interrupted).await {
            Ok(output) => report(&what, &output, relax),
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
        "Develop a skill (a plugin that gives this agent new tools) from Rust source, with no build step, through the Relax JIT. Experimental. Actions: `new` writes a starter skill crate into the workspace; `check` type-checks a skill crate and returns the compiler's errors; `run` calls one tool of the crate with a JSON input and returns what it printed; `register` adds a copy of the crate, as it is at that moment, to this profile as a dev skill, whose tools are regular tools after the next server start; register again after an edit. Build and try the crate with this tool; do not call cargo or relax yourself. `check` and `run` happen inside the session sandbox, as shell commands do: the crate must be in the workspace, and its dependencies come from cargo's cache when the sandbox has no network. Edit the source with the file tools between calls: the next `check` or `run` uses the edit. `check`, `run` and `register` ask the user for approval. The first `check` or `run` of a crate compiles it and can take a minute."
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
    use octos_agent::sandbox::{NoSandbox, SandboxUnavailable};
    use octos_agent::tools::{ToolApprovalRequester, ToolContext};

    struct Answer(ToolApprovalDecision);

    #[async_trait]
    impl ToolApprovalRequester for Answer {
        async fn request_approval(&self, _request: ToolApprovalRequest) -> ToolApprovalDecision {
            self.0
        }
    }

    /// A sandbox that runs nothing it is given: it notes the line and the
    /// cwd, and answers with a command of its own.
    #[derive(Default)]
    struct Recording {
        seen: std::sync::Mutex<Vec<(String, PathBuf)>>,
        refuse: Option<SandboxUnavailable>,
    }

    impl Sandbox for Recording {
        fn wrap_command(&self, line: &str, cwd: &Path) -> tokio::process::Command {
            let mut seen = self.seen.lock().unwrap();
            seen.push((line.to_owned(), cwd.to_path_buf()));
            let mut answer = tokio::process::Command::new("sh");
            answer.arg("-c").arg("echo confined; cat").current_dir(cwd);
            answer
        }

        fn refusal(&self) -> Option<&SandboxUnavailable> {
            self.refuse.as_ref()
        }
    }

    fn tool(skills: &Path, relax: &Path) -> SkillDevTool {
        SkillDevTool {
            skills_dir: skills.to_path_buf(),
            relax: relax.to_path_buf(),
        }
    }

    fn session(workspace: &Path, sandbox: Option<Arc<dyn Sandbox>>) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.session_scope = Some(Arc::new(
            SessionScope::solo(workspace.to_path_buf(), Vec::new()).unwrap(),
        ));
        ctx.sandbox = sandbox;
        ctx
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
        let tool = tool(&skills, Path::new("relax"));
        let ctx = session(&workspace, Some(Arc::new(NoSandbox)));
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
            let deny = Arc::new(Answer(ToolApprovalDecision::Deny));
            let denied = TOOL_APPROVAL_CTX.scope(deny, call(args)).await;
            assert!(denied.output.contains("denied by interactive approval"));
        }
        assert!(!skills.exists());
        assert!(!workspace.join("demo/target").exists());
    }

    /// Code the model wrote is compiled and run by the session's sandbox,
    /// with the session's workspace as its cwd, or not at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn check_and_run_reach_the_crate_only_through_the_session_sandbox() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        // A stand-in for Relax that leaves a trace when anything starts it.
        let (relax, trace) = (root.path().join("relax"), root.path().join("relax-ran"));
        std::fs::write(&relax, format!("#!/bin/sh\ntouch '{}'\n", trace.display())).unwrap();
        std::fs::set_permissions(&relax, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tool = tool(&root.path().join("skills"), &relax);
        let approve = || Arc::new(Answer(ToolApprovalDecision::Approve));
        let call = async |sandbox: Option<Arc<dyn Sandbox>>, args: Value| {
            let run = async { tool.execute(&args).await.unwrap() };
            let run = TOOL_CTX.scope(session(&workspace, sandbox), run);
            TOOL_APPROVAL_CTX.scope(approve(), run).await
        };
        let made = call(None, json!({"action": "new", "name": "demo"})).await;
        assert!(made.success, "{}", made.output);

        let sandbox = Arc::new(Recording::default());
        for (action, word) in [("check", "'check'"), ("run", "'run'")] {
            let args = json!({"action": action, "path": "demo", "tool": "demo", "input": {"a": 1}});
            // No sandbox on the call, or one that refuses: nothing starts.
            let bare = call(None, args.clone()).await;
            assert!(!bare.success && bare.output.contains("this call has none"));
            let refusing = Recording {
                refuse: Some(SandboxUnavailable {
                    requested: "bwrap".into(),
                    reason: "not installed".into(),
                    remediation: String::new(),
                }),
                ..Default::default()
            };
            let refusing = Arc::new(refusing);
            let refused = call(Some(refusing.clone()), args.clone()).await;
            assert!(!refused.success && refused.output.contains("sandbox unavailable"));
            assert!(refusing.seen.lock().unwrap().is_empty());

            // With one, the result is what the sandbox's own command gave.
            let ran = call(Some(sandbox.clone()), args).await;
            assert!(
                ran.success && ran.output.contains("confined"),
                "{}",
                ran.output
            );
            assert_eq!(ran.output.contains(r#"{"a":1}"#), action == "run");
            let (line, cwd) = sandbox.seen.lock().unwrap().pop().unwrap();
            assert_eq!(cwd, workspace);
            let relax = format!("exec '{}' {word} ", relax.display());
            assert!(line.contains(&relax) && line.contains("'demo'"), "{line}");
        }
        assert!(!trace.exists(), "Relax was started outside the sandbox");
    }

    /// The time limit, an interrupt and a dropped call each take the whole
    /// process tree with them, not only the first child.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_early_end_stops_every_process_the_call_started() {
        let root = tempfile::tempdir().unwrap();
        let late = |name: &str| {
            let marker = root.path().join(name);
            let script = format!("(sleep 1; touch '{}') & sleep 30", marker.display());
            (NoSandbox.wrap_command(&script, root.path()), marker)
        };
        let never = std::future::pending::<()>();
        let brief = Duration::from_millis(100);

        let (command, timed_out) = late("timed-out");
        let output = run_bounded(command, None, brief, never).await.unwrap();
        assert!(matches!(output.ended, Ended::TimedOut));

        let (command, interrupted) = late("interrupted");
        let interrupt = tokio::time::sleep(brief);
        let long = Duration::from_secs(60);
        let output = run_bounded(command, None, long, interrupt).await.unwrap();
        assert!(matches!(output.ended, Ended::Interrupted));

        let (command, dropped) = late("dropped");
        let call = run_bounded(command, None, long, std::future::pending::<()>());
        assert!(tokio::time::timeout(brief, call).await.is_err());

        tokio::time::sleep(Duration::from_millis(1500)).await;
        for marker in [timed_out, interrupted, dropped] {
            assert!(!marker.exists(), "{} outlived its call", marker.display());
        }
    }

    /// The tool is there only where the operator asked for it, a runtime
    /// exists, and the server is not host-scoped.
    #[test]
    fn skill_dev_is_offered_only_on_an_opted_in_server_with_a_runtime() {
        let relax = || Some(PathBuf::from("relax"));
        let offered = |flag: Option<&str>, relax: Option<PathBuf>, host_scoped: bool| {
            let mut tools = ToolRegistry::new();
            let skills = PathBuf::from("skills");
            let added = SkillDevTool::register_when(&mut tools, skills, host_scoped, flag, relax);
            assert_eq!(added, tools.get("skill_dev").is_some());
            added
        };
        for off in [None, Some("0"), Some(""), Some("true"), Some("yes")] {
            assert!(!offered(off, relax(), false), "{off:?}");
        }
        assert!(!offered(Some("1"), None, false));
        assert!(!offered(Some("1"), relax(), true));
        assert_eq!(offered(Some("1"), relax(), false), cfg!(unix));
    }
}
