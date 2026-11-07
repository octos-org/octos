// Tail repair and appends run under LedgerInner's writer mutex.
fn repair_latest_log(
    session_id: &SessionKey,
    dir: &Path,
) -> std::io::Result<Option<(PathBuf, u64)>> {
    use std::io::{Read, Seek, SeekFrom};
    match validate_session_directory(dir, session_id) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let files = match list_log_files(dir) {
        Ok(files) => files,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(path) = files.into_iter().max() else {
        return Ok(None);
    };
    let mut file = fs::File::open(&path)?;
    let len = file.metadata()?.len();
    let mut end = len;
    let mut buffer = [0u8; 4096];
    while end > 0 {
        let start = end.saturating_sub(buffer.len() as u64);
        let chunk = &mut buffer[..(end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(chunk)?;
        if let Some(at) = chunk.iter().rposition(|byte| *byte == b'\n') {
            end = start + at as u64 + 1;
            break;
        }
        end = start;
    }
    if end != len {
        let file = OpenOptions::new().write(true).open(&path)?;
        file.set_len(end)?;
        file.sync_data()?;
        warn!(target: "octos::ledger", session_id = %session_id.0,
            discarded_bytes = len - end, "truncated unterminated latest ledger record");
    }
    Ok(Some((path, end)))
}

/// Roll a partially written record back to `len`. The truncate runs on a
/// dedicated write-only handle opened by PATH, not on the append handle:
/// Windows maps `.append(true)` to FILE_APPEND_DATA (ignoring the write bit),
/// and SetEndOfFile on such a handle fails with Os 5. A GENERIC_WRITE handle
/// truncates correctly on every platform.
fn finish_record_rollback(path: &Path, pending: &mut Option<u64>) -> std::io::Result<()> {
    if let Some(len) = *pending {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(len)?;
        file.sync_data()?;
        *pending = None;
    }
    Ok(())
}

fn append_complete_record(
    path: &Path,
    file: &mut fs::File,
    pending: &mut Option<u64>,
    line: &[u8],
) -> std::io::Result<u64> {
    // A previous failed rollback cannot be bypassed by another append.
    finish_record_rollback(path, pending)?;
    let before = file.metadata()?.len();
    // No buffered writer: its Drop could retry a partial write after rollback.
    if let Err(write_error) = file.write_all(line) {
        *pending = Some(before);
        if let Err(rollback_error) = finish_record_rollback(path, pending) {
            return Err(std::io::Error::new(
                rollback_error.kind(),
                format!(
                    "ledger append failed ({write_error}); rollback to {before} failed ({rollback_error}); appends blocked"
                ),
            ));
        }
        return Err(write_error);
    }
    Ok(line.len() as u64)
}

#[derive(Debug, Clone)]
enum RecoveryFailure {
    Read,
    Malformed(String),
    Write(String),
}
impl RecoveryFailure {
    fn from_read(error: &std::io::Error, detail: String) -> Self {
        if error.kind() == std::io::ErrorKind::InvalidData {
            Self::Malformed(detail)
        } else {
            Self::Read
        }
    }
    fn retryable(&self) -> bool {
        matches!(self, Self::Read)
    }
    fn check(&self, key: &SessionKey) -> Result<(), LedgerError> {
        let (operation, kind, reason) = match self {
            Self::Read => return Ok(()),
            Self::Malformed(reason) => ("recover_session", std::io::ErrorKind::InvalidData, reason),
            Self::Write(reason) => ("session_unavailable", std::io::ErrorKind::Other, reason),
        };
        Err(LedgerError::new(
            operation,
            key,
            None,
            std::io::Error::new(kind, reason.clone()),
        ))
    }
}
fn hydrate_ledger_error(error: LedgerError) -> RpcError {
    warn!(target: "octos::ledger", %error, "hydrate ledger unavailable");
    let reason = if matches!(error.operation, "session_unavailable" | "append_log") {
        "durability_failed"
    } else if error.source.kind() == std::io::ErrorKind::InvalidData {
        "invalid_record"
    } else {
        "read_failed"
    };
    let mut rpc = hydrate_references_error(reason, None);
    rpc.message = error.to_string();
    rpc
}
fn check_session_available(key: &SessionKey, session: &SessionLedger) -> Result<(), LedgerError> {
    if let Some(reason) = &session.unavailable {
        return Err(LedgerError::new(
            "session_unavailable",
            key,
            Some(session.next_seq.saturating_add(1)),
            std::io::Error::other(reason.clone()),
        ));
    }
    Ok(())
}
impl UiProtocolLedger {
    fn check_recovery_latch(&self, key: &SessionKey) -> Result<(), LedgerError> {
        if let Some(failure) = self
            .index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(key)
            .and_then(|entry| entry.failed.as_ref())
        {
            failure.check(key)?;
        }
        Ok(())
    }
    fn clear_read_failure(&self, key: &SessionKey) {
        if let Some(entry) = self
            .index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(key)
            && entry
                .failed
                .as_ref()
                .is_some_and(RecoveryFailure::retryable)
        {
            entry.failed = None;
        }
    }
    // Called with the writer lock. Eviction must not turn an ambiguous write
    // into a successful same-process recovery or permit reusing its cursor.
    fn preserve_write_failure(&self, key: &SessionKey, session: &SessionLedger) {
        if let (Some(data_dir), Some(reason)) = (&self.config.data_dir, &session.unavailable) {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            let entry = index
                .entry(key.clone())
                .or_insert_with(|| SessionIndexEntry {
                    dir: data_dir
                        .join("ui-protocol")
                        .join(encode_session_dir_name(key)),
                    reconciled: true,
                    failed: None,
                });
            entry.failed = Some(RecoveryFailure::Write(reason.clone()));
        }
    }
    fn repair_session_tail(&self, key: &SessionKey, dir: &Path) -> std::io::Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        self.check_recovery_latch(key)
            .map_err(std::io::Error::other)?;
        if let Some(session) = inner.sessions.get(key) {
            check_session_available(key, session).map_err(std::io::Error::other)?;
            return Ok(());
        }
        repair_latest_log(key, dir).map(|_| ())
    }

    fn index_session_directories(&self, ui_dir: &Path) -> std::io::Result<usize> {
        // A FILE at the ledger root must fail the scan, not read as "no
        // sessions": on Windows read_dir(file) reports NotFound, which the
        // arm below would otherwise swallow into an empty index and admit
        // RAM-only rows after a cold-read failure.
        match fs::metadata(ui_dir) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    format!("ledger root {} is not a directory", ui_dir.display()),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        }
        let entries = match fs::read_dir(ui_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut found = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let key = if name.starts_with(HASHED_SESSION_PREFIX) {
                match read_directory_identity(&path) {
                    Ok(key) => Some(key),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::InvalidData | std::io::ErrorKind::NotFound
                        ) =>
                    {
                        None
                    }
                    Err(error) => return Err(error),
                }
            } else {
                decode_session_dir_name(name)
            };
            if let Some(key) = key {
                found.push((key, path));
            }
        }
        let count = found.len();
        let mut recovered = self
            .recovered_session_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
        for (key, dir) in found {
            recovered.insert(key.clone());
            index.entry(key).or_insert(SessionIndexEntry {
                dir,
                reconciled: false,
                failed: None,
            });
        }
        Ok(count)
    }
}

