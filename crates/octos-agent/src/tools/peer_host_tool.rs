//! Host-routed app tools of a host-owned app peer (UPCR-2026-035).
//!
//! A host (an OctoSense shell) declares, per app peer, the app's own tools
//! (`news.list`, `mail.send`, …) with `peer/tools/register`. The kernel offers
//! the model exactly those tools plus the generic kernel tools the host names,
//! and each call to an app tool is routed back to the HOST, which implements
//! it where the capability lives. This module is the model-facing half: one
//! [`HostRoutedTool`] per declared tool. It knows nothing about the wire; the
//! serve path supplies a [`HostToolRouter`] per turn that delivers the call
//! (`peer/tool/call`), waits for `peer/tool/result`, and writes the audit log.
//!
//! Risk enforcement happens here, before the host is ever asked. A call is
//! **attended** when it comes from one of the app's interactive clients (an
//! open request context of the peer, the app's own conversation) AND the turn
//! carries an approval bridge; otherwise the person is absent.
//!
//! - `read` and `act` tools run. A tool not marked `background` runs only
//!   attended.
//! - A gated tool (`destructive`, or marked `outward`) with `confirm: host`
//!   needs an explicit approval through the turn's EXISTING approval bridge
//!   ([`TOOL_APPROVAL_CTX`]): the same `approval/requested` →
//!   `approval/respond` path every other tool uses, on the calling session,
//!   with the exact arguments in the request. Declined or expired → an error
//!   result for the model; the host is not called. No approval bridge in the
//!   turn → refused (fail closed), never run unasked.
//! - A gated tool with `confirm: app` (the app's own sheet asks the person)
//!   goes straight to the host when attended, with `confirm_required: true`
//!   and no kernel approval, so the person is never asked twice. With the
//!   person absent it needs the kernel approval exactly like `confirm: host`.
//! - Every non-`read` call is claimed once, before any approval or host
//!   call, under `<session>/<turn>/<tool_call_id>/<argument digest>`: a
//!   re-dispatch of the same call never raises a second approval or reaches
//!   the host twice, while a provider that reuses ids (`call_1`) with other
//!   arguments is not mistaken for a duplicate.
//! - A non-`read` call whose host answer does not arrive in time ends as
//!   `outcome_unknown` ("do not retry"), never as a plain failure the model
//!   would retry.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    ConcurrencyClass, TOOL_APPROVAL_CTX, Tool, ToolApprovalDecision, ToolApprovalRequest,
    ToolContext, ToolOrigin, ToolResult,
};

/// Maximum serialized size of one call's arguments.
pub const HOST_TOOL_MAX_ARGS_BYTES: usize = 64 * 1024;

/// A declared tool's risk (ADR 0002 section 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostToolRisk {
    /// Looks at something. Runs.
    Read,
    /// Changes the app's own state. Runs.
    Act,
    /// Deletes, sends, spends, or otherwise reaches past the app. Runs only
    /// after the person confirms.
    Destructive,
}

/// Who confirms a gated call with the person.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostToolConfirm {
    /// The kernel's approval request, in the app's conversation.
    #[default]
    Host,
    /// The app's own confirmation sheet, when the person is present.
    App,
}

impl HostToolConfirm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::App => "app",
        }
    }
}

impl HostToolRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Act => "act",
            Self::Destructive => "destructive",
        }
    }
}

/// One host-declared app tool, as validated by the kernel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostToolDecl {
    /// Declared name in the app's namespace, e.g. `news.list`.
    pub name: String,
    /// The app that owns the tool (a cross-app tool may be registered on
    /// another app's peer). Empty in sets stored before the field existed;
    /// see [`HostToolDecl::owner_app`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app: String,
    /// Name the model sees (`news_list`): provider tool names cannot hold `.`.
    pub model_name: String,
    pub description: String,
    /// JSON Schema object for the arguments.
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    pub risk: HostToolRisk,
    /// May run in a turn with no interactive client.
    #[serde(default)]
    pub background: bool,
    /// Reaches past the app (send, post, share); gated like `destructive`.
    #[serde(default)]
    pub outward: bool,
    /// Who confirms a gated call when the person is present.
    #[serde(default)]
    pub confirm: HostToolConfirm,
}

impl HostToolDecl {
    /// The owning app: `app`, or the first segment of `name`.
    pub fn owner_app(&self) -> &str {
        if self.app.is_empty() {
            self.name.split('.').next().unwrap_or_default()
        } else {
            &self.app
        }
    }

    /// Destructive or outward: the person must confirm.
    pub fn gated(&self) -> bool {
        self.risk == HostToolRisk::Destructive || self.outward
    }

    /// Whether a call needs the kernel's explicit approval first: a gated
    /// tool the HOST confirms. A `confirm: app` tool's confirmation is the
    /// owning app's own sheet, for callers of every kind (the host hands it
    /// to the app), so the kernel asks nothing.
    pub fn requires_kernel_approval(&self) -> bool {
        self.gated() && self.confirm != HostToolConfirm::App
    }
}

