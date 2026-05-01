//! Environment filtering for subprocess-style tools.
//!
//! Gateway/profile secrets are available in the agent process so first-party
//! providers can resolve credentials. User-invoked subprocess tools should not
//! inherit those raw secrets unless the tool explicitly allowlists them.

use std::collections::HashSet;

use tokio::process::Command;

use crate::sandbox::BLOCKED_ENV_VARS;

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
///   - it is a harness-injected `OCTOS_*` var.
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
}

fn normalize_env_name(name: &str) -> String {
    name.to_ascii_uppercase()
}

fn env_name_tokens(upper_name: &str) -> impl Iterator<Item = &str> {
    upper_name
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
}

pub(crate) fn is_secret_env_name(name: &str) -> bool {
    let upper = normalize_env_name(name);
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

pub(crate) fn is_injection_env_name(name: &str) -> bool {
    BLOCKED_ENV_VARS
        .iter()
        .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

pub(crate) fn should_forward_env_name(name: &str, allowlist: &EnvAllowlist) -> bool {
    if is_injection_env_name(name) {
        return false;
    }
    !is_secret_env_name(name) || allowlist.contains(name)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_secret_env_names() {
        for name in [
            "OPENAI_API_KEY",
            "TAVILY_API_KEY",
            "SMTP_PASSWORD",
            "GITHUB_TOKEN",
            "GITHUB_PAT",
            "LARK_APP_SECRET",
            "SESSION_COOKIE",
            "AWS_SECRET_ACCESS_KEY",
            "private_key",
        ] {
            assert!(is_secret_env_name(name), "{name} should be secret");
        }
    }

    #[test]
    fn does_not_flag_common_non_secret_runtime_env_names() {
        for name in [
            "PATH",
            "HOME",
            "PWD",
            "USER",
            "OPENAI_BASE_URL",
            "OMINIX_API_URL",
            "OCTOS_PROFILE_ID",
            "PPT_TEMPLATE_DIR",
            "TOKENIZERS_PARALLELISM",
            "NPM_CONFIG_CACHE",
        ] {
            assert!(!is_secret_env_name(name), "{name} should not be secret");
        }
    }

    #[test]
    fn allowlist_permits_secret_names_but_not_injection_names() {
        let allowlist = EnvAllowlist::from_names(["OPENAI_API_KEY", "LD_PRELOAD"]);

        assert!(should_forward_env_name("OPENAI_API_KEY", &allowlist));
        assert!(!should_forward_env_name("TAVILY_API_KEY", &allowlist));
        assert!(!should_forward_env_name("LD_PRELOAD", &allowlist));
        assert!(should_forward_env_name("OPENAI_BASE_URL", &allowlist));
    }

    #[test]
    fn strict_allowlist_permits_listed_names() {
        let allowlist = EnvAllowlist::from_names(["MY_VAR", "OPENAI_API_KEY"]);
        assert!(should_forward_env_name_strict("MY_VAR", &allowlist));
        assert!(should_forward_env_name_strict("my_var", &allowlist)); // case-insensitive
        assert!(should_forward_env_name_strict("OPENAI_API_KEY", &allowlist));
    }

    #[test]
    fn strict_allowlist_drops_non_listed_secret_and_non_secret_names() {
        let allowlist = EnvAllowlist::from_names(["MY_VAR"]);
        // Secret not in the list: dropped.
        assert!(!should_forward_env_name_strict(
            "OPENAI_API_KEY",
            &allowlist
        ));
        // Non-secret not in the list and not runtime-essential: dropped.
        assert!(!should_forward_env_name_strict(
            "PPT_TEMPLATE_DIR",
            &allowlist
        ));
    }

    #[test]
    fn strict_allowlist_retains_runtime_essentials() {
        let allowlist = EnvAllowlist::from_names(["MY_VAR"]);
        for name in ["PATH", "HOME", "PWD", "USER", "LANG", "TZ", "TERM"] {
            assert!(
                should_forward_env_name_strict(name, &allowlist),
                "{name} should be retained"
            );
        }
    }

    #[test]
    fn strict_allowlist_retains_lc_locale_variants() {
        let allowlist = EnvAllowlist::from_names(["MY_VAR"]);
        for name in ["LC_CTYPE", "LC_ALL", "LC_MESSAGES", "LC_TIME"] {
            assert!(
                should_forward_env_name_strict(name, &allowlist),
                "{name} should be retained"
            );
        }
    }

    #[test]
    fn strict_allowlist_retains_octos_namespace() {
        let allowlist = EnvAllowlist::from_names(["MY_VAR"]);
        assert!(should_forward_env_name_strict("OCTOS_TASK_ID", &allowlist));
        assert!(should_forward_env_name_strict("OCTOS_WORK_DIR", &allowlist));
    }

    #[test]
    fn strict_allowlist_drops_injection_names_even_if_listed() {
        // Defense in depth — if a manifest somehow listed LD_PRELOAD,
        // strict gate still strips it.
        let allowlist = EnvAllowlist::from_names(["LD_PRELOAD", "MY_VAR"]);
        assert!(!should_forward_env_name_strict("LD_PRELOAD", &allowlist));
        assert!(should_forward_env_name_strict("MY_VAR", &allowlist));
    }
}
