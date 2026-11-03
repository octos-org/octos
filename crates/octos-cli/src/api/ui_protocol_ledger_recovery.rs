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

fn finish_record_rollback(file: &fs::File, pending: &mut Option<u64>) -> std::io::Result<()> {
    if let Some(len) = *pending {
        file.set_len(len)?;
        file.sync_data()?;
        *pending = None;
    }
    Ok(())
}

fn append_complete_record(
    file: &mut fs::File,
    pending: &mut Option<u64>,
    line: &[u8],
) -> std::io::Result<u64> {
    // A previous failed rollback cannot be bypassed by another append.
    finish_record_rollback(file, pending)?;
    let before = file.metadata()?.len();
    // No buffered writer: its Drop could retry a partial write after rollback.
    if let Err(write_error) = file.write_all(line) {
        *pending = Some(before);
        if let Err(rollback_error) = finish_record_rollback(file, pending) {
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

fn merge_recovered_session(session: &mut SessionLedger, snapshot: DiskSessionSnapshot, cap: usize) {
    let next_seq = session.next_seq;
    let active = (session.active_log_path.take(), session.active_log_bytes);
    let keep_active = next_seq > snapshot.head_seq || session.pending_rollback.is_some();
    let entries = std::mem::take(&mut session.entries);
    let external_prompts = std::mem::take(&mut session.external_prompts);
    session.disk_references_required |= session.recovery_pending
        || !snapshot.projection_complete
        || snapshot
            .retained_entries
            .front()
            .is_some_and(|entry| entry.seq > 1)
        || entries.front().is_some_and(|entry| entry.seq > 1);
    hydrate_session_from_snapshot(session, snapshot);
    session.recovery_pending = false;
    session.next_seq = session.next_seq.max(next_seq);
    if keep_active && active.0.is_some() {
        (session.active_log_path, session.active_log_bytes) = active;
    }
    session.external_prompts.extend(external_prompts);
    // Failed cold reads may have allocated the same seqs in RAM. Preserve each
    // distinct event, not merely whichever copy has the higher head cursor.
    let mut merged = std::collections::BTreeMap::<u64, Vec<LedgerEntry>>::new();
    for entry in session.entries.drain(..).chain(entries) {
        let same_seq = merged.entry(entry.seq).or_default();
        if let Some(old) = same_seq.iter_mut().find(|old| old.event == entry.event) {
            old.external_prompt |= entry.external_prompt;
        } else {
            same_seq.push(entry);
        }
    }
    session.entries = merged.into_values().flatten().collect();
    while session.entries.len() > cap {
        session.entries.pop_front();
    }
    session.in_memory_bytes = session.entries.iter().map(|entry| entry.bytes).sum();
}

impl UiProtocolLedger {
    fn index_session_directories(&self, ui_dir: &Path) -> std::io::Result<usize> {
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
                failed: false,
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
        let mut file = fs::File::open(&path).unwrap();
        let mut pending = Some(9);
        assert!(append_complete_record(&mut file, &mut pending, b"next\n").is_err());
        assert_eq!(pending, Some(9));
        assert_eq!(fs::read(&path).unwrap(), b"complete\npartial");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        append_complete_record(&mut file, &mut pending, b"next\n").unwrap();
        assert_eq!(pending, None);
        assert_eq!(fs::read(&path).unwrap(), b"complete\nnext\n");
    }

    #[test]
    fn ledger_write_error_latches_failed_rollback() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ledger.log");
        fs::write(&path, b"complete\n").unwrap();
        let mut file = fs::File::open(&path).unwrap();
        let mut pending = None;
        let error = append_complete_record(&mut file, &mut pending, b"next\n").unwrap_err();
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
            .write_record_locked(&key, &mut session, &event, false, chrono::Utc::now())
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
        let (_, head) = ledger.snapshot_with_cursor(&key, None).unwrap();
        let error = ledger.retained_file_mutations(&key, &head).unwrap_err();
        assert_eq!(error.data.unwrap()["reason"], "invalid_record");
        ledger.append_progress(event());
        assert!(!dir.join(SESSION_SNAPSHOT_FILE_NAME).exists());
    }
}