/// One call handed to the router.
#[derive(Debug, Clone, PartialEq)]
pub struct HostToolCall {
    /// The provider's tool-call id (the occurrence within the turn).
    pub tool_call_id: String,
    /// Declared name (`news.list`).
    pub name: String,
    /// The app that owns the tool.
    pub app: String,
    pub args: Value,
    pub risk: HostToolRisk,
    /// The app must confirm with the person itself (`confirm: app`,
    /// attended). `false` when the kernel already has the person's approval.
    pub confirm_required: bool,
    /// Destructive or outward: the host may still be waiting on the person,
    /// so the router honours an "awaiting confirmation" acknowledgement.
    pub gated: bool,
    /// `sha256:<hex>` of the arguments, for the host's own dedupe.
    pub args_digest: String,
}

/// How a routed call ended.
#[derive(Debug, Clone, PartialEq)]
pub enum HostToolCallOutcome {
    /// The host ran the tool; `data` is its structured result.
    Ok(Value),
    /// The call failed. `kind` is machine-readable (`host_error`,
    /// `timeout`, `host_unavailable`, `result_too_large`, `cancelled`, …).
    Error { kind: String, message: String },
}

/// One audit row, written by the router.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostToolAudit {
    pub tool: String,
    /// The app that owns the tool.
    pub app: String,
    pub tool_call_id: String,
    pub risk: &'static str,
    /// `allowed`, `approved`, `app_confirms`, `denied`, `expired`,
    /// `approval_unavailable`, `duplicate`, `busy`, `outcome_unknown_before`,
    /// `approved_after_unknown`, `not_background`, `invalid_args` (and
    /// `late_result`, written by the router).
    pub decision: &'static str,
    /// `ok`, `error:<kind>`, `unknown` (no answer in time; the app may have
    /// acted), or `not_called`.
    pub outcome: String,
    pub duration_ms: u64,
    pub args_bytes: usize,
    pub result_bytes: usize,
}

/// Result of [`HostToolRouter::claim_occurrence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OccurrenceClaim {
    Claimed,
    /// Already claimed: the call is a re-dispatch.
    Duplicate,
    /// Too many unexpired claims are held; the call is refused (host_busy).
    Busy,
}

/// The serve-side half: delivers calls to the host and records them.
#[async_trait]
pub trait HostToolRouter: Send + Sync {
    /// Claim the occurrence `(tool_call_id, args_digest)` in this turn.
    fn claim_occurrence(&self, tool_call_id: &str, args_digest: &str) -> OccurrenceClaim;
    /// Whether the same `(tool, args_digest)` ended `outcome_unknown` for
    /// this session (within the router's retention): the app may have acted.
    fn outcome_unknown_before(&self, tool: &str, args_digest: &str) -> bool;
    /// Remember that `(tool, args_digest)` ended `outcome_unknown`.
    fn mark_outcome_unknown(&self, tool: &str, args_digest: &str);
    /// Forget the marker (the person approved running it again).
    fn clear_outcome_unknown(&self, tool: &str, args_digest: &str);
    /// Deliver the call to the host and wait for its result (the router owns
    /// the timeout, cancellation and result size cap).
    async fn call(&self, call: HostToolCall) -> HostToolCallOutcome;
    /// Append one audit row.
    fn record(&self, audit: HostToolAudit);
    /// Upper bound of one host call, used as the tool's dispatch timeout.
    fn call_timeout(&self) -> Duration;
}

/// The calling side of a host-routed tool: the peer whose session calls it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostToolCaller {
    /// `app_peer` or `system` (a host session that is not a peer).
    pub kind: String,
    pub peer: Option<String>,
    pub session_id: String,
    pub context_id: Option<String>,
}

/// A host-declared app tool offered to the model.
pub struct HostRoutedTool {
    decl: HostToolDecl,
    caller: HostToolCaller,
    router: Arc<dyn HostToolRouter>,
    approval_ttl: Duration,
    /// The calling session is one of the app's interactive clients (an open
    /// request context of the peer).
    interactive_session: bool,
}

impl HostRoutedTool {
    pub fn new(
        decl: HostToolDecl,
        router: Arc<dyn HostToolRouter>,
        approval_ttl: Duration,
        interactive_session: bool,
    ) -> Self {
        Self {
            decl,
            caller: HostToolCaller::default(),
            router,
            approval_ttl,
            interactive_session,
        }
    }

    /// Who calls through this tool (shown on its approvals).
    pub fn with_caller(mut self, caller: HostToolCaller) -> Self {
        self.caller = caller;
        self
    }

    pub fn decl(&self) -> &HostToolDecl {
        &self.decl
    }

