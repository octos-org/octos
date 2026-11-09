//! Per-profile content catalog: indexes agent-generated files (reports, audio,
//! slides, images, etc.) and provides query/filter/delete operations.
//!
//! Storage: JSON file at `{data_dir}/content-catalog.json`.
//! Thumbnails: JPEG images at `{data_dir}/.thumbnails/{id}.jpg`.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

const CATALOG_FILENAME: &str = "content-catalog.json";
const THUMBNAIL_DIR: &str = ".thumbnails";
const THUMBNAIL_MAX_WIDTH: u32 = 200;

// Directories to skip during full_scan (internal data, not user content).
const SKIP_DIRS: &[&str] = &[
    "sessions",
    "memory",
    "skills",
    "history",
    "users", // per-user dirs scanned separately via workspace/ path
    "logs",
    ".thumbnails",
    "whatsapp-auth",
    "bundled-app-skills",
    "voice_profiles",
];

// ── Data model ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ContentCategory {
    Report,
    Audio,
    Slides,
    Image,
    Video,
    Other,
}

impl ContentCategory {
    /// Determine category from file extension.
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "md" | "txt" | "pdf" | "docx" | "doc" | "rtf" | "html" | "htm" | "csv" | "json"
            | "xml" | "log" => Self::Report,
            "mp3" | "wav" | "ogg" | "m4a" | "opus" | "flac" | "aac" | "wma" => Self::Audio,
            "pptx" | "ppt" | "key" | "odp" => Self::Slides,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "tiff" | "tif" => {
                Self::Image
            }
            "mp4" | "webm" | "mov" | "avi" | "mkv" | "flv" => Self::Video,
            _ => Self::Other,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::Audio => "audio",
            Self::Slides => "slides",
            Self::Image => "image",
            Self::Video => "video",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentEntry {
    pub id: String,
    pub filename: String,
    pub path: String,
    pub category: ContentCategory,
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

// ── Query types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ContentQuery {
    pub category: Option<String>,
    pub search: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    #[serde(default = "default_sort")]
    pub sort: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

impl Default for ContentQuery {
    fn default() -> Self {
        Self {
            category: None,
            search: None,
            from: None,
            to: None,
            sort: default_sort(),
            limit: default_limit(),
            offset: 0,
        }
    }
}

fn default_sort() -> String {
    "newest".into()
}
fn default_limit() -> usize {
    50
}

#[derive(Debug, Serialize)]
pub struct ContentQueryResult {
    pub entries: Vec<ContentEntry>,
    pub total: usize,
}

// ── Catalog ────────────────────────────────────────────────────────────

pub struct ContentCatalog {
    entries: Vec<ContentEntry>,
    catalog_path: PathBuf,
    thumbnail_dir: PathBuf,
}

impl ContentCatalog {
    /// Open or create a catalog for a profile data directory.
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let catalog_path = data_dir.join(CATALOG_FILENAME);
        let thumbnail_dir = data_dir.join(THUMBNAIL_DIR);

        let entries = if catalog_path.exists() {
            let content = std::fs::read_to_string(&catalog_path)?;
            serde_json::from_str(&content).unwrap_or_else(|e| {
                warn!("corrupt content catalog, starting fresh: {e}");
                Vec::new()
            })
        } else {
            Vec::new()
        };

        Ok(Self {
            entries,
            catalog_path,
            thumbnail_dir,
        })
    }

    /// Persist catalog to disk (atomic write via temp + rename).
    fn save(&self) -> io::Result<()> {
        let json = serde_json::to_string_pretty(&self.entries).map_err(io::Error::other)?;
        let tmp = self.catalog_path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &self.catalog_path)?;
        Ok(())
    }

    /// Index a single file. Returns the new entry ID, or None if already indexed.
    pub fn index_file(
        &mut self,
        path: &Path,
        session_id: Option<&str>,
        tool_name: Option<&str>,
        caption: Option<&str>,
    ) -> io::Result<Option<String>> {
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let path_str = abs_path.to_string_lossy().to_string();

        // Skip if already indexed by path.
        if self.entries.iter().any(|e| e.path == path_str) {
            return Ok(None);
        }

        let meta = std::fs::metadata(&abs_path)?;
        if !meta.is_file() {
            return Ok(None);
        }

        let filename = abs_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let ext = abs_path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let category = ContentCategory::from_extension(&ext);
        let id = uuid::Uuid::now_v7().to_string();

        // Generate thumbnail for images.
        let thumbnail_path = if category == ContentCategory::Image {
            self.generate_thumbnail(&abs_path, &id).ok()
        } else {
            None
        };

        let entry = ContentEntry {
            id: id.clone(),
            filename,
            path: path_str,
            category,
            size_bytes: meta.len(),
            created_at: Utc::now(),
            thumbnail_path,
            session_id: session_id.map(|s| s.to_string()),
            tool_name: tool_name.map(|s| s.to_string()),
            caption: caption.map(|s| s.to_string()),
        };

        self.entries.push(entry);
        self.save()?;
        Ok(Some(id))
    }

