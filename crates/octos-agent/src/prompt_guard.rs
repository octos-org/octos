//! Prompt injection detection and sanitization (defense-in-depth).
//!
//! Scans text (tool output, user messages) for prompt injection patterns
//! and optionally defangs them before they enter the conversation history.
//!
//! **Not a security boundary.** This module detects naive plaintext injection
//! attempts (e.g., "ignore previous instructions") via regex pattern matching.
//! It is bypassed by: base64 encoding, URL encoding, HTML entities, Unicode
//! homoglyphs, zero-width characters, and RTL override characters. These are
//! documented as known limitations in the test suite.
//!
//! Architectural controls are the real mitigations:
//! - **Sandbox isolation** prevents tool-level damage regardless of prompt state.
//! - **Tool policy** (allow/deny lists) restricts which tools the agent can invoke.
//! - **Human-in-the-loop** (hook `before_tool_call` with exit code 1) blocks
//!   high-impact actions pending user approval.
//!
//! This module provides logging and best-effort sanitization as an additional
//! layer, not as a substitute for the above.

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;
use tracing::warn;

/// Categories of prompt injection threats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreatKind {
    /// Attempts to override the system prompt (e.g., "ignore previous instructions").
    SystemOverride,
    /// Role confusion attacks (e.g., "System: you are now...").
    RoleConfusion,
    /// Injection of tool-call JSON/XML to trick the agent into executing tools.
    ToolCallInjection,
    /// Attempts to extract system prompt or secrets.
    SecretExtraction,
    /// Generic instruction injection ("you must", "always respond with").
    InstructionInjection,
}

impl ThreatKind {
    fn label(&self) -> &'static str {
        match self {
            Self::SystemOverride => "system-override",
            Self::RoleConfusion => "role-confusion",
            Self::ToolCallInjection => "tool-call-injection",
            Self::SecretExtraction => "secret-extraction",
            Self::InstructionInjection => "instruction-injection",
        }
    }
}

/// Severity of a detected threat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Informational — log only.
    Low,
    /// Suspicious — sanitize the content.
    Medium,
    /// Likely injection — sanitize and warn.
    High,
}

/// A single detected threat in scanned text.
#[derive(Debug, Clone)]
pub struct Threat {
    pub kind: ThreatKind,
    pub severity: Severity,
    /// Byte range of the match in the scanned text.
    pub span: Range<usize>,
    /// Short description of what was detected.
    pub description: String,
}

/// Result of scanning text for injection threats.
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub threats: Vec<Threat>,
}

impl ScanResult {
    /// Returns `true` if no threats were detected.
    pub fn is_clean(&self) -> bool {
        self.threats.is_empty()
    }

    /// Highest severity among all detected threats.
    pub fn max_severity(&self) -> Option<Severity> {
        self.threats.iter().map(|t| t.severity).max()
    }
}

// ---------------------------------------------------------------------------
// Detection patterns
// ---------------------------------------------------------------------------

struct PatternDef {
    regex: LazyLock<Regex>,
    kind: ThreatKind,
    severity: Severity,
    description: &'static str,
}

macro_rules! pattern {
    ($re:expr, $kind:expr, $sev:expr, $desc:expr) => {
        PatternDef {
            regex: LazyLock::new(|| Regex::new($re).unwrap()),
            kind: $kind,
            severity: $sev,
            description: $desc,
        }
    };
}