    fn refuse(
        &self,
        ctx: &ToolContext,
        started: Instant,
        args_bytes: usize,
        decision: &'static str,
        message: String,
    ) -> ToolResult {
        self.router.record(HostToolAudit {
            tool: self.decl.name.clone(),
            app: self.decl.owner_app().to_owned(),
            tool_call_id: ctx.tool_id.clone(),
            risk: self.decl.risk.as_str(),
            decision,
            outcome: "not_called".into(),
            duration_ms: started.elapsed().as_millis() as u64,
            args_bytes,
            result_bytes: 0,
        });
        ToolResult {
            output: message,
            success: false,
            structured_metadata: Some(json!({
                "kind": "peer_host_tool_refused",
                "tool": self.decl.name,
                "decision": decision,
            })),
            ..Default::default()
        }
    }
}

/// Minimal argument check against the declared schema: an object carrying
/// every `required` property. The host validates fully.
fn check_args(schema: &Value, args: &Value) -> Result<(), String> {
    let Some(object) = args.as_object() else {
        return Err("arguments must be a JSON object".into());
    };
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let missing: Vec<&str> = required
            .iter()
            .filter_map(Value::as_str)
            .filter(|key| !object.contains_key(*key))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "missing required argument(s): {}",
                missing.join(", ")
            ));
        }
    }
    Ok(())
}

#[async_trait]
impl Tool for HostRoutedTool {
    // The model-facing name: provider tool names cannot hold the `.` of the
    // declared name.
    #[allow(clippy::misnamed_getters)]
    fn name(&self) -> &str {
        &self.decl.model_name
    }

    fn description(&self) -> &str {
        &self.decl.description
    }

    fn input_schema(&self) -> Value {
        self.decl.input_schema.clone()
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        match self.decl.risk {
            HostToolRisk::Read => ConcurrencyClass::Safe,
            _ => ConcurrencyClass::Exclusive,
        }
    }

    fn execution_timeout_secs(&self) -> Option<u64> {
        Some(self.router.call_timeout().as_secs().saturating_add(5))
    }

