//! App/account memory namespaces for sessions that share one profile
//! (UPCR-2026-034, host-owned app peers).
//!
//! A [`super::ProfileRuntime`] owns ONE provider configuration and, until
//! now, ONE set of memory stores: long-term memory (`MEMORY.md`, daily notes,
//! the bank), the episode store and the Recall/Knowledge index. Every session
//! of the profile captured into and was injected from those stores, so giving
//! two app peers different working directories did not isolate what either
//! remembered.
//!
//! A session bound to a memory namespace instead runs on a separate set of
//! stores rooted at `<data_dir>/memory-namespaces/<segment>/…`. The binding is
//! host-supplied and durable (see [`crate::peers::app_binding`]); nothing an
//! app sends selects it. Capture (`save_memory`, episodes), retrieval
//! (`recall_memory`, `memory_search`, `memory_load`), the automatic memory
//! prompt segment and episodic recall all use the namespaced stores, and the
//! background extraction sweep never reads a bound session's transcript into
//! the profile's own memory. Namespaces do not nest in either direction: a
//! namespaced session sees neither the profile's private memory nor another
//! namespace's.
//!
//! Handles are cached process-wide by root (redb allows one open per file per
//! process), so every session of the same namespace shares one set of stores.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use eyre::{Result, WrapErr};
use octos_memory::{EpisodeStore, MemoryStore, RecallStore};

use super::ProfileRuntime;

/// Directory under the profile data dir that holds every namespace root.
pub(crate) const MEMORY_NAMESPACES_DIR: &str = "memory-namespaces";

/// Upper bound on a namespace string. A namespace is an address, not data.
pub(crate) const MEMORY_NAMESPACE_MAX_BYTES: usize = 200;

/// Maximum number of `/`-separated segments.
pub(crate) const MEMORY_NAMESPACE_MAX_SEGMENTS: usize = 8;

/// Validate a memory namespace and return its canonical form.
///
/// Grammar: 1..=8 segments separated by `/`; each segment is 1..=64 bytes of
/// `[a-z0-9._-]` and starts with `[a-z0-9]` (so `.`/`..` and hidden names are
/// impossible). The whole string is at most 200 bytes. Every segment becomes
/// one directory component, so a valid namespace can never escape
/// [`MEMORY_NAMESPACES_DIR`].
pub(crate) fn validate_memory_namespace(raw: &str) -> Result<String, String> {
    let ns = raw.trim();
    if ns.is_empty() {
        return Err("memory_namespace must not be empty".to_owned());
    }
    if ns.len() > MEMORY_NAMESPACE_MAX_BYTES {
        return Err(format!(
            "memory_namespace exceeds {MEMORY_NAMESPACE_MAX_BYTES} bytes"
        ));
    }
    let segments: Vec<&str> = ns.split('/').collect();
    if segments.len() > MEMORY_NAMESPACE_MAX_SEGMENTS {
        return Err(format!(
            "memory_namespace has more than {MEMORY_NAMESPACE_MAX_SEGMENTS} segments"
        ));
    }
    for segment in &segments {
        let bytes = segment.as_bytes();
        if bytes.is_empty() || bytes.len() > 64 {
            return Err("each memory_namespace segment must be 1..=64 bytes".to_owned());
        }
        if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
            return Err(format!(
                "memory_namespace segment '{segment}' must start with [a-z0-9]"
            ));
        }
        if !bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        }) {
            return Err(format!(
                "memory_namespace segment '{segment}' may only contain [a-z0-9._-]"
            ));
        }
    }
    Ok(ns.to_owned())
}

/// The on-disk root of `namespace` under `data_dir`. `namespace` must already
/// be validated by [`validate_memory_namespace`].
pub(crate) fn memory_namespace_root(data_dir: &Path, namespace: &str) -> PathBuf {
    let mut root = data_dir.join(MEMORY_NAMESPACES_DIR);
    for segment in namespace.split('/') {
        root.push(segment);
    }
    root
}

/// Directory under the profile data dir holding kernel-provisioned app
/// workspaces (host-owned app peers staged without an explicit `cwd`).
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) const APP_WORKSPACES_DIR: &str = "app-workspaces";

/// The kernel-provisioned workspace of the app bound to `namespace` (already
/// validated): `<data_dir>/app-workspaces/<segment>/…`.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) fn app_workspace_root(data_dir: &Path, namespace: &str) -> PathBuf {
    let mut root = data_dir.join(APP_WORKSPACES_DIR);
    for segment in namespace.split('/') {
        root.push(segment);
    }
    root
}

