//! Search tools for host-scoped sessions. Every directory and file is opened
//! relative to an already-open workspace directory, without following links.
//! A concurrent shell cannot swap an ancestor for a link to private data.

use super::{Tool, ToolResult};
use async_trait::async_trait;
use eyre::{Result, eyre};
use globset::GlobBuilder;
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use regex::RegexBuilder;
use rustix::fs::{CWD, Dir, Mode, OFlags, openat};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::Read,
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};

const MAX_ENTRIES: usize = 20_000;
const MAX_DEPTH: usize = 32;
const MAX_FILE: u64 = 1_048_576;
const MAX_READ: u64 = 16 * MAX_FILE;
const MAX_OUTPUT: usize = 60_000;
const MAX_IGNORE_BYTES: u64 = 64 * 1024;
const MAX_IGNORE_LINES: usize = 2048;
const MAX_IGNORE_TOTAL: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
pub enum Kind {
    List,
    Glob,
    Grep,
}
pub struct HostConfinedSearch {
    root: PathBuf,
    kind: Kind,
}
impl HostConfinedSearch {
    pub fn new(root: impl Into<PathBuf>, kind: Kind) -> Self {
        Self {
            root: root.into(),
            kind,
        }
    }
}

/// A workspace image is read once through confined descriptors, then copied
/// into Octos's private media store before the model or another task sees it.
pub struct HostConfinedImage {
    root: PathBuf,
}
impl HostConfinedImage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn run(&self, args: &Value) -> Result<ToolResult> {
        const MAX_IMAGE: u64 = 5 * 1024 * 1024;
        let path = relative(&self.root, required(args, "path")?)?;
        let root = open_root(&self.root)?;
        let file = open_file(&root, &path)?;
        let size = file.metadata()?.len();
        let mut bytes = Vec::new();
        file.take(MAX_IMAGE + 1).read_to_end(&mut bytes)?;
        let format = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(("png", "image/png", true))
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Some(("jpeg", "image/jpeg", true))
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(("gif", "image/gif", true))
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(&b"WEBP"[..]) {
            Some(("webp", "image/webp", true))
        } else if bytes.starts_with(b"BM") {
            Some(("bmp", "image/bmp", false))
        } else if std::str::from_utf8(&bytes[..bytes.len().min(256)])
            .is_ok_and(|text| text.contains("<svg"))
        {
            Some(("svg", "image/svg+xml", false))
        } else {
            None
        };
        let Some((format, mime, supported)) = format else {
            return Ok(ToolResult {
                output: "view_image: unrecognized image format".into(),
                success: false,
                ..Default::default()
            });
        };
        let mut result = json!({"path":path.to_string_lossy(),"format":format,"mime_type":mime,"byte_length":size,"shown_to_model":false});
        let mut media = Vec::new();
        if size > MAX_IMAGE || bytes.len() as u64 > MAX_IMAGE {
            result["not_shown_because"] = json!("image exceeds 5 MiB");
        } else if !supported {
            result["not_shown_because"] =
                json!("format is not supported by the model; use PNG or JPEG");
        } else {
            let snapshot = octos_llm::vision::snapshot_image_bytes(&bytes)?;
            result["shown_to_model"] = json!(true);
            result["image_id"] = json!(snapshot.file_name().map(|name| name.to_string_lossy()));
            media.push(snapshot);
        }
        Ok(ToolResult {
            output: result.to_string(),
            success: true,
            model_media: media,
            ..Default::default()
        })
    }
}
#[async_trait]
impl Tool for HostConfinedImage {
    fn name(&self) -> &str {
        "view_image"
    }
    fn description(&self) -> &str {
        "Inspect a workspace image. PNG, JPEG, GIF and WEBP images up to 5 MiB are shown to the model; symbolic links are not followed."
    }
    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
    }
    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let root = self.root.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || Self { root }.run(&args)).await?
    }
}

