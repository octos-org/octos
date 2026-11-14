//! `octos skills new | dev | run | check | jit`: develop a skill from its
//! Rust source through the Relax JIT. Experimental; see
//! `docs/JIT_DEV_MODE.md`.

use std::collections::BTreeMap;
use std::path::Path;

use colored::Colorize;
use eyre::{Result, WrapErr};
use octos_agent::plugins::source_runtime::{self, DevSkill};

use super::skills::{looks_like_local_path, validate_skill_manifest};

/// The files of a starter skill crate. `{{name}}` is the skill's name and
/// `{{tool}}` the name of its one tool.
const SKILL_TEMPLATE: &[(&str, &str)] = &[
    ("Cargo.toml", include_str!("skill_template/Cargo.toml.tmpl")),
    ("src/main.rs", include_str!("skill_template/main.rs.tmpl")),
    (
        "manifest.json",
        include_str!("skill_template/manifest.json.tmpl"),
    ),
    ("SKILL.md", include_str!("skill_template/SKILL.md.tmpl")),
    (".gitignore", "/target\n"),
];

/// `scripts/jit/setup-relax.sh` and the patches it applies, carried in the
/// binary so that an installed Octos can build its Relax without a checkout.
const RELAX_SETUP_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/jit/setup-relax.sh"
));

macro_rules! relax_patches {
    ($($name:literal),* $(,)?) => {
        &[$((
            $name,
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../scripts/jit/relax-patches/",
                $name
            )),
        )),*]
    };
}

/// Every `*.patch` in `scripts/jit/relax-patches`, in order. A test holds
/// this list to the folder.
const RELAX_PATCHES: &[(&str, &str)] = relax_patches![
    "0001-relax-a-program-s-own-sources-of-a-built-in-macro-pa.patch",
    "0002-relax-a-fn-body-that-defines-a-macro_rules-is-parsed.patch",
    "0003-relax-two-tokens-of-one-expansion-keep-their-order-t.patch",
    "0004-relax-arm64-an-asm-block-gives-back-as-many-values-a.patch",
    "0005-relax-std-the-Command-PathBuf-OsString-and-Rc-method.patch",
    "0006-relax-a-by-value-use-of-a-temporary-Box-s-contents-d.patch",
    "0007-relax-live-an-edit-saved-right-after-inspect-pin-on-.patch",
    "0008-relax-std-LazyCell-and-LazyLock-give-mut-T-DerefMut-.patch",
];

/// Write a starter skill crate to `<parent>/<name>`. Returns its folder and
/// the name of its one tool.
pub(crate) fn scaffold_skill(name: &str, parent: &Path) -> Result<(std::path::PathBuf, String)> {
    let plain = name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !plain {
        eyre::bail!(
            "bad skill name '{name}': use lower-case letters, digits and hyphens, starting with a letter"
        );
    }
    let dir = parent.join(name);
    if dir.exists() {
        eyre::bail!("{} already exists", dir.display());
    }
    let tool = name.replace('-', "_");
    for (file, template) in SKILL_TEMPLATE {
        let path = dir.join(file);
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)
                .wrap_err_with(|| format!("cannot create {}", folder.display()))?;
        }
        let content = template
            .replace("{{name}}", name)
            .replace("{{tool}}", &tool);
        std::fs::write(&path, content)
            .wrap_err_with(|| format!("cannot write {}", path.display()))?;
    }
    validate_skill_manifest(&dir)?;
    Ok((dir, tool))
}

pub(super) fn cmd_new(name: &str, parent: &Path) -> Result<()> {
    let (dir, tool) = scaffold_skill(name, parent)?;
    println!("  {} Created skill crate {}", "OK".green(), dir.display());
    println!("  Try it from source (needs Relax: `octos skills jit setup`):");
    println!(
        "    octos skills run {} {tool} '{{\"text\":\"hello\"}}'",
        dir.display()
    );
    println!("    octos skills --profile <id> dev {}", dir.display());
    println!("  Or build and install it the usual way:");
    println!("    octos skills --profile <id> install {}", dir.display());
    Ok(())
}

