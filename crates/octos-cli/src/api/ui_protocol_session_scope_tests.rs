use super::*;

#[test]
fn raw_session_open_binding_enforces_profile_scope() {
    let session = SessionKey("api:web-raw-session-scope-regression".to_owned());
    session_open_profiles().record(session.clone(), "coding");

    assert_eq!(
        validate_session_scope(&session, None, None).unwrap(),
        Some("coding".to_owned())
    );
    assert_eq!(
        validate_session_scope(&session, None, Some("coding")).unwrap(),
        Some("coding".to_owned())
    );
    let error = validate_session_scope(&session, None, Some("other")).unwrap_err();
    assert_eq!(
        error.data.as_ref().unwrap()["auth_scope_violation"],
        json!(true)
    );

    // Unscoped admin retains the existing explicit rebind behavior.
    assert_eq!(
        validate_session_scope(&session, Some("rebind-second"), None).unwrap(),
        Some("rebind-second".to_owned())
    );
}