    fn blocks_on_human_input(&self) -> bool {
        self.decl.gated()
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::HostRouted
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let started = Instant::now();
        let args_text = serde_json::to_string(args).unwrap_or_default();
        let args_bytes = args_text.len();
        if args_bytes > HOST_TOOL_MAX_ARGS_BYTES {
            return Ok(self.refuse(
                ctx,
                started,
                args_bytes,
                "invalid_args",
                format!(
                    "{}: arguments are {args_bytes} bytes (max {HOST_TOOL_MAX_ARGS_BYTES})",
                    self.decl.name
                ),
            ));
        }
        if let Err(err) = check_args(&self.decl.input_schema, args) {
            return Ok(self.refuse(
                ctx,
                started,
                args_bytes,
                "invalid_args",
                format!("{}: {err}", self.decl.name),
            ));
        }

        let approvals = TOOL_APPROVAL_CTX.try_with(Clone::clone).ok();
        let attended = self.interactive_session && approvals.is_some();
        if !attended && !self.decl.background {
            return Ok(self.refuse(
                ctx,
                started,
                args_bytes,
                "not_background",
                format!(
                    "{} may only run while the person is in the app (the app did not mark it background)",
                    self.decl.name
                ),
            ));
        }

        let args_digest = crate::approval::digest_tool_args(args);
        if self.decl.risk != HostToolRisk::Read {
            match self.router.claim_occurrence(&ctx.tool_id, &args_digest) {
                OccurrenceClaim::Claimed => {}
                OccurrenceClaim::Duplicate => {
                    return Ok(self.refuse(
                        ctx,
                        started,
                        args_bytes,
                        "duplicate",
                        format!(
                            "{}: this exact call was already submitted; it is not asked or sent twice",
                            self.decl.name
                        ),
                    ));
                }
                OccurrenceClaim::Busy => {
                    return Ok(self.refuse(
                        ctx,
                        started,
                        args_bytes,
                        "busy",
                        format!(
                            "{}: too many calls are being tracked right now (host_busy); try later",
                            self.decl.name
                        ),
                    ));
                }
            }
        }

        // A call whose earlier identical run ended with an unknown outcome
        // (timed out or interrupted while the app worked) is sent again only
        // after the person approves it knowing that.
        let unknown_before = self.decl.risk != HostToolRisk::Read
            && self
                .router
                .outcome_unknown_before(&self.decl.name, &args_digest);
        let decision = if self.decl.requires_kernel_approval() || unknown_before {
            let Some(requester) = approvals else {
                let (decision, message) = if unknown_before {
                    (
                        "outcome_unknown_before",
                        format!(
                            "{}: the same call with the same arguments already ended with an \
                             unknown outcome; the app may have done it. It is not sent again \
                             without the person's approval: check with a read tool or ask the \
                             person.",
                            self.decl.name
                        ),
                    )
                } else {
                    (
                        "approval_unavailable",
                        format!(
                            "{} needs the person's approval and no approval channel is available; it was not run",
                            self.decl.name
                        ),
                    )
                };
                return Ok(self.refuse(ctx, started, args_bytes, decision, message));
            };
            let pretty = serde_json::to_string_pretty(args).unwrap_or(args_text.clone());
            let warning = if unknown_before {
                "The same call with these exact arguments ran before and its outcome is \
                 UNKNOWN: the app may already have done it. Approve only if it should run \
                 again.\n\n"
            } else {
                ""
            };
            let request = ToolApprovalRequest {
                tool_id: ctx.tool_id.clone(),
                tool_name: self.decl.model_name.clone(),
                title: if unknown_before {
                    format!("Run {} again? (earlier outcome unknown)", self.decl.name)
                } else {
                    format!("Approve {}", self.decl.name)
                },
                body: format!(
                    "{warning}{} ({}{}) wants to run with these exact arguments:\n{pretty}",
                    self.decl.name,
                    self.decl.risk.as_str(),
                    if self.decl.outward { ", outward" } else { "" },
                ),
                command: None,
                cwd: None,
                once_only: true,
                host_tool: Some(octos_core::ui_protocol::ApprovalHostToolDetails {
                    app: self.decl.owner_app().to_owned(),
                    tool: self.decl.name.clone(),
                    args: args.clone(),
                    risk: self.decl.risk.as_str().to_owned(),
                    outward: self.decl.outward,
                    calling_kind: self.caller.kind.clone(),
                    calling_peer: self.caller.peer.clone(),
                    calling_session_id: self.caller.session_id.clone(),
                    context_id: self.caller.context_id.clone(),
                    tool_call_id: Some(ctx.tool_id.clone()),
                    outcome_unknown_before: unknown_before,
                }),
            };
            match tokio::time::timeout(self.approval_ttl, requester.request_approval(request)).await
            {
                Err(_) => {
                    return Ok(self.refuse(
                        ctx,
                        started,
                        args_bytes,
                        "expired",
                        format!(
                            "{}: the approval request expired before the person answered; it was not run",
                            self.decl.name
                        ),
                    ));
                }
                Ok(ToolApprovalDecision::Deny) => {
                    return Ok(self.refuse(
                        ctx,
                        started,
                        args_bytes,
                        "denied",
                        format!("{}: the person declined; it was not run", self.decl.name),
                    ));
                }
                Ok(ToolApprovalDecision::Approve) if unknown_before => {
                    self.router
                        .clear_outcome_unknown(&self.decl.name, &args_digest);
                    "approved_after_unknown"
                }
                Ok(ToolApprovalDecision::Approve) => "approved",
            }
        } else if self.decl.gated() {
            "app_confirms"
        } else {
            "allowed"
        };

        let outcome = self
            .router
            .call(HostToolCall {
                tool_call_id: ctx.tool_id.clone(),
                name: self.decl.name.clone(),
                app: self.decl.owner_app().to_owned(),
                args: args.clone(),
                risk: self.decl.risk,
                confirm_required: decision == "app_confirms",
                gated: self.decl.gated(),
                args_digest: args_digest.clone(),
            })
            .await;
        if matches!(&outcome, HostToolCallOutcome::Error { kind, .. } if kind == "outcome_unknown")
        {
            self.router
                .mark_outcome_unknown(&self.decl.name, &args_digest);
        }
        let (result, outcome_label, result_bytes) = match outcome {
            HostToolCallOutcome::Ok(data) => {
                let output = serde_json::to_string(&data).unwrap_or_default();
                let bytes = output.len();
                (
                    ToolResult {
                        output,
                        success: true,
                        structured_metadata: Some(json!({
                            "kind": "peer_host_tool",
                            "tool": self.decl.name,
                            "decision": decision,
                        })),
                        ..Default::default()
                    },
                    "ok".to_owned(),
                    bytes,
                )
            }
            HostToolCallOutcome::Error { kind, message } => (
                ToolResult {
                    output: format!("{} failed ({kind}): {message}", self.decl.name),
                    success: false,
                    structured_metadata: Some(json!({
                        "kind": "peer_host_tool_error",
                        "tool": self.decl.name,
                        "error_kind": kind,
                        "decision": decision,
                    })),
                    ..Default::default()
                },
                if kind == "outcome_unknown" {
                    "unknown".to_owned()
                } else {
                    format!("error:{kind}")
                },
                message.len(),
            ),
        };
        self.router.record(HostToolAudit {
            tool: self.decl.name.clone(),
            app: self.decl.owner_app().to_owned(),
            tool_call_id: ctx.tool_id.clone(),
            risk: self.decl.risk.as_str(),
            decision,
            outcome: outcome_label,
            duration_ms: started.elapsed().as_millis() as u64,
            args_bytes,
            result_bytes,
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolApprovalRequester;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeRouter {
        calls: Mutex<Vec<HostToolCall>>,
        audits: Mutex<Vec<HostToolAudit>>,
        claimed: Mutex<std::collections::HashSet<String>>,
        unknown: Mutex<std::collections::HashSet<String>>,
    }

    #[async_trait]
    impl HostToolRouter for FakeRouter {
        fn claim_occurrence(&self, id: &str, digest: &str) -> OccurrenceClaim {
            if self
                .claimed
                .lock()
                .unwrap()
                .insert(format!("{id}/{digest}"))
            {
                OccurrenceClaim::Claimed
            } else {
                OccurrenceClaim::Duplicate
            }
        }
        fn clear_outcome_unknown(&self, tool: &str, digest: &str) {
            self.unknown
                .lock()
                .unwrap()
                .remove(&format!("{tool}/{digest}"));
        }
        fn outcome_unknown_before(&self, tool: &str, digest: &str) -> bool {
            self.unknown
                .lock()
                .unwrap()
                .contains(&format!("{tool}/{digest}"))
        }
        fn mark_outcome_unknown(&self, tool: &str, digest: &str) {
            self.unknown
                .lock()
                .unwrap()
                .insert(format!("{tool}/{digest}"));
        }
        async fn call(&self, call: HostToolCall) -> HostToolCallOutcome {
            self.calls.lock().unwrap().push(call.clone());
            HostToolCallOutcome::Ok(json!({ "echo": call.args }))
        }
        fn record(&self, audit: HostToolAudit) {
            self.audits.lock().unwrap().push(audit);
        }
        fn call_timeout(&self) -> Duration {
            Duration::from_secs(30)
        }
    }

    impl FakeRouter {
        fn decisions(&self) -> Vec<&'static str> {
            self.audits
                .lock()
                .unwrap()
                .iter()
                .map(|a| a.decision)
                .collect()
        }
    }

    struct Approver {
        decision: Option<ToolApprovalDecision>,
        asked: AtomicUsize,
        last: Mutex<Option<ToolApprovalRequest>>,
    }

    impl Approver {
        fn new(decision: Option<ToolApprovalDecision>) -> Arc<Self> {
            Arc::new(Self {
                decision,
                asked: AtomicUsize::new(0),
                last: Mutex::new(None),
            })
        }
        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ToolApprovalRequester for Approver {
        async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
            self.asked.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = Some(request);
            match self.decision {
                Some(decision) => decision,
                None => std::future::pending().await,
            }
        }
    }

    fn decl(name: &str, risk: HostToolRisk) -> HostToolDecl {
        HostToolDecl {
            name: name.into(),
            app: String::new(),
            model_name: name.replace('.', "_"),
            description: format!("{name} tool"),
            input_schema: json!({"type": "object", "required": ["id"]}),
            output_schema: None,
            risk,
            background: false,
            outward: false,
            confirm: HostToolConfirm::Host,
        }
    }

    /// A tool called from one of the app's interactive clients.
    fn in_app(decl: HostToolDecl, router: &Arc<FakeRouter>, ttl: Duration) -> HostRoutedTool {
        HostRoutedTool::new(decl, router.clone(), ttl, true)
    }

    /// A tool called from the peer's own session (no interactive client).
    fn in_peer(decl: HostToolDecl, router: &Arc<FakeRouter>, ttl: Duration) -> HostRoutedTool {
        HostRoutedTool::new(decl, router.clone(), ttl, false)
    }

    fn ctx(id: &str) -> ToolContext {
        ToolContext {
            tool_id: id.into(),
            ..ToolContext::zero()
        }
    }

    async fn run(
        tool: &HostRoutedTool,
        approver: Option<Arc<Approver>>,
        id: &str,
        args: Value,
    ) -> ToolResult {
        let ctx = ctx(id);
        match approver {
            Some(approver) => TOOL_APPROVAL_CTX
                .scope(
                    approver as Arc<dyn ToolApprovalRequester>,
                    tool.execute_with_context(&ctx, &args),
                )
                .await
                .unwrap(),
            None => tool.execute_with_context(&ctx, &args).await.unwrap(),
        }
    }

    const TTL: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn should_run_read_and_act_tools_without_approval() {
        let router = Arc::new(FakeRouter::default());
        for risk in [HostToolRisk::Read, HostToolRisk::Act] {
            let tool = in_app(decl("news.list", risk), &router, TTL);
            let approver = Approver::new(Some(ToolApprovalDecision::Deny));
            let result = run(&tool, Some(approver.clone()), "c1", json!({"id": 1})).await;
            assert!(result.success, "{}", result.output);
            assert_eq!(approver.asked(), 0);
        }
        assert_eq!(router.calls.lock().unwrap().len(), 2);
        assert_eq!(router.calls.lock().unwrap()[0].name, "news.list");
    }

    #[tokio::test]
    async fn should_run_destructive_only_after_an_explicit_approve_with_the_exact_arguments() {
        let router = Arc::new(FakeRouter::default());
        let tool = in_app(decl("mail.send", HostToolRisk::Destructive), &router, TTL);
        assert!(tool.blocks_on_human_input());
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        let result = run(
            &tool,
            Some(approver.clone()),
            "c1",
            json!({"id": "draft-7"}),
        )
        .await;
        assert!(result.success);
        let asked = approver.last.lock().unwrap().clone().unwrap();
        assert_eq!(asked.tool_id, "c1");
        assert_eq!(asked.tool_name, "mail_send");
        assert!(asked.body.contains("\"draft-7\""), "{}", asked.body);
        assert!(asked.once_only, "never answered or remembered by a scope");
        let calls = router.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].confirm_required, "the person already approved");
        assert_eq!(router.decisions(), ["approved"]);
    }

