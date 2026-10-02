//! Kernel-side git over agent-writable workspace repositories, without
//! honouring anything the agent can write.
//!
//! The workspace snapshot repos (`slides/<name>`, `sites/<name>`) live inside
//! the agent's workspace, so the model (through a sandboxed shell, or any
//! other writer) can edit `<project>/.git/config`, `.git/hooks/*` and
//! `.git/info/attributes`. Those files decide which programs git runs: clean
//! and smudge filters, `core.fsmonitor`, hooks (also via `core.hooksPath`),
//! `gpg.program`, `diff.external`, textconv drivers, `includeIf` pulls of other
//! config files. The kernel runs git unsandboxed, so honouring any of them is
//! arbitrary code execution as the kernel.
//!
//! Git has no switch that ignores repository-local config, and filter driver
//! names are unbounded, so enumerating `-c` overrides cannot be complete.
//! Instead every kernel git call here runs against a **private, per-call
//! `GIT_DIR`** in a fresh `0700` temp directory whose `config` the kernel
//! writes itself:
//!
//! - `GIT_DIR` = the private dir (config, `HEAD`, `refs/`, `hooks/` absent,
//!   `info/` absent). The project's own `.git/config`, `hooks/`, `info/` and
//!   `commondir` are never read.
//! - `GIT_OBJECT_DIRECTORY` = `<project>/.git/objects` and `GIT_INDEX_FILE` =
//!   `<project>/.git/index`: pure data, so history is shared with the
//!   project's `.git` and the agent keeps seeing it.
//! - system and global config are off (`GIT_CONFIG_NOSYSTEM`,
//!   `GIT_CONFIG_GLOBAL` = an empty private file, `HOME`/`XDG_CONFIG_HOME` =
//!   the private dir) and every inherited `GIT_*` variable is dropped, so no
//!   filter/diff/merge driver is defined anywhere: a worktree
//!   `.gitattributes` naming a driver is inert.
//! - `core.fsmonitor=false`, `core.hooksPath` = a missing private path,
//!   `commit.gpgSign=false`, `log.showSignature=false`, `gc.auto=0`,
//!   `maintenance.auto=false` are set both in the private config and on the
//!   command line (belt and braces).
//!
//! The branch tip is read from the project's `HEAD` + loose ref or
//! `packed-refs` by plain file parsing, and written back as a loose ref with
//! an exclusive temp file + rename (never through a symlink). Anything the
//! parser does not understand (reftable, a gitfile, a linked worktree, a
//! symlinked `.git`) fails closed.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use eyre::{Result, WrapErr, eyre};

/// The kernel-owned git directory for one call against one project.
pub(crate) struct PrivateGitDir {
    dir: tempfile::TempDir,
    work_tree: PathBuf,
    project_git: PathBuf,
    head: ProjectHead,
    tip: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProjectHead {
    /// `ref: refs/heads/<name>`.
    Branch(String),
    /// A detached commit id.
    Detached,
}

fn no_follow_is_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

fn no_follow_is_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

fn read_small_regular_file(path: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(eyre!("cannot stat {}: {e}", path.display())),
        Ok(meta) if !meta.file_type().is_file() => {
            Err(eyre!("{} is not a regular file", path.display()))
        }
        Ok(meta) if meta.len() > 16 * 1024 * 1024 => {
            Err(eyre!("{} is unexpectedly large", path.display()))
        }
        Ok(_) => {
            Ok(Some(std::fs::read_to_string(path).wrap_err_with(|| {
                format!("read {} failed", path.display())
            })?))
        }
    }
}

fn is_object_id(text: &str) -> bool {
    (text.len() == 40 || text.len() == 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_branch_ref(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("refs/heads/") else {
        return false;
    };
    !rest.is_empty()
        && rest.len() <= 200
        && rest.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.ends_with(".lock")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
        && !rest.contains("..")
}

/// Walk `project_git/<rel>` component by component refusing symlinks, and
/// create missing directories.
fn ensure_real_dirs(base: &Path, rel_dirs: &Path) -> Result<PathBuf> {
    let mut current = base.to_path_buf();
    for component in rel_dirs.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => return Err(eyre!("{} is not a real directory", current.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)
                    .wrap_err_with(|| format!("create {} failed", current.display()))?;
            }
            Err(e) => return Err(eyre!("cannot stat {}: {e}", current.display())),
        }
    }
    Ok(current)
}

