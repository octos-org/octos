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
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
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

impl Generation {
    /// A dead grant's record outlives its leases: without it the next request
    /// would read the same dead grant as a new invalidation and purge again.
    fn is_retained(&self) -> bool {
        Arc::strong_count(&self.lifetime.0) > 1
            || self.stamp.as_ref().is_some_and(|grant| !grant.active)
    }
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
        scopes.retain(|_, scope| scope.is_retained());
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
        scopes.retain(|_, scope| scope.is_retained());
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
        self.revoke_matching(session, profile, |current| current.number == generation)
    }

    /// Revoke only the generation still owned by this exact grant revision. The
    /// match and revocation are atomic, so a delayed pre-bind socket cannot revoke
    /// a replacement (including same-token reissue) or later browser generation.
    pub(crate) fn revoke_active_grant(
        &self,
        session: &SessionKey,
        profile: &str,
        token_hash: &str,
        created_at: chrono::DateTime<chrono::Utc>,
    ) -> Option<Vec<TurnId>> {
        self.revoke_matching(session, profile, |current| {
            current.stamp.as_ref().is_some_and(|grant| {
                grant.active && grant.token_hash == token_hash && grant.created_at == created_at
            })
        })
    }

    fn revoke_matching(
        &self,
        session: &SessionKey,
        profile: &str,
        matches: impl FnOnce(&Generation) -> bool,
    ) -> Option<Vec<TurnId>> {
        let mut scopes = self.scopes.lock().unwrap_or_else(|e| e.into_inner());
        let current = scopes.get_mut(&(session.clone(), profile.to_owned()))?;
        if current.revoked || !matches(current) {
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