    #[tokio::test]
    async fn should_describe_the_owning_app_the_tool_and_the_caller_when_asking_for_approval() {
        let router = Arc::new(FakeRouter::default());
        // A cross-app tool: Mail's `mail.send`, registered on the News peer.
        let mut cross = decl("mail.send", HostToolRisk::Destructive);
        cross.app = "mail".into();
        cross.outward = true;
        let tool = in_app(cross, &router, TTL).with_caller(HostToolCaller {
            kind: "app_peer".into(),
            peer: Some("news".into()),
            session_id: "dev:api:host#peerctx-news.ui-1".into(),
            context_id: Some("ui-1".into()),
        });
        let approver = Approver::new(Some(ToolApprovalDecision::Deny));
        run(&tool, Some(approver.clone()), "c9", json!({"id": "d-1"})).await;
        let asked = approver.last.lock().unwrap().clone().unwrap();
        let details = asked
            .host_tool
            .expect("host-tool details for the host's sheet");
        assert_eq!(details.app, "mail");
        assert_eq!(details.tool, "mail.send");
        assert_eq!(details.args, json!({"id": "d-1"}));
        assert_eq!(details.risk, "destructive");
        assert!(details.outward);
        assert_eq!(details.calling_kind, "app_peer");
        assert_eq!(details.calling_peer.as_deref(), Some("news"));
        assert_eq!(details.calling_session_id, "dev:api:host#peerctx-news.ui-1");
        assert_eq!(details.context_id.as_deref(), Some("ui-1"));
        assert_eq!(details.tool_call_id.as_deref(), Some("c9"));
        // Without an explicit `app`, the owner is the name's first segment.
        assert_eq!(decl("news.list", HostToolRisk::Read).owner_app(), "news");
    }

