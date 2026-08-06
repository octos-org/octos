//! The product-agnostic diagnostic report model: [`CheckStatus`], [`Check`],
//! and [`Report`]. Ported from octoscode's `doctor.rs` but with the
//! product-specific JSON keys (the binary's own version/schema) supplied by the
//! caller via [`Report::to_json`] arguments rather than baked in from this
//! crate's `CARGO_PKG_VERSION`.

use serde_json::{Value, json};

/// Pass / warn / fail per check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl CheckStatus {
    /// The flutter-doctor-style glyph rendered at the start of each line.
    pub fn glyph(self) -> &'static str {
        match self {
            CheckStatus::Pass => "[✓]",
            CheckStatus::Warn => "[!]",
            CheckStatus::Fail => "[✗]",
        }
    }

    /// Stable lower-case identifier for `--json` output.
    pub fn json_str(self) -> &'static str {
        match self {
            CheckStatus::Pass => "pass",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
        }
    }
}

/// A single diagnostic line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Group heading the line is rendered under.
    pub category: &'static str,
    /// Short check name (e.g. `octos on PATH`).
    pub name: String,
    pub status: CheckStatus,
    /// One-line detail shown after the name.
    pub detail: String,
    /// Actionable fix, rendered as a `→ fix:` line. `None` for passing checks.
    pub fix: Option<String>,
    /// Optional resolved value (path/version) shown in `--verbose` and JSON.
    pub value: Option<String>,
}

impl Check {
    /// Construct a passing check (no fix line).
    pub fn pass(
        category: &'static str,
        name: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Pass,
            detail: detail.into(),
            fix: None,
            value: None,
        }
    }

    /// Construct a warning check (soft problem; exit 0 unless `--strict`).
    pub fn warn(
        category: &'static str,
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Warn,
            detail: detail.into(),
            fix: Some(fix.into()),
            value: None,
        }
    }

    /// Construct a failing check (hard problem; exit 1).
    pub fn fail(
        category: &'static str,
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
            value: None,
        }
    }

    /// Attach a resolved value (path/version) for `--verbose`/JSON.
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }
}

