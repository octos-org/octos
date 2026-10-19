//! Owner-scoped configuration for services executed directly by a client.
//! No provider requests and no inheritance from host or parent credentials.
use super::{AppState, router::AuthIdentity};
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload, rand_core::RngCore},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
};

static MUTATION: Mutex<()> = Mutex::new(());
type Failure = (StatusCode, &'static str);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Service {
    provider: String,
    enabled: bool,
    params: BTreeMap<String, String>,
    key: String,
    revision: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    provider: String,
    enabled: bool,
    #[serde(default)]
    params: BTreeMap<String, String>,
    /// Omitted retains this capability's current key; empty explicitly removes it.
    key: Option<String>,
    expected_revision: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delete {
    expected_revision: String,
}

type Services = BTreeMap<String, Service>;
fn fail() -> Failure {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Client service storage unavailable",
    )
}
fn valid_capability(value: &str) -> bool {
    matches!(value, "asr" | "tts" | "admission")
}
fn private_dir(path: &FsPath) -> Result<(), Failure> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|_| fail())?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| fail())?;
    }
    #[cfg(not(unix))]
    return Err((
        StatusCode::SERVICE_UNAVAILABLE,
        "Client secret storage requires owner-only filesystem permissions",
    ));
    #[allow(unreachable_code)]
    Ok(())
}
fn private_file(path: &FsPath) -> Result<std::fs::File, Failure> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|_| fail())
}
struct Vault {
    root: PathBuf,
}
impl Vault {
    fn new(home: &FsPath) -> Self {
        Self {
            root: home.join("client-service-secrets"),
        }
    }
    /// Separate Octos instances can share one registry. Hold an OS lock
    /// across the revision check and atomic replacement, not just a Rust mutex.
    fn lock(&self) -> Result<std::fs::File, Failure> {
        private_dir(&self.root)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(self.root.join("store.lock"))
            .map_err(|_| fail())?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Client service storage busy; retry",
            )
        })?;
        Ok(file)
    }
    fn file(&self, owner: &str) -> PathBuf {
        self.root
            .join(format!("{:x}.enc", Sha256::digest(owner.as_bytes())))
    }
    fn cipher(&self) -> Result<ChaCha20Poly1305, Failure> {
        private_dir(&self.root)?;
        let path = self.root.join("master.key");
        if !path.exists() {
            let mut key = [0u8; 32];
            OsRng.fill_bytes(&mut key);
            let mut file = private_file(&path)?;
            file.write_all(&key).map_err(|_| fail())?;
            file.sync_all().map_err(|_| fail())?;
        }
        // Never repair permissions after reading potentially exposed material.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::symlink_metadata(&path).map_err(|_| fail())?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
                return Err(fail());
            }
        }
        let key = std::fs::read(path).map_err(|_| fail())?;
        ChaCha20Poly1305::new_from_slice(&key).map_err(|_| fail())
    }
    fn read(&self, owner: &str) -> Result<Services, Failure> {
        let path = self.file(owner);
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let encrypted = std::fs::read(path).map_err(|_| fail())?;
        if encrypted.len() < 28 {
            return Err(fail());
        }
        let plain = self
            .cipher()?
            .decrypt(
                Nonce::from_slice(&encrypted[..12]),
                Payload {
                    msg: &encrypted[12..],
                    aad: owner.as_bytes(),
                },
            )
            .map_err(|_| fail())?;
        serde_json::from_slice(&plain).map_err(|_| fail())
    }
    fn write(&self, owner: &str, value: &Services) -> Result<(), Failure> {
        let cipher = self.cipher()?;
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let plain = serde_json::to_vec(value).map_err(|_| fail())?;
        let encrypted = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &plain,
                    aad: owner.as_bytes(),
                },
            )
            .map_err(|_| fail())?;
        let temporary = self.root.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = private_file(&temporary)?;
            file.write_all(&nonce)
                .and_then(|_| file.write_all(&encrypted))
                .map_err(|_| fail())?;
            file.sync_all().map_err(|_| fail())?;
            std::fs::rename(&temporary, self.file(owner)).map_err(|_| fail())?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }
}
fn metadata(services: &Services) -> Value {
    Value::Object(
        services
            .iter()
            .map(|(id, s)| {
                (
                    id.clone(),
                    json!({
                        "provider": s.provider, "enabled": s.enabled, "params": s.params,
                        "has_credential": !s.key.is_empty(), "revision": s.revision,
                        "execution_target": "client"
                    }),
                )
            })
            .collect(),
    )
}
fn update(services: &mut Services, capability: &str, body: Update) -> Result<(), Failure> {
    if !valid_capability(capability)
        || body.provider.is_empty()
        || body.provider.len() > 80
        || !body
            .provider
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        || body.params.len() > 16
        || body.params.iter().any(|(k, v)| {
            !matches!(
                k.as_str(),
                "auth_mode" | "app_id" | "resource_id" | "voice" | "model" | "region" | "language"
            ) || v.len() > 1024
        })
        || body
            .key
            .as_ref()
            .is_some_and(|k| k.len() > 8192 || k.contains(['\r', '\n']))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid client service configuration",
        ));
    }
    let old = services.get(capability);
    if old.map(|s| s.revision.as_str()) != body.expected_revision.as_deref() {
        return Err((
            StatusCode::CONFLICT,
            "Configuration changed; reload before saving",
        ));
    }
    let key = match body.key {
        Some(key) => key.trim().to_owned(),
        None if old.is_some_and(|s| s.provider != body.provider) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "A new provider requires its own credential",
            ));
        }
        None => old.map(|s| s.key.clone()).unwrap_or_default(),
    };
    if body.enabled && key.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Enabled service requires your own credential",
        ));
    }
    services.insert(
        capability.into(),
        Service {
            provider: body.provider,
            enabled: body.enabled,
            params: body.params,
            key,
            revision: uuid::Uuid::new_v4().to_string(),
        },
    );
    Ok(())
}
fn context(
    state: &AppState,
    identity: &AuthIdentity,
    headers: &HeaderMap,
) -> Result<(Vault, String), Failure> {
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Profile storage unavailable",
    ))?;
    let owner = super::auth_handlers::resolve_my_profile_id(identity, ps, state, headers)
        .map_err(|s| (s, "Profile access denied"))?;
    // Raw owner record only; do not resolve inherited profile defaults.
    if ps.get(&owner).map_err(|_| fail())?.is_none() {
        return Err((StatusCode::NOT_FOUND, "Profile not found"));
    }
    Ok((Vault::new(ps.octos_home_dir()), owner))
}
fn response(value: Value) -> Response {
    (
        [
            ("cache-control", "no-store, max-age=0"),
            ("pragma", "no-cache"),
            ("vary", "Authorization, Cookie"),
        ],
        Json(value),
    )
        .into_response()
}
/// Metadata only. No key previews or raw secrets.
pub async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Extension(identity): Extension<AuthIdentity>,
) -> Result<Response, Failure> {
    let (vault, owner) = context(&state, &identity, &headers)?;
    let _guard = MUTATION.lock().map_err(|_| fail())?;
    let _file_guard = vault.lock()?;
    Ok(response(
        json!({"profile_id": owner, "services": metadata(&vault.read(&owner)?)}),
    ))
}
pub async fn put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Extension(identity): Extension<AuthIdentity>,
    Path(capability): Path<String>,
    Json(body): Json<Update>,
) -> Result<Response, Failure> {
    let (vault, owner) = context(&state, &identity, &headers)?;
    let _guard = MUTATION.lock().map_err(|_| fail())?;
    let _file_guard = vault.lock()?;
    let mut services = vault.read(&owner)?;
    update(&mut services, &capability, body)?;
    vault.write(&owner, &services)?;
    Ok(response(
        json!({"profile_id": owner, "services": metadata(&services)}),
    ))
}
pub async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Extension(identity): Extension<AuthIdentity>,
    Path(capability): Path<String>,
    Json(body): Json<Delete>,
) -> Result<Response, Failure> {
    let (vault, owner) = context(&state, &identity, &headers)?;
    if !valid_capability(&capability) {
        return Err((StatusCode::BAD_REQUEST, "Invalid capability"));
    }
    let _guard = MUTATION.lock().map_err(|_| fail())?;
    let _file_guard = vault.lock()?;
    let mut services = vault.read(&owner)?;
    if services.get(&capability).map(|s| s.revision.as_str())
        != Some(body.expected_revision.as_str())
    {
        return Err((
            StatusCode::CONFLICT,
            "Configuration changed; reload before deleting",
        ));
    }
    services.remove(&capability);
    vault.write(&owner, &services)?;
    Ok(response(
        json!({"profile_id": owner, "services": metadata(&services)}),
    ))
}
/// Explicit authenticated POST; only caller-owned client credentials, never LLM env_vars.
pub async fn sync(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Extension(identity): Extension<AuthIdentity>,
) -> Result<Response, Failure> {
    let (vault, owner) = context(&state, &identity, &headers)?;
    let _guard = MUTATION.lock().map_err(|_| fail())?;
    let _file_guard = vault.lock()?;
    let services = vault.read(&owner)?;
    let enabled: BTreeMap<_, _> = services.iter().filter(|(_, s)| s.enabled && !s.key.is_empty()).map(|(id, s)| (id, json!({"provider":s.provider,"enabled":true,"params":s.params,"key":s.key,"revision":s.revision,"execution_target":"client"}))).collect();
    Ok(response(json!({"profile_id": owner, "services": enabled})))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(revision: Option<String>) -> Update {
        Update {
            provider: "typesafe".into(),
            enabled: true,
            params: BTreeMap::new(),
            key: Some("synthetic-owner-key".into()),
            expected_revision: revision,
        }
    }
    #[test]
    fn client_services_encrypt_isolate_and_mask() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::new(dir.path());
        let mut services = BTreeMap::new();
        update(&mut services, "admission", input(None)).unwrap();
        vault.write("alice", &services).unwrap();
        let bytes = std::fs::read(vault.file("alice")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-owner-key"));
        assert_eq!(
            vault.read("alice").unwrap()["admission"].key,
            "synthetic-owner-key"
        );
        assert!(vault.read("bob").unwrap().is_empty());
        assert!(
            !metadata(&services)
                .to_string()
                .contains("synthetic-owner-key")
        );
        std::fs::copy(vault.file("alice"), vault.file("bob")).unwrap();
        assert!(vault.read("bob").is_err());
    }
    #[test]
    fn client_services_no_inheritance_stale_writes_or_provider_key_reuse() {
        let mut services = BTreeMap::new();
        let mut missing = input(None);
        missing.key = None;
        assert_eq!(
            update(&mut services, "admission", missing).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        update(&mut services, "admission", input(None)).unwrap();
        assert_eq!(
            update(&mut services, "admission", input(None))
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        let revision = services["admission"].revision.clone();
        let mut changed = input(Some(revision.clone()));
        changed.provider = "other".into();
        changed.key = None;
        assert!(update(&mut services, "admission", changed).is_err());
        let mut retained = input(Some(revision));
        retained.key = None;
        update(&mut services, "admission", retained).unwrap();
        assert_eq!(services["admission"].key, "synthetic-owner-key");
        assert!(update(&mut services, "../../env", input(None)).is_err());
    }
    #[tokio::test]
    async fn client_services_handlers_are_owner_scoped_and_sync_only_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
        for id in ["alice", "bob"] {
            store
                .save(&crate::profiles::UserProfile {
                    id: id.into(),
                    name: id.into(),
                    enabled: true,
                    public_subdomain: None,
                    data_dir: None,
                    parent_id: None,
                    config: Default::default(),
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                })
                .unwrap();
        }
        let state = Arc::new(AppState {
            profile_store: Some(store),
            ..AppState::empty_for_tests()
        });
        let alice = || AuthIdentity::User {
            id: "alice".into(),
            role: crate::user_store::UserRole::User,
        };
        let bob = || AuthIdentity::User {
            id: "bob".into(),
            role: crate::user_store::UserRole::User,
        };
        let mut forged = HeaderMap::new();
        forged.insert("x-profile-id", "alice".parse().unwrap());
        let put_result = put(
            State(state.clone()),
            HeaderMap::new(),
            Extension(alice()),
            Path("admission".into()),
            Json(input(None)),
        )
        .await
        .unwrap();
        assert_eq!(put_result.headers()["cache-control"], "no-store, max-age=0");
        let metadata_body = axum::body::to_bytes(put_result.into_body(), 100_000)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&metadata_body).contains("synthetic-owner-key"));
        let bob_response = sync(State(state.clone()), forged, Extension(bob()))
            .await
            .unwrap();
        let bob_body = axum::body::to_bytes(bob_response.into_body(), 100_000)
            .await
            .unwrap();
        let bob_value: Value = serde_json::from_slice(&bob_body).unwrap();
        assert_eq!(bob_value["profile_id"], "bob");
        assert_eq!(bob_value["services"], json!({}));
        let alice_response = sync(State(state.clone()), HeaderMap::new(), Extension(alice()))
            .await
            .unwrap();
        let alice_body = axum::body::to_bytes(alice_response.into_body(), 100_000)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&alice_body).unwrap();
        assert_eq!(value["services"]["admission"]["key"], "synthetic-owner-key");
        let revision = value["services"]["admission"]["revision"].as_str().unwrap();
        delete(
            State(state.clone()),
            HeaderMap::new(),
            Extension(alice()),
            Path("admission".into()),
            Json(Delete {
                expected_revision: revision.into(),
            }),
        )
        .await
        .unwrap();
        let after = sync(State(state), HeaderMap::new(), Extension(alice()))
            .await
            .unwrap();
        let after_body = axum::body::to_bytes(after.into_body(), 100_000)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&after_body).contains("synthetic-owner-key"));
    }
    #[test]
    fn client_services_lock_serializes_separate_store_instances() {
        let dir = tempfile::tempdir().unwrap();
        let first = Vault::new(dir.path());
        let second = Vault::new(dir.path());
        let guard = first.lock().unwrap();
        assert_eq!(
            second.lock().unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(guard);
        assert!(second.lock().is_ok());
    }
}
