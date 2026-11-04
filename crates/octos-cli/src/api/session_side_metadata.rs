use std::collections::HashMap;

use octos_core::SessionKey;

const SESSION_SIDE_MAP_CAP: usize = 4096;

#[derive(Debug, Clone)]
pub(crate) struct SessionSideEntry<V> {
    pub(crate) value: V,
    pub(crate) last_used: std::time::Instant,
}

/// Make room for a new idle-session side entry without ever evicting an
/// existing key on a refresh. The oldest entry is reconstructible on the next
/// open, exactly like the main session runtime cache's idle eviction.
fn session_side_reserve<K, V>(map: &mut HashMap<K, SessionSideEntry<V>>, key: &K)
where
    K: Eq + std::hash::Hash + Clone,
{
    if map.contains_key(key) || map.len() < SESSION_SIDE_MAP_CAP {
        return;
    }
    if let Some(oldest) = map
        .iter()
        .min_by_key(|(_, entry)| entry.last_used)
        .map(|(key, _)| key.clone())
    {
        map.remove(&oldest);
    }
}

pub(crate) fn session_side_insert<K, V>(map: &mut HashMap<K, SessionSideEntry<V>>, key: K, value: V)
where
    K: Eq + std::hash::Hash + Clone,
{
    session_side_reserve(map, &key);
    map.insert(
        key,
        SessionSideEntry {
            value,
            last_used: std::time::Instant::now(),
        },
    );
}

pub(crate) fn session_side_get<K, V>(
    map: &mut HashMap<K, SessionSideEntry<V>>,
    key: &K,
) -> Option<V>
where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
{
    map.get_mut(key).map(|entry| {
        entry.last_used = std::time::Instant::now();
        entry.value.clone()
    })
}

/// Per-session compaction-mode override, set from the `/context` menu via
/// `session/compact/mode/set`. `true` = force the LLM summary path, `false` =
/// force the heuristic. Absent = follow the server `--llm-compaction` default.
#[derive(Default)]
pub(crate) struct SessionCompactionStore {
    pub(crate) entries: std::sync::RwLock<HashMap<SessionKey, SessionSideEntry<bool>>>,
}

impl SessionCompactionStore {
    pub(crate) fn set(&self, session_id: &SessionKey, llm: bool) {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(|error| error.into_inner());
        session_side_insert(&mut entries, session_id.clone(), llm);
    }

    pub(crate) fn get(&self, session_id: &SessionKey) -> Option<bool> {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(|error| error.into_inner());
        session_side_get(&mut entries, session_id)
    }

    pub(crate) fn remove(&self, session_id: &SessionKey) {
        self.entries
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .remove(session_id);
    }
}
