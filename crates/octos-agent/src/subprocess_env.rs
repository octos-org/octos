//! Environment filtering for subprocess-style tools.
//!
//! Gateway/profile secrets are available in the agent process so first-party
//! providers can resolve credentials. User-invoked subprocess tools should not
//! inherit those raw secrets unless the tool explicitly allowlists them.

use std::collections::HashSet;

use tokio::process::Command;

// The provider-secret machinery (the injection denylist, the heuristic, AND the
// runtime-registered-secret registry) lives in `octos_core::env_hygiene` (the
// bottom crate), so it is the SINGLE source consulted by BOTH this subprocess
// sanitiser AND octos-core's controller-side git sanitiser. A name registered
// via `register_secret_env_names` (re-exported unchanged at
// `octos_agent::register_secret_env_names`) is therefore stripped from EVERY
// spawned subprocess — sandboxed worker git ops AND controller-side git ops.
use crate::sandbox::BLOCKED_ENV_VARS;
pub use octos_core::register_secret_env_names;
use octos_core::{
    is_registered_secret_env_name, is_secret_env_name, sanitize_git_config_group_env,
};

#[derive(Debug, Clone, Default)]
pub(crate) struct EnvAllowlist {
    names: HashSet<String>,
}

impl EnvAllowlist {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            names: names.into_iter().map(normalize_env_name).collect(),
        }
    }

    pub(crate) fn from_strings(names: &[String]) -> Self {
        Self::from_names(names.iter().map(String::as_str))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    fn contains(&self, name: &str) -> bool {
        self.names.contains(&normalize_env_name(name))
    }
}

/// Names that are always retained even when a strict manifest env
/// allowlist is in effect. These are runtime-essential vars that the
/// subprocess needs to function (PATH for binary lookup, locale, etc).
/// Adding to this list is a deliberate decision — every entry here is a
/// var the manifest can NOT exclude.
const ALWAYS_RETAIN_ENV_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "PWD",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_COLLATE",
    "LC_MONETARY",
    "TZ",
    "TERM",
];

fn is_always_retain_env_name(name: &str) -> bool {
    let upper = normalize_env_name(name);
    if ALWAYS_RETAIN_ENV_NAMES.iter().any(|&n| upper == n) {
        return true;
    }
    // Any LC_* locale variant.
    upper.starts_with("LC_")
}

/// Names the harness itself injects into plugin processes; the strict
/// allowlist gate must not strip these because the wrapper later sets
/// them to plumb harness identity (session id, work dir, event sink).
fn is_harness_env_name(name: &str) -> bool {
    let upper = normalize_env_name(name);
    upper.starts_with("OCTOS_")
}

/// Strict variant of [`should_forward_env_name`] for plugin tools that
/// declare a non-empty manifest env allowlist.
///
/// Default semantics ([`should_forward_env_name`]) gate **only** secret-like
/// vars: a manifest declaring `env: ["FOO"]` adds `FOO` to the allowlist
/// but does NOT restrict non-secret vars. That mismatches the plain reading
/// of the manifest field name.
///
/// This strict variant is used when the manifest's `env` list is non-empty.
/// It forwards a name iff:
/// - it is not a known process-hijack var (`LD_PRELOAD`, `DYLD_*`, ...),
/// - AND either:
///   - it is in the manifest allowlist, OR
///   - it is in [`ALWAYS_RETAIN_ENV_NAMES`] (runtime essentials), OR
///   - it is a non-secret harness-injected `OCTOS_*` var.
///
/// Any other env var — secret OR non-secret — is dropped.
pub(crate) fn should_forward_env_name_strict(name: &str, allowlist: &EnvAllowlist) -> bool {
    if is_injection_env_name(name) {
        return false;
    }
    if allowlist.contains(name) {
        return true;
    }
    if is_always_retain_env_name(name) {
        return true;
    }
    if is_secret_env_name(name) || is_registered_secret_env_name(name) {
        return false;
    }
    if is_harness_env_name(name) {
        return true;
    }
    false
}

/// Strict env sanitisation — see [`should_forward_env_name_strict`].
///
/// Use this when the plugin's manifest declares a non-empty `env`
/// allowlist. Strips every env var that isn't in the manifest list,
/// runtime essentials, or the `OCTOS_*` harness namespace.
pub(crate) fn sanitize_command_env_strict(cmd: &mut Command, allowlist: &EnvAllowlist) {
    for (key, _) in std::env::vars_os() {
        let Some(name) = key.to_str() else {
            continue;
        };
        if !should_forward_env_name_strict(name, allowlist) {
            cmd.env_remove(&key);
        }
    }

    for name in BLOCKED_ENV_VARS {
        cmd.env_remove(name);
    }
    sanitize_git_config_group_env(cmd.as_std_mut());
}

fn normalize_env_name(name: &str) -> String {
    name.to_ascii_uppercase()
}

pub(crate) fn is_injection_env_name(name: &str) -> bool {
    BLOCKED_ENV_VARS
        .iter()
        .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

pub(crate) fn should_forward_env_name(name: &str, allowlist: &EnvAllowlist) -> bool {
    if is_injection_env_name(name) {
        return false;
    }
    // Registered provider-key vars are treated like heuristic secrets: stripped
    // by default, but still allowlistable (skills that call LLMs declare them).
    let is_secret = is_secret_env_name(name) || is_registered_secret_env_name(name);
    !is_secret || allowlist.contains(name)
}

/// Strip provider/API-key and process-injection env vars from `cmd` using the
/// DEFAULT (empty-allowlist) policy — the exact sanitisation the shell tool
/// applies before every command ([`sanitize_command_env`] with
/// [`EnvAllowlist::empty`]). Public so an out-of-crate subprocess spawner that
/// runs at a LOWER trust level than the harness (e.g. the fleet worker's
/// sandboxed git ops, whose `.git` `filter.*`/hooks are worker-controlled) can
/// match the shell's secret hygiene and never hand controller credentials to
/// worker-planted code.
pub fn sanitize_default_subprocess_env(cmd: &mut Command) {
    sanitize_command_env(cmd, &EnvAllowlist::empty());
}

pub(crate) fn sanitize_command_env(cmd: &mut Command, allowlist: &EnvAllowlist) {
    for (key, _) in std::env::vars_os() {
        let Some(name) = key.to_str() else {
            continue;
        };
        if !should_forward_env_name(name, allowlist) {
            cmd.env_remove(&key);
        }
    }

    // Remove known code-injection variables even if they are not present in the
    // current process environment. This also clears values set earlier on `cmd`.
    for name in BLOCKED_ENV_VARS {
        cmd.env_remove(name);
    }
    sanitize_git_config_group_env(cmd.as_std_mut());
}