/// Replace `path` with `content` via an exclusive, no-follow temp file in the
/// same directory and a rename (which replaces a symlink, never follows it).
fn replace_file_atomically(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("{} has no parent", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| eyre!("{} has no file name", path.display()))?;
    let mut tmp_name = OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".octos-{}.tmp", std::process::id()));
    let tmp = parent.join(tmp_name);
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = opts
        .open(&tmp)
        .wrap_err_with(|| format!("create {} failed", tmp.display()))?;
    file.write_all(content.as_bytes())?;
    file.sync_all().ok();
    drop(file);
    std::fs::rename(&tmp, path).wrap_err_with(|| format!("rename onto {} failed", path.display()))
}

/// The null device, for git settings that want "no file here".
fn null_path() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

impl PrivateGitDir {
    /// Prepare a private git dir for the repository at `<work_tree>/.git`.
    /// The project `.git` must already exist as a real directory.
    pub(crate) fn open(work_tree: &Path) -> Result<Self> {
        let project_git = work_tree.join(".git");
        if !no_follow_is_dir(&project_git) {
            return Err(eyre!(
                "{} is not a plain git directory (symlink, gitfile or missing)",
                project_git.display()
            ));
        }
        for unsupported in ["reftable", "commondir"] {
            if std::fs::symlink_metadata(project_git.join(unsupported)).is_ok() {
                return Err(eyre!(
                    "{} uses {unsupported}, which the workspace snapshot does not support",
                    project_git.display()
                ));
            }
        }
        if !no_follow_is_dir(&project_git.join("objects")) {
            return Err(eyre!(
                "{} is not a real directory",
                project_git.join("objects").display()
            ));
        }
        let index = project_git.join("index");
        if std::fs::symlink_metadata(&index).is_ok() && !no_follow_is_file(&index) {
            return Err(eyre!("{} is not a regular file", index.display()));
        }

        let head_text = read_small_regular_file(&project_git.join("HEAD"))?
            .ok_or_else(|| eyre!("{} has no HEAD", project_git.display()))?;
        let head_text = head_text.trim();
        let (head, tip) = if let Some(target) = head_text.strip_prefix("ref:") {
            let target = target.trim();
            if !valid_branch_ref(target) {
                return Err(eyre!("unsupported HEAD target {target:?}"));
            }
            let tip = Self::read_branch_tip(&project_git, target)?;
            (ProjectHead::Branch(target.to_string()), tip)
        } else if is_object_id(head_text) {
            (ProjectHead::Detached, Some(head_text.to_ascii_lowercase()))
        } else {
            return Err(eyre!(
                "unsupported HEAD contents in {}",
                project_git.display()
            ));
        };

        let dir = tempfile::Builder::new()
            .prefix("octos-git-")
            .tempdir()
            .wrap_err("create private git dir failed")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        }
        let private = Self {
            dir,
            work_tree: work_tree.to_path_buf(),
            project_git,
            head,
            tip,
        };
        private.write_private_layout()?;
        Ok(private)
    }

    fn read_branch_tip(project_git: &Path, refname: &str) -> Result<Option<String>> {
        // `valid_branch_ref` bounds refname to plain components, so this join
        // stays inside `.git/refs/heads`; the reads never follow symlinks.
        if let Some(text) = read_small_regular_file(&project_git.join(refname))? {
            let id = text.trim();
            if !is_object_id(id) {
                return Err(eyre!("unsupported ref contents for {refname}"));
            }
            return Ok(Some(id.to_ascii_lowercase()));
        }
        if let Some(packed) = read_small_regular_file(&project_git.join("packed-refs"))? {
            for line in packed.lines() {
                if line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                if let Some((id, name)) = line.split_once(' ')
                    && name.trim() == refname
                    && is_object_id(id)
                {
                    return Ok(Some(id.to_ascii_lowercase()));
                }
            }
        }
        Ok(None)
    }

    fn sha256(&self) -> bool {
        self.tip.as_ref().is_some_and(|id| id.len() == 64)
    }

    fn missing_hooks_dir(&self) -> PathBuf {
        self.dir.path().join("no-hooks")
    }

    /// Render a path for embedding in a generated `.git/config` value.
    ///
    /// Git config values are quoted strings where a backslash starts an
    /// escape sequence, so a bare Windows path (`C:\Users\...`) written
    /// verbatim both breaks out of the quoting context and parses as
    /// invalid escapes ("bad config line N"). Forward slashes are
    /// accepted by git on every platform, including Git for Windows.
    fn config_hooks_value(path: &std::path::Path) -> String {
        path.display().to_string().replace('\\', "/")
    }

    fn write_private_layout(&self) -> Result<()> {
        let root = self.dir.path();
        std::fs::create_dir_all(root.join("objects"))?;
        std::fs::create_dir_all(root.join("refs/heads"))?;
        std::fs::create_dir_all(root.join("refs/tags"))?;
        std::fs::write(root.join("empty-global-config"), "")?;
        let format = if self.sha256() {
            "\trepositoryformatversion = 1\n[extensions]\n\tobjectformat = sha256\n[core]\n"
        } else {
            "\trepositoryformatversion = 0\n"
        };
        let hooks = Self::config_hooks_value(&self.missing_hooks_dir());
        let config = format!(
            "[core]\n{format}\tbare = false\n\tlogallrefupdates = false\n\tfsmonitor = false\n\
             \thooksPath = \"{hooks}\"\n\tuntrackedCache = false\n\tsymlinks = true\n\
             [gc]\n\tauto = 0\n[maintenance]\n\tauto = false\n[commit]\n\tgpgSign = false\n\
             [tag]\n\tgpgSign = false\n[log]\n\tshowSignature = false\n\
             [user]\n\tname = Octos Workspace\n\temail = octos@local\n",
        );
        std::fs::write(root.join("config"), config)?;
        match (&self.head, &self.tip) {
            (ProjectHead::Branch(name), tip) => {
                std::fs::write(root.join("HEAD"), format!("ref: {name}\n"))?;
                if let Some(id) = tip {
                    let path = root.join(name);
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(path, format!("{id}\n"))?;
                }
            }
            (ProjectHead::Detached, Some(id)) => {
                std::fs::write(root.join("HEAD"), format!("{id}\n"))?;
            }
            (ProjectHead::Detached, None) => unreachable!("detached HEAD always has an id"),
        }
        Ok(())
    }

    /// A `git` command bound to the private dir and the project's worktree,
    /// objects and index, with every inherited `GIT_*` variable dropped.
    pub(crate) fn command(&self) -> Command {
        let root = self.dir.path();
        let mut cmd = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("GIT_DIR", root)
            .env("GIT_WORK_TREE", &self.work_tree)
            .env("GIT_OBJECT_DIRECTORY", self.project_git.join("objects"))
            .env("GIT_INDEX_FILE", self.project_git.join("index"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.join("empty-global-config"))
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root)
            .current_dir(&self.work_tree)
            .args(["-c", "core.fsmonitor=false", "-c"])
            .arg(format!(
                "core.hooksPath={}",
                self.missing_hooks_dir().display()
            ))
            .args([
                "-c",
                "commit.gpgSign=false",
                "-c",
                "log.showSignature=false",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "-c",
            ])
            .arg(format!("core.attributesFile={}", null_path()));
        cmd
    }

    /// The commit id the private `HEAD` points at now, if any.
    fn private_tip(&self) -> Result<Option<String>> {
        let output = self
            .command()
            .args(["rev-parse", "--verify", "--quiet", "HEAD"])
            .output()
            .wrap_err("git rev-parse HEAD failed")?;
        if !output.status.success() {
            return Ok(None);
        }
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !is_object_id(&id) {
            return Err(eyre!("git rev-parse returned {id:?}"));
        }
        Ok(Some(id))
    }

    /// Copy the private tip back into the project's `.git` when it moved.
    pub(crate) fn publish_head(&self) -> Result<()> {
        let Some(new_tip) = self.private_tip()? else {
            return Ok(());
        };
        if self.tip.as_deref() == Some(new_tip.as_str()) {
            return Ok(());
        }
        match &self.head {
            ProjectHead::Branch(name) => {
                let rel = Path::new(name);
                let parent_rel = rel.parent().unwrap_or(Path::new(""));
                let parent = ensure_real_dirs(&self.project_git, parent_rel)?;
                let file = rel
                    .file_name()
                    .ok_or_else(|| eyre!("branch ref {name} has no leaf"))?;
                replace_file_atomically(&parent.join(file), &format!("{new_tip}\n"))
            }
            ProjectHead::Detached => {
                replace_file_atomically(&self.project_git.join("HEAD"), &format!("{new_tip}\n"))
            }
        }
    }
}

