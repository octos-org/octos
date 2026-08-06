//! Generic, product-agnostic local checks: terminal environment, config/data
//! directory writability, and the protocol-skew **adapter** over the pure
//! `octos_core::ui_protocol` comparator.
//!
//! No network here (Stage 1). The terminal checks are ported from octoscode's
//! `doctor.rs`; the writability checks take their directories as parameters so
//! callers (octos-cli) resolve the real `~/.config/octos` + `~/.octos`.

use std::path::Path;

use octos_core::ui_protocol::{
    ProtocolCompat, UI_PROTOCOL_SCHEMA_VERSION, UI_PROTOCOL_V1, UiProtocolCapabilities,
    compare_protocol,
};

use crate::report::Check;

const CAT_TERM: &str = "Terminal environment";
const CAT_CONFIG: &str = "Config & data";
const CAT_BACKEND: &str = "Backend";

// ---------------------------------------------------------------------------
// Terminal environment
// ---------------------------------------------------------------------------

/// All terminal-environment checks, reading the live `TERM`/locale/`COLORTERM`
/// env vars. Returns `[TERM, UTF-8 locale, CJK width, color support]`.
pub fn terminal_checks() -> Vec<Check> {
    let term = std::env::var("TERM").ok();
    let lang = std::env::var("LANG").ok();
    let lc_all = std::env::var("LC_ALL").ok();
    let lc_ctype = std::env::var("LC_CTYPE").ok();
    let colorterm = std::env::var("COLORTERM").ok();
    vec![
        term_check(term.as_deref()),
        locale_check(lang.as_deref(), lc_all.as_deref(), lc_ctype.as_deref()),
        cjk_check(),
        color_check(term.as_deref(), colorterm.as_deref()),
    ]
}

/// Whether a `TERM` value has a loadable terminfo entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminfoProbe {
    /// `infocmp` confirmed the terminfo entry loads.
    Found,
    /// `infocmp` ran but reported the entry is missing (non-zero exit).
    Missing,
    /// `infocmp` itself isn't available — we can't probe, so don't hard-fail.
    ProberAbsent,
}

fn term_check(term: Option<&str>) -> Check {
    term_check_with(term, probe_terminfo)
}

fn term_check_with(term: Option<&str>, probe: impl Fn(&str) -> TerminfoProbe) -> Check {
    match term {
        Some("dumb") => Check::warn(
            CAT_TERM,
            "TERM set",
            "TERM=dumb has no terminfo capabilities",
            "export TERM=xterm-256color",
        ),
        Some(t) if !t.is_empty() => match probe(t) {
            TerminfoProbe::Found | TerminfoProbe::ProberAbsent => {
                Check::pass(CAT_TERM, "TERM set", t.to_string()).with_value(t.to_string())
            }
            TerminfoProbe::Missing => Check::warn(
                CAT_TERM,
                "TERM set",
                format!("TERM=`{t}` has no terminfo entry (the TUI will report 'can't find terminfo database')"),
                "set TERM=xterm-256color or install the terminfo package for your terminal",
            )
            .with_value(t.to_string()),
        },
        _ => Check::warn(
            CAT_TERM,
            "TERM set",
            "TERM is unset; the TUI may not render or may report 'can't find terminfo database'",
            "export TERM=xterm-256color",
        ),
    }
}

/// Probe whether `term`'s terminfo entry is loadable via `infocmp`.
fn probe_terminfo(term: &str) -> TerminfoProbe {
    match std::process::Command::new("infocmp")
        .arg("-1")
        .arg(term)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => TerminfoProbe::Found,
        Ok(_) => TerminfoProbe::Missing,
        Err(_) => TerminfoProbe::ProberAbsent,
    }
}

fn locale_check(lang: Option<&str>, lc_all: Option<&str>, lc_ctype: Option<&str>) -> Check {
    let effective = lc_all.or(lc_ctype).or(lang);
    match effective {
        Some(v)
            if v.to_ascii_uppercase().contains("UTF-8")
                || v.to_ascii_uppercase().contains("UTF8") =>
        {
            Check::pass(CAT_TERM, "UTF-8 locale", v.to_string()).with_value(v.to_string())
        }
        Some(v) => Check::warn(
            CAT_TERM,
            "UTF-8 locale",
            format!("locale `{v}` is not UTF-8; box-drawing and CJK may break"),
            "export LANG=en_US.UTF-8 (or your locale with .UTF-8)",
        ),
        None => Check::warn(
            CAT_TERM,
            "UTF-8 locale",
            "no LANG/LC_ALL/LC_CTYPE set",
            "export LANG=en_US.UTF-8",
        ),
    }
}