    /// Walk the data directory and index any files not already in the catalog.
    /// Also prunes entries whose files no longer exist on disk.
    pub fn full_scan(&mut self, data_dir: &Path) -> io::Result<usize> {
        // Prune entries with missing files.
        let before = self.entries.len();
        self.entries.retain(|e| Path::new(&e.path).exists());
        let pruned = before - self.entries.len();
        if pruned > 0 {
            info!(pruned, "pruned stale catalog entries");
        }

        // Collect known paths as owned strings to avoid borrowing self.
        let known_paths: HashSet<String> = self.entries.iter().map(|e| e.path.clone()).collect();

        // Collect new file paths first, then index them (avoids borrow conflict).
        let mut new_paths: Vec<PathBuf> = Vec::new();
        Self::walk_dir(data_dir, &known_paths, &mut |path| {
            new_paths.push(path.to_path_buf());
        })?;

        // Also scan per-user workspace directories for agent-generated content
        let users_dir = data_dir.join("users");
        if users_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&users_dir) {
                for entry in entries.flatten() {
                    let ws = entry.path().join("workspace");
                    if ws.exists() {
                        Self::walk_dir(&ws, &known_paths, &mut |path| {
                            new_paths.push(path.to_path_buf());
                        })?;
                    }
                }
            }
        }

        let mut indexed = 0;
        for path in &new_paths {
            match self.index_file(path, None, None, None) {
                Ok(Some(_)) => indexed += 1,
                Ok(None) => {} // already indexed (race)
                Err(e) => warn!(path = %path.display(), "failed to index: {e}"),
            }
        }

        if indexed > 0 {
            self.save()?;
        }
        Ok(indexed)
    }

    /// Recursive directory walker that skips internal dirs.
    fn walk_dir(dir: &Path, known: &HashSet<String>, cb: &mut impl FnMut(&Path)) -> io::Result<()> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                    continue;
                }
                Self::walk_dir(&path, known, cb)?;
            } else if path.is_file() {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if name == CATALOG_FILENAME {
                    continue;
                }
                let path_str = path.to_string_lossy().to_string();
                if !known.contains(&path_str) {
                    cb(&path);
                }
            }
        }
        Ok(())
    }

    /// Query and filter catalog entries.
    pub fn query(&self, q: &ContentQuery) -> ContentQueryResult {
        let mut filtered: Vec<&ContentEntry> = self.entries.iter().collect();

        // Filter by category.
        if let Some(ref cat) = q.category {
            filtered.retain(|e| e.category.as_str() == cat.as_str());
        }

        // Filter by search (filename + caption substring).
        if let Some(ref search) = q.search {
            let lower = search.to_lowercase();
            filtered.retain(|e| {
                e.filename.to_lowercase().contains(&lower)
                    || e.caption
                        .as_deref()
                        .map(|c| c.to_lowercase().contains(&lower))
                        .unwrap_or(false)
            });
        }

        // Filter by date range.
        if let Some(from) = q.from {
            filtered.retain(|e| e.created_at >= from);
        }
        if let Some(to) = q.to {
            filtered.retain(|e| e.created_at <= to);
        }

        let total = filtered.len();

        // Sort.
        match q.sort.as_str() {
            "oldest" => filtered.sort_by_key(|a| a.created_at),
            "name" => filtered.sort_by_key(|a| a.filename.to_lowercase()),
            "size" => filtered.sort_by_key(|a| std::cmp::Reverse(a.size_bytes)),
            _ => filtered.sort_by_key(|a| std::cmp::Reverse(a.created_at)), // newest
        }

        // Paginate.
        let entries: Vec<ContentEntry> = filtered
            .into_iter()
            .skip(q.offset)
            .take(q.limit)
            .cloned()
            .collect();

        ContentQueryResult { entries, total }
    }

    /// Look up an entry by ID.
    pub fn get(&self, id: &str) -> Option<&ContentEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Delete an entry by ID, removing the file and thumbnail from disk.
    pub fn delete(&mut self, id: &str) -> io::Result<bool> {
        let Some(idx) = self.entries.iter().position(|e| e.id == id) else {
            return Ok(false);
        };
        let entry = &self.entries[idx];

        // Remove file from disk.
        if let Err(e) = std::fs::remove_file(&entry.path) {
            if e.kind() != io::ErrorKind::NotFound {
                warn!(path = %entry.path, "failed to delete content file: {e}");
            }
        }

        // Remove thumbnail.
        if let Some(ref thumb) = entry.thumbnail_path {
            let _ = std::fs::remove_file(thumb);
        }

        self.entries.remove(idx);
        self.save()?;
        Ok(true)
    }

    /// Delete multiple entries by ID.
    pub fn bulk_delete(&mut self, ids: &[String]) -> io::Result<usize> {
        let id_set: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
        let mut deleted = 0;

        // Collect files to remove first.
        let to_remove: Vec<(String, Option<String>)> = self
            .entries
            .iter()
            .filter(|e| id_set.contains(e.id.as_str()))
            .map(|e| (e.path.clone(), e.thumbnail_path.clone()))
            .collect();

        for (path, thumb) in &to_remove {
            if let Err(e) = std::fs::remove_file(path) {
                if e.kind() != io::ErrorKind::NotFound {
                    warn!(path, "failed to delete content file: {e}");
                }
            }
            if let Some(t) = thumb {
                let _ = std::fs::remove_file(t);
            }
            deleted += 1;
        }

        self.entries.retain(|e| !id_set.contains(e.id.as_str()));
        self.save()?;
        Ok(deleted)
    }

    /// Generate a JPEG thumbnail for an image file.
    fn generate_thumbnail(&self, path: &Path, id: &str) -> io::Result<String> {
        std::fs::create_dir_all(&self.thumbnail_dir)?;
        let thumb_path = self.thumbnail_dir.join(format!("{id}.jpg"));

        let img = image::open(path).map_err(|e| io::Error::other(format!("image open: {e}")))?;
        let thumb = img.thumbnail(THUMBNAIL_MAX_WIDTH, THUMBNAIL_MAX_WIDTH);
        thumb
            .save(&thumb_path)
            .map_err(|e| io::Error::other(format!("thumbnail save: {e}")))?;

        Ok(thumb_path.to_string_lossy().to_string())
    }

    /// Total number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ── Manager (shared across profiles) ───────────────────────────────────

