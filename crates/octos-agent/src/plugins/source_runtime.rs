//! Runs a Cargo-based skill from its Rust source through the Relax JIT.
//! Experimental; see `docs/JIT_DEV_MODE.md`.
//!
//! Two kinds of skill use it:
//!
//! - A **dev skill**. `octos skills dev <path>` registers a skill crate that
//!   lives somewhere else. Its folder in the skills directory holds a marker
//!   ([`DEV_MARKER_FILE`]), a copy of the crate's `manifest.json` and
//!   `SKILL.md`, and a generated `main` that runs the source. The loader
//!   refreshes the copies from the source every time it loads the skill.
//!   The marker counts only together with a registration that `dev` writes
//!   outside the skill's folder, so a marker that arrives inside an installed
//!   skill's tree is ignored.
//! - An **installed source skill** with no built executable, when the
//!   operator sets [`SOURCE_RUNTIME_ENV`] to `relax`. Without that it keeps
//!   the lazy `cargo build --release` launcher.
//!
//! Either way the executable Octos spawns is a small generated `sh` script,
//! so the loader's rules for an executable (a regular file, hashed at load
//! and again before a spawn under strict signing) hold unchanged.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

/// Marker file of a dev skill, inside its folder in the skills directory.
pub const DEV_MARKER_FILE: &str = ".octos-dev.json";

/// Folder in a skills directory with one registration per dev skill: a file
/// named after the skill that holds the SHA-256 of its marker. Only
/// `octos skills dev` writes here. Nothing an install copies lands here,
/// because an install only ever writes inside the skill's own folder.
///
/// A registration belongs to the folder it was made for. Whatever puts an
/// installed tree in that place takes the registration away
/// ([`forget_dev_skill`]), or moves it with the old folder when that folder
/// is kept for a rollback ([`move_dev_registration`]).
const DEV_REGISTRY_DIR: &str = ".octos-dev";

/// The receipt a reviewed install writes into the tree it publishes. A
/// folder that holds one is that reviewed object and never a dev skill.
pub const REVIEWED_INSTALL_RECEIPT_FILE: &str = ".reviewed-install.json";

/// Folder in a skills directory with the pinned source of each dev skill
/// that was registered from a copy: `<skills_dir>/.octos-dev-src/<name>`.
const DEV_PINNED_DIR: &str = ".octos-dev-src";

/// Bounds of one pinned copy.
const MAX_PINNED_FILES: usize = 2_000;
const MAX_PINNED_BYTES: u64 = 32 * 1024 * 1024;

/// Operator opt-in, in the style of `OCTOS_SKILLS_CARGO_BUILD`: `relax` runs
/// an installed source skill that has no built executable through the JIT.
pub const SOURCE_RUNTIME_ENV: &str = "OCTOS_SKILLS_SOURCE_RUNTIME";

/// Least timeout of a skill that runs from source: its first call compiles.
pub const MIN_SOURCE_TIMEOUT_SECS: u64 = 180;

/// Second line of a generated Relax launcher.
const RELAX_LAUNCHER_MARK: &str =
    "# octos source skill: runs from Rust source through the Relax JIT";

/// `relax run` options of every launcher. One fixed set, because the JIT
/// cache differs between attended and unattended runs. `--unattended`: a
/// compile error ends the call instead of waiting for an edit. `--silent`:
/// stderr carries only the skill's own lines and real errors.
const RELAX_RUN_FLAGS: &[&str] = &["--allow-c", "--silent", "--unattended"];

/// Tool name of the warm-up call. No skill has it, so the skill has nothing
/// to act on, and the JIT has compiled what a real call starts from.
const WARM_TOOL: &str = "__octos_skill_warm__";

/// What a dev skill's marker records: everything the launcher is made from,
/// resolved once by `octos skills dev`, so that a server started with a bare
/// environment writes the same launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevSkill {
    /// The skill crate: `Cargo.toml`, `manifest.json`, `SKILL.md`.
    pub source: PathBuf,
    /// `[package] name` of that crate.
    pub package: String,
    /// Root of the cargo workspace the crate belongs to.
    pub workspace: PathBuf,
    /// The Relax binary.
    pub relax: PathBuf,
    /// Folder put in front of `PATH` so that Relax finds cargo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo_bin_dir: Option<PathBuf>,
    /// Environment written into the launcher. Octos gives a skill a reduced
    /// environment, so what the JIT or the skill's build needs goes here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Set when `source` is a pinned copy ([`pin_source`]): the folder it
    /// was copied from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_from: Option<PathBuf>,
}

impl DevSkill {
    /// Resolve a skill crate folder into a marker. `extra_env` entries are
    /// written into the launcher on top of the toolchain's own variables.
    pub fn resolve(source: &Path, extra_env: &BTreeMap<String, String>) -> Result<Self> {
        let (source, package) = Self::open(source)?;
        let relax = relax_binary().ok_or_else(|| {
            eyre::eyre!(
                "no Relax binary found. Run `octos skills jit setup`, \
                 or point OCTOS_RELAX at a built `relax`"
            )
        })?;
        let cargo = cargo_binary()
            .ok_or_else(|| eyre::eyre!("cargo is not on PATH; the Relax JIT needs it"))?;
        let workspace = cargo_workspace_root(&cargo, &source)?;
        let mut env = toolchain_env();
        for (name, value) in extra_env {
            validate_env_name(name)?;
            env.insert(name.clone(), value.clone());
        }
        Ok(Self {
            source,
            package,
            workspace,
            relax,
            cargo_bin_dir: cargo.parent().map(Path::to_path_buf),
            env,
            pinned_from: None,
        })
    }

