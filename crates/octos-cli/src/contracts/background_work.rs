//! Connection-independent ownership of AppUI background work across turns.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use octos_agent::{TaskCancelError, TaskSupervisor};
use octos_core::{SessionKey, ui_protocol::TurnId};

type Scope = (SessionKey, String);

/// Only identity and validity are needed; never retain the bearer secret.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct GrantStamp {
    pub(crate) token_hash: String,
    pub(crate) active: bool,
}

/// Keeps a generation entry alive while an old socket or task can still act
/// on it. The registry holds one reference; it prunes entries with no clients.
#[derive(Clone, Default)]
pub(crate) struct ScopeLease(Arc<()>);

#[derive(Default)]
struct Generation {
    lifetime: ScopeLease,
    stamp: Option<GrantStamp>,
    number: u64,
    revoked: bool,
    turns: Vec<Weak<BackgroundWork>>,
}

#[derive(Default)]
pub(crate) struct BackgroundWorkStore {
    scopes: Mutex<HashMap<Scope, Generation>>,
}

pub(crate) struct Observation {
    pub(crate) lifetime: ScopeLease,
    pub(crate) generation: u64,
    /// Some means this observation retired the prior generation. The caller
    /// must purge its continuations before admitting any new work.
    pub(crate) retired: Option<Vec<TurnId>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Active,
    Cancelling,
    Revoked,
}

pub(crate) struct BackgroundWork {
    _scope_lifetime: ScopeLease,
    turn_id: TurnId,
    supervisors: Mutex<Vec<Weak<TaskSupervisor>>>,
    phase: Mutex<Phase>,
}

impl BackgroundWorkStore {
    /// Call under the AppUI admission lock, only at authenticated socket bind
    /// or interactive turn admission. Autonomous queue ticks do not read the
    /// grant file: this is request-time observation, not a revocation watcher.
    pub(crate) fn observe(
        &self,
        session: &SessionKey,
        profile: &str,
        stamp: Option<GrantStamp>,
    ) -> Observation {
        let mut scopes = self.scopes.lock().unwrap_or_else(|e| e.into_inner());
        scopes.retain(|_, scope| Arc::strong_count(&scope.lifetime.0) > 1);
        let current = scopes
            .entry((session.clone(), profile.to_owned()))
            .or_default();
        let changed = current.stamp != stamp;
        let invalidated = current.revoked
            || (changed
                && (current.stamp.as_ref().is_some_and(|grant| grant.active)
                    || stamp.as_ref().is_some_and(|grant| !grant.active)));
        let retired = if invalidated {
            let turns = std::mem::take(&mut current.turns);
            let already_revoked = current.revoked;
            current.number = current
                .number
                .checked_add(1)
                .expect("background generation overflow");
            current.revoked = false;
            (!already_revoked).then_some(turns)
        } else {
            None
        };
        current.stamp = stamp;
        let generation = current.number;
        let lifetime = current.lifetime.clone();
        drop(scopes);
        Observation {
            lifetime,
            generation,
            retired: retired.map(Self::cancel),
        }
    }

    /// Read-only observation of the CURRENT generation, changing nothing.
    /// Used when the grants file cannot be read for a NON-ingress (ordinary
    /// browser) turn: the turn keeps its existing capability without being
    /// mistaken for a revocation (which would retire/cancel legitimate
    /// background work). `retired` is always None; a later admission after
    /// the file is repaired observes normally.
    pub(crate) fn peek(&self, session: &SessionKey, profile: &str) -> Observation {
        let scopes = self.scopes.lock().unwrap_or_else(|e| e.into_inner());
        match scopes.get(&(session.clone(), profile.to_owned())) {
            Some(current) => Observation {
                lifetime: current.lifetime.clone(),
                generation: current.number,
                retired: None,
            },
            None => Observation {
                lifetime: ScopeLease::default(),
                generation: 0,
                retired: None,
            },
        }
    }

    /// Admission and registration are separate: cancellation must cover a turn
    /// still constructing its runtime, before any supervisor exists.
    pub(crate) fn admit(
        &self,
        session: &SessionKey,
        profile: &str,
        turn_id: &TurnId,
        expected: Option<u64>,
    ) -> Option<Arc<BackgroundWork>> {
        let mut scopes = self.scopes.lock().unwrap_or_else(|e| e.into_inner());
        scopes.retain(|_, scope| Arc::strong_count(&scope.lifetime.0) > 1);
        let current = scopes
            .entry((session.clone(), profile.to_owned()))
            .or_default();
        if current.revoked || expected.is_some_and(|epoch| epoch != current.number) {
            return None;
        }
        let work = Arc::new(BackgroundWork {
            _scope_lifetime: current.lifetime.clone(),
            turn_id: turn_id.clone(),
            supervisors: Mutex::new(Vec::new()),
            phase: Mutex::new(Phase::Active),
        });
        current.turns.retain(|entry| entry.strong_count() > 0);
        current.turns.push(Arc::downgrade(&work));
        Some(work)
    }

