use octos_agent::{EffectivePermissions, PermissionProfile, RuntimeMode};

#[test]
fn dangerous_profile_is_gated_to_solo_runtime() {
    assert!(
        EffectivePermissions::for_runtime(PermissionProfile::DangerFullAccess, RuntimeMode::Solo)
            .is_ok()
    );

    for runtime_mode in [RuntimeMode::Local, RuntimeMode::Tenant, RuntimeMode::Cloud] {
        let err =
            EffectivePermissions::for_runtime(PermissionProfile::DangerFullAccess, runtime_mode)
                .expect_err("dangerous must be rejected outside solo mode");
        assert_eq!(err.runtime_mode, runtime_mode);
        assert_eq!(err.requested, PermissionProfile::DangerFullAccess);
    }
}