    /// [`Self::resolve`] for a crate that is its own workspace, with a given
    /// Relax binary. It starts no process: `resolve` asks cargo for the
    /// workspace root, and a caller that must not run anything outside a
    /// sandbox for a crate the model wrote cannot do that.
    pub fn resolve_standalone(source: &Path, relax: &Path) -> Result<Self> {
        let (source, package) = Self::open(source)?;
        let manifest = std::fs::read_to_string(source.join("Cargo.toml")).unwrap_or_default();
        let own_workspace = toml::from_str::<toml::Value>(&manifest)
            .is_ok_and(|manifest| manifest.get("workspace").is_some_and(toml::Value::is_table));
        if !own_workspace {
            eyre::bail!(
                "{} is not its own workspace: add an empty `[workspace]` table to its Cargo.toml",
                source.display()
            );
        }
        let cargo = cargo_binary()
            .ok_or_else(|| eyre::eyre!("cargo is not on PATH; the Relax JIT needs it"))?;
        Ok(Self {
            workspace: source.clone(),
            source,
            package,
            relax: relax.to_path_buf(),
            cargo_bin_dir: cargo.parent().map(Path::to_path_buf),
            env: toolchain_env(),
            pinned_from: None,
        })
    }

    /// The canonical folder of a skill crate and its package name.
    fn open(source: &Path) -> Result<(PathBuf, String)> {
        if !cfg!(unix) {
            eyre::bail!("running a skill from source is not supported on this platform yet");
        }
        let source = source
            .canonicalize()
            .wrap_err_with(|| format!("no skill folder at {}", source.display()))?;
        for required in ["Cargo.toml", "manifest.json", "SKILL.md"] {
            if !source.join(required).is_file() {
                eyre::bail!("{} has no {required}", source.display());
            }
        }
        let package = cargo_package_name(&source).ok_or_else(|| {
            eyre::eyre!(
                "no [package] name in {}",
                source.join("Cargo.toml").display()
            )
        })?;
        Ok((source, package))
    }

    /// One `sh` line that runs `command`, a command of this crate from
    /// [`Self::run_command`] or [`Self::check_command`], for a caller that
    /// hands it to a sandbox. Every word is quoted. The environment of
    /// `command` is not part of the line: set it on the process that runs
    /// the line.
    ///
    /// Before the command, the line brings `Cargo.lock` up to date when the
    /// manifest is newer. A sandbox has no network unless its operator
    /// allows it, and Relax resolves a crate that has no lock file from the
    /// registry index. Cargo writes the lock file from its cache, or from
    /// the registry where the sandbox lets it. When cargo cannot, the line
    /// goes on and Relax names what is missing.
    pub fn sandbox_line(&self, command: &Command) -> String {
        let manifest = sh_quote_path(&self.source.join("Cargo.toml"));
        let lock = sh_quote_path(&self.workspace.join("Cargo.lock"));
        let metadata = format!("metadata --format-version 1 --manifest-path {manifest}");
        let mut line = format!(
            "if [ ! {lock} -nt {manifest} ]; then \
             {{ cargo {metadata} --offline || cargo --config net.retry=0 {metadata}; }} \
             >/dev/null 2>&1 && touch {lock}; fi; exec"
        );
        for word in std::iter::once(command.get_program()).chain(command.get_args()) {
            line.push(' ');
            line.push_str(&sh_quote(&word.to_string_lossy()));
        }
        line
    }

    fn launch(&self) -> RelaxLaunch<'_> {
        RelaxLaunch {
            relax: &self.relax,
            workspace: &self.workspace,
            package: &self.package,
            cargo_bin_dir: self.cargo_bin_dir.as_deref(),
            env: &self.env,
        }
    }

    /// A `relax <subcommand>` command for this crate, with the environment the
    /// launcher would set. The caller adds the options and the package.
    pub fn relax_command(&self, subcommand: &str) -> Command {
        let mut command = Command::new(&self.relax);
        command.arg(subcommand);
        command.env("RELAX_WORKSPACE", &self.workspace);
        if let Some(dir) = &self.cargo_bin_dir {
            let mut paths = vec![dir.clone()];
            if let Some(current) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&current));
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                command.env("PATH", joined);
            }
        }
        command.envs(&self.env);
        command
    }

    /// The command of one tool call: `relax run <options> <package> -- <tool>`.
    pub fn run_command(&self, tool: &str) -> Command {
        let mut command = self.relax_command("run");
        command
            .args(RELAX_RUN_FLAGS)
            .arg(&self.package)
            .arg("--")
            .arg(tool);
        command
    }

    /// The command of a type and borrow check. The skill is not run. Build
    /// scripts and procedural macros of the crate and its dependencies are.
    pub fn check_command(&self) -> Command {
        let mut command = self.relax_command("check");
        command.arg("--allow-c").arg(&self.package);
        command
    }
}