#[cfg(test)]
mod recovery_write_tests {
    use super::*;

    #[test]
    fn ledger_failed_rollback_blocks_append_until_repair() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ledger.log");
        fs::write(&path, b"complete\npartial").unwrap();
        let mut pending = Some(9);
        // Open the append handle while the file is writable, then mark the
        // FILE read-only: the rollback's write-only reopen now fails (a
        // genuine rollback failure, cross-platform) while the already-open
        // append handle keeps its write rights.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        let mut readonly = fs::metadata(&path).unwrap().permissions();
        readonly.set_readonly(true);
        fs::set_permissions(&path, readonly).unwrap();
        assert!(append_complete_record(&path, &mut file, &mut pending, b"next\n").is_err());
        assert_eq!(pending, Some(9));
        assert_eq!(fs::read(&path).unwrap(), b"complete\npartial");
        // Repair: make the file writable again; rollback + append succeed.
        // Test-local temp file: restoring writability via set_readonly(false)
        // is intentional (clippy's world-writable concern does not apply here).
        #[allow(clippy::permissions_set_readonly_false)]
        let mut writable = fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        writable.set_readonly(false);
        fs::set_permissions(&path, writable).unwrap();
        append_complete_record(&path, &mut file, &mut pending, b"next\n").unwrap();
        assert_eq!(pending, None);
        assert_eq!(fs::read(&path).unwrap(), b"complete\nnext\n");
    }

    #[test]
    fn ledger_write_error_latches_failed_rollback() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ledger.log");
        fs::write(&path, b"complete\n").unwrap();
        // A read-only FILE defeats the rollback's write-only reopen, and a
        // read-only handle defeats the write itself: both failures latch.
        let mut readonly = fs::metadata(&path).unwrap().permissions();
        readonly.set_readonly(true);
        fs::set_permissions(&path, readonly).unwrap();
        let mut file = fs::File::open(&path).unwrap();
        let mut pending = None;
        let error = append_complete_record(&path, &mut file, &mut pending, b"next\n").unwrap_err();
        assert!(error.to_string().contains("appends blocked"), "{error}");
        assert_eq!(pending, Some(9));
        assert_eq!(fs::read(path).unwrap(), b"complete\n");
    }
    #[test]
    fn ledger_pending_rollback_cannot_be_sealed_by_rotation() {
        let temp = tempfile::tempdir().unwrap();
        let key = SessionKey("local:rollback-rotation".into());
        let mut config = LedgerConfig::durable(temp.path().into());
        config.rotate_bytes = 1;
        let ledger = UiProtocolLedger::with_config(config);
        let dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&key));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger-0000.log");
        fs::write(&path, b"{}\npartial").unwrap();
        let mut session = SessionLedger::new();
        session.active_log_path = Some(path.clone());
        session.active_log_bytes = 3;
        session.pending_rollback = Some(3);
        session.next_seq = 1;
        let event = UiProtocolLedgerEvent::Progress(UiProgressEvent::new(
            key.clone(),
            None,
            octos_core::ui_protocol::UiProgressMetadata::new("test"),
        ));
        ledger
            .write_record_locked(&key, &mut session, &event, false, chrono::Utc::now(), 2)
            .unwrap();
        assert_eq!(list_log_files(&dir).unwrap(), vec![path.clone()]);
        let bytes = fs::read(path).unwrap();
        assert!(bytes.starts_with(b"{}\n{\"v\":"));
        assert!(!bytes.windows(7).any(|window| window == b"partial"));
        assert!(session.pending_rollback.is_none());
    }
    #[test]
    fn ledger_trimmed_recovery_keeps_snapshot_bootstrap_and_cadence() {
        let temp = tempfile::tempdir().unwrap();
        let key = SessionKey("local:trimmed-snapshot-cache".into());
        let mut config = LedgerConfig::durable(temp.path().into());
        config.retained_per_session = 3;
        config.snapshot_every_events = 5;
        let event = || {
            UiProgressEvent::new(
                key.clone(),
                None,
                octos_core::ui_protocol::UiProgressMetadata::new("status"),
            )
        };
        let ledger = UiProtocolLedger::with_config(config.clone());
        for _ in 0..20 {
            ledger.append_progress(event());
        }
        let dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&key));
        fs::remove_file(dir.join(SESSION_SNAPSHOT_FILE_NAME)).unwrap();
        drop(ledger);
        let ledger = UiProtocolLedger::recover(config).ledger;
        let (events, head) = ledger.snapshot_with_cursor(&key, None).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(head.seq, 20);
        let snapshot = ledger
            .read_session_snapshot(&key, &dir)
            .unwrap()
            .expect("ordinary trimmed recovery must bootstrap its cache");
        assert_eq!(snapshot.head_seq, 20);
        for _ in 0..5 {
            ledger.append_progress(event());
        }
        let snapshot = ledger.read_session_snapshot(&key, &dir).unwrap().unwrap();
        assert_eq!(
            snapshot.head_seq, 25,
            "normal recovered sessions must retain snapshot cadence"
        );
    }

    #[test]
    fn ledger_corrupt_recovery_does_not_cache_a_partial_projection() {
        let temp = tempfile::tempdir().unwrap();
        let key = SessionKey("local:corrupt-snapshot-cache".into());
        let mut config = LedgerConfig::durable(temp.path().into());
        config.snapshot_every_events = 1;
        let event = || {
            UiProgressEvent::new(
                key.clone(),
                None,
                octos_core::ui_protocol::UiProgressMetadata::new("status"),
            )
        };
        let ledger = UiProtocolLedger::with_config(config.clone());
        ledger.append_progress(event());
        let dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&key));
        let path = list_log_files(&dir).unwrap().pop().unwrap();
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"{malformed\n")
            .unwrap();
        fs::remove_file(dir.join(SESSION_SNAPSHOT_FILE_NAME)).unwrap();
        drop(ledger);
        let ledger = UiProtocolLedger::recover(config).ledger;
        let error = ledger.snapshot_with_cursor(&key, None).unwrap_err();
        assert_eq!(error.data.unwrap()["reason"], "invalid_record");
        assert!(
            ledger
                .try_append_progress_from(event(), ConnectionId::next())
                .is_err()
        );
        assert!(!dir.join(SESSION_SNAPSHOT_FILE_NAME).exists());
    }
}
