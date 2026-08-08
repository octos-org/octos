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

    // Safety: only allow read-only commands
    let allowed = [
        "grep", "cat", "ls", "find", "nm", "head", "tail", "wc", "file", "echo",
    ];
    let first_word = cmd.split_whitespace().next().unwrap_or("");
    if !allowed.iter().any(|a| first_word.starts_with(a)) {
        return Ok(ReAuditResult::CommandNotAllowed(cmd.to_string()));
    }

    // Re-run the command
    let output = Command::new("sh").arg("-c").arg(cmd).output()?;
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