struct RelaxLaunch<'a> {
    relax: &'a Path,
    workspace: &'a Path,
    package: &'a str,
    cargo_bin_dir: Option<&'a Path>,
    env: &'a BTreeMap<String, String>,
}

/// The Relax binary: `OCTOS_RELAX`, else `target/release/relax` under
/// `OCTOS_RELAX_DIR`, else under `~/.octos-jit/relax`, which is where
/// `octos skills jit setup` builds it.
pub fn relax_binary() -> Option<PathBuf> {
    relax_binary_from(
        std::env::var_os("OCTOS_RELAX"),
        std::env::var_os("OCTOS_RELAX_DIR"),
        dirs::home_dir(),
    )
}

fn relax_binary_from(
    explicit: Option<OsString>,
    checkout: Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let candidate = match (explicit, checkout) {
        (Some(path), _) if !path.is_empty() => PathBuf::from(path),
        (_, Some(dir)) if !dir.is_empty() => PathBuf::from(dir).join("target/release/relax"),
        _ => home?.join(".octos-jit/relax/target/release/relax"),
    };
    candidate.is_file().then_some(candidate)
}

/// The cargo the JIT would use: the one on `PATH`.
pub fn cargo_binary() -> Option<PathBuf> {
    which::which("cargo").ok()
}

/// What a sandbox must let a process read so that `relax` at this path can
/// compile a skill inside it: the Relax checkout (the binary reads its own
/// standard library sources), cargo's registry and configuration, and, for
/// cargo itself, its binaries and the rustup home. Only what exists.
pub fn sandbox_read_paths(relax: &Path) -> Vec<PathBuf> {
    // `<checkout>/target/release/relax`, else a binary on its own.
    let checkout = relax
        .ancestors()
        .nth(3)
        .filter(|_| relax.ends_with("target/release/relax"))
        .or_else(|| relax.parent())
        .map(Path::to_path_buf);
    let home = dirs::home_dir();
    let home_of = |variable: &str, default: &str| {
        std::env::var_os(variable)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| Some(home.as_ref()?.join(default)))
    };
    let cargo = home_of("CARGO_HOME", ".cargo");
    let in_cargo = |name: &str| cargo.as_ref().map(|cargo| cargo.join(name));
    [
        checkout,
        in_cargo("registry"),
        in_cargo("config.toml"),
        in_cargo("bin"),
        home_of("RUSTUP_HOME", ".rustup"),
    ]
    .into_iter()
    .flatten()
    .filter(|path| path.exists())
    .collect()
}

/// True when the operator asked for installed source skills to run through
/// the JIT ([`SOURCE_RUNTIME_ENV`] is `relax`).
pub fn installed_source_skills_use_relax() -> bool {
    std::env::var(SOURCE_RUNTIME_ENV).as_deref() == Ok("relax")
}

/// `[package] name` of the crate in `crate_dir`.
pub fn cargo_package_name(crate_dir: &Path) -> Option<String> {
    let cargo_toml = std::fs::read_to_string(crate_dir.join("Cargo.toml")).ok()?;
    let parsed: toml::Value = toml::from_str(&cargo_toml).ok()?;
    parsed
        .get("package")?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

fn cargo_workspace_root(cargo: &Path, crate_dir: &Path) -> Result<PathBuf> {
    let output = Command::new(cargo)
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .arg("--manifest-path")
        .arg(crate_dir.join("Cargo.toml"))
        .stdin(Stdio::null())
        .output()
        .wrap_err("failed to run cargo locate-project")?;
    if !output.status.success() {
        eyre::bail!(
            "cargo could not find the workspace of {}: {}",
            crate_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let manifest = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| eyre::eyre!("cargo gave no workspace for {}", crate_dir.display()))
}

/// The toolchain variables a launcher carries when they are set here.
fn toolchain_env() -> BTreeMap<String, String> {
    ["CARGO_HOME", "RUSTUP_HOME", "RELAX_HOME"]
        .into_iter()
        .filter_map(|name| Some((name.to_string(), std::env::var(name).ok()?)))
        .collect()
}

/// Reject a name that is not a plain shell variable name: it is written into
/// a script.
pub fn validate_env_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let plain = matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !plain {
        eyre::bail!("bad environment variable name '{name}'");
    }
    Ok(())
}

/// Single-quote a value for `sh`.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn sh_quote_path(path: &Path) -> String {
    sh_quote(&path.to_string_lossy())
}

