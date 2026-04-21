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

    fn contains(&self, name: &str) -> bool {
        self.names.contains(&normalize_env_name(name))
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
}
