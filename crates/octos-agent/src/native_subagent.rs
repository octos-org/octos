//! Process-local executable handles for native children. Durable task rows alone
//! are not executors: after restart follow-up explicitly reports unavailable.
use crate::steering::SharedSteerBuffer;
use crate::task_supervisor::{TaskCancelToken, TaskSupervisor};
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

pub(crate) type ContinueChild = Arc<
    dyn Fn(Vec<String>, Arc<TaskSupervisor>) -> BoxFuture<'static, Result<String, String>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub(crate) struct NativeAuthority {
    pub owner: Option<String>,
    pub scope: Option<Arc<octos_core::SessionScope>>,
    pub permissions: crate::tools::ToolPermissions,
}
impl From<&crate::tools::ToolContext> for NativeAuthority {
    fn from(ctx: &crate::tools::ToolContext) -> Self {
        Self {
            owner: ctx.parent_session_key.clone(),
            scope: ctx.session_scope.clone(),
            permissions: ctx.permissions.clone(),
        }
    }
}
impl NativeAuthority {
    fn validate(&self, caller: &Self) -> Result<(), String> {
        if self.owner != caller.owner {
            return Err("subagent belongs to a different parent session".into());
        }
        if self.scope != caller.scope {
            return Err("subagent parent workspace or session scope has changed; retained permissions cannot be reused".into());
        }
        if self.permissions != caller.permissions {
            return Err("subagent parent tool permissions have changed; retained permissions cannot be reused".into());
        }
        Ok(())
    }
}

struct Control {
    authority: NativeAuthority,
    buffer: SharedSteerBuffer,
    cancel: Arc<TaskCancelToken>,
    execution_supervisor: std::sync::Weak<TaskSupervisor>,
    finished: AtomicBool,
    changed: tokio::sync::Notify,
    state: tokio::sync::Mutex<State>,
}
struct State {
    continuation: Option<ContinueChild>,
    successor: Option<String>,
    continuation_error: Option<String>,
    caller_supervisor: Option<Arc<TaskSupervisor>>,
}
fn controls() -> &'static Mutex<HashMap<String, Arc<Control>>> {
    static CONTROLS: OnceLock<Mutex<HashMap<String, Arc<Control>>>> = OnceLock::new();
    CONTROLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Dropped after the worker, output lease, validators and notification finish.
/// Late steering remains queued and is picked up by the completion watcher.
pub(crate) struct ExecutionGuard(Arc<Control>);
impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.0.finished.store(true, Ordering::Release);
        self.0.changed.notify_one();
    }
}

pub(crate) fn register(
    id: String,
    authority: NativeAuthority,
    buffer: SharedSteerBuffer,
    supervisor: Arc<TaskSupervisor>,
    continuation: ContinueChild,
    chain_cancel: Option<Arc<TaskCancelToken>>,
) -> ExecutionGuard {
    let control = Arc::new(Control {
        authority,
        buffer,
        cancel: chain_cancel.unwrap_or_else(|| supervisor.cancel_token(&id)),
        execution_supervisor: Arc::downgrade(&supervisor),
        finished: AtomicBool::new(false),
        changed: tokio::sync::Notify::new(),
        state: tokio::sync::Mutex::new(State {
            continuation: Some(continuation),
            successor: None,
            continuation_error: None,
            caller_supervisor: Some(supervisor.clone()),
        }),
    });
    controls()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone(), control.clone());
    let watcher = control.clone();
    tokio::spawn(async move {
        watcher.changed.notified().await;
        if !watcher.cancel.is_cancelled() {
            // Same admission lock as explicit requests; a racing request can
            // create at most one successor and all others route to that child.
            if let Err(error) = follow_up(&id, &watcher.authority, None, supervisor, true).await {
                if let Some(supervisor) = watcher.execution_supervisor.upgrade() {
                    let previous = supervisor
                        .get_task(&id)
                        .and_then(|t| t.final_output)
                        .unwrap_or_default();
                    supervisor.record_final_output(
                        &id,
                        &format!("{previous}\n\nPending feedback was not executed: {error}"),
                    );
                }
                tracing::warn!(task_id = %id, %error, "late native child input could not continue");
            }
        }
    });
    ExecutionGuard(control)
}

#[derive(Debug)]
pub(crate) struct Accepted {
    pub task_id: String,
    pub resumed: bool,
    pub queued: bool,
}

