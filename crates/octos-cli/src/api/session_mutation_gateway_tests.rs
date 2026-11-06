use super::*;
use octos_bus::{ApiChannel, Channel, SessionHandle, SessionManager};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct RunningGateway(tokio::task::JoinHandle<eyre::Result<()>>);

impl Drop for RunningGateway {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn real_gateway(
    state: &AppState,
    data_dir: &std::path::Path,
    deleted: Arc<AtomicUsize>,
) -> RunningGateway {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let channel = ApiChannel::new(
        port,
        Some("gateway-storage-test-token".into()),
        Arc::new(AtomicBool::new(false)),
        Arc::new(tokio::sync::Mutex::new(
            SessionManager::open(data_dir).unwrap(),
        )),
        None,
    )
    .with_on_session_deleted(move |_| {
        deleted.fetch_add(1, Ordering::SeqCst);
    });
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let server = RunningGateway(tokio::spawn(async move { channel.start(sender).await }));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            assert!(!server.0.is_finished(), "gateway exited before binding");
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("gateway listener readiness");
    register_gateway(
        state,
        MAIN_PROFILE_ID,
        port,
        Some("gateway-storage-test-token"),
    )
    .await;
    server
}

async fn gateway_storage_failure_and_retry(deleting: bool) {
    let (_local, state) = legacy_fixture(MAIN_PROFILE_ID, false).await;
    let data = tempfile::tempdir().unwrap();
    let key = SessionKey::with_profile(MAIN_PROFILE_ID, "api", "web-legacy");
    // The per-user transcript remains readable while the flat write/delete
    // target is an unexpected directory. Portable, deterministic I/O failure;
    // neither root privileges nor a synthetic HTTP response can bypass it.
    let mut handle = SessionHandle::open(data.path(), &key);
    handle
        .add_message(Message::user("gateway history survives failure"))
        .await
        .unwrap();
    let mut flat = SessionManager::open(data.path()).unwrap();
    flat.add_message(&key, Message::user("flat history"))
        .await
        .unwrap();
    let flat_file = find_jsonl(flat.sessions_dir()).unwrap();
    std::fs::remove_file(&flat_file).unwrap();
    std::fs::create_dir(&flat_file).unwrap();
    let deleted = Arc::new(AtomicUsize::new(0));
    let _gateway = real_gateway(&state, data.path(), deleted.clone()).await;

    let response = legacy_mutation(&state, deleting, AuthIdentity::Admin).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        deleted.load(Ordering::SeqCst),
        0,
        "failed delete must not stop the actor"
    );
    let fresh = SessionManager::open(data.path()).unwrap();
    let preserved = fresh.load(&key).await.unwrap();
    assert_eq!(
        preserved.messages[0].content,
        "gateway history survives failure"
    );
    assert_ne!(preserved.title.as_deref(), Some("新的标题"));

    std::fs::remove_dir(&flat_file).unwrap();
    assert_eq!(
        legacy_mutation(&state, deleting, AuthIdentity::Admin)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let fresh = SessionManager::open(data.path()).unwrap();
    if deleting {
        assert!(fresh.load(&key).await.is_none());
        assert_eq!(deleted.load(Ordering::SeqCst), 1);
        assert_eq!(
            legacy_mutation(&state, true, AuthIdentity::Admin)
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            deleted.load(Ordering::SeqCst),
            1,
            "absent delete is idempotent"
        );
    } else {
        assert_eq!(
            fresh.load(&key).await.unwrap().title.as_deref(),
            Some("新的标题")
        );
        assert_eq!(deleted.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn session_mutation_real_gateway_rename_storage_failure_is_retryable() {
    gateway_storage_failure_and_retry(false).await;
}

#[tokio::test]
async fn session_mutation_real_gateway_delete_storage_failure_is_retryable() {
    gateway_storage_failure_and_retry(true).await;
}