    pub(crate) fn revoke_generation(
        &self,
        session: &SessionKey,
        profile: &str,
        generation: u64,
    ) -> Option<Vec<TurnId>> {
        let mut scopes = self.scopes.lock().unwrap_or_else(|e| e.into_inner());
        let current = scopes.get_mut(&(session.clone(), profile.to_owned()))?;
        if current.number != generation || current.revoked {
            return None;
        }
        current.revoked = true;
        let turns = std::mem::take(&mut current.turns);
        drop(scopes);
        Some(Self::cancel(turns))
    }

    fn cancel(turns: Vec<Weak<BackgroundWork>>) -> Vec<TurnId> {
        let live: Vec<_> = turns
            .into_iter()
            .filter_map(|work| work.upgrade())
            .collect();
        // Close admission before cancellation. Terminal notifications may still
        // publish during Cancelling; every producer is closed before the caller
        // purges the queue under the same lock used for continuation admission.
        for work in &live {
            *work.phase.lock().unwrap_or_else(|e| e.into_inner()) = Phase::Cancelling;
        }
        for work in &live {
            let supervisors: Vec<_> = work
                .supervisors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
            for supervisor in supervisors {
                for task in supervisor.get_all_tasks() {
                    if !task.status.is_terminal() {
                        BackgroundWork::cancel_task(&supervisor, &task.id);
                    }
                }
            }
        }
        for work in &live {
            *work.phase.lock().unwrap_or_else(|e| e.into_inner()) = Phase::Revoked;
        }
        live.into_iter().map(|work| work.turn_id.clone()).collect()
    }

    #[cfg(test)]
    pub(crate) fn register(
        &self,
        session: &SessionKey,
        profile: &str,
        turn: &TurnId,
        supervisor: &Arc<TaskSupervisor>,
    ) -> Arc<BackgroundWork> {
        let _observed = self.observe(session, profile, None);
        let work = self.admit(session, profile, turn, None).unwrap();
        work.attach(supervisor);
        work
    }

    #[cfg(test)]
    fn revoke(&self, session: &SessionKey, profile: &str) -> Vec<TurnId> {
        let generation = self
            .scopes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(session.clone(), profile.to_owned()))
            .unwrap()
            .number;
        self.revoke_generation(session, profile, generation)
            .unwrap()
    }
}

impl BackgroundWork {
    pub(crate) fn while_authorized(&self, publish: impl FnOnce()) {
        let phase = self.phase.lock().unwrap_or_else(|e| e.into_inner());
        if *phase != Phase::Revoked {
            publish();
        }
    }

    pub(crate) fn attach(self: &Arc<Self>, supervisor: &Arc<TaskSupervisor>) {
        // Publish membership before installing the listener. A cancellation
        // racing this setup either snapshots this child, or the child sees
        // the closed phase below. Inheritance runs before the child's worker.
        {
            let mut supervisors = self.supervisors.lock().unwrap_or_else(|e| e.into_inner());
            supervisors.retain(|entry| entry.strong_count() > 0);
            supervisors.push(Arc::downgrade(supervisor));
        }
        let work = self.clone();
        let owner = Arc::downgrade(supervisor);
        supervisor.set_on_change_listener("ingress_revocation", move |task| {
            let cancelled = *work.phase.lock().unwrap_or_else(|e| e.into_inner()) != Phase::Active;
            if cancelled
                && !task.status.is_terminal()
                && let Some(owner) = owner.upgrade()
            {
                Self::cancel_task(&owner, &task.id);
            }
        });
        let work = self.clone();
        supervisor.set_on_inherited(move |child| work.attach(child));
        let cancelled = *self.phase.lock().unwrap_or_else(|e| e.into_inner()) != Phase::Active;
        if cancelled {
            for task in supervisor.get_all_tasks() {
                if !task.status.is_terminal() {
                    Self::cancel_task(supervisor, &task.id);
                }
            }
        }
    }