/// Build the patched Relax under `OCTOS_RELAX_DIR` (default
/// `~/.octos-jit/relax`) with the setup script and patches this binary
/// carries.
pub(super) fn cmd_jit_setup() -> Result<()> {
    if !cfg!(unix) {
        eyre::bail!("the Relax JIT setup is not supported on this platform yet");
    }
    let staging = tempfile::tempdir().wrap_err("cannot create a temporary folder")?;
    let script = staging.path().join("setup-relax.sh");
    std::fs::write(&script, RELAX_SETUP_SCRIPT)?;
    let patches = staging.path().join("relax-patches");
    std::fs::create_dir(&patches)?;
    for (name, patch) in RELAX_PATCHES {
        std::fs::write(patches.join(name), patch)?;
    }
    let status = std::process::Command::new("sh")
        .arg(&script)
        .status()
        .wrap_err("failed to start sh")?;
    exit_like(status)
}

/// Say what the source runtime would use on this machine.
pub(super) fn cmd_jit_status() -> Result<()> {
    println!("{}", "Relax JIT for skills (experimental)".cyan().bold());
    let relax = source_runtime::relax_binary();
    match &relax {
        Some(relax) => println!("  {}   {}", "Relax:".dimmed(), relax.display()),
        None => println!(
            "  {}   not found. Run `octos skills jit setup`, or point OCTOS_RELAX at a built `relax`",
            "Relax:".dimmed()
        ),
    }
    match source_runtime::cargo_binary() {
        Some(cargo) => println!("  {}   {}", "cargo:".dimmed(), cargo.display()),
        None => println!("  {}   not on PATH; the JIT needs it", "cargo:".dimmed()),
    }
    let runtime = if source_runtime::installed_source_skills_use_relax() {
        "relax"
    } else {
        "cargo (set OCTOS_SKILLS_SOURCE_RUNTIME=relax to change)"
    };
    println!(
        "  {} {runtime}",
        "Installed source skills run through:".dimmed()
    );
    if let Some(relax) = relax {
        // The agent's `skill_dev` runs the JIT inside the session sandbox.
        println!(
            "  {}",
            "A session sandbox must be able to read (profile config, sandbox.read_allow_paths):"
                .dimmed()
        );
        for path in source_runtime::sandbox_read_paths(&relax) {
            println!("    {}", path.display());
        }
    }
    Ok(())
}

/// Parse `--env NAME=VALUE` and `--env NAME` (the current value) arguments.
fn parse_dev_env(args: &[String]) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for arg in args {
        let (name, value) = match arg.split_once('=') {
            Some((name, value)) => (name, value.to_string()),
            None => (
                arg.as_str(),
                std::env::var(arg)
                    .map_err(|_| eyre::eyre!("--env {arg}: the variable is not set"))?,
            ),
        };
        source_runtime::validate_env_name(name)?;
        env.insert(name.to_string(), value);
    }
    Ok(env)
}

/// A skill to run or check: a skill crate folder, or the name of a skill
/// registered with `dev`, which also brings the environment it was given.
fn resolve_source_skill(skills_dir: &Path, skill: &str) -> Result<DevSkill> {
    let path = Path::new(skill);
    if looks_like_local_path(skill) || path.join("Cargo.toml").is_file() {
        return DevSkill::resolve(path, &BTreeMap::new());
    }
    let named = !skill.is_empty() && !skill.contains(['/', '\\', '\0']) && !skill.starts_with('.');
    if named && let Some(dev) = source_runtime::read_dev_marker(&skills_dir.join(skill))? {
        return Ok(dev);
    }
    eyre::bail!(
        "'{skill}' is neither a skill crate folder nor a skill registered with `octos skills dev`"
    )
}

/// Register the skill crate at `path` as a dev skill of `skills_dir`.
/// Returns the resolved crate and the skill's folder.
pub(crate) fn register_source_skill(
    skills_dir: &Path,
    path: &Path,
    env: &BTreeMap<String, String>,
    force: bool,
) -> Result<(DevSkill, std::path::PathBuf)> {
    let dev = DevSkill::resolve(path, env)?;
    validate_skill_manifest(&dev.source)?;
    let dest = source_runtime::register_dev_skill(skills_dir, &dev, force)?;
    Ok((dev, dest))
}

