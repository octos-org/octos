//! Reachable PDF extraction and dependency regression coverage for #2898.

#[test]
fn pdf_parser_lock_excludes_versions_before_the_recursion_limit_fix() {
    let lock: toml::Value = toml::from_str(include_str!("../../../Cargo.lock")).unwrap();
    let parsers: Vec<_> = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|package| package["name"].as_str() == Some("lopdf"))
        .collect();
    assert!(
        !parsers.is_empty(),
        "verify the PDF parser dependency graph"
    );
    for parser in parsers {
        let version = parser["version"].as_str().unwrap();
        let mut parts = version.split('.');
        let major: u64 = parts.next().unwrap().parse().unwrap();
        let minor: u64 = parts.next().unwrap().parse().unwrap();
        assert!(
            (major, minor) >= (0, 42),
            "lopdf {version} predates the RUSTSEC-2026-0187 recursion-limit fix"
        );
    }
}

#[tokio::test]
async fn read_file_extracts_text_from_a_valid_pdf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("text.pdf");
    std::fs::write(&path, include_bytes!("fixtures/parser-text.pdf")).unwrap();
    let text = octos_agent::tools::read_no_follow(&path).await.unwrap();
    assert!(text.contains("Octos parser regression"), "{text:?}");
}

#[tokio::test]
async fn read_file_returns_a_controlled_error_for_a_malformed_pdf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("malformed.pdf");
    std::fs::write(&path, b"%PDF-1.4\nnot a PDF object table").unwrap();
    let error = octos_agent::tools::read_no_follow(&path).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("pdf extraction failed"));
}
