//! Workspace membership for existing, independently opened peer sessions.
//! The serve process owns this registry and its files, just as it owns the
//! peer blackboard. No client or model chooses a storage root or caller ID.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use octos_core::SessionKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const LIST: &str = "peer/team/list";
pub(crate) const LEADER: &str = "peer/team/leader/set";
pub(crate) const MESSAGE: &str = "peer/team/message";
pub(crate) const UPDATED: &str = "peer/team/updated";
pub(crate) const FEATURE: &str = "peer.workspace_team.v1";
pub(crate) const MESSAGE_KIND: &str = "workspace_peer_message";
const MAX_MEMBERS: usize = 1024;
const MAX_RESULT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Member {
    pub agent_id: String,
    pub session_id: SessionKey,
    pub joined_at_ms: i64,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub result_turn_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Team {
    pub workspace: PathBuf,
    pub revision: u64,
    pub leader: String,
    #[serde(default)]
    pub leadership_epoch: u64,
    pub members: Vec<Member>,
}

impl Team {
    pub fn member(&self, id: &str) -> Option<&Member> {
        self.members
            .iter()
            .find(|m| m.agent_id == id || m.session_id.0 == id)
    }

    pub fn index(&self, caller: &SessionKey) -> String {
        let mut text = format!(
            "Workspace peer team: {} (revision {})\nYou are {}. Coordinator: {}.\n\
             Use peer_send_input with a workspace agent_id to message a member; \
             busy members receive a queued follow-up. The coordinator assigns \
             work with peer_assign and gathers results with peer_gather. Members retain their own \
             user tasks and permissions. Coordinate file edits before changing \
             the same files. Do not send acknowledgment-only replies.\n",
            self.workspace.display(),
            self.revision,
            self.member(&caller.0)
                .map(|m| m.agent_id.as_str())
                .unwrap_or("unknown"),
            self.leader,
        );
        for member in &self.members {
            let role = if member.agent_id == self.leader {
                "coordinator"
            } else {
                "member"
            };
            text.push_str(&format!(
                "- {}  {}  session={}\n",
                member.agent_id, role, member.session_id
            ));
        }
        text
    }

    pub fn gather(&self, ids: Option<&[String]>) -> String {
        let mut text = String::new();
        for member in &self.members {
            if ids.is_some_and(|ids| {
                !ids.iter()
                    .any(|id| id == &member.agent_id || id == &member.session_id.0)
            }) {
                continue;
            }
            text.push_str(&format!(
                "\n## {} (session {})\n{}\n",
                member.agent_id,
                member.session_id,
                member
                    .result
                    .as_deref()
                    .unwrap_or("No completed turn result recorded.")
            ));
        }
        text
    }
}

#[derive(Default)]
struct State {
    teams: HashMap<PathBuf, Team>,
    // Presence is deliberately not durable. Restart does not imply that old
    // clients are connected. Multiple UIs on one session remain one member.
    connections: HashMap<(PathBuf, SessionKey), HashSet<u64>>,
    watches: HashMap<u64, HashMap<(String, SessionKey), serde_json::Value>>,
}

#[derive(Default)]
pub(crate) struct WorkspaceTeams(Mutex<State>);

fn location(profile_data: &Path, workspace: &Path) -> Result<(PathBuf, PathBuf), String> {
    let workspace = workspace
        .canonicalize()
        .map_err(|e| format!("resolve workspace: {e}"))?;
    let key = crate::peers::workspace_scope_encode(&workspace).ok_or("workspace has no scope")?;
    let digest = format!("{:x}", Sha256::digest(key.as_bytes()));
    Ok((
        profile_data
            .join("peers")
            .join("workspace-teams")
            .join(digest),
        workspace,
    ))
}

fn persist(dir: &Path, team: &Team) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create team directory: {e}"))?;
    // Reuse the peer blackboard's anchored, fsynced, atomic writer.
    super::peer_io::write_peer_file_atomic(
        dir,
        "team.json",
        &serde_json::to_string(team).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("persist workspace team: {e}"))
}

