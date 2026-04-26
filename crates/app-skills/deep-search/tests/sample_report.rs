// Integration test that dumps a sample synthesized report to disk for
// manual inspection. Run with:
//
//   cargo test -p deep-search --test sample_report dump_sample_synthesized_report -- --nocapture
//
// The resulting file lands in `target/sample_report.md`. Used by the W3
// PR to demonstrate the C1 user-facing change.

#[test]
#[ignore = "demo-only: run with --ignored to dump a sample report"]
fn dump_sample_synthesized_report() {
    // We can't import private items from the deep-search bin, so this
    // test reproduces the structural shape `build_report` emits using
    // the same heading/citation conventions. Kept in lockstep with the
    // unit test `build_report_with_synthesis_includes_synthesis_section_and_sources`.
    let report = include_str!("fixtures/sample_synthesized_report.md");
    let path = std::env::temp_dir().join("octos-w3-sample-report.md");
    std::fs::write(&path, report).unwrap();
    eprintln!("wrote sample synthesized report to: {}", path.display());
}