/// Create `<work_tree>/.git` with `git init`, without templates and without
/// reading system or global config, then record the snapshot identity in it
/// (for the agent's own later use of the repo; the kernel never reads it).
pub(crate) fn init_project_repo(work_tree: &Path) -> Result<()> {
    let scratch = tempfile::Builder::new()
        .prefix("octos-git-init-")
        .tempdir()
        .wrap_err("create git init scratch dir failed")?;
    let empty_global = scratch.path().join("empty-global-config");
    let templates = scratch.path().join("templates");
    std::fs::write(&empty_global, "")?;
    std::fs::create_dir_all(&templates)?;
    let mut cmd = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    let output = cmd
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &empty_global)
        .env("HOME", scratch.path())
        .env("XDG_CONFIG_HOME", scratch.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("init")
        .arg("--quiet")
        .arg(format!("--template={}", templates.display()))
        .arg("--initial-branch=main")
        .arg("--")
        .arg(work_tree)
        .output()
        .wrap_err("failed to spawn git init")?;
    if !output.status.success() {
        return Err(eyre!(
            "git init failed in {}: {}",
            work_tree.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let config = work_tree.join(".git").join("config");
    let mut text = read_small_regular_file(&config)?.unwrap_or_default();
    text.push_str("[user]\n\tname = Octos Workspace\n\temail = octos@local\n");
    replace_file_atomically(&config, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_plain_branch_refs_when_validating_head_targets() {
        assert!(valid_branch_ref("refs/heads/main"));
        assert!(valid_branch_ref("refs/heads/feature/x-1"));
        for bad in [
            "refs/heads/",
            "refs/heads/../../config",
            "refs/heads/.hidden",
            "refs/heads/a..b",
            "refs/heads/x.lock",
            "refs/tags/v1",
            "refs/heads/a//b",
        ] {
            assert!(!valid_branch_ref(bad), "{bad}");
        }
    }

    #[test]
    fn should_render_config_hooks_value_without_backslashes() {
        // A bare Windows path written into the generated config produced
        // "bad config line 6" on every git invocation (#2662): the
        // backslash starts an invalid config escape and the unquoted
        // value could not contain spaces either.
        let windowsish =
            std::path::Path::new("C:\\Users\\runneradmin\\app\\.octos-git-1\\no-hooks");
        let rendered = PrivateGitDir::config_hooks_value(windowsish);
        assert_eq!(rendered, "C:/Users/runneradmin/app/.octos-git-1/no-hooks");
        assert!(
            !rendered.contains('\\'),
            "backslashes must not survive: {rendered}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn should_refuse_private_git_when_project_git_is_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(real.join(".git/objects")).unwrap();
        std::fs::write(real.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::os::unix::fs::symlink(real.join(".git"), project.join(".git")).unwrap();
        assert!(PrivateGitDir::open(&project).is_err());
    }

    #[test]
    fn should_refuse_private_git_when_project_git_is_a_gitfile() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert!(PrivateGitDir::open(temp.path()).is_err());
    }
}