/// The memory stores one session reads and writes.
#[derive(Clone)]
pub struct SessionMemory {
    /// `None` for the profile's own memory; the namespace string otherwise.
    pub namespace: Option<String>,
    pub episodes: Arc<EpisodeStore>,
    pub memory_store: Arc<MemoryStore>,
    pub recall: Arc<RecallStore>,
    /// Whether the background refresh/consolidation pipeline serves these
    /// stores. Always `false` for a namespace: the sweep runs only over the
    /// profile's own memory, so namespaced sessions do not advertise the
    /// `memory_note` capture path whose notes nothing would consolidate.
    pub refresh_enabled: bool,
}

impl SessionMemory {
    /// The profile's own memory (every session that is not app-bound).
    pub fn profile(profile: &ProfileRuntime) -> Self {
        Self {
            namespace: None,
            episodes: profile.memory.clone(),
            memory_store: profile.memory_store.clone(),
            recall: profile.recall.clone(),
            refresh_enabled: profile.memory_refresh_enabled,
        }
    }

    /// The stores of `namespace` for `profile`, opened on first use and
    /// shared by every session bound to the same namespace in this process.
    pub async fn namespaced(profile: &ProfileRuntime, namespace: &str) -> Result<Self> {
        let namespace = validate_memory_namespace(namespace).map_err(|err| eyre::eyre!(err))?;
        let root = memory_namespace_root(&profile.data_dir, &namespace);
        let bundle = open_bundle(profile, &root).await?;
        Ok(Self {
            namespace: Some(namespace),
            episodes: bundle.episodes.clone(),
            memory_store: bundle.memory_store.clone(),
            recall: bundle.recall.clone(),
            refresh_enabled: false,
        })
    }
}

struct Bundle {
    episodes: Arc<EpisodeStore>,
    memory_store: Arc<MemoryStore>,
    recall: Arc<RecallStore>,
}

// One handle set per namespace root per process, kept for the process
// lifetime (redb holds a file lock per open; reopening between two turns
// would re-index the stores), mirroring `open_recall_store`. The async lock is
// held across the open so two sessions binding the same namespace at once
// cannot both try to take the redb lock.
fn bundles() -> &'static tokio::sync::Mutex<HashMap<PathBuf, Arc<Bundle>>> {
    static BUNDLES: OnceLock<tokio::sync::Mutex<HashMap<PathBuf, Arc<Bundle>>>> = OnceLock::new();
    BUNDLES.get_or_init(Default::default)
}

#[cfg_attr(not(feature = "api"), allow(dead_code))]
fn bundle_key(data_dir: &Path, namespace: &str) -> Option<PathBuf> {
    let namespace = validate_memory_namespace(namespace).ok()?;
    let root = memory_namespace_root(data_dir, &namespace);
    Some(std::fs::canonicalize(&root).unwrap_or(root))
}

/// Forget this process's handles to `namespace`'s stores (a closed request
/// context never runs again, UPCR-2026-034). The stores close once the last
/// runtime still holding them (a cached, now-refusing session) is dropped,
/// instead of staying open for the life of the process.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) async fn release_namespace_stores(data_dir: &Path, namespace: &str) {
    if let Some(key) = bundle_key(data_dir, namespace) {
        bundles().lock().await.remove(&key);
    }
}

/// Whether this process holds `namespace`'s stores open.
#[cfg(all(test, feature = "api"))]
pub(crate) async fn namespace_stores_open(data_dir: &Path, namespace: &str) -> bool {
    match bundle_key(data_dir, namespace) {
        Some(key) => bundles().lock().await.contains_key(&key),
        None => false,
    }
}

async fn open_bundle(profile: &ProfileRuntime, root: &Path) -> Result<Arc<Bundle>> {
    std::fs::create_dir_all(root)
        .wrap_err_with(|| format!("create memory namespace root {}", root.display()))?;
    let key = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut map = bundles().lock().await;
    if let Some(existing) = map.get(&key).cloned() {
        return Ok(existing);
    }
    let dimension = profile
        .embedder
        .as_ref()
        .map_or(octos_memory::EPISODIC_INDEX_DIMENSION, |e| e.dimension());
    let episodes = Arc::new(
        EpisodeStore::open_with_dimension(&key, dimension)
            .await
            .wrap_err_with(|| format!("open namespaced episode store {}", key.display()))?,
    );
    let memory_store = Arc::new(
        MemoryStore::open(&key)
            .await
            .wrap_err_with(|| format!("open namespaced memory store {}", key.display()))?,
    );
    let recall =
        super::profile::open_recall_store(&key, &profile.config, profile.embedder.as_deref())
            .await
            .wrap_err_with(|| format!("open namespaced recall store {}", key.display()))?;
    let bundle = Arc::new(Bundle {
        episodes,
        memory_store,
        recall,
    });
    map.insert(key, bundle.clone());
    Ok(bundle)
}

