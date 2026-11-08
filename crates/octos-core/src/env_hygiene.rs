//! Shared subprocess environment hygiene: the process-injection var denylist and
//! the secret-name heuristic, plus a [`std::process::Command`] sanitizer.
//!
//! This lives in octos-core (the bottom crate) so there is ONE source of truth
//! for both:
//! - the agent's sandboxed subprocess spawners (octos-agent re-exports
//!   [`BLOCKED_ENV_VARS`] / [`is_secret_env_name`] and layers its
//!   allowlist + runtime-registered-provider-key machinery on top), and
//! - octos-core's OWN controller-side git ops
//!   ([`crate::git_worktree`]), which spawn `git` directly (OUTSIDE the worker
//!   sandbox) and must not hand a lower-trust worker's planted code any
//!   controller secret or a code-injection var.

use std::collections::{BTreeMap, HashSet};
use std::process::Command;
use std::sync::{LazyLock, RwLock};

/// Environment variables blocked before spawning a subprocess (code-injection
/// vectors: shared-library preload, runtime option injection, shell startup).
///
/// Shared across sandbox backends, MCP server spawning, hooks, the browser tool
/// (all via octos-agent's re-export), AND octos-core's controller-side git ops.
pub const BLOCKED_ENV_VARS: &[&str] = &[
    // Linux: shared library injection
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    // macOS: dylib injection
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_VERSIONED_LIBRARY_PATH",
    // Runtime-specific code injection
    "NODE_OPTIONS",
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "PERL5OPT",
    "RUBYOPT",
    "RUBYLIB",
    "JAVA_TOOL_OPTIONS",
    // Shell startup injection
    "BASH_ENV",
    "ENV",
    "ZDOTDIR",
];

/// Exact env-var names registered at runtime as provider secrets (e.g. the
/// resolved LLM `api_key_env`, or Google Vertex's `VERTEX_SA_JSON`). These are
/// stripped from subprocess environments EXACTLY like heuristic secrets, closing
/// the gap where a provider credential's NAME does not look secret to
/// [`is_secret_env_name`] (e.g. `VERTEX_SA_JSON`, `ANTHROPIC_CREDS`).
///
/// The registry lives HERE (the bottom crate) so it is the SINGLE source
/// consulted by BOTH octos-agent's subprocess sanitiser AND octos-core's
/// controller-side git sanitiser ([`sanitize_git_command_env`]) — a name
/// registered via [`register_secret_env_names`] is stripped from every spawned
/// subprocess, sandboxed or not.
static REGISTERED_SECRET_ENV: LazyLock<RwLock<HashSet<String>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

/// Register env-var names that must ALWAYS be stripped from subprocess
/// environments (provider credentials whose name the heuristic may miss).
/// Idempotent; names are normalized (ASCII-uppercased). Safe to call repeatedly
/// from provider construction.
pub fn register_secret_env_names<I, S>(names: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut set = REGISTERED_SECRET_ENV
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for name in names {
        let normalized = normalize_env_name(name.as_ref());
        if !normalized.is_empty() {
            set.insert(normalized);
        }
    }
}

/// Whether `name` was registered as a provider secret via
/// [`register_secret_env_names`].
pub fn is_registered_secret_env_name(name: &str) -> bool {
    let normalized = normalize_env_name(name);
    REGISTERED_SECRET_ENV
        .read()
        .map(|set| set.contains(&normalized))
        .unwrap_or(false)
}

fn normalize_env_name(name: &str) -> String {
    name.to_ascii_uppercase()
}

fn env_name_tokens(upper_name: &str) -> impl Iterator<Item = &str> {
    upper_name
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
}