fn relax_launcher_script(launch: &RelaxLaunch<'_>) -> String {
    let mut script = String::from("#!/bin/sh\n");
    script.push_str(RELAX_LAUNCHER_MARK);
    script.push('\n');
    script
        .push_str("# Generated by Octos. Do not edit: it is written again when the skill loads.\n");
    if let Some(dir) = launch.cargo_bin_dir {
        script.push_str(&format!(
            "PATH={}:\"$PATH\"; export PATH\n",
            sh_quote_path(dir)
        ));
    }
    script.push_str(&format!(
        "RELAX_WORKSPACE={}; export RELAX_WORKSPACE\n",
        sh_quote_path(launch.workspace)
    ));
    for (name, value) in launch.env {
        // Names were checked when the marker was made; a hand-edited marker
        // with a bad name loses that variable instead of breaking the script.
        if validate_env_name(name).is_ok() {
            script.push_str(&format!("{name}={}; export {name}\n", sh_quote(value)));
        }
    }
    script.push_str(&format!(
        "exec {} run {} {} -- \"$@\"\n",
        sh_quote_path(launch.relax),
        RELAX_RUN_FLAGS.join(" "),
        sh_quote(launch.package)
    ));
    script
}

/// True for a `main` that this module generated.
pub fn is_generated_relax_launcher(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .is_ok_and(|content| content.lines().nth(1) == Some(RELAX_LAUNCHER_MARK))
}

/// The launcher of an installed source skill: the crate in `plugin_dir`, run
/// with the toolchain this process sees. `None` when there is no Relax, no
/// cargo, or no package to run.
pub fn installed_relax_launcher(plugin_dir: &Path) -> Option<String> {
    if !cfg!(unix) {
        return None;
    }
    let package = cargo_package_name(plugin_dir)?;
    let relax = relax_binary()?;
    let cargo_bin_dir = cargo_binary()
        .and_then(|cargo| cargo.parent().map(Path::to_path_buf))
        .or_else(|| {
            let fallback = dirs::home_dir()?.join(".cargo/bin");
            fallback.join("cargo").is_file().then_some(fallback)
        })?;
    let env = toolchain_env();
    Some(relax_launcher_script(&RelaxLaunch {
        relax: &relax,
        workspace: plugin_dir,
        package: &package,
        cargo_bin_dir: Some(&cargo_bin_dir),
        env: &env,
    }))
}

fn registration_path(plugin_dir: &Path) -> Option<PathBuf> {
    Some(
        plugin_dir
            .parent()?
            .join(DEV_REGISTRY_DIR)
            .join(plugin_dir.file_name()?),
    )
}

fn marker_digest(marker: &[u8]) -> String {
    format!("{:x}", Sha256::digest(marker))
}

/// The marker of a dev skill folder, or `None` for any other skill folder.
///
/// A marker without the registration that `octos skills dev` wrote for
/// exactly these bytes is not a dev skill: it came with an installed tree, or
/// outlived a registration that was replaced. So is a marker in a folder
/// with a reviewed-install receipt. It is ignored, so the folder loads as
/// the installed skill it is.
pub fn read_dev_marker(plugin_dir: &Path) -> Result<Option<DevSkill>> {
    let path = plugin_dir.join(DEV_MARKER_FILE);
    let marker = match std::fs::read(&path) {
        Ok(marker) => marker,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).wrap_err_with(|| format!("cannot read {}", path.display()));
        }
    };
    // A reviewed install stays the object its receipt names, whatever
    // record is left beside it.
    let registered = !plugin_dir.join(REVIEWED_INSTALL_RECEIPT_FILE).exists()
        && registration_path(plugin_dir)
            .and_then(|registration| std::fs::read_to_string(registration).ok())
            .is_some_and(|digest| digest.trim() == marker_digest(&marker));
    if !registered {
        warn!(
            marker = %path.display(),
            "ignoring a dev skill marker that `octos skills dev` did not register"
        );
        return Ok(None);
    }
    serde_json::from_slice(&marker)
        .map(Some)
        .wrap_err_with(|| format!("invalid dev skill marker {}", path.display()))
}

/// True when `plugin_dir` is a registered dev skill folder.
pub fn is_dev_skill(plugin_dir: &Path) -> bool {
    matches!(read_dev_marker(plugin_dir), Ok(Some(_)))
}

/// Drop the registration of a skill folder, and its pinned source if it has
/// one. Call it when the folder is removed and when an installed tree is put
/// in its place: the registration was made for the dev skill that stood
/// there, not for what replaces it.
///
/// A registration that is not there is fine. One that cannot be removed is
/// an error, and the caller must not go on to put a tree in that place: the
/// record would speak for it.
pub fn forget_dev_skill(plugin_dir: &Path) -> Result<()> {
    if let Some(registration) = registration_path(plugin_dir) {
        gone(std::fs::remove_file(&registration)).wrap_err_with(|| {
            format!(
                "cannot remove the dev registration {}",
                registration.display()
            )
        })?;
    }
    if let Some(pinned) = pinned_path(plugin_dir) {
        gone(std::fs::remove_dir_all(&pinned))
            .wrap_err_with(|| format!("cannot remove the pinned source {}", pinned.display()))?;
    }
    Ok(())
}

