use super::*;
use crate::user_store::UserRole;

async fn fixture(key: &SessionKey) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let mut sessions = octos_bus::SessionManager::open(dir.path()).unwrap();
    sessions
        .add_message(key, Message::user("keep this conversation"))
        .await
        .unwrap();
    let state = Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(sessions))),
        ..AppState::empty_for_tests()
    });
    (dir, state)
}

async fn rename(
    state: &Arc<AppState>,
    key: &SessionKey,
    identity: Option<AuthIdentity>,
) -> Response {
    update_session_title(
        State(state.clone()),
        HeaderMap::new(),
        identity.map(Extension),
        axum::extract::Path(key.0.clone()),
        Json(UpdateTitleRequest {
            title: "新的标题".to_owned(),
        }),
    )
    .await
}

async fn delete(
    state: &Arc<AppState>,
    key: &SessionKey,
    identity: Option<AuthIdentity>,
) -> Response {
    delete_session(
        State(state.clone()),
        HeaderMap::new(),
        identity.map(Extension),
        axum::extract::Path(key.0.clone()),
    )
    .await
}

#[tokio::test]
async fn session_mutation_canonical_key_roundtrips_without_touching_prefixed_alias() {
    let key = SessionKey::with_profile_topic("coding", "api", "web-mutation", "review");
    let (dir, state) = fixture(&key).await;
    let alias = SessionKey::with_profile("_main", "api", &key.0);
    state
        .sessions
        .as_ref()
        .unwrap()
        .lock()
        .await
        .add_message(&alias, Message::user("wrong store candidate"))
        .await
        .unwrap();
    assert_eq!(
        rename(&state, &key, Some(AuthIdentity::Admin))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let fresh = octos_bus::SessionManager::open(dir.path()).unwrap();
    assert_eq!(
        fresh.load(&key).await.unwrap().title.as_deref(),
        Some("新的标题")
    );
    assert_ne!(
        fresh.load(&alias).await.unwrap().title.as_deref(),
        Some("新的标题")
    );

    assert_eq!(
        delete(&state, &key, Some(AuthIdentity::Admin))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let fresh = octos_bus::SessionManager::open(dir.path()).unwrap();
    assert!(fresh.load(&key).await.is_none());
    assert!(fresh.load(&alias).await.is_some());
    // Deleting the same authoritative key again remains idempotent.
    assert_eq!(
        delete(&state, &key, Some(AuthIdentity::Admin))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn session_mutation_enforces_canonical_profile_ownership() {
    let key = SessionKey::with_profile("coding", "api", "web-owner");
    let (dir, state) = fixture(&key).await;
    let other = AuthIdentity::User {
        id: "other".to_owned(),
        role: UserRole::User,
    };
    for identity in [Some(other), None] {
        assert_eq!(
            rename(&state, &key, identity.clone()).await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            delete(&state, &key, identity).await.status(),
            StatusCode::FORBIDDEN
        );
        assert!(
            octos_bus::SessionManager::open(dir.path())
                .unwrap()
                .load(&key)
                .await
                .is_some()
        );
    }
    let owner = AuthIdentity::User {
        id: "coding".to_owned(),
        role: UserRole::User,
    };
    assert_eq!(
        rename(&state, &key, Some(owner.clone())).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        delete(&state, &key, Some(owner)).await.status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn session_mutation_does_not_expand_api_access_to_other_channels() {
    for channel in ["telegram", "cli", "local"] {
        let key = SessionKey::with_profile("coding", channel, "web-other-channel");
        let (dir, state) = fixture(&key).await;
        assert_eq!(
            rename(&state, &key, Some(AuthIdentity::Admin))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            delete(&state, &key, Some(AuthIdentity::Admin))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert!(
            octos_bus::SessionManager::open(dir.path())
                .unwrap()
                .load(&key)
                .await
                .is_some()
        );
    }
}

#[tokio::test]
async fn session_mutation_unknown_canonical_rename_is_not_success() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(dir.path()).unwrap(),
        ))),
        ..AppState::empty_for_tests()
    });
    let key = SessionKey::with_profile("coding", "api", "web-missing");
    assert_eq!(
        rename(&state, &key, Some(AuthIdentity::Admin))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

fn find_jsonl(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_jsonl(&path) {
                return Some(found);
            }
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            return Some(path);
        }
    }
    None
}

#[tokio::test]
async fn session_mutation_storage_failure_is_reported_and_retryable() {
    for deleting in [false, true] {
        // A read-only parent permits loading the title but rejects its atomic
        // rewrite; replacing the JSONL with a directory would test a missing
        // readable session instead of a failed title write.
        #[cfg(not(unix))]
        if !deleting {
            continue;
        }
        let key = SessionKey::with_profile("coding", "api", "web-io-error");
        let (dir, state) = fixture(&key).await;
        let file = find_jsonl(dir.path()).expect("seeded session JSONL");
        let backup = dir.path().join("saved-jsonl");
        #[cfg(unix)]
        let original_permissions = std::fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions();
        if deleting {
            std::fs::rename(&file, &backup).unwrap();
            std::fs::create_dir(&file).unwrap();
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    file.parent().unwrap(),
                    std::fs::Permissions::from_mode(0o500),
                )
                .unwrap();
                // Privileged runners can bypass directory permissions.
                let probe = file.parent().unwrap().join("write-probe");
                if std::fs::write(&probe, b"").is_ok() {
                    std::fs::remove_file(probe).unwrap();
                    std::fs::set_permissions(file.parent().unwrap(), original_permissions).unwrap();
                    continue;
                }
            }
        }
        let response = if deleting {
            delete(&state, &key, Some(AuthIdentity::Admin)).await
        } else {
            rename(&state, &key, Some(AuthIdentity::Admin)).await
        };
        // Restore permissions even if the assertion fails, so TempDir cleanup
        // cannot leave a read-only test fixture behind.
        if deleting {
            std::fs::remove_dir(&file).unwrap();
            std::fs::rename(&backup, &file).unwrap();
        } else {
            #[cfg(unix)]
            std::fs::set_permissions(file.parent().unwrap(), original_permissions).unwrap();
        }
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let response = if deleting {
            delete(&state, &key, Some(AuthIdentity::Admin)).await
        } else {
            rename(&state, &key, Some(AuthIdentity::Admin)).await
        };
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let loaded = octos_bus::SessionManager::open(dir.path())
            .unwrap()
            .load(&key)
            .await;
        if deleting {
            assert!(loaded.is_none());
        } else {
            assert_eq!(loaded.unwrap().title.as_deref(), Some("新的标题"));
        }
    }
}