pub(crate) async fn follow_up(
    id: &str,
    authority: &NativeAuthority,
    message: Option<String>,
    supervisor: Arc<TaskSupervisor>,
    pending_only: bool,
) -> Result<Accepted, String> {
    let mut target = id.to_string();
    loop {
        let control = controls().lock().unwrap_or_else(|e| e.into_inner()).get(&target).cloned()
            .ok_or_else(|| "native subagent executor unavailable; task metadata cannot deliver input or resume execution".to_string())?;
        control.authority.validate(authority)?;
        let mut state = control.state.lock().await;
        if control.cancel.is_cancelled() {
            return Err("cancelled subagent cannot accept input or resume".into());
        }
        if let Some(next) = state.successor.as_ref() {
            target = next.clone();
            continue;
        }
        if !control.finished.load(Ordering::Acquire) {
            if let Some(message) = message {
                control.buffer.push(message);
                state.continuation_error = None;
                state.caller_supervisor = Some(supervisor);
                return Ok(Accepted {
                    task_id: target,
                    resumed: false,
                    queued: true,
                });
            }
            return Ok(Accepted {
                task_id: target,
                resumed: false,
                queued: false,
            });
        }
        if pending_only && control.buffer.is_empty() {
            return Ok(Accepted {
                task_id: target,
                resumed: false,
                queued: false,
            });
        }
        let caller = if pending_only {
            state
                .caller_supervisor
                .as_ref()
                .ok_or("native continuation has no caller supervisor")?
                .clone()
        } else {
            supervisor
        };
        let mut messages = control.buffer.drain();
        if let Some(message) = message {
            messages.push(message);
        }
        if messages.is_empty() {
            messages.push("Continue the assigned task from your previous work.".into());
        }
        let next =
            (state
                .continuation
                .as_ref()
                .ok_or("native executor was closed")?)(messages.clone(), caller.clone())
            .await;
        match next {
            Ok(next) => {
                if control.cancel.is_cancelled() {
                    let _ = caller.cancel(&next);
                    cancel(&next);
                    return Err("subagent was cancelled during continuation admission".into());
                }
                state.successor = Some(next.clone());
                state.continuation = None;
                state.caller_supervisor = None;
                return Ok(Accepted {
                    task_id: next,
                    resumed: true,
                    queued: true,
                });
            }
            Err(error) => {
                state.continuation_error = Some(error.clone());
                // A refused admission does not silently discard feedback.
                for message in messages {
                    control.buffer.push(message);
                }
                return Err(error);
            }
        }
    }
}

pub(crate) struct NativeStatus {
    pub task: crate::task_supervisor::BackgroundTask,
    pub pending: bool,
    pub error: Option<String>,
}

/// Resolve predecessors to the actual latest execution. A queued handoff is
/// nonterminal even before its successor ID has been allocated.
pub(crate) async fn status(
    id: &str,
    authority: &NativeAuthority,
) -> Result<Option<NativeStatus>, String> {
    let mut target = id.to_owned();
    loop {
        let control = controls()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&target)
            .cloned();
        let Some(control) = control else {
            return Ok(None);
        };
        control.authority.validate(authority)?;
        let state = control.state.lock().await;
        if let Some(next) = &state.successor {
            target = next.clone();
            continue;
        }
        let task = control
            .execution_supervisor
            .upgrade()
            .and_then(|s| s.get_task(&target));
        return Ok(task.map(|task| NativeStatus {
            task,
            pending: !control.buffer.is_empty() && state.continuation_error.is_none(),
            error: state.continuation_error.clone(),
        }));
    }
}

/// The supervisor can be a fresh per-turn instance restored from disk. Bridge
/// its cancellation to the actual process-local worker token as well.
pub(crate) fn cancel(id: &str) {
    let retired = {
        let mut registry = controls().lock().unwrap_or_else(|e| e.into_inner());
        let Some(chain) = registry.get(id).map(|control| control.cancel.clone()) else {
            return;
        };
        let retired = registry
            .iter()
            .filter(|(_, c)| Arc::ptr_eq(&c.cancel, &chain))
            .map(|(id, c)| (id.clone(), c.clone()))
            .collect::<Vec<_>>();
        registry.retain(|_, control| !Arc::ptr_eq(&control.cancel, &chain));
        retired
    };
    // Update the execution owner's record too: per-turn restored supervisors
    // have independent task maps. Otherwise its late failure would overwrite
    // the caller's Cancelled snapshot on their shared ledger.
    for (id, control) in &retired {
        if let Some(supervisor) = control.execution_supervisor.upgrade() {
            let _ = supervisor.cancel(id);
        }
    }
    for (_, control) in retired {
        control.cancel.cancel();
    }
}

pub(crate) fn cancellation_token(id: &str) -> Option<Arc<TaskCancelToken>> {
    controls()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .map(|control| control.cancel.clone())
}