/// `Ok` also when what was to be removed or moved is not there.
fn gone(done: std::io::Result<()>) -> std::io::Result<()> {
    match done {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Clear the place an installed tree is about to take: the dev registration
/// of that name first, then what stands there. The tree that lands is an
/// installed skill, whatever marker it carries. When the registration cannot
/// be removed nothing is touched and the install must stop.
pub fn clear_for_installed_tree(dest: &Path) -> Result<()> {
    forget_dev_skill(dest)?;
    gone(std::fs::remove_dir_all(dest))
        .wrap_err_with(|| format!("cannot replace {}", dest.display()))
}

fn pinned_path(plugin_dir: &Path) -> Option<PathBuf> {
    Some(
        plugin_dir
            .parent()?
            .join(DEV_PINNED_DIR)
            .join(plugin_dir.file_name()?),
    )
}

/// Copy the skill crate at `source` into the skills directory and return the
/// copy, `<skills_dir>/.octos-dev-src/<name>`.
///
/// A dev skill registered on the copy runs what was copied. Whoever can
/// write to `source` afterwards cannot change what runs: a later edit
/// reaches the skill only through a new copy. Build output (`target`) and
/// `.git` are left out. A crate that is not self-contained is refused,
/// because the copy would still follow a place that changes: a crate with a
/// symbolic link, with a `.cargo` folder, or with a manifest that names a
/// path outside the crate or a local git repository
/// ([`refuse_outside_references`]). A failed copy leaves an earlier one in
/// place.
///
/// Not seen: source that pulls in an outside file by an absolute path
/// (`include!`, `#[path]`), and what the skill reads when it runs.
pub fn pin_source(skills_dir: &Path, source: &Path) -> Result<PathBuf> {
    let source = source
        .canonicalize()
        .wrap_err_with(|| format!("no skill folder at {}", source.display()))?;
    let name = source
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.starts_with('.'))
        .ok_or_else(|| eyre::eyre!("cannot take a skill name from {}", source.display()))?;
    let root = skills_dir.join(DEV_PINNED_DIR);
    std::fs::create_dir_all(&root).wrap_err_with(|| format!("cannot create {}", root.display()))?;
    let stage = tempfile::Builder::new()
        .prefix(&format!(".{name}-"))
        .tempdir_in(&root)?;
    let (mut files, mut bytes) = (0usize, 0u64);
    copy_for_pin(&source, &source, stage.path(), &mut files, &mut bytes)?;
    let dest = root.join(name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .wrap_err_with(|| format!("cannot replace {}", dest.display()))?;
    }
    std::fs::rename(stage.path(), &dest)
        .wrap_err_with(|| format!("cannot write {}", dest.display()))?;
    // The staging folder is now the copy; there is nothing left to clean up.
    let _ = stage.keep();
    Ok(dest)
}

/// Copy the folder `from` of the crate at `root` to `to`.
fn copy_for_pin(
    root: &Path,
    from: &Path,
    to: &Path,
    files: &mut usize,
    bytes: &mut u64,
) -> Result<()> {
    for entry in
        std::fs::read_dir(from).wrap_err_with(|| format!("cannot read {}", from.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        if from == root && (name == "target" || name == ".git") {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(&name));
        // `file_type` does not follow a link.
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if name == ".cargo" {
                eyre::bail!(
                    "{} is a cargo configuration folder; a pinned skill may not carry one, \
                     because it can point the build at places outside the crate",
                    src.display()
                );
            }
            std::fs::create_dir(&dst)?;
            copy_for_pin(root, &src, &dst, files, bytes)?;
        } else if kind.is_file() {
            if PINNED_MANIFESTS.iter().any(|manifest| name == *manifest) {
                refuse_outside_references(&src, root)?;
            }
            *files += 1;
            *bytes += entry.metadata()?.len();
            if *files > MAX_PINNED_FILES || *bytes > MAX_PINNED_BYTES {
                eyre::bail!(
                    "the skill crate is too large to pin: more than {MAX_PINNED_FILES} files or {} MiB",
                    MAX_PINNED_BYTES / (1024 * 1024)
                );
            }
            std::fs::copy(&src, &dst).wrap_err_with(|| format!("cannot copy {}", src.display()))?;
        } else {
            eyre::bail!(
                "{} is a link or a special file; a pinned skill is made of regular files only",
                src.display()
            );
        }
    }
    Ok(())
}

/// The files of a crate whose values can send the build somewhere else.
const PINNED_MANIFESTS: [&str; 3] = ["Cargo.toml", "rust-toolchain.toml", "rust-toolchain"];

/// Refuse a manifest of the crate at `root` that names a place outside the
/// crate: every `path` and `build` value (a path dependency, a patch, a
/// target, a build script, a custom toolchain) that is absolute or steps
/// out of the crate, and every `git` value that is a local repository.
fn refuse_outside_references(manifest: &Path, root: &Path) -> Result<()> {
    let text = std::fs::read_to_string(manifest)
        .wrap_err_with(|| format!("cannot read {}", manifest.display()))?;
    let parsed: toml::Value = match toml::from_str(&text) {
        Ok(parsed) => parsed,
        // The old form of `rust-toolchain`: one line with a toolchain name.
        Err(_) if manifest.ends_with("rust-toolchain") && !text.contains(['/', '\\']) => {
            return Ok(());
        }
        Err(error) => eyre::bail!("cannot read {} as TOML: {error}", manifest.display()),
    };
    let base = manifest.parent().unwrap_or(root);
    match outside_reference(&parsed, base, root) {
        Some(found) => eyre::bail!(
            "{} names a place outside the crate ({found}); a pinned skill must be \
             self-contained: keep path dependencies inside the crate folder and \
             use no local git repository",
            manifest.display()
        ),
        None => Ok(()),
    }
}

fn outside_reference(value: &toml::Value, base: &Path, root: &Path) -> Option<String> {
    match value {
        toml::Value::Table(table) => table.iter().find_map(|(key, value)| match value {
            toml::Value::String(text) => {
                let outside = match key.as_str() {
                    "path" | "build" => leaves_crate(base, text, root),
                    "git" => {
                        let url = text.trim().to_ascii_lowercase();
                        url.starts_with("file:") || !url.contains("://")
                    }
                    _ => false,
                };
                outside.then(|| format!("{key} = {text:?}"))
            }
            nested => outside_reference(nested, base, root),
        }),
        toml::Value::Array(items) => items
            .iter()
            .find_map(|item| outside_reference(item, base, root)),
        _ => None,
    }
}

/// True when `path`, written in a manifest in the folder `base`, is absolute
/// or steps out of `root`. No file is looked at: the crate holds no links.
fn leaves_crate(base: &Path, path: &str, root: &Path) -> bool {
    let mut at = base.to_path_buf();
    for part in Path::new(path).components() {
        match part {
            std::path::Component::Normal(name) => at.push(name),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                at.pop();
            }
            _ => return true,
        }
    }
    !at.starts_with(root)
}