impl WorkspaceTeams {
    pub fn join(
        &self,
        profile_data: &Path,
        workspace: &Path,
        session: &SessionKey,
        connection: u64,
    ) -> Result<Team, String> {
        let (dir, workspace) = location(profile_data, workspace)?;
        let mut state = self.0.lock().map_err(|_| "workspace team lock poisoned")?;
        let mut team = if let Some(team) = state.teams.get(&dir) {
            team.clone()
        } else if dir.join("team.json").exists() {
            let body = super::peer_io::read_peer_file(&dir, "team.json", 32 * 1024 * 1024)
                .ok_or("cannot read workspace team")?;
            let team: Team =
                serde_json::from_str(&body).map_err(|e| format!("invalid workspace team: {e}"))?;
            if team.workspace != workspace || team.member(&team.leader).is_none() {
                return Err("invalid workspace team identity or leader".into());
            }
            team
        } else {
            Team {
                workspace,
                revision: 0,
                leader: String::new(),
                leadership_epoch: 1,
                members: Vec::new(),
            }
        };
        if team.member(&session.0).is_none() {
            if team.members.len() >= MAX_MEMBERS {
                return Err("workspace team member limit reached".into());
            }
            let id = format!("workspace-{}", team.members.len() + 1);
            if team.members.is_empty() {
                team.leader = id.clone();
            }
            team.members.push(Member {
                agent_id: id,
                session_id: session.clone(),
                joined_at_ms: chrono::Utc::now().timestamp_millis(),
                result: None,
                result_turn_id: None,
            });
            team.revision += 1;
            persist(&dir, &team)?;
        }
        state.teams.insert(dir.clone(), team.clone());
        state
            .connections
            .entry((dir, session.clone()))
            .or_default()
            .insert(connection);
        Ok(team)
    }

    pub fn disconnect(&self, connection: u64) {
        if let Ok(mut state) = self.0.lock() {
            state.watches.remove(&connection);
            state.connections.retain(|_, ids| {
                ids.remove(&connection);
                !ids.is_empty()
            });
        }
    }

    pub fn watch(
        &self,
        connection: u64,
        profile: &str,
        session: &SessionKey,
        snapshot: serde_json::Value,
    ) {
        if let Ok(mut state) = self.0.lock() {
            state
                .watches
                .entry(connection)
                .or_default()
                .insert((profile.to_owned(), session.clone()), snapshot);
        }
    }

    pub fn watches(&self, connection: u64) -> Vec<(String, SessionKey, serde_json::Value)> {
        self.0
            .lock()
            .ok()
            .and_then(|state| state.watches.get(&connection).cloned())
            .unwrap_or_default()
            .into_iter()
            .map(|((profile, session), snapshot)| (profile, session, snapshot))
            .collect()
    }

    pub fn get(
        &self,
        profile_data: &Path,
        workspace: &Path,
        caller: &SessionKey,
    ) -> Result<Team, String> {
        let (dir, _) = location(profile_data, workspace)?;
        let state = self.0.lock().map_err(|_| "workspace team lock poisoned")?;
        let team = state
            .teams
            .get(&dir)
            .ok_or("open the session before using its workspace team")?;
        if team.member(&caller.0).is_none() {
            return Err("session is not a workspace team member".into());
        }
        Ok(team.clone())
    }

    pub fn attached(&self, profile_data: &Path, workspace: &Path, session: &SessionKey) -> bool {
        let Ok((dir, _)) = location(profile_data, workspace) else {
            return false;
        };
        self.0
            .lock()
            .ok()
            .is_some_and(|s| s.connections.contains_key(&(dir, session.clone())))
    }

    pub fn set_leader(
        &self,
        profile_data: &Path,
        workspace: &Path,
        caller: &SessionKey,
        target: &str,
        expected_revision: u64,
    ) -> Result<Team, String> {
        let (dir, _) = location(profile_data, workspace)?;
        let mut state = self.0.lock().map_err(|_| "workspace team lock poisoned")?;
        let old = state.teams.get(&dir).ok_or("workspace team is not open")?;
        if old.member(&caller.0).is_none() {
            return Err("session is not a workspace team member".into());
        }
        if old.revision != expected_revision {
            return Err(
                "team revision changed; refresh /agents and select the leader again".into(),
            );
        }
        let target = old
            .member(target)
            .ok_or("unknown workspace team member")?
            .agent_id
            .clone();
        let mut team = old.clone();
        if team.leader != target {
            team.leader = target;
            team.leadership_epoch += 1;
            team.revision += 1;
            persist(&dir, &team)?;
            state.teams.insert(dir, team.clone());
        }
        Ok(team)
    }

