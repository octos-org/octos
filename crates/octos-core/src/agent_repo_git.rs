//! Kernel-side `git` over a repository the agent can write to.
//!
//! The kernel runs `git` unsandboxed in repositories the agent works in (the
//! shell tool's change receipt, the budget-exhaustion checkpoint, the spawn
//! tool's worker worktrees, the `git` tool's blame). The agent — through a
//! sandboxed shell — can edit that repository's `.git/config` and `hooks/`,
//! and those files decide which programs git runs. Unlike the workspace
//! snapshot repos (which run through a private git dir, see
//! `octos_agent::private_git`), these are the user's own repositories, so the
//! user's global/system config (identity, LFS filters) must keep working.
//!
//! [`agent_repo_git`] therefore keeps global and system config but overrides
//! every program-launching setting that comes from the REPOSITORY scope
//! (`.git/config`, `config.worktree`, and anything they `include`):
//!
//! - fixed keys, always: `core.hooksPath` = the null device (no hook ever
//!   runs), `core.fsmonitor=false`, commit/tag signing
//!   and `log.showSignature` off, `gc.auto=0`, `maintenance.auto=false`;
//! - named drivers and program keys found at local/worktree scope
//!   (`filter.<n>.clean|smudge|process`, `merge.<n>.driver`, `gpg[.<fmt>].program`, `core.sshCommand`,
//!   `core.editor`, `core.pager`, `pager.*`, `credential[.<url>].helper`, …)
//!   are overridden to the empty string, which git treats as "no program".
//!
//! Diff drivers (`diff.external`, `diff.<n>.command|textconv`) cannot be
//! emptied that way — git tries to run the empty string — so a caller that
//! produces a diff (`diff`, `log -p`, `show`, `blame`) MUST also pass
//! `--no-ext-diff --no-textconv` (blame: `--no-textconv`).
//!
//! Overrides are passed through `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_<n>`
//! (command scope, which beats repository scope) so a subsection name
//! containing `=` cannot break them. The driver list is read with
//! `git config --list --show-scope --includes` immediately before the
//! command; a process of the agent's that rewrites the config in the window
//! between the two calls is the residual race, which only running the call
//! inside the agent's own sandbox removes.

use std::path::Path;
use std::process::Command;

#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";
#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";

/// Program-launching settings always overridden, whatever the repo says.
const FIXED_OVERRIDES: &[(&str, &str)] = &[
    ("core.hooksPath", NULL_DEVICE),
    ("core.fsmonitor", "false"),
    ("commit.gpgSign", "false"),
    ("tag.gpgSign", "false"),
    ("log.showSignature", "false"),
    ("gc.auto", "0"),
    ("maintenance.auto", "false"),
    ("core.sshCommand", ""),
    ("core.editor", ":"),
    ("sequence.editor", ":"),
    ("core.pager", "cat"),
    ("core.askPass", ""),
];

/// Whether a repository-scope config key names a program git may launch.
fn launches_a_program(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let (section, rest) = key.split_once('.').unwrap_or((key.as_str(), ""));
    let var = rest.rsplit('.').next().unwrap_or("");
    let has_subsection = rest.contains('.');
    match section {
        "filter" => has_subsection && matches!(var, "clean" | "smudge" | "process"),
        "merge" => has_subsection && var == "driver",
        "gpg" => var == "program",
        "credential" => var == "helper",
        "pager" => true,
        "core" => matches!(
            var,
            "fsmonitor"
                | "hookspath"
                | "sshcommand"
                | "gitproxy"
                | "askpass"
                | "editor"
                | "pager"
                | "alternaterefscommand"
        ),
        "sequence" => var == "editor",
        "uploadpack" => var == "packobjectshook",
        "remote" => matches!(var, "uploadpack" | "receivepack"),
        "submodule" => var == "update",
        _ => false,
    }
}

/// Repository-scope keys (local/worktree, includes resolved) that launch a
/// program. `None` when the config could not be listed.
fn repository_program_keys(repo: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["config", "--list", "--show-scope", "--includes", "-z"])
        .output()
        .ok()?;
    if !output.status.success() {
        // Not a repository (or no config): nothing repository-scoped to mask.
        return Some(Vec::new());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // `-z --show-scope`: `<scope>\0<key>\n<value>\0` per entry.
    let mut fields = text.split('\0');
    let mut keys = Vec::new();
    while let (Some(scope), Some(entry)) = (fields.next(), fields.next()) {
        if !matches!(scope, "local" | "worktree") {
            continue;
        }
        let key = entry.split_once('\n').map_or(entry, |(key, _)| key);
        if launches_a_program(key) && !keys.iter().any(|k: &String| k == key) {
            keys.push(key.to_string());
        }
    }
    Some(keys)
}