static PATTERNS: LazyLock<Vec<PatternDef>> = LazyLock::new(|| {
    vec![
        // System override attempts
        pattern!(
            r"(?i)(?:ignore|forget|disregard|override)\s+(?:all\s+|your\s+|the\s+)?(?:previous|prior|above|earlier|my|your|the)\s+(?:instructions?|prompts?|rules?|directives?|guidelines?|constraints?)",
            ThreatKind::SystemOverride,
            Severity::High,
            "attempt to override system instructions"
        ),
        pattern!(
            r"(?i)(?:new|updated|revised|real|actual|true)\s+(?:system\s+)?(?:instructions?|prompt|directives?|rules?)(?:\s*:|(?:\s+are))",
            ThreatKind::SystemOverride,
            Severity::High,
            "fake system instruction injection"
        ),
        // Role confusion
        pattern!(
            r"(?i)^(?:system|assistant|admin|root)\s*:\s*.{10,}",
            ThreatKind::RoleConfusion,
            Severity::High,
            "role impersonation prefix"
        ),
        pattern!(
            r"(?i)you\s+are\s+now\s+(?:a\s+)?(?:different|new|unrestricted|jailbroken|DAN|evil)",
            ThreatKind::RoleConfusion,
            Severity::High,
            "identity reassignment attempt"
        ),
        // Tool call injection
        pattern!(
            r#"\{\s*"(?:name|function|tool_name)"\s*:\s*"[^"]+"\s*,\s*"(?:arguments|parameters|input)"\s*:"#,
            ThreatKind::ToolCallInjection,
            Severity::High,
            "JSON tool call injection"
        ),
        pattern!(
            r"(?i)<(?:tool_call|function_call|invoke)\s*>",
            ThreatKind::ToolCallInjection,
            Severity::Medium,
            "XML tool call injection tag"
        ),
        // Secret extraction
        pattern!(
            r"(?i)(?:print|show|reveal|display|repeat|echo|tell\s+me)\s+(?:the\s+|me\s+the\s+|your\s+)?(?:entire\s+)?(?:system\s+)?(?:prompt|instructions?|rules?|secret|api[\s_-]*key|password|token|credentials?)",
            ThreatKind::SecretExtraction,
            Severity::Medium,
            "attempt to extract system prompt or secrets"
        ),
        // "output" as extraction verb requires explicit article/possessive to avoid
        // false positives on technical phrases like "max output tokens".
        pattern!(
            r"(?i)output\s+(?:the|me|your|my)\s+(?:entire\s+)?(?:system\s+)?(?:prompt|instructions?|rules?|secret|api[\s_-]*key|password|token|credentials?)",
            ThreatKind::SecretExtraction,
            Severity::Medium,
            "attempt to extract system prompt or secrets"
        ),
        pattern!(
            r"(?i)what\s+(?:is|are)\s+your\s+(?:system\s+)?(?:prompt|instructions?|rules?|secret|directives?)",
            ThreatKind::SecretExtraction,
            Severity::Low,
            "inquiry about system instructions"
        ),
        // Generic instruction injection
        pattern!(
            r"(?i)(?:from\s+now\s+on|henceforth|going\s+forward),?\s+(?:you\s+)?(?:must|should|will|shall|always|never)",
            ThreatKind::InstructionInjection,
            Severity::Medium,
            "persistent instruction injection"
        ),
        pattern!(
            r"(?i)\[(?:system|INST|SYS)\]",
            ThreatKind::InstructionInjection,
            Severity::Medium,
            "bracketed system marker injection"
        ),
    ]
});

/// Scan text for prompt injection patterns.
pub fn scan(text: &str) -> ScanResult {
    let mut threats = Vec::new();

    for pat in PATTERNS.iter() {
        for m in pat.regex.find_iter(text) {
            threats.push(Threat {
                kind: pat.kind,
                severity: pat.severity,
                span: m.range(),
                description: pat.description.to_string(),
            });
        }
    }

    // Sort by position for stable output.
    threats.sort_by_key(|t| t.span.start);

    ScanResult { threats }
}

/// Defang detected injection patterns in text by wrapping them in markers
/// that make them inert to the LLM while preserving readability.
pub fn sanitize_injection(text: &str) -> String {
    let result = scan(text);
    if result.is_clean() {
        return text.to_string();
    }

    // Log detected threats.
    for threat in &result.threats {
        let snippet = &text[threat.span.clone()];
        let preview = if snippet.len() > 80 {
            format!("{}...", &snippet[..80])
        } else {
            snippet.to_string()
        };
        match threat.severity {
            Severity::High => {
                warn!(
                    kind = threat.kind.label(),
                    severity = "high",
                    "prompt injection detected: {} — \"{}\"",
                    threat.description,
                    preview,
                );
            }
            Severity::Medium => {
                warn!(
                    kind = threat.kind.label(),
                    severity = "medium",
                    "prompt injection detected: {} — \"{}\"",
                    threat.description,
                    preview,
                );
            }
            Severity::Low => {
                tracing::debug!(
                    kind = threat.kind.label(),
                    severity = "low",
                    "possible prompt injection: {} — \"{}\"",
                    threat.description,
                    preview,
                );
            }
        }
    }

    // Only sanitize Medium+ severity threats. Build output by replacing
    // matched spans with defanged versions, processing in reverse order
    // to preserve byte offsets.
    let mut output = text.to_string();
    let sanitizable: Vec<_> = result
        .threats
        .iter()
        .filter(|t| t.severity >= Severity::Medium)
        .collect();

    // Process in reverse order so earlier replacements don't shift later offsets.
    for threat in sanitizable.iter().rev() {
        let matched = &text[threat.span.clone()];
        let defanged = format!("[injection-blocked:{}]", threat.kind.label());
        // Only replace if the span is still in bounds (overlapping threats).
        if threat.span.end <= output.len() {
            output.replace_range(threat.span.clone(), &defanged);
        } else {
            // Span shifted due to earlier replacement; search for the matched
            // text starting near the original position rather than using
            // replacen(_, _, 1) which always searches from the beginning and
            // could replace the wrong occurrence.
            let search_start = threat.span.start.min(output.len());
            if let Some(rel) = output[search_start..].find(matched) {
                let abs_start = search_start + rel;
                output.replace_range(abs_start..abs_start + matched.len(), &defanged);
            } else if let Some(abs_start) = output.find(matched) {
                // Last resort: search from the beginning.
                output.replace_range(abs_start..abs_start + matched.len(), &defanged);
            }
        }
    }

    output
}