    /// Linearizes coordinator admission with user-driven leader transfers.
    /// The closure must not re-enter WorkspaceTeams.
    pub fn with_leader<T>(
        &self,
        profile_data: &Path,
        workspace: &Path,
        caller: &SessionKey,
        epoch: u64,
        admit: impl FnOnce(&Team) -> Result<T, String>,
    ) -> Result<T, String> {
        let (dir, _) = location(profile_data, workspace)?;
        let state = self.0.lock().map_err(|_| "workspace team lock poisoned")?;
        let team = state.teams.get(&dir).ok_or("workspace team is not open")?;
        if team.leadership_epoch != epoch
            || team
                .member(&team.leader)
                .is_none_or(|m| &m.session_id != caller)
        {
            return Err("coordinator changed; only the current coordinator may assign work".into());
        }
        admit(team)
    }

    pub fn record_result(
        &self,
        profile_data: &Path,
        workspace: &Path,
        session: &SessionKey,
        turn: &str,
        result: &str,
    ) -> Result<(), String> {
        let (dir, _) = location(profile_data, workspace)?;
        let mut state = self.0.lock().map_err(|_| "workspace team lock poisoned")?;
        let Some(old) = state.teams.get(&dir) else {
            return Ok(());
        };
        let mut team = old.clone();
        let Some(member) = team.members.iter_mut().find(|m| &m.session_id == session) else {
            return Ok(());
        };
        let mut end = result.len().min(MAX_RESULT_BYTES);
        while !result.is_char_boundary(end) {
            end -= 1;
        }
        member.result = Some(result[..end].to_owned());
        member.result_turn_id = Some(turn.to_owned());
        persist(&dir, &team)?;
        state.teams.insert(dir, team);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_are_idempotent_and_leadership_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let a = SessionKey("local:a".into());
        let b = SessionKey("local:b".into());
        let teams = WorkspaceTeams::default();
        teams.join(temp.path(), workspace.path(), &a, 1).unwrap();
        teams.join(temp.path(), workspace.path(), &a, 2).unwrap();
        let joined = teams.join(temp.path(), workspace.path(), &b, 3).unwrap();
        assert_eq!(joined.members.len(), 2);
        assert_eq!(joined.leader, "workspace-1");
        let elected = teams
            .set_leader(
                temp.path(),
                workspace.path(),
                &a,
                "workspace-2",
                joined.revision,
            )
            .unwrap();
        assert!(
            teams
                .set_leader(
                    temp.path(),
                    workspace.path(),
                    &b,
                    "workspace-1",
                    joined.revision
                )
                .is_err()
        );
        teams.disconnect(1);
        assert!(teams.attached(temp.path(), workspace.path(), &a));
        teams.disconnect(2);
        assert!(!teams.attached(temp.path(), workspace.path(), &a));
        let restarted = WorkspaceTeams::default();
        let recovered = restarted
            .join(temp.path(), workspace.path(), &a, 4)
            .unwrap();
        assert_eq!(recovered.leader, elected.leader);
        assert_eq!(recovered.revision, elected.revision);
    }

    #[test]
    fn profiles_and_workspaces_cannot_reach_other_members() {
        let p1 = tempfile::tempdir().unwrap();
        let p2 = tempfile::tempdir().unwrap();
        let w1 = tempfile::tempdir().unwrap();
        let w2 = tempfile::tempdir().unwrap();
        let teams = WorkspaceTeams::default();
        let a = SessionKey("local:a".into());
        let b = SessionKey("local:b".into());
        teams.join(p1.path(), w1.path(), &a, 1).unwrap();
        teams.join(p1.path(), w2.path(), &b, 2).unwrap();
        teams.join(p2.path(), w1.path(), &b, 3).unwrap();
        assert!(teams.get(p1.path(), w2.path(), &a).is_err());
        assert!(teams.get(p2.path(), w1.path(), &a).is_err());
        assert!(teams.set_leader(p1.path(), w1.path(), &a, &b.0, 1).is_err());
    }