fn git_config_index(name: &str, prefix: &str) -> Option<usize> {
    let suffix = name.strip_prefix(prefix)?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

fn is_git_config_group_name(name: &str) -> bool {
    name == "GIT_CONFIG_COUNT"
        || git_config_index(name, "GIT_CONFIG_KEY_").is_some()
        || git_config_index(name, "GIT_CONFIG_VALUE_").is_some()
}

/// Heuristic: does `name` look like it holds a credential (API key, token,
/// password, cookie, …)? Used to strip provider/API secrets from subprocess
/// environments so lower-trust spawned code can't read them.
pub fn is_secret_env_name(name: &str) -> bool {
    let upper = normalize_env_name(name);
    // Git's official config protocol is a group, not a credential named KEY.
    // Group-level validation/removal happens in sanitize_git_config_group_env;
    // deleting just KEY_n leaves COUNT/VALUE_n behind and breaks every git op.
    if is_git_config_group_name(&upper) {
        return false;
    }
    let tokens: Vec<&str> = env_name_tokens(&upper).collect();

    if tokens.iter().any(|token| {
        matches!(
            *token,
            "TOKEN"
                | "SECRET"
                | "PASSWORD"
                | "PASSCODE"
                | "PASSPHRASE"
                | "CREDENTIAL"
                | "CREDENTIALS"
                | "PAT"
                | "BEARER"
                | "AUTHORIZATION"
                | "COOKIE"
        )
    }) {
        return true;
    }

    upper.contains("APIKEY")
        || upper.contains("API_KEY")
        || upper.contains("ACCESSKEY")
        || upper.contains("SECRETKEY")
        || upper.contains("PRIVATEKEY")
        || upper == "KEY"
        || upper.ends_with("_KEY")
        || upper.contains("_KEY_")
}

/// Keep Git's official `GIT_CONFIG_COUNT`/`KEY_n`/`VALUE_n` environment group
/// intact. A structurally incomplete or inconsistent group is removed whole,
/// including command-specific overrides and inherited entries. Empty values are
/// valid; existing global/system config files are not read or modified here.
pub fn sanitize_git_config_group_env(cmd: &mut Command) {
    let mut effective = BTreeMap::<String, Option<std::ffi::OsString>>::new();
    for (name, value) in std::env::vars_os() {
        let Ok(name) = name.into_string() else {
            continue;
        };
        let normalized = normalize_env_name(&name);
        if is_git_config_group_name(&normalized) {
            effective.insert(normalized, Some(value));
        }
    }
    for (name, value) in cmd.get_envs() {
        let Some(name) = name.to_str() else { continue };
        let normalized = normalize_env_name(name);
        if is_git_config_group_name(&normalized) {
            effective.insert(normalized, value.map(std::ffi::OsStr::to_owned));
        }
    }
    effective.retain(|_, value| value.is_some());
    if effective.is_empty() {
        return;
    }

    let count = effective
        .get("GIT_CONFIG_COUNT")
        .and_then(|value| value.as_ref())
        .and_then(|value| value.to_str())
        .and_then(|value| value.parse::<usize>().ok());
    let mut indices = BTreeMap::<usize, (bool, bool)>::new();
    let mut duplicate = false;
    for name in effective.keys() {
        for (prefix, value_side) in [("GIT_CONFIG_KEY_", false), ("GIT_CONFIG_VALUE_", true)] {
            if let Some(index) = git_config_index(name, prefix) {
                let sides = indices.entry(index).or_default();
                let side = if value_side {
                    &mut sides.1
                } else {
                    &mut sides.0
                };
                duplicate |= std::mem::replace(side, true);
            }
        }
    }
    let valid = !duplicate
        && count.is_some_and(|count| {
            indices.len() == count
                && indices
                    .iter()
                    .all(|(index, (key, value))| *index < count && *key && *value)
        });
    if !valid {
        for name in effective.keys() {
            cmd.env_remove(name);
        }
    }
}

/// Strip provider secrets + code-injection vars from a `std::process::Command`
/// that octos-core spawns DIRECTLY (the controller-side git ops), so the child —
/// and any exec it might trigger — does not inherit any provider credential or an
/// injection var from the controller's environment.
///
/// This strips EXACTLY the set octos-agent's `sanitize_default_subprocess_env`
/// does for the worker-sandboxed git ops: heuristic secrets ([`is_secret_env_name`])
/// AND runtime-REGISTERED provider secrets ([`is_registered_secret_env_name`],
/// e.g. `VERTEX_SA_JSON` — a name the heuristic does not flag) AND
/// [`BLOCKED_ENV_VARS`]. Because the registry is shared (this crate), a name
/// registered anywhere is stripped from BOTH surfaces.
pub fn sanitize_git_command_env(cmd: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let Some(name) = key.to_str() else {
            continue;
        };
        if is_secret_env_name(name) || is_registered_secret_env_name(name) {
            cmd.env_remove(&key);
        }
    }
    // Remove known code-injection vars even if absent from the current env (also
    // clears any value set earlier on `cmd`).
    for name in BLOCKED_ENV_VARS {
        cmd.env_remove(name);
    }
    sanitize_git_config_group_env(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_provider_secret_is_flagged_even_when_not_heuristic() {
        // `VERTEX_SA_JSON` is NOT flagged by the heuristic (no secret token / no
        // `_KEY`), so it is caught ONLY once registered — the gap the registry
        // closes. Use a unique fixture so it can't collide with another test in
        // this (process-global) registry.
        let fixture = "OCTOS_ENVHYGIENE_FIXTURE_SA_JSON";
        assert!(
            !is_secret_env_name(fixture),
            "fixture must be a name the heuristic does NOT flag"
        );
        assert!(!is_registered_secret_env_name(fixture));
        register_secret_env_names([fixture]);
        assert!(is_registered_secret_env_name(fixture));
        // case-insensitive
        assert!(is_registered_secret_env_name(
            "octos_envhygiene_fixture_sa_json"
        ));
    }

    fn config_env(cmd: &Command) -> Vec<(String, Option<String>)> {
        cmd.get_envs()
            .filter(|(name, _)| name.to_string_lossy().starts_with("GIT_CONFIG_"))
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn git_config_groups_survive_whole_or_are_removed_whole() {
        let mut valid = Command::new("true");
        valid
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "octos.envhygiene.marker")
            .env("GIT_CONFIG_VALUE_0", "first")
            .env("GIT_CONFIG_KEY_1", "octos.envhygiene.empty")
            .env("GIT_CONFIG_VALUE_1", "");
        sanitize_git_command_env(&mut valid);
        let env = config_env(&valid);
        assert_eq!(env.len(), 5, "a valid multi-index group stays intact");
        assert!(
            env.iter().any(|(name, value)| {
                name == "GIT_CONFIG_VALUE_1" && value.as_deref() == Some("")
            })
        );
        assert!(!is_secret_env_name("GIT_CONFIG_KEY_0"));

        for variables in [
            &[("GIT_CONFIG_COUNT", "not-a-number")][..],
            &[
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "octos.envhygiene.marker"),
            ][..],
            &[("GIT_CONFIG_COUNT", "1"), ("GIT_CONFIG_VALUE_0", "orphan")][..],
            &[
                ("GIT_CONFIG_KEY_0", "octos.envhygiene.marker"),
                ("GIT_CONFIG_VALUE_0", "orphan"),
            ][..],
            &[
                ("GIT_CONFIG_COUNT", "0"),
                ("GIT_CONFIG_KEY_0", "octos.envhygiene.extra"),
                ("GIT_CONFIG_VALUE_0", "extra"),
            ][..],
        ] {
            let mut malformed = Command::new("true");
            for (name, value) in variables {
                malformed.env(name, value);
            }
            sanitize_git_command_env(&mut malformed);
            assert!(
                config_env(&malformed)
                    .iter()
                    .all(|(_, value)| value.is_none()),
                "malformed group must be removed whole: {variables:?}"
            );
        }
    }

    #[test]
    fn real_git_reads_sanitized_config_groups_without_touching_global_config() {
        for mode in ["single", "multi"] {
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap())
                .arg("env_hygiene::tests::real_git_config_group_child")
                .arg("--exact")
                .arg("--ignored")
                .arg("--test-threads=1")
                .env("OCTOS_ENV_HYGIENE_TEST_MODE", mode)
                .env("GIT_CONFIG_KEY_0", "octos.envhygiene.marker")
                .env("GIT_CONFIG_VALUE_0", "from-group");
            if mode == "multi" {
                child
                    .env("GIT_CONFIG_COUNT", "2")
                    .env("GIT_CONFIG_KEY_1", "octos.envhygiene.empty")
                    .env("GIT_CONFIG_VALUE_1", "");
            } else {
                child.env("GIT_CONFIG_COUNT", "1");
            }
            let status = child.status().unwrap();
            assert!(
                status.success(),
                "sanitized real git failed for {mode} config group"
            );
        }
    }

    #[test]
    fn sanitize_git_command_env_strips_injection_vars() {
        let mut cmd = Command::new("true");
        sanitize_git_command_env(&mut cmd);
        let removed: Vec<String> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                if v.is_none() {
                    Some(k.to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect();
        for var in BLOCKED_ENV_VARS {
            assert!(removed.iter().any(|r| r == *var), "must strip {var}");
        }
    }
}