/// Overrides (`key`, empty value) for every program-launching key the
/// repository scope of `repo` sets, or `None` when its config could not be
/// listed at all.
pub fn repository_program_overrides(repo: &Path) -> Option<Vec<(String, String)>> {
    let keys = repository_program_keys(repo)?;
    Some(
        keys.into_iter()
            .map(|key| {
                let value = if key.to_ascii_lowercase().ends_with(".fsmonitor") {
                    "false"
                } else {
                    ""
                };
                (key, value.to_string())
            })
            .collect(),
    )
}

/// Append config overrides to `cmd` as command-scope `GIT_CONFIG_*` entries
/// (arbitrary key characters are safe there, unlike `-c key=value`).
pub fn apply_config_overrides(cmd: &mut Command, overrides: &[(String, String)]) {
    cmd.env_remove("GIT_CONFIG_PARAMETERS");
    cmd.env("GIT_CONFIG_COUNT", overrides.len().to_string());
    for (index, (key, value)) in overrides.iter().enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{index}"), key)
            .env(format!("GIT_CONFIG_VALUE_{index}"), value);
    }
}

/// `git -C <repo>` with every repository-scope program setting overridden.
/// See the module docs for what is and is not covered.
pub fn agent_repo_git(repo: &Path) -> Command {
    let Some(dynamic) = repository_program_overrides(repo) else {
        // Could not even list the config: refuse to run anything by pointing
        // git at a repository that cannot exist.
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo).env("GIT_DIR", NULL_DEVICE);
        return cmd;
    };
    let mut overrides: Vec<(String, String)> = FIXED_OVERRIDES
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    for (key, value) in dynamic {
        if !overrides.iter().any(|(k, _)| k.eq_ignore_ascii_case(&key)) {
            overrides.push((key, value));
        }
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    crate::env_hygiene::sanitize_git_command_env(&mut cmd);
    apply_config_overrides(&mut cmd, &overrides);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A committed repo whose local config/hooks an agent has poisoned.
    fn poisoned_repo() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let markers = temp.path().join("markers");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&markers).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.name", "T"]);
        git(&repo, &["config", "user.email", "t@t"]);
        std::fs::write(repo.join("f.txt"), "one\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "seed"]);
        let touch = |name: &str| format!("touch '{}'", markers.join(name).display());
        let included = temp.path().join("included.gitconfig");
        std::fs::write(
            &included,
            format!(
                "[filter \"q\"]\n\tclean = \"{}; cat\"\n",
                touch("INCLUDED_CLEAN")
            ),
        )
        .unwrap();
        let evil = format!(
            "[filter \"p\"]\n\tclean = \"{clean}; cat\"\n\tprocess = \"{process}\"\n\
             [core]\n\tfsmonitor = \"{fsm}\"\n\thooksPath = {hooks}\n\
             [diff]\n\texternal = \"{ext}\"\n[diff \"x\"]\n\ttextconv = \"{tc}; cat\"\n\
             [include]\n\tpath = {inc}\n",
            clean = touch("CLEAN"),
            process = touch("PROCESS"),
            fsm = touch("FSMONITOR"),
            hooks = temp.path().join("hooks").display(),
            ext = touch("EXTERNAL"),
            tc = touch("TEXTCONV"),
            inc = included.display(),
        );
        let config = repo.join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(&evil);
        std::fs::write(&config, text).unwrap();
        let hooks = temp.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        for hook in [
            "pre-commit",
            "post-commit",
            "post-checkout",
            "reference-transaction",
        ] {
            let path = hooks.join(hook);
            std::fs::write(&path, format!("#!/bin/sh\n{}\n", touch(hook))).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        std::fs::write(
            repo.join(".gitattributes"),
            "*.txt filter=p diff=x\n*.md filter=q\n",
        )
        .unwrap();
        (temp, repo, markers)
    }

    fn markers_in(markers: &Path) -> Vec<String> {
        std::fs::read_dir(markers)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn should_not_run_local_config_programs_when_kernel_runs_git_in_agent_repo() {
        let (_temp, repo, markers) = poisoned_repo();
        std::fs::write(repo.join("f.txt"), "two\n").unwrap();
        std::fs::write(repo.join("g.md"), "md\n").unwrap();
        for args in [
            &["status", "--porcelain"][..],
            &["diff", "--no-ext-diff", "--no-textconv", "HEAD"][..],
            &["add", "-A"][..],
            &["commit", "-qm", "checkpoint"][..],
            &["worktree", "add", "-q", "../wt", "HEAD"][..],
        ] {
            let out = agent_repo_git(&repo).args(args).output().unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let found = markers_in(&markers);
        assert!(
            found.is_empty(),
            "agent-controlled git config executed: {found:?}"
        );
    }
}
