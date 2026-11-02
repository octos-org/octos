//! Connection-independent ownership of AppUI background work across turns.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use octos_agent::{TaskCancelError, TaskSupervisor};
use octos_core::{SessionKey, ui_protocol::TurnId};

type Scope = (SessionKey, String);

#[derive(Default)]
pub(crate) struct BackgroundWorkStore {
    turns: Mutex<HashMap<Scope, Vec<Weak<BackgroundWork>>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Active,
    Cancelling,
    Revoked,
}

pub(crate) struct BackgroundWork {
    session: SessionKey,
    turn_id: TurnId,
    supervisor: Weak<TaskSupervisor>,
    phase: Mutex<Phase>,
}

impl BackgroundWorkStore {
    pub(crate) fn register(
        &self,
        session: &SessionKey,
        profile: &str,
        turn_id: &TurnId,
        supervisor: &Arc<TaskSupervisor>,
    ) -> Arc<BackgroundWork> {
        let work = Arc::new(BackgroundWork {
            session: session.clone(),
            turn_id: turn_id.clone(),
            supervisor: Arc::downgrade(supervisor),
            phase: Mutex::new(Phase::Active),
        });
        let mut turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
        turns.retain(|_, entries| {
            entries.retain(|entry| entry.strong_count() > 0);
            !entries.is_empty()
        });
        turns
            .entry((session.clone(), profile.to_owned()))
            .or_default()
            .push(Arc::downgrade(&work));
        // A browser may still be finishing a foreground turn in this session.
        // Its old supervisor must stop another child at its running transition
        // after the snapshot below was cancelled. A new authenticated turn gets a fresh lease.
        let registration_work = work.clone();
        supervisor.set_on_change_listener("ingress_revocation", move |task| {
            if !task.status.is_terminal()
                && *registration_work
                    .phase
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    != Phase::Active
            {
                registration_work.cancel_task(&task.id);
            }
        });
        work
    }

    pub(crate) fn revoke(&self, session: &SessionKey, profile: &str) -> Vec<TurnId> {
        let turns = self
            .turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(session.clone(), profile.to_owned()))
            .unwrap_or_default();
        let live: Vec<_> = turns
            .into_iter()
            .filter_map(|work| work.upgrade())
            .collect();
        // Serialize with terminal callbacks before cancelling or purging the
        // queue. A callback either finished enqueueing before this barrier or
        // observes revocation; it cannot enqueue after the subsequent purge.
        for work in &live {
            *work.phase.lock().unwrap_or_else(|e| e.into_inner()) = Phase::Cancelling;
        }
        for work in &live {
            if let Some(supervisor) = work.supervisor.upgrade() {
                for task in supervisor.get_tasks_for_session(&work.session.0) {
                    if !task.status.is_terminal() {
                        work.cancel_task(&task.id);
                    }
                }
            }
        }
        // Cancellation notifications may publish synchronously above so task
        // state remains observable. Close every producer before the caller
        // purges pending continuations under the admission lock.
        for work in &live {
            *work.phase.lock().unwrap_or_else(|e| e.into_inner()) = Phase::Revoked;
        }
        live.into_iter().map(|work| work.turn_id.clone()).collect()
    }
}

impl BackgroundWork {
    pub(crate) fn while_authorized(&self, publish: impl FnOnce()) {
        let phase = self.phase.lock().unwrap_or_else(|e| e.into_inner());
        if *phase != Phase::Revoked {
            publish();
        }
    }

    fn cancel_task(&self, task_id: &str) {
        let Some(supervisor) = self.supervisor.upgrade() else {
            return; // The owning runtime has already dropped its supervisor.
        };
        match supervisor.cancel(task_id) {
            Ok(()) | Err(TaskCancelError::AlreadyTerminal) => {}
            Err(error) => {
                tracing::warn!(%task_id, %error, "revoked ingress task cancellation failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_agent::TaskStatus;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn ingress_revocation_cancels_old_turns_but_not_other_sessions_or_profiles() {
        let store = BackgroundWorkStore::default();
        let session = SessionKey("bare-session".into());
        let other = SessionKey("other-session".into());
        let mut supervisors = Vec::new();
        for (scope, profile) in [
            (&session, "a"),
            (&session, "a"),
            (&session, "b"),
            (&other, "a"),
        ] {
            let supervisor = Arc::new(TaskSupervisor::new());
            let work = store.register(scope, profile, &TurnId::new(), &supervisor);
            let task = supervisor.register("spawn", "child", Some(&scope.0));
            supervisor.mark_running(&task);
            // The foreground turn can drop its lease. The worker's supervisor
            // still owns the callbacks and must remain cancellable.
            drop(work);
            supervisors.push((supervisor, task));
        }
        store.revoke(&session, "a");
        for (i, (supervisor, task)) in supervisors.iter().enumerate() {
            assert_eq!(
                supervisor.get_task(task).unwrap().status,
                if i < 2 {
                    TaskStatus::Cancelled
                } else {
                    TaskStatus::Running
                }
            );
        }
        let late = supervisors[0]
            .0
            .register("spawn", "late-child", Some(&session.0));
        supervisors[0].0.mark_running(&late);
        assert_eq!(
            supervisors[0].0.get_task(&late).unwrap().status,
            TaskStatus::Cancelled
        );
        let fresh = Arc::new(TaskSupervisor::new());
        let _fresh_work = store.register(&session, "a", &TurnId::new(), &fresh);
        let fresh_task = fresh.register("spawn", "fresh-child", Some(&session.0));
        fresh.mark_running(&fresh_task);
        assert_eq!(
            fresh.get_task(&fresh_task).unwrap().status,
            TaskStatus::Running
        );
    }

    #[test]
    fn ingress_revocation_closes_terminal_producers_after_cancellation_notification() {
        let store = BackgroundWorkStore::default();
        let session = SessionKey("terminal-race".into());
        let supervisor = Arc::new(TaskSupervisor::new());
        let work = store.register(&session, "a", &TurnId::new(), &supervisor);
        let published = Arc::new(AtomicUsize::new(0));
        let seen = published.clone();
        let callback_work = work.clone();
        supervisor.set_on_change(move |task| {
            if task.status.is_terminal() {
                callback_work.while_authorized(|| {
                    seen.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        supervisor.register("spawn", "child", Some(&session.0));
        store.revoke(&session, "a");
        assert_eq!(
            published.load(Ordering::SeqCst),
            1,
            "cancellation remains visible"
        );
        work.while_authorized(|| {
            published.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(
            published.load(Ordering::SeqCst),
            1,
            "late terminal re-entry is suppressed"
        );
        let weak = Arc::downgrade(&supervisor);
        drop(supervisor);
        assert!(
            weak.upgrade().is_none(),
            "callbacks do not pin the supervisor"
        );
    }
}