fn cjk_check() -> Check {
    Check::pass(
        CAT_TERM,
        "CJK width",
        "uses unicode-width for double-width glyphs (also depends on terminal font)",
    )
}

fn color_check(term: Option<&str>, colorterm: Option<&str>) -> Check {
    let truecolor = colorterm
        .map(|c| c.contains("truecolor") || c.contains("24bit"))
        .unwrap_or(false);
    let has_256 = term.map(|t| t.contains("256color")).unwrap_or(false);
    if truecolor {
        Check::pass(CAT_TERM, "color support", "truecolor (24-bit)")
    } else if has_256 {
        Check::pass(CAT_TERM, "color support", "256-color")
    } else {
        Check::warn(
            CAT_TERM,
            "color support",
            "no truecolor/256-color advertised; themes may look flat",
            "use a 256-color terminal and set TERM=xterm-256color (COLORTERM=truecolor)",
        )
    }
}

// ---------------------------------------------------------------------------
// Config & data directory writability
// ---------------------------------------------------------------------------

/// Writability check for a config dir, named appropriately.
pub fn config_writability_check(dir: &Path) -> Check {
    writability_check("config dir", dir)
}

/// Writability check for a data dir, named appropriately.
pub fn data_writability_check(dir: &Path) -> Check {
    writability_check("data dir", dir)
}

/// Check that a directory exists and is writable (or creatable). A missing dir
/// that can be created is a `[!]` warn with a `mkdir -p` fix; a path that exists
/// but isn't a directory is a `[✗]` failure (the `mkdir -p` hint would fail).
pub fn writability_check(name: &'static str, dir: &Path) -> Check {
    if dir.is_dir() {
        if is_writable(dir) {
            Check::pass(CAT_CONFIG, name, "present and writable")
                .with_value(dir.display().to_string())
        } else {
            Check::fail(
                CAT_CONFIG,
                name,
                format!("{} is not writable", dir.display()),
                format!("chmod u+w {}", dir.display()),
            )
            .with_value(dir.display().to_string())
        }
    } else if dir.exists() {
        Check::fail(
            CAT_CONFIG,
            name,
            format!("{} exists but is not a directory", dir.display()),
            format!(
                "remove the file at {} or point the dir elsewhere",
                dir.display()
            ),
        )
        .with_value(dir.display().to_string())
    } else {
        Check::warn(
            CAT_CONFIG,
            name,
            format!("{} does not exist yet", dir.display()),
            format!("mkdir -p {}", dir.display()),
        )
        .with_value(dir.display().to_string())
    }
}

fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".octos-doctor-write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Protocol skew (adapter over the pure octos-core comparator)
// ---------------------------------------------------------------------------

