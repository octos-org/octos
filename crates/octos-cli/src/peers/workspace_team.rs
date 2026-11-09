//! Workspace membership for existing, independently opened peer sessions.
//! The serve process owns this registry and its files, just as it owns the
//! peer blackboard. No client or model chooses a storage root or caller ID.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use octos_core::SessionKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const LIST: &str = octos_core::ui_protocol::methods::PEER_TEAM_LIST;
pub(crate) const LEADER: &str = octos_core::ui_protocol::methods::PEER_TEAM_LEADER_SET;
pub(crate) const MESSAGE: &str = octos_core::ui_protocol::methods::PEER_TEAM_MESSAGE;
pub(crate) const UPDATED: &str = "peer/team/updated";
pub(crate) const FEATURE: &str = "peer.workspace_team.v1";
pub(crate) const MESSAGE_KIND: &str = "workspace_peer_message";
const MAX_MEMBERS: usize = 1024;
/// New-member admissions at or past this seat count warn the operator (#2741).
const CAPACITY_WARN_AT: usize = MAX_MEMBERS / 4 * 3;
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

/// Seat numbers must never repeat among listed members: peers address each
/// other by agent_id. Derive the next number from the highest existing seat
/// instead of the member count.
fn next_agent_number(team: &Team) -> usize {
    team.members
        .iter()
        .filter_map(|m| m.agent_id.strip_prefix("workspace-"))
        .filter_map(|suffix| suffix.parse::<usize>().ok())
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

/// #2741 — members accumulate for the workspace's lifetime, so a long-lived
/// workspace reaches the cap holding mostly stale seats. To admit the
/// joining session, evict the stalest members that have no live connection
/// and no turn in flight (they rejoin with a fresh seat on their next open).
/// A connected or turn-active member is never evicted: if even the
/// disconnected idle seats cannot make room, the team is left untouched so a
/// full team stays a full team.
fn evict_stale_members_for_join(
    team: &mut Team,
    connections: &HashMap<(PathBuf, SessionKey), HashSet<u64>>,
    turn_active: &HashSet<SessionKey>,
    dir: &Path,
) -> Vec<Member> {
    let surplus = team.members.len() + 1 - MAX_MEMBERS;
    let mut stale: Vec<usize> = (0..team.members.len())
        .filter(|&i| {
            !turn_active.contains(&team.members[i].session_id)
                && connections
                    .get(&(dir.to_path_buf(), team.members[i].session_id.clone()))
                    .is_none_or(|seats| seats.is_empty())
        })
        .collect();
    stale.sort_by_key(|&i| team.members[i].joined_at_ms);
    if stale.len() < surplus {
        return Vec::new();
    }
    let evicting: HashSet<usize> = stale[..surplus].iter().copied().collect();
    let mut evicted = Vec::with_capacity(surplus);
    let mut kept = Vec::with_capacity(team.members.len() - surplus);
    for (i, member) in std::mem::take(&mut team.members).into_iter().enumerate() {
        if evicting.contains(&i) {
            evicted.push(member);
        } else {
            kept.push(member);
        }
    }
    team.members = kept;
    if evicted.iter().any(|m| m.agent_id == team.leader) {
        // Mirror the join-time election: the earliest-joined survivor leads,
        // and the epoch bump fences a coordinator evicted mid-flight. There
        // is always a survivor — eviction keeps MAX_MEMBERS - 1 seats — so
        // the team file never records a leader outside its member list.
        team.leader = team
            .members
            .iter()
            .min_by_key(|m| m.joined_at_ms)
            .map(|m| m.agent_id.clone())
            .unwrap_or_default();
        team.leadership_epoch += 1;
    }
    team.revision += 1;
    evicted
}

impl WorkspaceTeams {
    /// `turn_active` carries the sessions with a turn in flight (the
    /// process-global active-turn registry); eviction never takes a seat
    /// whose turn could still record a result into the team.
    pub fn join(
        &self,
        profile_data: &Path,
        workspace: &Path,
        session: &SessionKey,
        connection: u64,
        turn_active: &HashSet<SessionKey>,
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
                let evicted =
                    evict_stale_members_for_join(&mut team, &state.connections, turn_active, &dir);
                if evicted.is_empty() {
                    return Err("workspace team member limit reached".into());
                }
                tracing::warn!(
                    evicted = evicted.len(),
                    seats = team.members.len(),
                    "workspace team hit the member limit and evicted its disconnected members"
                );
            }
            let id = format!("workspace-{}", next_agent_number(&team));
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
            if team.members.len() >= CAPACITY_WARN_AT {
                tracing::warn!(
                    seats = team.members.len(),
                    cap = MAX_MEMBERS,
                    "workspace team is approaching the member limit"
                );
            }
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
