//! Real Core restart coverage for chat-folder placement (T-0670): a profile
//! without pre-upgrade chat data keeps its chats in the server-level root
//! after every restart; an upgraded profile with real legacy data keeps its
//! per-profile root.
#![cfg(all(unix, feature = "api"))]

#[path = "support/serve_process.rs"]
mod serve_process;

use std::{path::Path, process::Child, time::Duration};

use futures::{SinkExt, StreamExt};
use octos_cli::profiles::{ProfileStore, UserProfile};
use serde_json::{Value, json};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

const ADMIN: &str = "chat-workspace-restart-admin";
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(data: &Path, workspace: &Path, port: u16) -> Server {
    let mut command = serve_process::serve_command(port, data);
    command
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--cwd")
        .arg(workspace)
        .env("OCTOS_AUTH_TOKEN", ADMIN)
        .env("OCTOS_NO_MODEL_DOWNLOAD", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = command.spawn().unwrap();
    assert!(serve_process::wait_for_port(port, Duration::from_secs(30)));
    Server(child)
}

async fn connect(port: u16) -> Socket {
    let mut request = format!("ws://127.0.0.1:{port}/api/ui-protocol/ws")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {ADMIN}").parse().unwrap());
    request.headers_mut().insert(
        "X-Octos-Ui-Features",
        "session.chat_workspace.v1,session.workspace_cwd.v1"
            .parse()
            .unwrap(),
    );
    connect_async(request).await.unwrap().0
}

async fn rpc(ws: &mut Socket, method: &str, params: Value) -> Value {
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":method,"method":method,"params":params})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let frame = ws.next().await.expect("socket closed").expect("frame");
            if let Message::Text(text) = frame {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["id"] == method {
                    return value;
                }
            }
        }
    })
    .await
    .expect("RPC response")
}

fn install_profile(data: &Path) -> std::path::PathBuf {
    let profile: UserProfile = serde_json::from_value(json!({
        "id": "ada",
        "name": "Ada",
        "enabled": true,
        "created_at": "2026-11-24T00:00:00Z",
        "updated_at": "2026-11-24T00:00:00Z",
        "config": {
            "llm": {"primary": {
                "family_id": "openai",
                "model_id": "offline-fixture",
                "route": {"base_url": "http://127.0.0.1:9/v1", "api_key_env": "FIXTURE_KEY"}
            }},
            "env_vars": {"FIXTURE_KEY": "offline"}
        }
    }))
    .unwrap();
    let store = ProfileStore::open_unified(data).unwrap();
    store.save(&profile).unwrap();
    store.resolve_data_dir(&profile)
}

async fn create_chat(ws: &mut Socket, chat_id: &str) -> String {
    let created = rpc(
        ws,
        "session/chat/create",
        json!({
            "session_id": format!("ada:api:{chat_id}"),
            "profile_id": "ada",
        }),
    )
    .await;
    assert!(created.get("error").is_none(), "chat {chat_id}: {created}");
    created["result"]["workspace_root"]
        .as_str()
        .unwrap_or_else(|| panic!("chat {chat_id} has no workspace_root: {created}"))
        .to_owned()
}

async fn open_refused_in_chat_folder(ws: &mut Socket, chat_root: &str) {
    let opened = rpc(
        ws,
        "session/open",
        json!({
            "session_id": "ada:api:code",
            "profile_id": "ada",
            "cwd": chat_root,
        }),
    )
    .await;
    assert_eq!(
        opened["error"]["data"]["kind"],
        json!("chat_workspace_reserved"),
        "a code session must not open inside a chat folder: {opened}"
    );
}

fn assert_private_dir(root: &str) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(root).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "{root}");
}

