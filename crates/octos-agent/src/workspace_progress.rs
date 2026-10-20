//! Bounded, workspace-local change observation and artifact-bound evidence.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};

const MAX_DIRS: usize = 512;
const MAX_FILES: usize = 8192;
const MAX_PENDING: usize = 1024;
const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
const MAX_SCAN_BYTES: u64 = 32 * 1024 * 1024;

/// Reads relative regular files through a directory descriptor. Never follows
/// a symlink or accepts `..`; evidence reading has the same boundary as change
/// observation. Contents are never included in model-facing summaries.
pub(crate) struct ConfinedRoot {
    pub path: PathBuf,
    #[cfg(unix)]
    directory: std::fs::File,
}

impl ConfinedRoot {
    pub fn new(root: &Path) -> io::Result<Self> {
        let path = root.canonicalize()?;
        if !path.is_dir() {
            return Err(io::Error::other("workspace is not a directory"));
        }
        #[cfg(unix)]
        let directory = std::fs::File::from(rustix::fs::open(
            &path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?);
        Ok(Self {
            path,
            #[cfg(unix)]
            directory,
        })
    }

    pub fn open(&self, relative: &Path, limit: u64) -> io::Result<std::fs::File> {
        let parts: Vec<_> = relative.components().collect();
        if parts.is_empty() || parts.iter().any(|p| !matches!(p, Component::Normal(_))) {
            return Err(io::Error::other(
                "expected a workspace-relative path without traversal",
            ));
        }
        #[cfg(unix)]
        let file = {
            use rustix::fs::{Mode, OFlags, openat};
            let mut parent = self.directory.try_clone()?;
            for part in &parts[..parts.len() - 1] {
                parent = std::fs::File::from(openat(
                    &parent,
                    part.as_os_str(),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
            }
            std::fs::File::from(openat(
                &parent,
                parts.last().unwrap().as_os_str(),
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?)
        };
        #[cfg(not(unix))]
        let file = {
            let mut path = self.path.clone();
            for part in parts {
                path.push(part.as_os_str());
                if path.symlink_metadata()?.is_symlink() {
                    return Err(io::Error::other("symlink rejected"));
                }
            }
            std::fs::File::open(path)?
        };
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() > limit {
            return Err(io::Error::other(
                "file type or size outside observation bounds",
            ));
        }
        Ok(file)
    }

    pub fn read(&self, relative: &Path, limit: u64) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open(relative, limit)?
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(io::Error::other("file grew beyond observation bound"));
        }
        Ok(bytes)
    }
}

/// Per-turn observer. Events are coalesced per drain, not counted as edits.
/// Multiple actors in the workspace are deliberately not attributed to a
/// particular tool. Large/binary files and dependency directories are excluded.
pub struct WorkspaceProgress {
    root: ConfinedRoot,
    watcher: RecommendedWatcher,
    pending: Arc<Mutex<BTreeSet<PathBuf>>>,
    incomplete: Arc<AtomicBool>,
    directories: HashSet<PathBuf>,
    fingerprints: HashMap<PathBuf, [u8; 32]>,
}

fn included(path: &Path) -> bool {
    path.components().all(|part| match part {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            !name.starts_with('.')
                && !matches!(
                    name.as_ref(),
                    "node_modules" | "target" | "vendor" | "__pycache__"
                )
        }
        _ => false,
    })
}

impl WorkspaceProgress {
    pub fn new(root: &Path) -> io::Result<Self> {
        let root = ConfinedRoot::new(root)?;
        let pending = Arc::new(Mutex::new(BTreeSet::new()));
        let incomplete = Arc::new(AtomicBool::new(false));
        let queue = pending.clone();
        let loss = incomplete.clone();
        let watcher =
            notify::recommended_watcher(move |event: notify::Result<Event>| match event {
                Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                    if event.need_rescan() {
                        loss.store(true, Ordering::Relaxed);
                    }
                    let Ok(mut queue) = queue.lock() else {
                        return;
                    };
                    for path in event.paths {
                        if queue.len() >= MAX_PENDING {
                            loss.store(true, Ordering::Relaxed);
                            break;
                        }
                        queue.insert(path);
                    }
                }
                Err(_) => loss.store(true, Ordering::Relaxed),
                _ => {}
            })
            .map_err(io::Error::other)?;
        let mut result = Self {
            root,
            watcher,
            pending,
            incomplete,
            directories: HashSet::new(),
            fingerprints: HashMap::new(),
        };
        result.discover(Path::new(""), false, &mut Vec::new());
        Ok(result)
    }

    pub fn incomplete(&self) -> bool {
        self.incomplete.load(Ordering::Relaxed)
    }

    fn fingerprint(&self, relative: &Path, budget: &mut u64) -> io::Result<[u8; 32]> {
        let bytes = self.root.read(relative, MAX_SOURCE_BYTES.min(*budget))?;
        *budget = budget.saturating_sub(bytes.len() as u64);
        if bytes.contains(&0) {
            return Err(io::Error::other("binary source excluded"));
        }
        Ok(Sha256::digest(&bytes).into())
    }

