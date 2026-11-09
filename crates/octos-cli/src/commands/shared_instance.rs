//! Discovery record for a local server. Written only by the database owner;
//! clients verify the advertised instance over authenticated OUP before use.
use eyre::{Result, WrapErr};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(super) const RECORD: &str = "shared-instance.json";

pub(super) struct Publication(PathBuf);

impl Drop for Publication {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(super) fn publish(
    data_dir: &Path,
    cwd: &Path,
    port: u16,
    token: &str,
    identity: &str,
) -> Result<(Publication, Value)> {
    let data_dir = data_dir.canonicalize()?;
    let cwd = cwd.canonicalize()?;
    let identity = json!({"version": 1, "instance_id": identity, "data_dir": data_dir,
        "cwd": cwd, "protocol": "peer.workspace_team.v1"});
    let mut record = identity.clone();
    record["pid"] = json!(std::process::id());
    record["endpoint"] = json!(format!("ws://127.0.0.1:{port}/api/ui-protocol/ws"));
    record["auth_token"] = json!(token);
    let path = data_dir.join(RECORD);
    let temp = data_dir.join(format!(".shared-instance-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(&serde_json::to_vec(&record)?)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;
        #[cfg(unix)]
        std::fs::File::open(&data_dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.wrap_err("publish shared Octos instance")?;
    Ok((Publication(path), identity))
}