/// Closing is distinct from cancelling one execution: terminal handles still
/// own resumable context and must release it. Lock admission before retiring
/// the alias chain, including a successor admitted by a racing resume.
pub(crate) async fn close(id: &str, authority: &NativeAuthority) -> Result<bool, String> {
    let mut target = id.to_string();
    loop {
        let control = controls()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&target)
            .cloned();
        let Some(control) = control else {
            return Ok(false);
        };
        control.authority.validate(authority)?;
        let mut state = control.state.lock().await;
        if let Some(next) = &state.successor {
            target = next.clone();
            continue;
        }
        control.cancel.cancel();
        if let Some(supervisor) = control.execution_supervisor.upgrade() {
            let _ = supervisor.cancel(&target);
        }
        cancel(&target);
        state.continuation = None;
        state.caller_supervisor = None;
        return Ok(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority() -> NativeAuthority {
        NativeAuthority {
            owner: Some("race-owner".into()),
            scope: None,
            permissions: crate::tools::ToolPermissions::allow_all(),
        }
    }

    #[tokio::test]
    async fn should_not_lose_feedback_admitted_across_native_completion() {
        let supervisor = Arc::new(TaskSupervisor::new());
        let id = supervisor.register("spawn", "completion-race", Some("race-owner"));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(tokio::sync::Notify::new());
        let callback: ContinueChild = {
            let delivered = delivered.clone();
            let entered = entered.clone();
            Arc::new(move |messages, _| {
                let delivered = delivered.clone();
                let entered = entered.clone();
                Box::pin(async move {
                    *delivered.lock().unwrap() = messages;
                    entered.notify_one();
                    Ok("next".into())
                })
            })
        };
        let buffer = Arc::new(crate::steering::SteerBuffer::default());
        let guard = register(
            id.clone(),
            authority(),
            buffer.clone(),
            supervisor,
            callback,
            None,
        );
        let control = controls().lock().unwrap().get(&id).unwrap().clone();
        // This is the admitted sender between its active-state read and push.
        let admission = control.state.lock().await;
        assert!(!control.finished.load(Ordering::Acquire));
        drop(guard);
        tokio::task::yield_now().await;
        buffer.push("feedback at the completion boundary".into());
        drop(admission);
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        assert_eq!(
            *delivered.lock().unwrap(),
            ["feedback at the completion boundary"]
        );
        cancel(&id);
    }

    #[tokio::test]
    async fn should_cancel_successor_when_native_admission_is_cancelled() {
        let supervisor = Arc::new(TaskSupervisor::new());
        let id = supervisor.register("spawn", "admission-race", Some("race-owner"));
        supervisor.mark_completed(&id, vec![]);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let callback: ContinueChild = {
            let entered = entered.clone();
            let release = release.clone();
            Arc::new(move |_, supervisor| {
                let entered = entered.clone();
                let release = release.clone();
                Box::pin(async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(supervisor.register("spawn", "successor", Some("race-owner")))
                })
            })
        };
        let guard = register(
            id.clone(),
            authority(),
            Arc::new(crate::steering::SteerBuffer::default()),
            supervisor.clone(),
            callback,
            None,
        );
        drop(guard);
        let pending = {
            let id = id.clone();
            let supervisor = supervisor.clone();
            tokio::spawn(async move {
                follow_up(&id, &authority(), Some("repair".into()), supervisor, false).await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        cancel(&id);
        release.notify_one();
        assert!(pending.await.unwrap().is_err());
        let tasks = supervisor.get_all_tasks();
        assert_eq!(tasks.len(), 2);
        assert_eq!(
            tasks.iter().find(|task| task.id != id).unwrap().status,
            crate::TaskStatus::Cancelled
        );
    }
}

#[cfg(test)]
mod failed_handoff_tests {
    use super::*;
    #[tokio::test]
    async fn should_report_failed_native_handoff_instead_of_completed_predecessor() {
        let temp = tempfile::tempdir().unwrap();
        let registry = crate::tools::ToolRegistry::with_builtins(temp.path());
        let supervisor = registry.supervisor();
        let id = supervisor.register("spawn", "failed-handoff", Some("handoff-owner"));
        supervisor.record_final_output(&id, "old answer");
        supervisor.mark_completed(&id, Vec::new());
        let ctx = crate::tools::ToolContext {
            task_supervisor: Some(supervisor.clone()),
            parent_session_key: Some("handoff-owner".into()),
            ..crate::tools::ToolContext::zero()
        };
        let authority = NativeAuthority::from(&ctx);
        let guard = register(
            id.clone(),
            authority.clone(),
            Arc::new(crate::steering::SteerBuffer::default()),
            supervisor.clone(),
            Arc::new(|_, _| {
                Box::pin(async { Err("output claim is still owned by another worker".into()) })
            }),
            None,
        );
        let accepted = follow_up(
            &id,
            &authority,
            Some("repair".into()),
            supervisor.clone(),
            false,
        )
        .await
        .unwrap();
        assert!(accepted.queued);
        drop(guard);
        let waited = registry
            .execute_with_context(
                &ctx,
                "wait_agent",
                &serde_json::json!({"agent_id":id,"timeout_ms":5000}),
            )
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&waited.output).unwrap();
        assert_eq!(body["agents"][0]["status"], "failed");
        assert_eq!(body["agents"][0]["continuation_failed"], true);
        assert_eq!(body["agents"][0]["execution_status"], "completed");
        assert!(
            body["agents"][0]["error"]
                .as_str()
                .unwrap()
                .contains("claim")
        );
        assert_eq!(
            supervisor.get_task(&id).unwrap().status,
            crate::TaskStatus::Completed
        );
        assert_eq!(supervisor.get_all_tasks().len(), 1);
        cancel(&id);
    }
}