/// Move the registration of the skill folder `from` to the name of `to`,
/// after the folder itself was renamed that way inside its skills directory.
///
/// An install that keeps the old folder for a rollback calls this for both
/// renames. The registration then stays with the dev skill it was made for:
/// it is not there for the tree that takes the old name, and it is back when
/// a rollback restores the folder. `to` never keeps a registration of its
/// own. A pinned source moves the same way.
///
/// On an error the registration is still whole under the name of `from`,
/// and the caller has to undo its rename of the folder.
pub fn move_dev_registration(from: &Path, to: &Path) -> Result<()> {
    forget_dev_skill(to)?;
    let mut moved = Vec::new();
    for path_of in [registration_path, pinned_path] {
        let (Some(old), Some(new)) = (path_of(from), path_of(to)) else {
            continue;
        };
        if let Err(error) = gone(std::fs::rename(&old, &new)) {
            for (old, new) in moved {
                let _ = std::fs::rename(new, old);
            }
            return Err(error)
                .wrap_err_with(|| format!("cannot move the dev registration {}", old.display()));
        }
        moved.push((old, new));
    }
    Ok(())
}

/// Bring a dev skill folder up to date with its source: the manifest, the
/// `SKILL.md`, and the launcher. Called by `octos skills dev` and by the
/// loader before it reads the manifest, so a manifest edit is picked up by
/// the next load without a second `dev`.
pub fn sync_dev_skill(plugin_dir: &Path, dev: &DevSkill) -> Result<()> {
    let name = plugin_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let source_manifest = dev.source.join("manifest.json");
    let content = std::fs::read_to_string(&source_manifest).wrap_err_with(|| {
        format!(
            "dev skill '{name}': its source {} is gone or has no manifest.json. \
             Run `octos skills dev <path>` again, or `octos skills remove {name}`",
            dev.source.display()
        )
    })?;
    let mut manifest: serde_json::Value = serde_json::from_str(&content)
        .wrap_err_with(|| format!("invalid {}", source_manifest.display()))?;
    // A hash or signature in the manifest pins the built executable. The
    // launcher is not that file, so the copy leaves both out.
    if let Some(object) = manifest.as_object_mut() {
        object.remove("sha256");
        object.remove("signature");
    }
    let mut manifest_copy = serde_json::to_string_pretty(&manifest)?;
    manifest_copy.push('\n');
    write_if_changed(
        &plugin_dir.join("manifest.json"),
        manifest_copy.as_bytes(),
        false,
    )?;
    if let Ok(skill_md) = std::fs::read(dev.source.join("SKILL.md")) {
        write_if_changed(&plugin_dir.join("SKILL.md"), &skill_md, false)?;
    }
    write_if_changed(
        &plugin_dir.join("main"),
        relax_launcher_script(&dev.launch()).as_bytes(),
        true,
    )
}

/// Replace a file through a rename, and only when its content differs: a
/// tool call that starts at this moment sees the old file or the new one.
fn write_if_changed(path: &Path, content: &[u8], executable: bool) -> Result<()> {
    if std::fs::read(path).is_ok_and(|current| current == content) {
        return Ok(());
    }
    // A dot name: the loader's scan for an executable skips it.
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let staged = path.with_file_name(format!(".{file_name}.octos-new"));
    std::fs::write(&staged, content)
        .wrap_err_with(|| format!("cannot write {}", staged.display()))?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    std::fs::rename(&staged, path).wrap_err_with(|| format!("cannot write {}", path.display()))
}

