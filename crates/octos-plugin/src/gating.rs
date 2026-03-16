use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::manifest::Requirements;

/// Result of a single gating check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateCheck {
    pub gate: String,
    pub passed: bool,
    pub detail: String,
}

/// Aggregate result of all gating checks for a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatingResult {
    /// Individual check results.
    pub checks: Vec<GateCheck>,
    /// Whether all checks passed.
    pub passed: bool,
    /// Human-readable summary of failures (empty if all passed).
    pub summary: String,
}

impl GatingResult {
    /// Create a result indicating all checks passed (no requirements).
    pub fn all_passed() -> Self {
        GatingResult {
            checks: Vec::new(),
            passed: true,
            summary: String::new(),
        }
    }
}

/// Check whether the given requirements are satisfied.
///
/// `env_vars` should contain the environment variables to check against.
/// Typically this is the real `std::env::vars()` collected into a map, but
/// it can also include profile-injected variables.
pub fn check_requirements(reqs: &Requirements, env_vars: &HashMap<String, String>) -> GatingResult {
    let mut checks = Vec::new();

    // Check required binaries on PATH.
    for bin in &reqs.bins {
        let found = which::which(bin).is_ok();
        checks.push(GateCheck {
            gate: format!("bin:{bin}"),
            passed: found,
            detail: if found {
                format!("binary '{bin}' found on PATH")
            } else {
                format!("binary '{bin}' not found on PATH")
            },
        });
    }

    // Check required environment variables.
    for var in &reqs.env {
        let set = env_vars.contains_key(var.as_str());
        checks.push(GateCheck {
            gate: format!("env:{var}"),
            passed: set,
            detail: if set {
                format!("env var '{var}' is set")
            } else {
                format!("env var '{var}' is not set")
            },
        });
    }

    // Check OS constraint.
    if !reqs.os.is_empty() {
        let current_os = std::env::consts::OS;
        let matched = reqs.os.iter().any(|os| os == current_os);
        checks.push(GateCheck {
            gate: format!("os:{}", reqs.os.join(",")),
            passed: matched,
            detail: if matched {
                format!("current OS '{current_os}' is in allowed list")
            } else {
                format!(
                    "current OS '{current_os}' not in allowed list [{}]",
                    reqs.os.join(", ")
                )
            },
        });
    }

    let passed = checks.iter().all(|c| c.passed);
    let summary = if passed {
        String::new()
    } else {
        let failures: Vec<&str> = checks
            .iter()
            .filter(|c| !c.passed)
            .map(|c| c.detail.as_str())
            .collect();
        failures.join("; ")
    };

    GatingResult {
        checks,
        passed,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_requirements_always_pass() {
        let reqs = Requirements::default();
        let env = HashMap::new();
        let result = check_requirements(&reqs, &env);
        assert!(result.passed);
        assert!(result.checks.is_empty());
    }

    #[test]
    fn missing_env_var_fails() {
        let reqs = Requirements {
            env: vec!["NONEXISTENT_SECRET_KEY_12345".to_string()],
            ..Default::default()
        };
        let env = HashMap::new();
        let result = check_requirements(&reqs, &env);
        assert!(!result.passed);
        assert_eq!(result.checks.len(), 1);
        assert!(!result.checks[0].passed);
        assert!(result.summary.contains("NONEXISTENT_SECRET_KEY_12345"));
    }

    #[test]
    fn present_env_var_passes() {
        let reqs = Requirements {
            env: vec!["MY_VAR".to_string()],
            ..Default::default()
        };
        let mut env = HashMap::new();
        env.insert("MY_VAR".to_string(), "value".to_string());
        let result = check_requirements(&reqs, &env);
        assert!(result.passed);
    }

    #[test]
    fn bin_check_for_common_binary() {
        // `ls` or `sh` should exist on any Unix system
        let reqs = Requirements {
            bins: vec!["sh".to_string()],
            ..Default::default()
        };
        let env = HashMap::new();
        let result = check_requirements(&reqs, &env);
        assert!(result.passed);
    }

    #[test]
    fn bin_check_for_nonexistent_binary() {
        let reqs = Requirements {
            bins: vec!["nonexistent_binary_xyz_99999".to_string()],
            ..Default::default()
        };
        let env = HashMap::new();
        let result = check_requirements(&reqs, &env);
        assert!(!result.passed);
        assert!(result.summary.contains("nonexistent_binary_xyz_99999"));
    }

    #[test]
    fn os_check_current_platform() {
        let current_os = std::env::consts::OS.to_string();
        let reqs = Requirements {
            os: vec![current_os.clone()],
            ..Default::default()
        };
        let env = HashMap::new();
        let result = check_requirements(&reqs, &env);
        assert!(result.passed);
    }

    #[test]
    fn os_check_wrong_platform() {
        // Pick an OS that is definitely not the current one.
        let fake_os = if std::env::consts::OS == "linux" {
            "windows"
        } else {
            "linux"
        };
        let reqs = Requirements {
            os: vec![fake_os.to_string()],
            ..Default::default()
        };
        let env = HashMap::new();
        let result = check_requirements(&reqs, &env);
        assert!(!result.passed);
    }

    #[test]
    fn multiple_checks_all_must_pass() {
        let reqs = Requirements {
            bins: vec!["sh".to_string()],
            env: vec!["SOME_VAR".to_string()],
            os: vec![std::env::consts::OS.to_string()],
        };
        // env var missing → should fail overall
        let env = HashMap::new();
        let result = check_requirements(&reqs, &env);
        assert!(!result.passed);

        // now provide env var
        let mut env2 = HashMap::new();
        env2.insert("SOME_VAR".to_string(), "1".to_string());
        let result2 = check_requirements(&reqs, &env2);
        assert!(result2.passed);
    }

    #[test]
    fn summary_contains_all_failures() {
        let reqs = Requirements {
            bins: vec!["no_such_bin_aaa".to_string()],
            env: vec!["NO_SUCH_ENV_BBB".to_string()],
            ..Default::default()
        };
        let env = HashMap::new();
        let result = check_requirements(&reqs, &env);
        assert!(!result.passed);
        assert!(result.summary.contains("no_such_bin_aaa"));
        assert!(result.summary.contains("NO_SUCH_ENV_BBB"));
    }
}