    #[test]
    fn concurrent_joins_and_elections_have_one_winner() {
        let profile = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let teams = WorkspaceTeams::default();
        std::thread::scope(|scope| {
            for n in 0..8 {
                let (teams, p, w) = (&teams, profile.path(), workspace.path());
                scope.spawn(move || {
                    teams
                        .join(p, w, &SessionKey(format!("local:{n}")), n)
                        .unwrap()
                });
            }
        });
        let caller = SessionKey("local:0".into());
        let team = teams
            .get(profile.path(), workspace.path(), &caller)
            .unwrap();
        assert_eq!(team.members.len(), 8);
        assert_eq!(
            team.members
                .iter()
                .map(|m| &m.agent_id)
                .collect::<HashSet<_>>()
                .len(),
            8
        );
        let results = std::thread::scope(|scope| {
            let jobs: Vec<_> = team
                .members
                .iter()
                .filter(|m| m.agent_id != team.leader)
                .map(|member| {
                    let (teams, caller, p, w, revision) = (
                        &teams,
                        &caller,
                        profile.path(),
                        workspace.path(),
                        team.revision,
                    );
                    scope.spawn(move || teams.set_leader(p, w, caller, &member.agent_id, revision))
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    }
    #[test]
    fn workspace_team_old_coordinator_is_fenced_even_after_re_election() {
        let profile = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let teams = WorkspaceTeams::default();
        let a = SessionKey("local:a".into());
        let b = SessionKey("local:b".into());
        let first = teams.join(profile.path(), workspace.path(), &a, 1).unwrap();
        let joined = teams.join(profile.path(), workspace.path(), &b, 2).unwrap();
        assert!(
            teams
                .with_leader(
                    profile.path(),
                    workspace.path(),
                    &a,
                    first.leadership_epoch,
                    |_| Ok(())
                )
                .is_ok()
        );
        let elected = teams
            .set_leader(
                profile.path(),
                workspace.path(),
                &a,
                "workspace-2",
                joined.revision,
            )
            .unwrap();
        let back = teams
            .set_leader(
                profile.path(),
                workspace.path(),
                &b,
                "workspace-1",
                elected.revision,
            )
            .unwrap();
        let mut admitted = false;
        assert!(
            teams
                .with_leader(
                    profile.path(),
                    workspace.path(),
                    &a,
                    first.leadership_epoch,
                    |_| {
                        admitted = true;
                        Ok(())
                    }
                )
                .is_err()
        );
        assert!(!admitted);
        assert!(
            teams
                .with_leader(
                    profile.path(),
                    workspace.path(),
                    &a,
                    back.leadership_epoch,
                    |_| Ok(())
                )
                .is_ok()
        );
    }

    #[test]
    fn workspace_team_failed_persistence_does_not_admit_membership() {
        let profile = tempfile::NamedTempFile::new().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let teams = WorkspaceTeams::default();
        let a = SessionKey("local:a".into());
        assert!(teams.join(profile.path(), workspace.path(), &a, 1).is_err());
        assert!(!teams.attached(profile.path(), workspace.path(), &a));
        assert!(teams.get(profile.path(), workspace.path(), &a).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_team_aliases_and_results_survive_restart() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("repo");
        std::fs::create_dir(&workspace).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&workspace, &alias).unwrap();
        let a = SessionKey("local:a".into());
        let teams = WorkspaceTeams::default();
        teams.join(root.path(), &workspace, &a, 1).unwrap();
        assert_eq!(
            teams
                .join(root.path(), &alias, &a, 2)
                .unwrap()
                .members
                .len(),
            1
        );
        teams
            .record_result(root.path(), &alias, &a, "turn-a", &"文".repeat(10000))
            .unwrap();
        let restarted = WorkspaceTeams::default();
        let team = restarted.join(root.path(), &workspace, &a, 3).unwrap();
        assert_eq!(team.members[0].result_turn_id.as_deref(), Some("turn-a"));
        assert!(team.members[0].result.as_ref().unwrap().len() <= MAX_RESULT_BYTES);
        assert!(team.gather(None).contains("文"));
    }
}
