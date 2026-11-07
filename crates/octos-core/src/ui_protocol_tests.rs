// #3069 intentionally retains cross-artifact protocol drift guards, not
// per-field serde shape tests: spec §10 numeric errors and §14 executable
// examples must stay in lockstep with the public Rust wire contract.

use super::*;

// §10 ↔ `rpc_error_codes` parity: spec §10 pins every numeric error code by
// name and value, so docblocks may cite "Spec pins this at N" truthfully and
// the two sides cannot drift apart silently.

/// The §10 slice of the UI Protocol spec, cut the same fail-closed way as the
/// §6 catalog test in `octos-cli`'s `api::ui_protocol_tests`: header to the
/// next `## ` heading, erroring when either boundary moves.
fn spec_section_10_slice() -> String {
    let spec_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md"
    );
    let spec = std::fs::read_to_string(spec_path)
        .unwrap_or_else(|e| panic!("cannot read UI Protocol spec at {spec_path}: {e}"));
    let start = spec
        .find("## 10. Error Model")
        .expect("UI Protocol spec is missing `## 10. Error Model`");
    let rest = &spec[start..];
    let end = rest
        .find("\n## ")
        .expect("UI Protocol spec §10 must be followed by a `## ` section heading");
    rest[..end].to_string()
}

/// Parse the `pub const NAME: i64 = VALUE;` lines out of the
/// `rpc_error_codes` module. Reading the constants from source (instead of
/// hand-copying them here) is what makes the §10 parity test fail when a new
/// constant lands without a spec pin.
fn parse_rpc_error_code_constants() -> Vec<(String, i64)> {
    let source_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui_protocol.rs");
    let source = std::fs::read_to_string(source_path)
        .unwrap_or_else(|e| panic!("cannot read ui_protocol.rs at {source_path}: {e}"));
    let module = &source[source
        .find("pub mod rpc_error_codes")
        .expect("ui_protocol.rs is missing `pub mod rpc_error_codes`")..];
    // Bound the module at its column-0 closing brace; everything inside is
    // indented, so a bare `\n}` can only be the module's own terminator.
    let module = module
        .split("\n}")
        .next()
        .expect("rpc_error_codes module must be closed by a column-0 `}`");

    let mut constants = Vec::new();
    for line in module.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, value)) = rest.split_once(": i64 = ") else {
            panic!("rpc_error_codes constant not in `pub const NAME: i64 = VALUE;` form: {line}");
        };
        let value: i64 = value
            .trim_end_matches(';')
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("cannot parse rpc_error_codes value for {name}: {e}"));
        constants.push((name.to_string(), value));
    }
    assert!(
        constants.len() >= 20,
        "parsed only {} constants from rpc_error_codes — the §10 parity scan \
         must fail closed when the constant format changes",
        constants.len()
    );
    constants
}

/// Every `-32ddd` literal in `slice` that is not one of `pinned`. Digit
/// guards on both sides keep longer numeric runs from partially matching.
fn unpinned_numeric_codes(slice: &str, pinned: &[i64]) -> Vec<i64> {
    let bytes = slice.as_bytes();
    let mut orphans = Vec::new();
    let mut i = 0;
    while i + 6 <= bytes.len() {
        let candidate = bytes[i] == b'-'
            && bytes[i + 1] == b'3'
            && bytes[i + 2] == b'2'
            && bytes[i + 3..i + 6].iter().all(|b| b.is_ascii_digit())
            && (i == 0 || !bytes[i - 1].is_ascii_digit())
            && (i + 6 == bytes.len() || !bytes[i + 6].is_ascii_digit());
        if candidate {
            let code: i64 = slice[i..i + 6]
                .parse()
                .expect("candidate bytes validated as digits");
            if !pinned.contains(&code) && !orphans.contains(&code) {
                orphans.push(code);
            }
            i += 6;
        } else {
            i += 1;
        }
    }
    orphans
}

#[test]
fn unpinned_numeric_code_scan_flags_unknown_and_accepts_pinned_codes() {
    let pinned = [-32100i64, -32004];
    assert_eq!(
        unpinned_numeric_codes("mix `-32100` and `-32004` with orphan `-32999`", &pinned),
        vec![-32999]
    );
    assert!(unpinned_numeric_codes("no numeric codes in this slice", &pinned).is_empty());
    // A longer numeric run must not partially match a code literal.
    assert!(unpinned_numeric_codes(" `-3200004` is out of range", &pinned).is_empty());
    // A code literal glued to a preceding digit is part of that longer run,
    // and the scanner's start-of-slice / end-of-slice boundaries must hold.
    assert!(unpinned_numeric_codes("9-32999", &pinned).is_empty());
    assert_eq!(unpinned_numeric_codes("-32999", &pinned), vec![-32999]);
    assert_eq!(
        unpinned_numeric_codes("orphan -32998", &pinned),
        vec![-32998]
    );
}