    fn cancel_task(supervisor: &TaskSupervisor, task_id: &str) {
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
    #[test]
    fn ingress_revocation_reaches_three_nested_supervisors_after_parent_drops() {
        let store = BackgroundWorkStore::default();
        let session = SessionKey("nested-background".into());
        let root = Arc::new(TaskSupervisor::new());
        let work = store.register(&session, "a", &TurnId::new(), &root);
        let mut parent = root.clone();
        let mut children = Vec::new();
        for level in 0..3 {
            let child = Arc::new(TaskSupervisor::new());
            child.inherit_child_observers(&parent);
            let task = child.register("spawn", &format!("nested-{level}"), Some("private-child"));
            child.mark_running(&task);
            children.push((child.clone(), task));
            parent = child;
        }
        drop(work);
        drop(root);
        store.revoke(&session, "a");
        for (child, task) in &children {
            assert_eq!(child.get_task(task).unwrap().status, TaskStatus::Cancelled);
            assert!(child.cancel_token(task).is_cancelled());
        }
        let late = parent.register("spawn", "late-grandchild", Some("private-child"));
        parent.mark_running(&late);
        assert_eq!(
            parent.get_task(&late).unwrap().status,
            TaskStatus::Cancelled
        );
    }
    fn grant(hash: &str, active: bool) -> Option<GrantStamp> {
        Some(GrantStamp {
            token_hash: hash.to_owned(),
            active,
        })
    }

    #[test]
    fn ingress_revocation_old_socket_cannot_cancel_new_grant_or_later_browser() {
        for next in [grant("fresh", true), grant("old", false), None] {
            let store = BackgroundWorkStore::default();
            let session = SessionKey("generation".into());
            let first = store.observe(&session, "a", grant("old", true));
            let old = store
                .admit(&session, "a", &TurnId::new(), Some(first.generation))
                .unwrap();
            let old_supervisor = Arc::new(TaskSupervisor::new());
            old.attach(&old_supervisor);
            let old_task = old_supervisor.register("spawn", "old", Some(&session.0));
            let replacement = store.observe(&session, "a", next.clone());
            assert!(replacement.retired.is_some());
            assert_ne!(first.generation, replacement.generation);
            assert_eq!(
                old_supervisor.get_task(&old_task).unwrap().status,
                TaskStatus::Cancelled
            );
            let fresh = store.admit(&session, "a", &TurnId::new(), None).unwrap();
            let fresh_supervisor = Arc::new(TaskSupervisor::new());
            fresh.attach(&fresh_supervisor);
            let fresh_task = fresh_supervisor.register("spawn", "fresh", Some(&session.0));
            fresh_supervisor.mark_running(&fresh_task);
            assert!(
                store
                    .revoke_generation(&session, "a", first.generation)
                    .is_none()
            );
            assert_eq!(
                fresh_supervisor.get_task(&fresh_task).unwrap().status,
                TaskStatus::Running
            );
            assert!(store.observe(&session, "a", next).retired.is_none());
            assert!(
                store
                    .admit(&session, "a", &TurnId::new(), Some(first.generation))
                    .is_none()
            );
        }
    }

    #[test]
    fn ingress_revocation_covers_late_runtime_and_late_descendant_registration() {
        let store = BackgroundWorkStore::default();
        let session = SessionKey("late-runtime".into());
        let first = store.observe(&session, "a", grant("old", true));
        let work = store.admit(&session, "a", &TurnId::new(), None).unwrap();
        store
            .revoke_generation(&session, "a", first.generation)
            .unwrap();
        assert!(store.admit(&session, "a", &TurnId::new(), None).is_none());
        let root = Arc::new(TaskSupervisor::new());
        work.attach(&root);
        let child = Arc::new(TaskSupervisor::new());
        child.inherit_child_observers(&root);
        let task = child.register("spawn", "late", Some("private-child"));
        child.mark_running(&task);
        assert_eq!(child.get_task(&task).unwrap().status, TaskStatus::Cancelled);
        let fresh = store.observe(&session, "a", grant("old", false));
        assert!(
            fresh.retired.is_none(),
            "queue was already purged at revocation"
        );
        assert_ne!(fresh.generation, first.generation);
        assert!(store.admit(&session, "a", &TurnId::new(), None).is_some());
    }
    #[test]
    fn ingress_revocation_first_observation_of_invalid_grant_retires_persisted_queue() {
        let store = BackgroundWorkStore::default();
        let session = SessionKey("restarted".into());
        let observation = store.observe(&session, "a", grant("old", false));
        assert_eq!(observation.retired, Some(Vec::new()));
        assert!(
            store
                .observe(&session, "a", grant("old", false))
                .retired
                .is_none()
        );
    }
    #[test]
    fn ingress_revocation_registry_keeps_old_sockets_but_reclaims_unused_scopes() {
        let store = BackgroundWorkStore::default();
        let old = SessionKey("old".into());
        let socket = store.observe(&old, "a", grant("old", true));
        let work = store.admit(&old, "a", &TurnId::new(), None).unwrap();
        let new = SessionKey("new".into());
        let fresh = store.observe(&new, "a", None);
        assert_eq!(store.scopes.lock().unwrap().len(), 2);
        drop(socket);
        store.observe(&new, "a", None);
        assert_eq!(
            store.scopes.lock().unwrap().len(),
            2,
            "live task retains generation"
        );
        drop(work);
        store.observe(&new, "a", None);
        assert_eq!(store.scopes.lock().unwrap().len(), 1);
        drop(fresh);
        store.observe(&SessionKey("third".into()), "a", None);
        assert_eq!(store.scopes.lock().unwrap().len(), 1);
    }
}