/// A fresh profile's chats stay in the server-level root across every
/// restart; startup registration must not mistake "the profile exists" for
/// "it has pre-upgrade chats" (F-0634-1).
// Serializes real fixed-port child processes for the whole async lifecycle.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn should_keep_new_chats_in_the_server_root_across_real_restarts() {
    let _serial = serve_process::serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let profile_data = install_profile(&data);
    let port = serve_process::find_free_port();
    // Compare canonical paths: on macOS the server reports /private/var/...
    // while the fixture TempDir is spelled /var/... (alan0x review). The
    // server root exists after the first boot; the profile data dir exists
    // from install, so canonicalize and join the tail instead.
    let legacy_root = std::fs::canonicalize(&profile_data).unwrap().join("chats");

    let mut roots = Vec::new();
    for (boot, chat_id) in ["chat-a", "chat-b", "chat-c"].iter().enumerate() {
        let server = start(&data, &workspace, port);
        let server_root =
            std::fs::canonicalize(data.join("chat-workspaces")).expect("server root exists");
        let mut ws = connect(port).await;
        let root = create_chat(&mut ws, chat_id).await;
        assert!(
            Path::new(&root).starts_with(&server_root),
            "boot {}: chat {} landed outside the server root {}: {root}",
            boot + 1,
            chat_id,
            server_root.display()
        );
        assert!(
            !Path::new(&root).starts_with(&legacy_root),
            "boot {}: chat {} landed in the legacy root: {root}",
            boot + 1,
            chat_id
        );
        assert_private_dir(&root);
        // A written artifact stays in the chat's own folder.
        std::fs::write(
            Path::new(&root).join("output.txt"),
            format!("from {chat_id}"),
        )
        .unwrap();
        // The hiding rule refuses a code session inside the chat folder.
        open_refused_in_chat_folder(&mut ws, &root).await;
        roots.push(root);
        drop(ws);
        drop(server);
    }
    // Earlier chats still resolve to their original folders after restarts.
    for (root, chat_id) in roots.iter().zip(["chat-a", "chat-b", "chat-c"]) {
        assert_eq!(
            std::fs::read_to_string(Path::new(root).join("output.txt")).unwrap(),
            format!("from {chat_id}")
        );
    }
    // The fresh profile never gained a legacy root with content.
    assert!(
        std::fs::read_dir(&legacy_root)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true),
        "fresh profile legacy root must stay empty: {}",
        legacy_root.display()
    );
    let _server = start(&data, &workspace, port);
    let mut ws = connect(port).await;
    let reopened = create_chat(&mut ws, "chat-a").await;
    assert_eq!(reopened, roots[0], "chat-a moved across restarts");
}

/// A profile with real pre-upgrade chat data keeps its per-profile legacy
/// root: old chats resolve and new chats stay beside them after restarts.
// Serializes real fixed-port child processes for the whole async lifecycle.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn should_keep_an_upgraded_profile_on_its_legacy_root_across_real_restarts() {
    let _serial = serve_process::serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let profile_data = install_profile(&data);

    // Plant a chat exactly the way a pre-upgrade server left it: the folder
    // is the sha256 digest of the base session key, with its record beside it.
    let legacy_root = profile_data.join("chats");
    let digest = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(b"ada:api:old-chat"))[..32].to_owned()
    };
    let old_folder = legacy_root.join(&digest);
    std::fs::create_dir_all(&old_folder).unwrap();
    std::fs::write(old_folder.join("old.txt"), "pre-upgrade").unwrap();
    std::fs::write(
        legacy_root.join(format!("{digest}.json")),
        serde_json::to_vec(&json!({
            "session_id": "ada:api:old-chat",
            "profile_id": "ada",
            "created_at": "2026-11-20T00:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();
    let port = serve_process::find_free_port();

    for (boot, chat_id) in ["new-chat-1", "new-chat-2"].iter().enumerate() {
        let server = start(&data, &workspace, port);
        let mut ws = connect(port).await;
        // The pre-upgrade chat resolves in place.
        let old_root = create_chat(&mut ws, "old-chat").await;
        assert_eq!(
            std::fs::canonicalize(&old_folder).unwrap(),
            std::fs::canonicalize(&old_root).unwrap(),
            "boot {}: old chat moved",
            boot + 1
        );
        // New chats of the upgraded profile stay beside the old one.
        let root = create_chat(&mut ws, chat_id).await;
        assert!(
            root.starts_with(
                std::fs::canonicalize(&legacy_root)
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            ),
            "boot {}: upgraded profile chat left the legacy root: {root}",
            boot + 1
        );
        assert_private_dir(&root);
        std::fs::write(
            Path::new(&root).join("output.txt"),
            format!("from {chat_id}"),
        )
        .unwrap();
        open_refused_in_chat_folder(&mut ws, &root).await;
        assert_eq!(
            std::fs::read_to_string(old_folder.join("old.txt")).unwrap(),
            "pre-upgrade"
        );
        drop(ws);
        drop(server);
    }
}