#[test]
fn rpc_error_code_names_and_values_are_unique() {
    let constants = parse_rpc_error_code_constants();
    let mut names: Vec<&str> = constants.iter().map(|(name, _)| name.as_str()).collect();
    names.sort_unstable();
    let mut values: Vec<i64> = constants.iter().map(|(_, value)| *value).collect();
    values.sort_unstable();
    let duplicate_names: Vec<_> = names.windows(2).filter(|pair| pair[0] == pair[1]).collect();
    assert!(
        duplicate_names.is_empty(),
        "rpc_error_codes must not reuse a constant name: {duplicate_names:?}"
    );
    let duplicate_values: Vec<_> = values
        .windows(2)
        .filter(|pair| pair[0] == pair[1])
        .collect();
    assert!(
        duplicate_values.is_empty(),
        "rpc_error_codes must not reuse a numeric code: {duplicate_values:?}"
    );
}

#[test]
fn spec_section_10_pins_every_rpc_error_code() {
    let constants = parse_rpc_error_code_constants();
    let slice = spec_section_10_slice();

    let mut missing = Vec::new();
    for (name, value) in &constants {
        // §10 names codes in snake_case, so the constant's name is pinned in
        // its lowercase form (`UNKNOWN_SESSION` must appear as
        // `unknown_session`). Both pins must land on one line — the catalog
        // rows read `-32100` (`unknown_session`) — so swapping the number
        // between two rows cannot satisfy the name and value separately.
        let name_pin = format!("`{}`", name.to_lowercase());
        let value_pin = format!("`{value}`");
        if !slice
            .lines()
            .any(|line| line.contains(&name_pin) && line.contains(&value_pin))
        {
            missing.push(format!("{value_pin} ({name_pin})"));
        }
    }
    assert!(
        missing.is_empty(),
        "spec §10 is missing pins that rpc_error_codes declares: {missing:?} — \
         every constant must be pinned in §10 by name and value on one line"
    );
}

#[test]
fn spec_section_10_carries_no_numeric_code_outside_rpc_error_codes() {
    let constants = parse_rpc_error_code_constants();
    let pinned: Vec<i64> = constants.iter().map(|(_, value)| *value).collect();
    let orphans = unpinned_numeric_codes(&spec_section_10_slice(), &pinned);
    assert!(
        orphans.is_empty(),
        "spec §10 carries numeric codes that rpc_error_codes does not declare: {orphans:?}"
    );
}

// Treat the published v2 JSON examples as executable wire fixtures. Round trips
// catch forgotten required fields AND examples whose fields serde would ignore.
#[test]
fn spec_v2_projection_examples_match_rust_wire_types() {
    let spec = include_str!("../../../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md");
    assert_spec_v2_projection_examples_match_rust_wire_types(spec);
}

#[test]
fn spec_v2_projection_examples_accept_windows_line_endings() {
    let spec = include_str!("../../../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md")
        .replace("\r\n", "\n")
        .replace('\n', "\r\n");
    assert_spec_v2_projection_examples_match_rust_wire_types(&spec);
}

fn assert_spec_v2_projection_examples_match_rust_wire_types(spec: &str) {
    // Git can check the Markdown out with CRLF on Windows.
    let spec = spec.replace("\r\n", "\n");
    let section = spec
        .split_once("## 14. Canonical v2 Projection Envelope\n")
        .expect("canonical projection section")
        .1
        .split_once("\n## 15.")
        .expect("bounded projection section")
        .0;
    let mut tags = std::collections::BTreeSet::new();
    let mut envelope_count = 0;
    for block in section.split("```json\n").skip(1) {
        let value: Value = serde_json::from_str(block.split_once("\n```").unwrap().0)
            .expect("valid JSON in projection spec");
        let serialized = if value.get("payload").is_some() {
            envelope_count += 1;
            let envelope: EnvelopeWireV2 = serde_json::from_value(value.clone())
                .expect("spec envelope matches flattened wire DTO");
            serde_json::to_value(envelope).unwrap()
        } else {
            let payload: PayloadV2 = serde_json::from_value(value.clone())
                .expect("spec payload matches Rust tagged union");
            tags.insert(value["type"].as_str().unwrap().to_owned());
            serde_json::to_value(payload).unwrap()
        };
        assert_eq!(
            serialized, value,
            "spec example loses fields or changes defaults"
        );
    }
    assert_eq!(envelope_count, 1);
    assert_eq!(
        tags.into_iter().collect::<Vec<_>>(),
        vec![
            "assistant_delta",
            "assistant_persisted",
            "background/spawn_complete",
            "file_attached",
            "reasoning_delta",
            "tool_end",
            "tool_progress",
            "tool_start",
            "turn_terminal",
            "user_message",
        ],
        "publish every current payload tag exactly as serialized"
    );
}
