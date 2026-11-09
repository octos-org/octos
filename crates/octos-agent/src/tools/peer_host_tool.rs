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