    fn discover(&mut self, relative: &Path, report: bool, changed: &mut Vec<PathBuf>) {
        let mut budget = MAX_SCAN_BYTES;
        let absolute = self.root.path.join(relative);
        let root = self.root.path.clone();
        let walk = ignore::WalkBuilder::new(&absolute)
            .follow_links(false)
            .filter_entry(move |entry| {
                entry.path() == root || entry.path().strip_prefix(&root).is_ok_and(included)
            })
            .build();
        for entry in walk {
            let Ok(entry) = entry else {
                self.incomplete.store(true, Ordering::Relaxed);
                continue;
            };
            let Ok(path) = entry
                .path()
                .strip_prefix(&self.root.path)
                .map(Path::to_path_buf)
            else {
                continue;
            };
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                if self.directories.contains(&path) {
                    continue;
                }
                if self.directories.len() >= MAX_DIRS {
                    self.incomplete.store(true, Ordering::Relaxed);
                    break;
                }
                if self
                    .watcher
                    .watch(entry.path(), RecursiveMode::NonRecursive)
                    .is_err()
                {
                    self.incomplete.store(true, Ordering::Relaxed);
                }
                self.directories.insert(path);
            } else if entry.file_type().is_some_and(|t| t.is_file()) {
                if self.fingerprints.len() >= MAX_FILES || budget == 0 {
                    self.incomplete.store(true, Ordering::Relaxed);
                    break;
                }
                if let Ok(hash) = self.fingerprint(&path, &mut budget) {
                    let previous = self.fingerprints.insert(path.clone(), hash);
                    if report && previous != Some(hash) {
                        changed.push(path);
                    }
                }
            }
        }
    }

    pub fn drain(&mut self) -> Vec<PathBuf> {
        let paths = self
            .pending
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default();
        let mut changed = Vec::new();
        let mut budget = MAX_SCAN_BYTES;
        for absolute in paths {
            let Ok(relative) = absolute.strip_prefix(&self.root.path) else {
                continue;
            };
            if relative.as_os_str().is_empty() {
                continue;
            }
            if !included(relative) {
                continue;
            }
            if absolute.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                // Parent-directory notifications are not a reason to rescan
                // the entire tree. Only discover previously unseen directories.
                if !self.directories.contains(relative) {
                    self.discover(relative, true, &mut changed);
                }
                continue;
            }
            if self.fingerprints.len() >= MAX_FILES && !self.fingerprints.contains_key(relative) {
                self.incomplete.store(true, Ordering::Relaxed);
                continue;
            }
            match self.fingerprint(relative, &mut budget) {
                Ok(hash) => {
                    if self.fingerprints.insert(relative.to_path_buf(), hash) != Some(hash) {
                        changed.push(relative.to_path_buf());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if self.fingerprints.remove(relative).is_some() {
                        changed.push(relative.to_path_buf());
                    }
                }
                Err(_) => {}
            }
            if budget == 0 {
                self.incomplete.store(true, Ordering::Relaxed);
                break;
            }
        }
        changed.sort();
        changed.dedup();
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_changed(observer: &mut WorkspaceProgress) -> Vec<PathBuf> {
        let end = Instant::now() + Duration::from_secs(4);
        loop {
            let changed = observer.drain();
            if !changed.is_empty() || Instant::now() >= end {
                return changed;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    #[test]
    fn should_observe_shell_write_even_when_command_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("layout.py"), "before").unwrap();
        let mut observer = WorkspaceProgress::new(dir.path()).unwrap();
        #[cfg(unix)]
        {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg("printf after > layout.py; exit 7")
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(!status.success());
            assert_eq!(
                wait_changed(&mut observer),
                vec![PathBuf::from("layout.py")]
            );
        }
    }

    #[test]
    fn should_observe_atomic_replacement_and_ignore_noop() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("layout.py");
        std::fs::write(&file, "before").unwrap();
        let mut observer = WorkspaceProgress::new(dir.path()).unwrap();
        std::fs::write(&file, "before").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(observer.drain().is_empty());
        std::fs::write(dir.path().join(".replacement"), "after").unwrap();
        std::fs::rename(dir.path().join(".replacement"), &file).unwrap();
        assert_eq!(
            wait_changed(&mut observer),
            vec![PathBuf::from("layout.py")]
        );
    }

    #[test]
    fn should_reject_symlinks_and_traversal_in_evidence_reads() {
        let dir = tempfile::tempdir().unwrap();
        let root = ConfinedRoot::new(dir.path()).unwrap();
        assert!(root.read(Path::new("../outside"), 100).is_err());
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("private"), "secret").unwrap();
            std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
            assert!(root.read(Path::new("link/private"), 100).is_err());
        }
    }

    #[test]
    fn should_observe_new_source_directory_without_dependency_noise() {
        let dir = tempfile::tempdir().unwrap();
        let mut observer = WorkspaceProgress::new(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/noise.js"), "noise").unwrap();
        std::fs::create_dir(dir.path().join("source")).unwrap();
        std::fs::write(dir.path().join("source/new.py"), "new").unwrap();
        assert_eq!(
            wait_changed(&mut observer),
            vec![PathBuf::from("source/new.py")]
        );
    }
}
