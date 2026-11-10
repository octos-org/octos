//! Bearer parsing and the constant-time primitive used by the HTTP auth gate.

use octos_agent::mcp_server::parse_bearer_token;

#[test]
fn bearer_token_parsing_and_constant_time_comparison() {
    assert_eq!(
        parse_bearer_token(Some("Bearer secret123")),
        Some("secret123".into())
    );
    // Case-insensitive scheme, collapses extra whitespace (RFC 6750 §2.1).
    assert_eq!(
        parse_bearer_token(Some("bearer  secret123")),
        Some("secret123".into())
    );
    assert_eq!(parse_bearer_token(None), None);
    assert_eq!(parse_bearer_token(Some("Basic abc")), None);
    assert_eq!(parse_bearer_token(Some("Bearer ")), None);

    // Same-length wrong content and different lengths are both rejected.
    assert!(octos_core::constant_time_eq(
        b"super-secret",
        b"super-secret"
    ));
    assert!(!octos_core::constant_time_eq(
        b"super-secret",
        b"super-secre"
    ));
    assert!(!octos_core::constant_time_eq(
        b"super-secret",
        b"wrong-secret"
    ));
}