/// Adapter: run the pure [`compare_protocol`] comparator and turn its
/// [`ProtocolCompat`] result into a [`Check`]. `server` is the advertised (or
/// compiled-in) capabilities; `required` is the caller's required-feature set.
///
/// - [`ProtocolCompat::Compatible`] → `[✓]`.
/// - [`ProtocolCompat::MissingFeatures`] → `[!]` (degraded; upgrade the server).
/// - [`ProtocolCompat::SchemaIncompatible`] → `[✗]` (cannot interoperate).
pub fn protocol_skew_check<'a, I>(server: &UiProtocolCapabilities, required: I) -> Check
where
    I: IntoIterator<Item = &'a str>,
{
    match compare_protocol(server, required) {
        ProtocolCompat::Compatible => Check::pass(
            CAT_BACKEND,
            "protocol skew",
            format!(
                "compatible ({UI_PROTOCOL_V1} schema v{}, all required features present)",
                server.version.schema_version
            ),
        )
        .with_value(format!(
            "{UI_PROTOCOL_V1} schema v{UI_PROTOCOL_SCHEMA_VERSION}"
        )),
        ProtocolCompat::MissingFeatures(missing) => Check::warn(
            CAT_BACKEND,
            "protocol skew",
            format!(
                "server is missing required features: {}",
                missing.join(", ")
            ),
            "upgrade the octos server to advertise these features, or expect degraded behavior",
        ),
        ProtocolCompat::SchemaIncompatible { server, client } => Check::fail(
            CAT_BACKEND,
            "protocol skew",
            format!("server schema v{server} is incompatible with the client's v{client}"),
            "upgrade whichever side is on the wrong protocol family/schema",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::CheckStatus;
    use octos_core::ui_protocol::{
        UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1, UI_PROTOCOL_FEATURE_USER_QUESTION_V1,
    };

    // --- terminal -----------------------------------------------------------

    #[test]
    fn term_check_warns_when_unset_or_dumb() {
        let found = |_: &str| TerminfoProbe::Found;
        assert_eq!(term_check_with(None, found).status, CheckStatus::Warn);
        assert_eq!(
            term_check_with(Some("dumb"), found).status,
            CheckStatus::Warn
        );
        assert_eq!(
            term_check_with(Some("xterm-256color"), found).status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn term_check_warns_when_terminfo_entry_missing() {
        let missing = |_: &str| TerminfoProbe::Missing;
        let check = term_check_with(Some("xterm-256color"), missing);
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("terminfo"));
    }

    #[test]
    fn term_check_passes_when_prober_absent() {
        let absent = |_: &str| TerminfoProbe::ProberAbsent;
        assert_eq!(
            term_check_with(Some("xterm-256color"), absent).status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn locale_check_requires_utf8() {
        assert_eq!(
            locale_check(Some("en_US.UTF-8"), None, None).status,
            CheckStatus::Pass
        );
        assert_eq!(
            locale_check(Some("C"), None, None).status,
            CheckStatus::Warn
        );
        assert_eq!(locale_check(None, None, None).status, CheckStatus::Warn);
        // LC_ALL overrides LANG.
        assert_eq!(
            locale_check(Some("C"), Some("en_US.UTF-8"), None).status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn color_check_recognizes_truecolor_and_256() {
        assert_eq!(
            color_check(Some("xterm"), Some("truecolor")).status,
            CheckStatus::Pass
        );
        assert_eq!(
            color_check(Some("xterm-256color"), None).status,
            CheckStatus::Pass
        );
        assert_eq!(color_check(Some("xterm"), None).status, CheckStatus::Warn);
    }

    // --- writability --------------------------------------------------------

    #[test]
    fn writability_check_passes_for_writable_tempdir() {
        let dir = std::env::temp_dir();
        assert_eq!(writability_check("tmp", &dir).status, CheckStatus::Pass);
    }

    #[test]
    fn writability_check_warns_for_missing_dir() {
        let missing = std::env::temp_dir().join("octos-diagnostics-doctor-nope-xyz-12345");
        let _ = std::fs::remove_dir_all(&missing);
        let check = writability_check("missing", &missing);
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.fix.unwrap().contains("mkdir -p"));
    }

    #[test]
    fn writability_check_fails_when_path_is_a_file() {
        let file = std::env::temp_dir().join("octos-diagnostics-datadir-as-file-98765");
        let _ = std::fs::remove_file(&file);
        std::fs::write(&file, b"not a dir").expect("create probe file");
        let check = writability_check("data dir", &file);
        let _ = std::fs::remove_file(&file);
        assert_eq!(check.status, CheckStatus::Fail);
        let fix = check.fix.unwrap();
        assert!(fix.contains("remove the file"));
        assert!(!fix.contains("mkdir -p"));
    }

    // --- protocol skew adapter ---------------------------------------------

    #[test]
    fn protocol_skew_passes_for_full_protocol() {
        let server = UiProtocolCapabilities::full_protocol();
        let check = protocol_skew_check(
            &server,
            [
                UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1,
                UI_PROTOCOL_FEATURE_USER_QUESTION_V1,
            ],
        );
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn protocol_skew_warns_when_feature_missing() {
        let mut server = UiProtocolCapabilities::full_protocol();
        server
            .supported_features
            .retain(|f| f != UI_PROTOCOL_FEATURE_USER_QUESTION_V1);
        let check = protocol_skew_check(&server, [UI_PROTOCOL_FEATURE_USER_QUESTION_V1]);
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains(UI_PROTOCOL_FEATURE_USER_QUESTION_V1));
    }

    #[test]
    fn protocol_skew_fails_on_older_schema() {
        if UI_PROTOCOL_SCHEMA_VERSION == 0 {
            return;
        }
        let mut server = UiProtocolCapabilities::full_protocol();
        server.version.schema_version = UI_PROTOCOL_SCHEMA_VERSION - 1;
        let check = protocol_skew_check(&server, [UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1]);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("incompatible"));
    }
}