/// Manages content catalogs for multiple profiles, lazy-loading on first access.
pub struct ContentCatalogManager {
    catalogs: tokio::sync::Mutex<std::collections::HashMap<String, Arc<RwLock<ContentCatalog>>>>,
    profile_store: Arc<crate::profiles::ProfileStore>,
}

impl ContentCatalogManager {
    pub fn new(profile_store: Arc<crate::profiles::ProfileStore>) -> Self {
        Self {
            catalogs: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            profile_store,
        }
    }

    /// Get or create a catalog for a profile, identified by profile ID.
    pub async fn get_catalog(&self, profile_id: &str) -> io::Result<Arc<RwLock<ContentCatalog>>> {
        let mut map = self.catalogs.lock().await;
        if let Some(cat) = map.get(profile_id) {
            return Ok(Arc::clone(cat));
        }

        let profile = self
            .profile_store
            .get(profile_id)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "profile not found"))?;

        let data_dir = self.profile_store.resolve_data_dir(&profile);
        let catalog = ContentCatalog::open(&data_dir)?;
        let arc = Arc::new(RwLock::new(catalog));
        map.insert(profile_id.to_string(), Arc::clone(&arc));
        Ok(arc)
    }

    /// Get catalog for a profile and run a full scan.
    pub async fn get_catalog_with_scan(
        &self,
        profile_id: &str,
    ) -> io::Result<Arc<RwLock<ContentCatalog>>> {
        let catalog = self.get_catalog(profile_id).await?;
        let profile = self
            .profile_store
            .get(profile_id)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "profile not found"))?;
        let data_dir = self.profile_store.resolve_data_dir(&profile);
        let mut cat = catalog.write().await;
        let indexed = cat.full_scan(&data_dir)?;
        if indexed > 0 {
            info!(profile_id, indexed, "content catalog scan complete");
        }
        drop(cat);
        Ok(catalog)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────