/// Register a pinned copy of the skill crate at `path` as a dev skill of
/// `skills_dir`: what runs afterwards is the copy, not `path`. Returns the
/// crate as registered and the skill's folder.
pub(crate) fn register_pinned_skill(
    skills_dir: &Path,
    path: &Path,
) -> Result<(DevSkill, std::path::PathBuf)> {
    let origin = DevSkill::resolve(path, &BTreeMap::new())?;
    validate_skill_manifest(&origin.source)?;
    if origin.workspace != origin.source {
        eyre::bail!(
            "{} belongs to the cargo workspace at {}. A pinned skill must be its own workspace: add an empty `[workspace]` table to its Cargo.toml",
            origin.source.display(),
            origin.workspace.display()
        );
    }
    let name = origin.source.file_name().unwrap_or_default();
    let installed = skills_dir.join(name);
    if installed.exists() && !source_runtime::is_dev_skill(&installed) {
        eyre::bail!(
            "skill '{}' is already installed at {}. Remove it first (`octos skills remove`)",
            name.to_string_lossy(),
            installed.display()
        );
    }
    let pinned = source_runtime::pin_source(skills_dir, &origin.source)?;
    let mut dev = DevSkill::resolve(&pinned, &BTreeMap::new())?;
    dev.pinned_from = Some(origin.source);
    let dest = source_runtime::register_dev_skill(skills_dir, &dev, false)?;
    Ok((dev, dest))
}

pub(super) fn cmd_dev(
    skills_dir: &Path,
    path: &Path,
    env: &[String],
    warm: bool,
    force: bool,
) -> Result<()> {
    let (dev, dest) = register_source_skill(skills_dir, path, &parse_dev_env(env)?, force)?;
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    println!(
        "  {} Registered '{}' as a dev skill",
        "OK".green(),
        name.cyan()
    );
    println!("     {}  {}", "source:".dimmed(), dev.source.display());
    println!("     {} {}", "package:".dimmed(), dev.package);
    println!("     {}  {}", "folder:".dimmed(), dest.display());
    if warm {
        println!("  Compiling it once through the JIT. Later calls start from the cache.");
        if let Err(report) = source_runtime::warm_dev_skill(&dest) {
            eprintln!("{report}");
            eyre::bail!(
                "the skill does not start. It stays registered, and each call reports this until the source is fixed"
            );
        }
    }
    println!(
        "  A server reads its skills when it starts: restart one that is already running.\n  After that a source edit is live on the next tool call. A change to manifest.json\n  or SKILL.md is read at the next server start."
    );
    Ok(())
}

pub(super) fn cmd_run(
    skills_dir: &Path,
    skill: &str,
    tool: &str,
    input: Option<&str>,
) -> Result<()> {
    use std::io::{IsTerminal as _, Write as _};
    use std::process::Stdio;

    let dev = resolve_source_skill(skills_dir, skill)?;
    let mut command = dev.run_command(tool);
    let input = match input {
        Some(json) => Some(json),
        None if std::io::stdin().is_terminal() => Some("{}"),
        None => None,
    };
    let status = match input {
        Some(json) => {
            let mut child = command
                .stdin(Stdio::piped())
                .spawn()
                .wrap_err("failed to start relax")?;
            if let Some(mut stdin) = child.stdin.take() {
                // A skill that exits without reading its input is its own business.
                let _ = stdin.write_all(json.as_bytes());
            }
            child.wait()?
        }
        None => command.status().wrap_err("failed to start relax")?,
    };
    exit_like(status)
}

pub(super) fn cmd_check(skills_dir: &Path, skill: &str) -> Result<()> {
    let dev = resolve_source_skill(skills_dir, skill)?;
    let status = dev
        .check_command()
        .status()
        .wrap_err("failed to start relax")?;
    exit_like(status)
}

/// End with the exit code of the program that was run for the user.
fn exit_like(status: std::process::ExitStatus) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A patch added to the folder but not to the binary would give an
    /// installed Octos a different Relax than a checkout builds.
    #[test]
    fn the_binary_carries_exactly_the_patches_the_setup_script_applies() {
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/jit/relax-patches");
        let mut on_disk: Vec<String> = std::fs::read_dir(folder)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".patch"))
            .collect();
        on_disk.sort();
        let carried: Vec<&str> = RELAX_PATCHES.iter().map(|(name, _)| *name).collect();
        assert_eq!(carried, on_disk);
    }

    /// The starter crate must pass the same manifest validation an install
    /// applies, and must refuse to write over an existing folder.
    #[test]
    fn a_new_skill_passes_manifest_validation_and_never_overwrites() {
        let parent = tempfile::tempdir().unwrap();
        cmd_new("my-skill", parent.path()).unwrap();
        let manifest =
            std::fs::read_to_string(parent.path().join("my-skill/manifest.json")).unwrap();
        assert!(manifest.contains(r#""name": "my_skill""#) && !manifest.contains("{{"));
        assert!(cmd_new("my-skill", parent.path()).is_err());
        assert!(cmd_new("../escape", parent.path()).is_err());
    }
}