/// Erase the stores of `namespace` (already validated) and of every namespace
/// nested under it (a peer's request contexts, `<ns>/ctx-<id>`): drop the
/// process's open handles first, so a namespace bound again later opens fresh
/// stores instead of the erased files, then delete the directory. Returns
/// whether a directory was removed. Used by `peer/purge` (UPCR-2026-034).
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) async fn erase_memory_namespace(data_dir: &Path, namespace: &str) -> Result<bool> {
    let root = memory_namespace_root(data_dir, namespace);
    let canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    {
        let mut map = bundles().lock().await;
        map.retain(|key, _| !(key.starts_with(&canonical) || key.starts_with(&root)));
    }
    // Never through a symlink out of the kernel's memory stores.
    crate::peers::purge::remove_tree_within(&data_dir.join(MEMORY_NAMESPACES_DIR), &root)
        .wrap_err_with(|| format!("erase memory namespace {}", root.display()))
}

/// Re-register the profile's memory tools on a session registry against
/// `memory`'s stores. Only tools the profile policy left in the registry are
/// replaced (a policy-denied tool stays absent), and `memory_note` is removed
/// when the stores are not served by the refresh pipeline.
pub(crate) fn rebind_memory_tools(
    tools: &mut octos_agent::ToolRegistry,
    memory: &SessionMemory,
    embedder: Option<Arc<dyn octos_llm::EmbeddingProvider>>,
) {
    if tools.get_tool("recall_memory").is_some() {
        tools.register(
            octos_agent::RecallMemoryTool::new(memory.memory_store.clone())
                .with_recall(memory.recall.clone(), embedder.clone()),
        );
    }
    if tools.get_tool("memory_search").is_some() {
        tools.register(octos_agent::MemorySearchTool::new(
            memory.recall.clone(),
            embedder.clone(),
        ));
    }
    if tools.get_tool("memory_load").is_some() {
        tools.register(octos_agent::MemoryLoadTool::new(
            memory.recall.clone(),
            memory.memory_store.clone(),
        ));
    }
    if tools.get_tool("save_memory").is_some() {
        tools.register(octos_agent::SaveMemoryTool::new(
            memory.memory_store.clone(),
        ));
    }
    if tools.get_tool("record_memory_use").is_some() {
        tools.register(octos_agent::RecordMemoryUseTool::new(
            memory.memory_store.clone(),
        ));
    }
    if memory.refresh_enabled {
        if tools.get_tool("memory_note").is_some() {
            tools.register(octos_agent::MemoryNoteTool::new(
                memory.memory_store.clone(),
            ));
        }
    } else {
        tools.retain(|name| name != "memory_note");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_app_account_namespaces() {
        assert_eq!(
            validate_memory_namespace("app/dev.rinx/acct-3f2a").unwrap(),
            "app/dev.rinx/acct-3f2a"
        );
        assert_eq!(validate_memory_namespace(" system ").unwrap(), "system");
    }

    #[test]
    fn should_reject_namespaces_that_could_escape_or_alias() {
        for bad in [
            "",
            "/",
            "app//x",
            "../x",
            "app/..",
            "app/.hidden",
            "App/x",
            "app/x y",
            "app\\x",
            "app/x/",
            "a/b/c/d/e/f/g/h/i",
            &"a".repeat(65),
            &format!(
                "{}/{}/{}/{}",
                "a".repeat(60),
                "b".repeat(60),
                "c".repeat(60),
                "d".repeat(60)
            ),
        ] {
            assert!(
                validate_memory_namespace(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn should_root_each_segment_as_one_directory_under_the_namespaces_dir() {
        let root = memory_namespace_root(Path::new("/data"), "app/rinx/acct-1");
        assert_eq!(root, Path::new("/data/memory-namespaces/app/rinx/acct-1"));
    }
}