/// Aggregated diagnostic report.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn new(checks: Vec<Check>) -> Self {
        Self { checks }
    }

    /// Append a single check (builder-ish convenience for callers assembling a
    /// report incrementally).
    pub fn push(&mut self, check: Check) {
        self.checks.push(check);
    }

    /// Append a batch of checks.
    pub fn extend(&mut self, checks: impl IntoIterator<Item = Check>) {
        self.checks.extend(checks);
    }

    /// `(passed, warnings, failures)`.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut pass = 0;
        let mut warn = 0;
        let mut fail = 0;
        for c in &self.checks {
            match c.status {
                CheckStatus::Pass => pass += 1,
                CheckStatus::Warn => warn += 1,
                CheckStatus::Fail => fail += 1,
            }
        }
        (pass, warn, fail)
    }

    /// Exit code: `1` on any failure, or (with `strict`) any warning.
    pub fn exit_code(&self, strict: bool) -> i32 {
        let (_, warn, fail) = self.counts();
        if fail > 0 || (strict && warn > 0) {
            1
        } else {
            0
        }
    }

    /// Render the flutter-doctor-style human report to a string.
    pub fn render(&self, verbose: bool, strict: bool) -> String {
        let mut out = String::new();
        let mut last_category: Option<&str> = None;
        for check in &self.checks {
            if last_category != Some(check.category) {
                if last_category.is_some() {
                    out.push('\n');
                }
                out.push_str(check.category);
                out.push('\n');
                last_category = Some(check.category);
            }
            out.push_str(check.status.glyph());
            out.push(' ');
            out.push_str(&check.name);
            if !check.detail.is_empty() {
                out.push_str(" — ");
                out.push_str(&check.detail);
            }
            if verbose {
                if let Some(value) = &check.value {
                    out.push_str(" (");
                    out.push_str(value);
                    out.push(')');
                }
            }
            out.push('\n');
            if let Some(fix) = &check.fix {
                out.push_str("    → fix: ");
                out.push_str(fix);
                out.push('\n');
            }
        }

        let (pass, warn, fail) = self.counts();
        out.push('\n');
        if fail == 0 && (warn == 0 || !strict) {
            out.push_str(&format!(
                "• Doctor summary: {pass} passed, {warn} warning(s). No fatal issues found."
            ));
        } else {
            out.push_str(&format!(
                "• Doctor summary: {pass} passed, {warn} warning(s), {fail} failure(s)."
            ));
        }
        out.push('\n');
        out
    }

    /// Render the support-bundle JSON.
    ///
    /// `product` is the binary name (e.g. `octos`), `version` is the caller's
    /// own `CARGO_PKG_VERSION` (passed IN — never this crate's), and
    /// `protocol` / `schema_version` describe the compiled-in UI protocol so
    /// the bundle is self-describing for skew triage.
    pub fn to_json(
        &self,
        strict: bool,
        product: &str,
        version: &str,
        protocol: &str,
        schema_version: u32,
    ) -> Value {
        let (pass, warn, fail) = self.counts();
        let checks: Vec<_> = self
            .checks
            .iter()
            .map(|c| {
                json!({
                    "category": c.category,
                    "name": c.name,
                    "status": c.status.json_str(),
                    "detail": c.detail,
                    "fix": c.fix,
                    "value": c.value,
                })
            })
            .collect();
        json!({
            "checks": checks,
            "summary": {
                "passed": pass,
                "warnings": warn,
                "failures": fail,
            },
            "exit_code": self.exit_code(strict),
            "product": product,
            "version": version,
            "protocol": protocol,
            "schema_version": schema_version,
            "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_groups_by_category_and_shows_fix_lines() {
        let checks = vec![
            Check::pass("Cat A", "ok thing", "all good"),
            Check::warn("Cat A", "warny thing", "soft problem", "do the fix"),
            Check::fail("Cat B", "broken thing", "hard problem", "fix me"),
        ];
        let report = Report::new(checks);
        let text = report.render(false, false);
        assert!(text.contains("Cat A\n"));
        assert!(text.contains("Cat B\n"));
        assert!(text.contains("[✓] ok thing"));
        assert!(text.contains("[!] warny thing"));
        assert!(text.contains("[✗] broken thing"));
        assert!(text.contains("    → fix: do the fix"));
        assert!(text.contains("    → fix: fix me"));
        assert!(!text.contains("→ fix: \n"));
        assert!(text.contains("1 passed, 1 warning(s), 1 failure(s)"));
    }

    #[test]
    fn exit_code_is_one_on_failure_zero_on_warnings() {
        let warn_only = Report::new(vec![Check::warn("c", "n", "d", "f")]);
        assert_eq!(warn_only.exit_code(false), 0);
        assert_eq!(warn_only.exit_code(true), 1); // strict promotes warnings

        let with_fail = Report::new(vec![Check::fail("c", "n", "d", "f")]);
        assert_eq!(with_fail.exit_code(false), 1);
    }

    #[test]
    fn json_carries_caller_version_not_crate_version() {
        let report = Report::new(vec![Check::pass("c", "n", "d")]);
        // The caller passes in its own version; the bundle must echo exactly
        // that, never octos-diagnostics' own CARGO_PKG_VERSION.
        let json = report.to_json(false, "octos", "9.9.9-caller", "octos-ui/v1alpha1", 1);
        assert_eq!(json["summary"]["passed"], 1);
        assert_eq!(json["product"], "octos");
        assert_eq!(json["version"], "9.9.9-caller");
        assert_eq!(json["protocol"], "octos-ui/v1alpha1");
        assert_eq!(json["schema_version"], 1);
        assert!(json["checks"].is_array());
    }

    #[test]
    fn verbose_appends_value_in_parens() {
        let report = Report::new(vec![
            Check::pass("Cat", "thing", "detail").with_value("/usr/local/bin/octos"),
        ]);
        let plain = report.render(false, false);
        let verbose = report.render(true, false);
        assert!(!plain.contains("(/usr/local/bin/octos)"));
        assert!(verbose.contains("(/usr/local/bin/octos)"));
    }
}
