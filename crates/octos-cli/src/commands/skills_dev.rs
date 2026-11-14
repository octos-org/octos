//! `octos skills dev | run | check`: develop a skill from its Rust source
//! through the Relax JIT. Experimental; see `docs/JIT_DEV_MODE.md`.

use std::collections::BTreeMap;
use std::path::Path;

use colored::Colorize;
use eyre::{Result, WrapErr};
use octos_agent::plugins::source_runtime::{self, DevSkill};

use super::skills::{looks_like_local_path, validate_skill_manifest};

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

pub(super) fn cmd_dev(
    skills_dir: &Path,
    path: &Path,
    env: &[String],
    warm: bool,
    force: bool,
) -> Result<()> {
    let dev = DevSkill::resolve(path, &parse_dev_env(env)?)?;
    validate_skill_manifest(&dev.source)?;
    let dest = source_runtime::register_dev_skill(skills_dir, &dev, force)?;
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