    #[tokio::test]
    async fn should_never_call_the_host_when_approval_is_declined_expired_or_unavailable() {
        let router = Arc::new(FakeRouter::default());
        let mut gated = decl("mail.send", HostToolRisk::Destructive);
        gated.background = true;
        let tool = in_app(gated, &router, Duration::from_millis(50));

        let denied = run(
            &tool,
            Some(Approver::new(Some(ToolApprovalDecision::Deny))),
            "c1",
            json!({"id": 1}),
        )
        .await;
        assert!(!denied.success && denied.output.contains("declined"));

        let expired = run(&tool, Some(Approver::new(None)), "c2", json!({"id": 1})).await;
        assert!(!expired.success && expired.output.contains("expired"));

        let unavailable = run(&tool, None, "c3", json!({"id": 1})).await;
        assert!(!unavailable.success && unavailable.output.contains("no approval channel"));

        assert!(router.calls.lock().unwrap().is_empty());
        assert_eq!(
            router.decisions(),
            ["denied", "expired", "approval_unavailable"]
        );
    }

    #[tokio::test]
    async fn should_gate_an_outward_act_tool_like_destructive() {
        let router = Arc::new(FakeRouter::default());
        let mut post = decl("social.post", HostToolRisk::Act);
        post.outward = true;
        let tool = in_app(post, &router, TTL);
        let approver = Approver::new(Some(ToolApprovalDecision::Deny));
        let result = run(&tool, Some(approver.clone()), "c1", json!({"id": 1})).await;
        assert!(!result.success);
        assert_eq!(approver.asked(), 1);
        assert!(router.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn should_raise_one_approval_per_occurrence() {
        let router = Arc::new(FakeRouter::default());
        let tool = in_app(decl("mail.send", HostToolRisk::Destructive), &router, TTL);
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        assert!(
            run(&tool, Some(approver.clone()), "c1", json!({"id": 1}))
                .await
                .success
        );
        let again = run(&tool, Some(approver.clone()), "c1", json!({"id": 1})).await;
        assert!(!again.success && again.output.contains("not asked or sent twice"));
        assert_eq!(approver.asked(), 1);
        assert_eq!(router.calls.lock().unwrap().len(), 1);

        // A provider reusing `c1` with other arguments is a new call.
        assert!(
            run(&tool, Some(approver.clone()), "c1", json!({"id": 2}))
                .await
                .success
        );
        assert_eq!(approver.asked(), 2);
        assert!(
            router.calls.lock().unwrap()[0]
                .args_digest
                .starts_with("sha256:")
        );
    }

    #[test]
    fn should_digest_arguments_independently_of_key_order() {
        let a: Value = serde_json::from_str(r#"{"b": 1, "a": {"y": 2, "x": 3}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a": {"x": 3, "y": 2}, "b": 1}"#).unwrap();
        assert_eq!(
            crate::approval::digest_tool_args(&a),
            crate::approval::digest_tool_args(&b)
        );
        // Canonicalization must also reach objects nested inside arrays.
        let c: Value = serde_json::from_str(r#"{"list": [{"y": 2, "x": 3}]}"#).unwrap();
        let d: Value = serde_json::from_str(r#"{"list": [{"x": 3, "y": 2}]}"#).unwrap();
        assert_eq!(
            crate::approval::digest_tool_args(&c),
            crate::approval::digest_tool_args(&d)
        );
    }

    #[test]
    fn should_still_digest_array_order_as_significant() {
        let a = json!({"items": [1, 2, 3]});
        let b = json!({"items": [3, 2, 1]});
        assert_ne!(
            crate::approval::digest_tool_args(&a),
            crate::approval::digest_tool_args(&b)
        );
    }

    #[tokio::test]
    async fn should_send_an_act_call_once_but_let_reads_repeat() {
        let router = Arc::new(FakeRouter::default());
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        let act = in_app(decl("news.topics_set", HostToolRisk::Act), &router, TTL);
        assert!(
            run(&act, Some(approver.clone()), "c1", json!({"id": 1}))
                .await
                .success
        );
        let again = run(&act, Some(approver.clone()), "c1", json!({"id": 1})).await;
        assert!(!again.success && again.output.contains("not asked or sent twice"));

        let read = in_app(decl("news.list", HostToolRisk::Read), &router, TTL);
        assert!(
            run(&read, Some(approver.clone()), "c2", json!({"id": 1}))
                .await
                .success
        );
        assert!(
            run(&read, Some(approver.clone()), "c2", json!({"id": 1}))
                .await
                .success
        );
        assert_eq!(router.calls.lock().unwrap().len(), 3);
        assert_eq!(approver.asked(), 0);
    }

    #[tokio::test]
    async fn should_report_an_unanswered_act_call_as_unknown_not_failed() {
        struct SilentRouter(Mutex<Vec<HostToolAudit>>, Mutex<Vec<String>>);
        #[async_trait]
        impl HostToolRouter for SilentRouter {
            fn claim_occurrence(&self, _: &str, _: &str) -> OccurrenceClaim {
                OccurrenceClaim::Claimed
            }
            fn clear_outcome_unknown(&self, tool: &str, digest: &str) {
                self.1
                    .lock()
                    .unwrap()
                    .retain(|k| k != &format!("{tool}/{digest}"));
            }
            fn outcome_unknown_before(&self, tool: &str, digest: &str) -> bool {
                self.1.lock().unwrap().contains(&format!("{tool}/{digest}"))
            }
            fn mark_outcome_unknown(&self, tool: &str, digest: &str) {
                self.1.lock().unwrap().push(format!("{tool}/{digest}"));
            }
            async fn call(&self, _: HostToolCall) -> HostToolCallOutcome {
                HostToolCallOutcome::Error {
                    kind: "outcome_unknown".into(),
                    message: "do not retry".into(),
                }
            }
            fn record(&self, audit: HostToolAudit) {
                self.0.lock().unwrap().push(audit);
            }
            fn call_timeout(&self) -> Duration {
                Duration::from_secs(1)
            }
        }
        let router = Arc::new(SilentRouter(Mutex::new(Vec::new()), Mutex::new(Vec::new())));
        let mut act = decl("news.topics_set", HostToolRisk::Act);
        act.background = true;
        let tool = HostRoutedTool::new(act, router.clone(), TTL, true);
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        let result = run(&tool, Some(approver.clone()), "c1", json!({"id": 1})).await;
        assert!(!result.success && result.output.contains("do not retry"));
        assert_eq!(router.0.lock().unwrap()[0].outcome, "unknown");
        assert_eq!(approver.asked(), 0, "an act call needs no approval");

        // A retry under a NEW tool-call id with the same arguments is not
        // sent without the person: refused with no approval channel...
        let retry = run(&tool, None, "c2", json!({"id": 1})).await;
        assert!(!retry.success && retry.output.contains("not sent again"));
        assert_eq!(
            router.0.lock().unwrap()[1].decision,
            "outcome_unknown_before"
        );

        // ...declined when the person says no to a request that states the
        // earlier outcome is unknown...
        let decline = Approver::new(Some(ToolApprovalDecision::Deny));
        let declined = run(&tool, Some(decline.clone()), "c3", json!({"id": 1})).await;
        assert!(!declined.success);
        let asked = decline.last.lock().unwrap().clone().unwrap();
        assert!(
            asked.body.contains("UNKNOWN") && asked.once_only,
            "{}",
            asked.body
        );

        // ...and sent again only after the person approves exactly that.
        run(&tool, Some(approver.clone()), "c4", json!({"id": 1})).await;
        assert_eq!(approver.asked(), 1);
        assert_eq!(
            router.0.lock().unwrap()[3].decision,
            "approved_after_unknown"
        );

        // Other arguments are a different call.
        run(&tool, Some(approver), "c5", json!({"id": 2})).await;
        assert_eq!(router.0.lock().unwrap()[4].decision, "allowed");
    }

    #[tokio::test]
    async fn should_let_the_app_confirm_when_the_person_is_present() {
        let router = Arc::new(FakeRouter::default());
        let mut send = decl("rinx.send_message", HostToolRisk::Destructive);
        send.confirm = HostToolConfirm::App;
        let tool = in_app(send, &router, TTL);
        let approver = Approver::new(Some(ToolApprovalDecision::Deny));
        assert!(
            run(&tool, Some(approver.clone()), "c1", json!({"id": 1}))
                .await
                .success
        );
        assert_eq!(approver.asked(), 0, "never asked twice");
        assert!(router.calls.lock().unwrap()[0].confirm_required);
        assert_eq!(router.decisions(), ["app_confirms"]);
    }

    #[tokio::test]
    async fn should_hand_an_app_confirmed_call_to_the_owning_app_whoever_calls() {
        let router = Arc::new(FakeRouter::default());
        let mut send = decl("rinx.send_message", HostToolRisk::Destructive);
        send.confirm = HostToolConfirm::App;
        send.background = true;

        // The peer's own session (a background run, the system agent's
        // input), with or without an approval bridge: never a kernel
        // approval; the owning app confirms with its own sheet.
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        let tool = in_peer(send.clone(), &router, TTL);
        assert!(
            run(&tool, Some(approver.clone()), "c1", json!({"id": 1}))
                .await
                .success
        );
        let tool = in_app(send, &router, TTL);
        assert!(run(&tool, None, "c2", json!({"id": 1})).await.success);
        assert_eq!(approver.asked(), 0);
        let calls = router.calls.lock().unwrap();
        assert!(calls.iter().all(|c| c.confirm_required), "{calls:?}");
        assert_eq!(router.decisions(), ["app_confirms", "app_confirms"]);
    }

    #[tokio::test]
    async fn should_refuse_foreground_tools_unattended_and_bad_arguments() {
        let router = Arc::new(FakeRouter::default());
        let approver = Approver::new(Some(ToolApprovalDecision::Approve));
        let foreground = decl("news.list", HostToolRisk::Read);

        let no_bridge = run(
            &in_app(foreground.clone(), &router, TTL),
            None,
            "c1",
            json!({"id": 1}),
        )
        .await;
        assert!(!no_bridge.success && no_bridge.output.contains("background"));
        let peer_session = run(
            &in_peer(foreground.clone(), &router, TTL),
            Some(approver.clone()),
            "c2",
            json!({"id": 1}),
        )
        .await;
        assert!(!peer_session.success && peer_session.output.contains("background"));

        let tool = in_app(foreground, &router, TTL);
        let missing = run(&tool, Some(approver.clone()), "c3", json!({})).await;
        assert!(!missing.success && missing.output.contains("id"));
        let huge = json!({"id": "x".repeat(HOST_TOOL_MAX_ARGS_BYTES)});
        let too_big = run(&tool, Some(approver.clone()), "c4", huge).await;
        assert!(!too_big.success && too_big.output.contains("bytes"));
        assert!(router.calls.lock().unwrap().is_empty());

        let mut background = decl("news.list", HostToolRisk::Read);
        background.background = true;
        let tool = in_peer(background, &router, TTL);
        assert!(run(&tool, None, "c5", json!({"id": 1})).await.success);
    }

    #[test]
    fn should_mark_a_host_routed_tool_by_origin_whatever_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = crate::tools::ToolRegistry::with_builtins(dir.path());
        assert_eq!(registry.origin("read_file"), Some(ToolOrigin::Builtin));
        let router = Arc::new(FakeRouter::default());
        registry.register(in_app(decl("news.list", HostToolRisk::Read), &router, TTL));
        // Even a host tool that took a built-in tool's name.
        let mut masquerade = decl("search.grep", HostToolRisk::Read);
        masquerade.model_name = "grep".into();
        registry.register(in_app(masquerade, &router, TTL));
        assert_eq!(registry.origin("news_list"), Some(ToolOrigin::HostRouted));
        assert_eq!(registry.origin("grep"), Some(ToolOrigin::HostRouted));
        assert_eq!(registry.origin("missing"), None);

        // Snapshots carry the origin.
        let snapshot = registry.snapshot_excluding(&["news_list"]);
        assert_eq!(snapshot.origin("grep"), Some(ToolOrigin::HostRouted));
        assert_eq!(snapshot.origin("news_list"), None);

        // Selecting by origin drops every host tool, keeps the built-ins.
        registry.retain_builtin(|_| true);
        let names = registry.tool_names();
        assert!(!names.iter().any(|n| n == "news_list" || n == "grep"));
        assert!(names.iter().any(|n| n == "read_file"));
        assert_eq!(registry.origin("grep"), None);
    }
}