/// Register `dev` in `skills_dir` under the name of its source folder, the
/// name `octos skills install <path>` would give it. Returns the skill folder.
///
/// A folder of that name that is not a dev skill is an installed skill: it is
/// replaced only with `force`.
pub fn register_dev_skill(skills_dir: &Path, dev: &DevSkill, force: bool) -> Result<PathBuf> {
    let name = dev
        .source
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.starts_with('.'))
        .ok_or_else(|| eyre::eyre!("cannot take a skill name from {}", dev.source.display()))?;
    std::fs::create_dir_all(skills_dir)
        .wrap_err_with(|| format!("cannot create {}", skills_dir.display()))?;
    let dest = skills_dir.join(name);
    if dest.exists() && !is_dev_skill(&dest) {
        if !force {
            eyre::bail!(
                "skill '{name}' is already installed at {}. Remove it first \
                 (`octos skills remove {name}`) or pass --force",
                dest.display()
            );
        }
        std::fs::remove_dir_all(&dest)
            .wrap_err_with(|| format!("cannot replace {}", dest.display()))?;
    }
    std::fs::create_dir_all(&dest)?;
    let mut marker = serde_json::to_string_pretty(dev)?;
    marker.push('\n');
    write_if_changed(&dest.join(DEV_MARKER_FILE), marker.as_bytes(), false)?;
    let registration = skills_dir.join(DEV_REGISTRY_DIR).join(name);
    std::fs::create_dir_all(skills_dir.join(DEV_REGISTRY_DIR))?;
    std::fs::write(&registration, marker_digest(marker.as_bytes()))
        .wrap_err_with(|| format!("cannot write {}", registration.display()))?;
    sync_dev_skill(&dest, dev)?;
    if dev.pinned_from.is_none() {
        // A registration on a live source replaces a pinned one.
        if let Some(pinned) = pinned_path(&dest) {
            let _ = std::fs::remove_dir_all(pinned);
        }
    }
    Ok(dest)
}