fn open_root(root: &Path) -> Result<File> {
    if !root.is_absolute() || root.canonicalize()?.as_path() != root {
        return Err(eyre!("workspace changed"));
    }
    Ok(File::from(openat(
        CWD,
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}
fn open_dir(root: &File, path: &Path) -> Result<File> {
    let mut dir = root.try_clone()?;
    for part in path.components() {
        match part {
            Component::Normal(name) => {
                let fd = openat(
                    &dir,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?;
                dir = File::from(fd);
            }
            Component::CurDir => {}
            _ => return Err(eyre!("path outside workspace")),
        }
    }
    Ok(dir)
}
fn open_file(root: &File, path: &Path) -> Result<File> {
    let parent = path.parent().ok_or_else(|| eyre!("invalid file path"))?;
    let name = path.file_name().ok_or_else(|| eyre!("invalid file path"))?;
    let dir = open_dir(root, parent)?;
    let fd = openat(
        &dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let file = File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(eyre!("not a regular file"));
    }
    Ok(file)
}
fn relative(root: &Path, value: &str) -> Result<PathBuf> {
    if value.len() > 4096 || value.contains('\0') {
        return Err(eyre!("invalid path"));
    }
    let path = Path::new(value);
    let path = if path.is_absolute() {
        path.strip_prefix(root)
            .map_err(|_| eyre!("path outside workspace"))?
    } else {
        path
    };
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(eyre!("path outside workspace"));
    }
    if path == Path::new(".") {
        Ok(PathBuf::new())
    } else {
        Ok(path.to_path_buf())
    }
}
fn pattern(value: &str) -> Result<globset::GlobMatcher> {
    if value.is_empty() || value.len() > 1024 || value.starts_with('/') || value.contains("..") {
        return Err(eyre!("invalid glob pattern"));
    }
    Ok(GlobBuilder::new(value)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

/// Start a glob at its literal directory prefix, as the personal CLI does.
/// This permits `target/**/*.rs` while broad globs still prune build trees.
fn glob_anchor(value: &Path) -> (PathBuf, bool) {
    let mut anchor = PathBuf::new();
    for component in value.components() {
        let Component::Normal(name) = component else {
            return (anchor, true);
        };
        if name
            .to_string_lossy()
            .chars()
            .any(|c| matches!(c, '*' | '?' | '[' | '{' | '\\'))
        {
            return (anchor, true);
        }
        anchor.push(name);
    }
    (anchor, false)
}

struct IgnoreLayer {
    ignore: Option<Gitignore>,
    gitignore: Option<Gitignore>,
    exclude: Option<Gitignore>,
    has_git: bool,
}

/// Build matchers from already-open directories. Never let the ignore crate
/// reopen a workspace path: a concurrent shell may replace an ancestor.
fn ignore_file(dir: &File, name: &str, base: &Path, used: &mut u64) -> Result<Option<Gitignore>> {
    let fd = match openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(_) => return Ok(None),
    };
    let file = File::from(fd);
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Ok(None);
    }
    let cap = MAX_IGNORE_BYTES.min(MAX_IGNORE_TOTAL.saturating_sub(*used));
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(eyre!("ignore file exceeds shared search limit"));
    }
    *used += bytes.len() as u64;
    let text = std::str::from_utf8(&bytes).map_err(|_| eyre!("ignore file is not UTF-8"))?;
    let mut builder = GitignoreBuilder::new(base);
    for (index, line) in text.lines().enumerate() {
        if index >= MAX_IGNORE_LINES {
            return Err(eyre!("ignore file has too many rules"));
        }
        // Octos's ignore walker keeps valid lines when another line is invalid.
        let _ = builder.add_line(None, line);
    }
    Ok(builder.build().ok())
}

fn ignore_layer(dir: &File, base: &Path, used: &mut u64) -> Result<IgnoreLayer> {
    let git_dir = openat(
        dir,
        ".git",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()
    .map(File::from);
    let git_file = git_dir.is_none()
        && openat(
            dir,
            ".git",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .ok()
        .map(File::from)
        .is_some_and(|file| file.metadata().is_ok_and(|metadata| metadata.is_file()));
    let info = git_dir.as_ref().and_then(|git| {
        openat(
            git,
            "info",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()
        .map(File::from)
    });
    let exclude = match info {
        Some(info) => ignore_file(&info, "exclude", base, used)?,
        None => None,
    };
    Ok(IgnoreLayer {
        ignore: ignore_file(dir, ".ignore", base, used)?,
        gitignore: ignore_file(dir, ".gitignore", base, used)?,
        exclude,
        has_git: git_dir.is_some() || git_file,
    })
}

fn ignored(layers: &[IgnoreLayer], path: &Path, is_dir: bool) -> bool {
    // Like Octos's `ignore` walker, .ignore wins over .gitignore regardless
    // of depth; within one kind, the nearest directory wins.
    for matcher in layers
        .iter()
        .rev()
        .filter_map(|layer| layer.ignore.as_ref())
    {
        match matcher.matched(path, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
    }
    if !layers.iter().any(|layer| layer.has_git) {
        return false;
    }
    for layer in layers.iter().rev() {
        if let Some(matcher) = &layer.gitignore {
            match matcher.matched(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        if layer.has_git {
            break;
        }
    }
    for layer in layers.iter().rev() {
        if let Some(matcher) = &layer.exclude {
            match matcher.matched(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        if layer.has_git {
            break;
        }
    }
    false
}

struct Walk {
    seen: usize,
    read: u64,
    truncated: bool,
    stopped: bool,
    honor_ignore: bool,
    layers: Vec<IgnoreLayer>,
    ignore_read: u64,
}
impl Walk {
    fn new() -> Self {
        Self {
            seen: 0,
            read: 0,
            truncated: false,
            stopped: false,
            honor_ignore: false,
            layers: Vec::new(),
            ignore_read: 0,
        }
    }

    /// Load the workspace's ancestors for an explicitly rooted grep. Each
    /// component stays attached to its parent descriptor throughout the walk.
    fn grep_root(&mut self, root: &File, path: &Path) -> Result<File> {
        self.honor_ignore = true;
        let mut dir = root.try_clone()?;
        let mut base = PathBuf::new();
        for part in path.components() {
            match part {
                Component::Normal(name) => {
                    self.layers
                        .push(ignore_layer(&dir, &base, &mut self.ignore_read)?);
                    dir = File::from(openat(
                        &dir,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?);
                    base.push(name);
                }
                Component::CurDir => {}
                _ => return Err(eyre!("path outside workspace")),
            }
        }
        Ok(dir)
    }
    fn visit<F>(&mut self, dir: &File, base: &Path, depth: usize, visitor: &mut F) -> Result<()>
    where
        F: FnMut(&Path, &mut File, bool, &mut Self) -> bool,
    {
        if depth > MAX_DEPTH {
            self.truncated = true;
            return Ok(());
        }
        if self.honor_ignore {
            self.layers
                .push(ignore_layer(dir, base, &mut self.ignore_read)?);
        }
        let mut entries = Dir::read_from(dir)?;
        let mut names = Vec::new();
        while let Some(entry) = entries.read() {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            if self.seen + names.len() >= MAX_ENTRIES {
                self.truncated = true;
                break;
            }
            let name = std::ffi::OsStr::from_bytes(name);
            if name
                .to_str()
                .is_none_or(|s| s.chars().any(char::is_control))
            {
                continue;
            }
            names.push(name.to_os_string());
        }
        names.sort();
        for name in names {
            self.seen += 1;
            if self.seen > MAX_ENTRIES {
                self.truncated = true;
                break;
            }
            let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let (fd, is_dir) = match openat(dir, &name, dir_flags, Mode::empty()) {
                Ok(fd) => (fd, true),
                Err(_) => match openat(
                    dir,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                    Mode::empty(),
                ) {
                    Ok(fd) => (fd, false),
                    Err(_) => continue,
                },
            };
            let mut file = File::from(fd);
            if !is_dir && !file.metadata().is_ok_and(|m| m.is_file()) {
                continue;
            }
            let path = base.join(&name);
            if self.honor_ignore && ignored(&self.layers, &path, is_dir) {
                continue;
            }
            if !visitor(&path, &mut file, is_dir, self) {
                self.truncated = true;
                self.stopped = true;
                break;
            }
            if is_dir
                && name != ".git"
                && (self.honor_ignore
                    || !matches!(name.to_str(), Some("node_modules" | "target" | "Library")))
            {
                self.visit(&file, &path, depth + 1, visitor)?;
            }
            if self.stopped {
                break;
            }
            if self.seen >= MAX_ENTRIES {
                self.truncated = true;
                break;
            }
        }
        if self.honor_ignore {
            self.layers.pop();
        }
        Ok(())
    }
}
fn limit(args: &Value, fallback: usize) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(fallback as u64)
        .clamp(1, 500) as usize
}
fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("missing {key}"))
}
fn lines_result(lines: &[String], truncated: bool) -> String {
    let mut output = lines.join("\n");
    if truncated {
        output.push_str("\n[search truncated; narrow the path or pattern]");
    }
    if output.is_empty() {
        output.push_str("No matches.");
    }
    output
}
impl HostConfinedSearch {
    fn run(&self, args: &Value) -> Result<ToolResult> {
        let root = open_root(&self.root)?;
        let mut walk = Walk::new();
        let mut lines = Vec::<String>::new();
        match self.kind {
            Kind::List => {
                let path = relative(&self.root, required(args, "path")?)?;
                let dir = open_dir(&root, &path)?;
                let mut entries = Dir::read_from(&dir)?;
                let mut output_bytes = 0usize;
                while let Some(entry) = entries.read() {
                    let entry = entry?;
                    let name = entry.file_name().to_bytes();
                    if name == b"." || name == b".." {
                        continue;
                    }
                    let name = std::ffi::OsStr::from_bytes(name);
                    let Some(name_text) =
                        name.to_str().filter(|s| !s.chars().any(char::is_control))
                    else {
                        continue;
                    };
                    if lines.len() >= 500 || output_bytes + name_text.len() + 8 > MAX_OUTPUT {
                        walk.truncated = true;
                        break;
                    }
                    let directory = openat(
                        &dir,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .is_ok();
                    let regular = !directory
                        && openat(
                            &dir,
                            name,
                            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                            Mode::empty(),
                        )
                        .ok()
                        .map(File::from)
                        .is_some_and(|f| f.metadata().is_ok_and(|m| m.is_file()));
                    if directory || regular {
                        output_bytes += name_text.len() + 8;
                        lines.push(format!(
                            "[{}] {name_text}",
                            if directory { "dir" } else { "file" }
                        ));
                    }
                }
                lines.sort();
            }
            Kind::Glob => {
                let raw = required(args, "pattern")?;
                let requested = relative(&self.root, raw)?;
                let value = requested
                    .to_str()
                    .ok_or_else(|| eyre!("invalid glob pattern"))?;
                let glob = pattern(value)?;
                let maximum = limit(args, 100);
                let (anchor, has_meta) = glob_anchor(&requested);
                if !has_meta {
                    if open_file(&root, &anchor).is_ok() {
                        lines.push(anchor.display().to_string());
                    }
                } else if let Ok(dir) = open_dir(&root, &anchor) {
                    walk.visit(&dir, &anchor, 0, &mut |path, _file, is_dir, _| {
                        if !is_dir && glob.is_match(path) {
                            lines.push(path.display().to_string());
                        }
                        lines.len() < maximum
                            && lines.iter().map(String::len).sum::<usize>() < MAX_OUTPUT
                    })?;
                }
            }
            Kind::Grep => {
                let needle = required(args, "pattern")?;
                if needle.is_empty() || needle.len() > 4096 {
                    return Err(eyre!("invalid regex"));
                }
                let regex = RegexBuilder::new(needle)
                    .case_insensitive(
                        args.get("ignore_case")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                    .size_limit(1 << 20)
                    .build()?;
                let filter = args
                    .get("file_pattern")
                    .and_then(Value::as_str)
                    .map(pattern)
                    .transpose()?;
                let path = relative(
                    &self.root,
                    args.get("path").and_then(Value::as_str).unwrap_or("."),
                )?;
                let maximum = limit(args, 50);
                let context = args
                    .get("context")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(5) as usize;
                let mut matches = 0usize;
                let mut visit = |path: &Path, file: &mut File, is_dir: bool, walk: &mut Walk| {
                    if is_dir
                        || filter.as_ref().is_some_and(|g| {
                            !g.is_match(path) && !path.file_name().is_some_and(|n| g.is_match(n))
                        })
                    {
                        return true;
                    }
                    let Ok(metadata) = file.metadata() else {
                        return true;
                    };
                    if metadata.len() > MAX_FILE || walk.read + metadata.len() > MAX_READ {
                        walk.truncated = true;
                        return true;
                    }
                    let mut bytes = Vec::new();
                    if file.take(MAX_FILE + 1).read_to_end(&mut bytes).is_err() {
                        return true;
                    }
                    walk.read += bytes.len() as u64;
                    if walk.read > MAX_READ {
                        walk.truncated = true;
                        return false;
                    }
                    if bytes.len() as u64 > MAX_FILE {
                        walk.truncated = true;
                        return true;
                    }
                    let Ok(text) = std::str::from_utf8(&bytes) else {
                        return true;
                    };
                    if text.contains('\0') {
                        return true;
                    }
                    let source: Vec<&str> = text.lines().collect();
                    let mut emitted = 0usize;
                    for (i, line) in source.iter().enumerate() {
                        if !regex.is_match(line) {
                            continue;
                        }
                        matches += 1;
                        let start = i.saturating_sub(context).max(emitted);
                        let end = (i + context + 1).min(source.len());
                        for (n, value) in source.iter().enumerate().take(end).skip(start) {
                            let value: String = value.chars().take(500).collect();
                            lines.push(format!(
                                "{}{}{}{}{}",
                                path.display(),
                                if n == i { ":" } else { "-" },
                                n + 1,
                                if n == i { ":" } else { "-" },
                                value
                            ));
                        }
                        emitted = end;
                        if matches >= maximum
                            || lines.iter().map(String::len).sum::<usize>() >= MAX_OUTPUT
                        {
                            return false;
                        }
                    }
                    true
                };
                if let Ok(dir) = walk.grep_root(&root, &path) {
                    walk.visit(&dir, &path, 0, &mut visit)?;
                } else {
                    let mut file = open_file(&root, &path)?;
                    if !visit(&path, &mut file, false, &mut walk) {
                        walk.truncated = true;
                    }
                }
            }
        }
        Ok(ToolResult {
            output: lines_result(&lines, walk.truncated),
            success: true,
            ..Default::default()
        })
    }
}
#[async_trait]
impl Tool for HostConfinedSearch {
    fn name(&self) -> &str {
        match self.kind {
            Kind::List => "list_dir",
            Kind::Glob => "glob",
            Kind::Grep => "grep",
        }
    }
    fn description(&self) -> &str {
        match self.kind {
            Kind::List => "List a workspace directory without following symbolic links.",
            Kind::Glob => {
                "Find workspace paths matching a glob. Results are bounded and do not follow symbolic links."
            }
            Kind::Grep => {
                "Search workspace files using a regular expression. Results are bounded and do not follow symbolic links."
            }
        }
    }
    fn tags(&self) -> &[&str] {
        &["search", "code"]
    }
    fn input_schema(&self) -> Value {
        match self.kind {
            Kind::List => {
                json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
            }
            Kind::Glob => {
                json!({"type":"object","properties":{"pattern":{"type":"string"},"limit":{"type":"integer"}},"required":["pattern"]})
            }
            Kind::Grep => {
                json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"file_pattern":{"type":"string"},"limit":{"type":"integer"},"context":{"type":"integer"},"ignore_case":{"type":"boolean"}},"required":["pattern"]})
            }
        }
    }
    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let root = self.root.clone();
        let kind = self.kind;
        let args = args.clone();
        tokio::task::spawn_blocking(move || Self { root, kind }.run(&args)).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::symlink,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    #[tokio::test]
    async fn search_never_follows_links_outside_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("work");
        let private = temp.path().join("private");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&private).unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(
            root.join("nested/hello.txt"),
            "before\nneedle in room\nafter\n",
        )
        .unwrap();
        fs::write(private.join("secrets.txt"), "needle synthetic-secret").unwrap();
        symlink(&private, root.join("escape")).unwrap();
        symlink(private.join("secrets.txt"), root.join("file-link.txt")).unwrap();
        let root = root.canonicalize().unwrap();
        let list = HostConfinedSearch::new(&root, Kind::List);
        let glob = HostConfinedSearch::new(&root, Kind::Glob);
        let grep = HostConfinedSearch::new(&root, Kind::Grep);
        let image = HostConfinedImage::new(&root);
        let listing = list.execute(&json!({"path":"."})).await.unwrap().output;
        assert!(
            listing.contains("nested")
                && !listing.contains("escape")
                && !listing.contains("file-link")
        );
        assert!(list.execute(&json!({"path":"escape"})).await.is_err());
        assert!(
            grep.execute(&json!({"pattern":"needle","path":"escape"}))
                .await
                .is_err()
        );
        assert!(list.execute(&json!({"path":"../private"})).await.is_err());
        assert!(
            image
                .execute(&json!({"path":"file-link.txt"}))
                .await
                .is_err()
        );
        assert!(
            !image
                .execute(&json!({"path":"nested/hello.txt"}))
                .await
                .unwrap()
                .success
        );
        let found = glob
            .execute(&json!({"pattern":"**/*.txt"}))
            .await
            .unwrap()
            .output;
        assert!(
            found.contains("nested/hello.txt")
                && !found.contains("secret")
                && !found.contains("file-link")
        );
        let found = grep
            .execute(&json!({"pattern":"NEEDLE","ignore_case":true,"context":1}))
            .await
            .unwrap()
            .output;
        assert!(
            found.contains("needle in room")
                && found.contains("before")
                && found.contains("after")
                && !found.contains("synthetic-secret")
        );
        let single = grep
            .execute(&json!({"pattern":"needle","path":"nested/hello.txt"}))
            .await
            .unwrap();
        assert!(single.output.contains("needle in room"));
        assert!(
            grep.execute(&json!({"pattern":"needle","path":"file-link.txt"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn grep_honors_workspace_ignore_rules_and_glob_keeps_cli_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::create_dir_all(root.join("target/generated")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join(".gitignore"), "*.tmp\n!important.tmp\nignored/\n").unwrap();
        fs::write(root.join(".ignore"), "blocked.tmp\n").unwrap();
        fs::write(root.join(".git/info/exclude"), "local-only.txt\n").unwrap();
        fs::write(root.join("nested/.gitignore"), "!child.tmp\ninner.log\n").unwrap();
        for name in [
            "kept.txt",
            "hidden.tmp",
            "important.tmp",
            "blocked.tmp",
            "local-only.txt",
            "nested/child.tmp",
            "nested/inner.log",
            "nested/visible.txt",
            "ignored/hidden.txt",
            "target/generated/anchor.rs",
            "node_modules/pkg/visible.js",
        ] {
            fs::write(root.join(name), "needle\n").unwrap();
        }
        let root = root.canonicalize().unwrap();
        let grep = HostConfinedSearch::new(&root, Kind::Grep);
        let query = json!({"pattern":"needle", "limit":100});
        let confined = grep.execute(&query).await.unwrap().output;
        // Octos's personal grep uses this same walker. Keep the comparison
        // within the synthetic workspace instead of reading a user's global
        // git configuration or any parent-directory ignore file.
        let original = ignore::WalkBuilder::new(&root)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .parents(false)
            .build()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .filter(|entry| {
                fs::read_to_string(entry.path()).is_ok_and(|text| text.contains("needle"))
            })
            .map(|entry| {
                entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap()
                    .display()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for name in [
            "kept.txt",
            "important.tmp",
            "nested/child.tmp",
            "nested/visible.txt",
            "target/generated/anchor.rs",
            "node_modules/pkg/visible.js",
        ] {
            assert!(
                confined.contains(name) && original.contains(name),
                "{name}: confined={confined}; cli={original}"
            );
        }
        for name in [
            "hidden.tmp",
            "blocked.tmp",
            "local-only.txt",
            "inner.log",
            "ignored/hidden.txt",
        ] {
            assert!(
                !confined.contains(name) && !original.contains(name),
                "{name}: confined={confined}; cli={original}"
            );
        }
        let nested = grep
            .execute(&json!({"pattern":"needle", "path":"nested"}))
            .await
            .unwrap()
            .output;
        assert!(
            nested.contains("nested/child.tmp")
                && nested.contains("nested/visible.txt")
                && !nested.contains("inner.log")
        );
        let explicit = grep
            .execute(&json!({"pattern":"needle", "path":"hidden.tmp"}))
            .await
            .unwrap()
            .output;
        assert!(explicit.contains("hidden.tmp"));
        let glob = HostConfinedSearch::new(&root, Kind::Glob);
        let all = glob
            .execute(&json!({"pattern":"**/*.tmp"}))
            .await
            .unwrap()
            .output;
        assert!(all.contains("hidden.tmp") && all.contains("blocked.tmp"));
        let anchored = glob
            .execute(&json!({"pattern":"target/**/*.rs"}))
            .await
            .unwrap()
            .output;
        assert!(anchored.contains("target/generated/anchor.rs"));
        let broad = glob
            .execute(&json!({"pattern":"**/*.rs"}))
            .await
            .unwrap()
            .output;
        assert!(!broad.contains("target/generated/anchor.rs"));
        let exact = glob
            .execute(&json!({"pattern":"nested/visible.txt"}))
            .await
            .unwrap()
            .output;
        assert!(exact.contains("nested/visible.txt"));
    }

    #[tokio::test]
    async fn ignore_files_are_bounded_and_cannot_follow_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(temp.path().join("private-ignore"), "visible.txt\n").unwrap();
        symlink(temp.path().join("private-ignore"), root.join(".gitignore")).unwrap();
        fs::write(root.join("visible.txt"), "needle").unwrap();
        let root = root.canonicalize().unwrap();
        let grep = HostConfinedSearch::new(&root, Kind::Grep);
        let output = grep
            .execute(&json!({"pattern":"needle"}))
            .await
            .unwrap()
            .output;
        assert!(output.contains("visible.txt"));
        fs::remove_file(root.join(".gitignore")).unwrap();
        fs::write(
            root.join(".gitignore"),
            vec![b'x'; MAX_IGNORE_BYTES as usize + 1],
        )
        .unwrap();
        assert!(grep.execute(&json!({"pattern":"needle"})).await.is_err());
    }

    #[tokio::test]
    async fn concurrent_directory_swap_does_not_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("work");
        let private = temp.path().join("private");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&private).unwrap();
        fs::create_dir(root.join("race")).unwrap();
        fs::write(root.join("race/file.txt"), "workspace-only").unwrap();
        fs::write(private.join("file.txt"), "needle synthetic-secret").unwrap();
        let root = root.canonicalize().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_root = root.clone();
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                if fs::rename(worker_root.join("race"), worker_root.join("held")).is_ok() {
                    symlink(&private, worker_root.join("race")).unwrap();
                    std::thread::yield_now();
                    fs::remove_file(worker_root.join("race")).unwrap();
                    fs::rename(worker_root.join("held"), worker_root.join("race")).unwrap();
                }
            }
        });
        let grep = HostConfinedSearch::new(&root, Kind::Grep);
        for _ in 0..200 {
            let result = grep.execute(&json!({"pattern":"needle"})).await.unwrap();
            assert!(
                !result.output.contains("synthetic-secret"),
                "{}",
                result.output
            );
        }
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
    }
}
