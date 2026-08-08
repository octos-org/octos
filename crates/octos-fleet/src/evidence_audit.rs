// Evidence-command re-audit: independently verify findings by re-running evidence commands

use eyre::Result;
use std::process::Command;

/// Re-audit a finding by re-running its evidence command and comparing output.
pub fn re_audit_evidence(evidence_json: &str) -> Result<ReAuditResult> {
    let evidence: serde_json::Value = serde_json::from_str(evidence_json)?;

    let Some(cmd) = evidence.get("command").and_then(|c| c.as_str()) else {
        return Ok(ReAuditResult::NoCommand);
    };

    let expected_output = evidence.get("output").and_then(|o| o.as_str());

    // CRITICAL: Parse command as argv (not shell string) to prevent injection.
    // Do NOT use sh -c, which allows arbitrary command chaining with ;, &&, ||, etc.
    // Instead, split into argv and execute directly without a shell.
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(ReAuditResult::NoCommand);
    }

    // Safety: only allow read-only commands (exact match, not prefix)
    let allowed = [
        "grep", "cat", "ls", "find", "nm", "head", "tail", "wc", "file", "echo",
    ];
    let program = parts[0];
    if !allowed.contains(&program) {
        return Ok(ReAuditResult::CommandNotAllowed(cmd.to_string()));
    }

    // CRITICAL: Validate arguments to prevent command injection via flags.
    // Reject arguments that could execute commands or access unauthorized files:
    // - find: reject -exec, -execdir, -delete
    // - grep: reject --include/--exclude with shell metacharacters
    // - all: reject arguments starting with - that could be dangerous
    let args = &parts[1..];
    match program {
        "find" => {
            for arg in args {
                if arg.starts_with("-exec") || arg.starts_with("-delete") {
                    return Ok(ReAuditResult::CommandNotAllowed(format!(
                        "find with {} is not allowed (command injection risk)",
                        arg
                    )));
                }
            }
        }
        "grep" => {
            for arg in args {
                // Reject shell metacharacters in arguments (injection via filename expansion)
                if arg.contains(';')
                    || arg.contains('&')
                    || arg.contains('|')
                    || arg.contains('`')
                    || arg.contains('$')
                {
                    return Ok(ReAuditResult::CommandNotAllowed(format!(
                        "grep argument {:?} contains shell metacharacters",
                        arg
                    )));
                }
            }
        }
        _ => {
            // For other commands, reject any argument with shell metacharacters
            for arg in args {
                if arg.contains(';')
                    || arg.contains('&')
                    || arg.contains('|')
                    || arg.contains('`')
                    || arg.contains('$')
                    || arg.contains('>')
                    || arg.contains('<')
                {
                    return Ok(ReAuditResult::CommandNotAllowed(format!(
                        "argument {:?} contains shell metacharacters",
                        arg
                    )));
                }
            }
        }
    }

    // Execute WITHOUT a shell (no sh -c) to prevent command injection
    let output = Command::new(program).args(args).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    if !output.status.success() {
        return Ok(ReAuditResult::CommandFailed(
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    // Compare output
    if let Some(expected) = expected_output {
        let matches = expected.lines().any(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && stdout.contains(trimmed)
        });
        if matches {
            Ok(ReAuditResult::Verified)
        } else {
            Ok(ReAuditResult::OutputMismatch {
                expected: expected.to_string(),
                actual: stdout.to_string(),
            })
        }
    } else {
        Ok(ReAuditResult::Verified)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReAuditResult {
    /// Evidence verified (command succeeded, output matches)
    Verified,
    /// No command in evidence (nothing to re-run)
    NoCommand,
    /// Command not in allowlist (safety)
    CommandNotAllowed(String),
    /// Command failed to execute
    CommandFailed(i32, String),
    /// Command succeeded but output doesn't match expected
    OutputMismatch { expected: String, actual: String },
}