/// Run the launcher of a dev skill folder once, so that the JIT compiles and
/// caches the skill before its first real call. `Err` carries what Relax
/// reported when the skill does not start: a compile error, a failed build
/// script, a native dependency that did not build.
pub fn warm_dev_skill(plugin_dir: &Path) -> Result<(), String> {
    let output = Command::new(plugin_dir.join("main"))
        .arg(WARM_TOOL)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("cannot start the launcher: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    // What the skill itself prints for the unknown tool is not an error
    // here. A line of Relax's own is.
    let relax_failed = stderr.contains("could not compile")
        || stderr.lines().any(|line| line.starts_with("relax: "));
    if relax_failed {
        return Err(stderr.trim().to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev_skill(root: &Path) -> DevSkill {
        let source = root.join("src-tree/it's a skill");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("manifest.json"),
            r#"{"name":"demo","version":"1.0.0","sha256":"ab","signature":"cd","tools":[]}"#,
        )
        .unwrap();
        std::fs::write(source.join("SKILL.md"), "---\nname: demo\n---\n").unwrap();
        DevSkill {
            source,
            package: "demo".into(),
            workspace: root.join("src-tree/it's a skill"),
            relax: root.join("relax"),
            cargo_bin_dir: Some(root.join("cargo bin")),
            env: BTreeMap::from([("FONTS".to_string(), "it's $HOME".to_string())]),
            pinned_from: None,
        }
    }

    /// The launcher is a shell script built from paths and values the user
    /// chose. Run it with a stand-in for `relax` and check that every one of
    /// them arrives whole, quotes and spaces included.
    #[cfg(unix)]
    #[test]
    fn dev_skill_launcher_passes_paths_env_and_arguments_through_the_shell_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dev = dev_skill(root.path());
        std::fs::write(
            &dev.relax,
            "#!/bin/sh\nprintf '%s\\n' \"$RELAX_WORKSPACE\" \"$FONTS\" \"${PATH%%:*}\" \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&dev.relax, std::fs::Permissions::from_mode(0o755)).unwrap();
        let skills = root.path().join("skills");

        let dest = register_dev_skill(&skills, &dev, false).unwrap();

        assert_eq!(dest, skills.join("it's a skill"));
        assert!(is_generated_relax_launcher(&dest.join("main")));
        let output = Command::new(dest.join("main"))
            .args(["get_time", "a b"])
            .output()
            .unwrap();
        let lines: Vec<String> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            lines,
            [
                dev.workspace.to_string_lossy().as_ref(),
                "it's $HOME",
                root.path().join("cargo bin").to_string_lossy().as_ref(),
                "run",
                "--allow-c",
                "--silent",
                "--unattended",
                "demo",
                "--",
                "get_time",
                "a b",
            ]
        );
        // The copy drops the pin of the built executable, and a later load
        // picks up a manifest edit.
        let copy = std::fs::read_to_string(dest.join("manifest.json")).unwrap();
        assert!(!copy.contains("sha256") && !copy.contains("signature"));
        std::fs::write(
            dev.source.join("manifest.json"),
            r#"{"name":"demo","version":"2.0.0","tools":[]}"#,
        )
        .unwrap();
        sync_dev_skill(&dest, &read_dev_marker(&dest).unwrap().unwrap()).unwrap();
        assert!(
            std::fs::read_to_string(dest.join("manifest.json"))
                .unwrap()
                .contains("2.0.0")
        );
    }

    /// `dev` must never destroy an installed skill by accident, a marker
    /// that came inside an installed tree must not turn it into a dev skill,
    /// and a dev skill whose source has gone must say so instead of loading
    /// stale files.
    #[test]
    fn only_a_registered_marker_makes_a_dev_skill_and_an_installed_skill_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let dev = dev_skill(root.path());
        let skills = root.path().join("skills");
        let installed = skills.join("it's a skill");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("main"), "real binary").unwrap();
        std::fs::write(
            installed.join(DEV_MARKER_FILE),
            serde_json::to_string(&dev).unwrap(),
        )
        .unwrap();

        assert_eq!(read_dev_marker(&installed).unwrap(), None);
        let refused = register_dev_skill(&skills, &dev, false).unwrap_err();
        assert!(refused.to_string().contains("already installed"));
        assert_eq!(
            std::fs::read_to_string(installed.join("main")).unwrap(),
            "real binary"
        );

        let dest = register_dev_skill(&skills, &dev, true).unwrap();
        assert!(is_dev_skill(&dest));
        std::fs::remove_dir_all(&dev.source).unwrap();
        let gone = sync_dev_skill(&dest, &dev).unwrap_err();
        assert!(gone.to_string().contains("octos skills dev"));
    }
    /// What `skill_dev register` approves is what runs: the copy holds the
    /// crate as it was, without build output, and nothing that can lead back
    /// to files that change.
    #[cfg(unix)]
    #[test]
    fn a_pinned_copy_is_the_crate_as_it_was_and_refuses_links() {
        let root = tempfile::tempdir().unwrap();
        let (skills, source) = (root.path().join("skills"), root.path().join("ws/demo"));
        for dir in ["src/nested", "target/relax", ".git"] {
            std::fs::create_dir_all(source.join(dir)).unwrap();
        }
        for file in [
            "Cargo.toml",
            "src/main.rs",
            "src/nested/a.rs",
            "target/relax/x",
            ".git/HEAD",
        ] {
            std::fs::write(source.join(file), "v1").unwrap();
        }
        let manifest = "[package]\nname = \"demo\"\n[lib]\npath = \"src/nested/a.rs\"\n";
        std::fs::write(source.join("Cargo.toml"), manifest).unwrap();
        let pinned = pin_source(&skills, &source).unwrap();
        assert_eq!(pinned, skills.join(DEV_PINNED_DIR).join("demo"));
        let read = |file: &str| std::fs::read_to_string(pinned.join(file)).ok();
        assert_eq!(read("src/nested/a.rs").as_deref(), Some("v1"));
        assert!(!pinned.join("target").exists() && !pinned.join(".git").exists());

        // An edit of the source does not reach the copy. A link is refused
        // and leaves the copy as it was. A new pin takes the new state.
        std::fs::write(source.join("src/main.rs"), "v2").unwrap();
        std::fs::remove_file(source.join("src/nested/a.rs")).unwrap();
        assert_eq!(read("src/main.rs").as_deref(), Some("v1"));
        std::os::unix::fs::symlink(root.path(), source.join("src/out")).unwrap();
        assert!(pin_source(&skills, &source).is_err());
        assert_eq!(read("src/main.rs").as_deref(), Some("v1"));
        std::fs::remove_file(source.join("src/out")).unwrap();
        // So is a crate that points outside itself: the copy would still
        // build from that place.
        std::fs::create_dir(source.join("src/.cargo")).unwrap();
        assert!(pin_source(&skills, &source).is_err());
        std::fs::remove_dir(source.join("src/.cargo")).unwrap();
        for outside in [
            "[dependencies]\nh = { path = \"../h\" }",
            "[package]\nbuild = \"/b.rs\"",
            "[patch.crates-io]\nh = { git = \"file:///h\" }",
            "not a manifest",
        ] {
            std::fs::write(source.join("Cargo.toml"), outside).unwrap();
            assert!(pin_source(&skills, &source).is_err(), "{outside}");
        }
        std::fs::write(source.join("Cargo.toml"), manifest).unwrap();
        pin_source(&skills, &source).unwrap();
        assert_eq!(read("src/main.rs").as_deref(), Some("v2"));
        assert_eq!(read("src/nested/a.rs"), None);

        // The copy stays with its skill when an install moves the folder
        // aside and back. Removing the skill removes the copy.
        let (skill, aside) = (skills.join("demo"), skills.join(".kept"));
        move_dev_registration(&skill, &aside).unwrap();
        assert!(!pinned.exists());
        move_dev_registration(&aside, &skill).unwrap();
        assert_eq!(read("src/main.rs").as_deref(), Some("v2"));
        forget_dev_skill(&skill).unwrap();
        assert!(!pinned.exists());
    }
}
