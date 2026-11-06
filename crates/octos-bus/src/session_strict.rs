use super::*;

// Read and parse the same bytes before a whole-file update. The permissive
// display loader must never supply a partial snapshot to these writers.
fn read_segment_for_update(path: &Path, active: bool) -> Result<Option<SegmentFile>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).wrap_err_with(|| {
                format!(
                    "cannot read session transcript for update: {}",
                    path.display()
                )
            });
        }
    };
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| eyre::eyre!("session transcript has no metadata: {}", path.display()))?;
    let meta: SessionMeta = serde_json::from_str(header)
        .wrap_err_with(|| format!("invalid session metadata: {}", path.display()))?;
    eyre::ensure!(
        meta.schema_version <= CURRENT_SESSION_SCHEMA,
        "session schema {} is newer than supported {}: {}",
        meta.schema_version,
        CURRENT_SESSION_SCHEMA,
        path.display()
    );
    let mut timeline = Vec::new();
    let mut rows = text.split_inclusive('\n');
    let mut offset = rows.next().expect("validated header").len();
    let mut rows = rows.enumerate().peekable();
    let mut suffix = None;
    while let Some((index, raw)) = rows.next() {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        if !line.trim().is_empty() {
            if let Some(item) = parse_timeline_row(line) {
                timeline.push(item);
            } else if rows.peek().is_some_and(|(_, receipt)| {
                tail::is_sealed(line, offset as u64, receipt.trim_end_matches('\n'))
            }) {
                offset += rows.next().expect("checked receipt").1.len();
            } else if active
                && rows.peek().is_none()
                && !raw.ends_with('\n')
                && tail::is_incomplete(line)
            {
                suffix = Some(tail::seal_suffix(line, offset as u64));
            } else {
                return Err(eyre::eyre!(
                    "invalid session row {}: {}",
                    index + 2,
                    path.display()
                ));
            }
        }
        offset += raw.len();
    }
    let mut bytes = text.len() as u64;
    if let Some(suffix) = suffix {
        use std::io::Write;
        // Validation above used one snapshot. Append only the repair receipt;
        // all callers hold the session's persistence/lifecycle lock.
        let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
        file.write_all(suffix.as_bytes())?;
        file.sync_all()?;
        bytes += suffix.len() as u64;
    }
    Ok(Some(SegmentFile {
        meta,
        timeline,
        bytes,
    }))
}

fn load_for_update(active: &Path, key: &SessionKey, budget: u64) -> Result<Option<Session>> {
    let dir = segments_dir(active);
    let active_file = read_segment_for_update(active, true)?;
    let recover_active = active_file.is_none();
    let sealed_total = match &active_file {
        Some(file) => file.meta.sealed_segments,
        None => {
            // A missing active file can be an interrupted seal, not a new
            // session. Read the newest segment once to recover the active metadata.
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error).wrap_err("read sealed session directory"),
            };
            let mut last = 0;
            for entry in entries {
                let path = entry.wrap_err("read sealed session entry")?.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                    && let Some(index) = path
                        .file_stem()
                        .and_then(|name| name.to_str())
                        .and_then(|name| name.parse::<u32>().ok())
                {
                    last = last.max(index);
                }
            }
            last
        }
    };
    let mut active_file = active_file;
    let mut recovered_newest = None;
    if recover_active && sealed_total > 0 {
        let path = segment_path(&dir, sealed_total);
        let newest = read_segment_for_update(&path, false)?
            .ok_or_else(|| eyre::eyre!("missing sealed session segment: {}", path.display()))?;
        let recovered = SessionMeta {
            sealed_segments: sealed_total,
            base_seq: newest.meta.base_seq + fold_session_timeline(newest.timeline.clone()).len(),
            ..newest.meta.clone()
        };
        let bytes = serde_json::to_string(&recovered)?.len() as u64 + 1;
        active_file = Some(SegmentFile {
            meta: recovered,
            timeline: Vec::new(),
            bytes,
        });
        recovered_newest = Some(newest);
    }
    let Some(active_file) = active_file else {
        return Ok(None);
    };
    let mut spent = active_file.bytes;
    let mut loaded = Vec::new();
    let mut base_seq = active_file.meta.base_seq;
    for index in (1..=sealed_total).rev() {
        let path = segment_path(&dir, index);
        let size = std::fs::metadata(&path)
            .wrap_err_with(|| format!("stat sealed session segment: {}", path.display()))?
            .len();
        if spent.saturating_add(size) > budget {
            break;
        }
        let segment = if let Some(newest) = recovered_newest.take() {
            newest
        } else {
            read_segment_for_update(&path, false)?
                .ok_or_else(|| eyre::eyre!("missing sealed session segment: {}", path.display()))?
        };
        spent = spent.saturating_add(size);
        base_seq = segment.meta.base_seq;
        loaded.push(segment);
    }
    let loaded_sealed = loaded.len() as u32;
    let mut timeline = Vec::new();
    for segment in loaded.into_iter().rev() {
        timeline.extend(segment.timeline);
    }
    timeline.extend(active_file.timeline);
    let meta = active_file.meta;
    let messages = fold_session_timeline(timeline);
    if recover_active {
        use std::io::Write;
        // Publish the seal's missing active metadata from the strict snapshot;
        // the ordinary rewrite guard can then retain its sealed-count check.
        let recovered = meta.clone();
        let tmp = rewrite_tmp_path(active);
        let result = (|| -> Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            writeln!(file, "{}", serde_json::to_string(&recovered)?)?;
            file.sync_all()?;
            std::fs::rename(&tmp, active)?;
            if let Some(parent) = active.parent() {
                fsync_dir(parent);
            }
            Ok(())
        })();
        if let Err(error) = result {
            if let Err(cleanup) = std::fs::remove_file(&tmp)
                && cleanup.kind() != std::io::ErrorKind::NotFound
            {
                warn!(path = %tmp.display(), %cleanup, "failed to remove recovery temporary file");
            }
            return Err(error).wrap_err("recover active session from strict snapshot");
        }
    }
    Ok(Some(Session {
        key: key.clone(),
        parent_key: meta.parent_key.map(SessionKey),
        topic: meta.topic,
        summary: meta.summary,
        title: meta.title,
        title_manual: meta.title_manual,
        child_contracts: meta.child_contracts,
        messages,
        base_seq,
        sealed_segments: sealed_total,
        loaded_sealed,
        created_at: meta.created_at,
        updated_at: meta.updated_at,
    }))
}

impl SessionHandle {
    /// Load the normal transcript window for a writer, propagating read and parse
    /// failures. Only a genuinely absent transcript starts an empty session.
    /// Callers doing read-modify-write must also hold the per-key persist lock.
    pub fn try_open(data_dir: &Path, key: &SessionKey) -> Result<Self> {
        let lifecycle = lifecycle::Guard::acquire(data_dir, key)?;
        let epoch = lifecycle.epoch();
        let sessions_dir = data_dir
            .join("users")
            .join(Self::encode_path_component(key.base_key()))
            .join("sessions");
        let path = sessions_dir.join(Self::topic_filename(key));
        let marker = Self::migration_marker_path(&sessions_dir, key);
        let mut session = load_for_update(&path, key, session_load_budget_bytes())?;
        if session.is_none()
            && !marker
                .try_exists()
                .wrap_err("read session migration marker")?
        {
            let legacy = SessionManager::session_path_static(&data_dir.join("sessions"), key);
            session = load_for_update(&legacy, key, u64::MAX)?;
            if let Some(loaded) = &session {
                lifecycle.check_live(&epoch)?;
                std::fs::create_dir_all(&sessions_dir).wrap_err("create session directory")?;
                Self::rewrite_blocking(&path, loaded)?;
                match std::fs::remove_file(&legacy) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).wrap_err("remove migrated session"),
                }
                remove_segments(&legacy)?;
                std::fs::write(&marker, b"migrated-from-flat\n")
                    .wrap_err("write session migration marker")?;
            }
        }
        std::fs::create_dir_all(&sessions_dir).wrap_err("create session directory")?;
        Ok(Self {
            sessions_dir,
            session: session.unwrap_or_else(|| Session::new(key.clone())),
            observer_root: data_dir.to_owned(),
            epoch,
            delivery_only: false,
        })
    }

    /// Fallible presence check for lifecycle writers; a stat failure must not
    /// turn a known parent into an orphan.
    pub fn try_session_exists(data_dir: &Path, key: &SessionKey) -> std::io::Result<bool> {
        let canonical = data_dir
            .join("users")
            .join(Self::encode_path_component(key.base_key()))
            .join("sessions")
            .join(Self::topic_filename(key));
        let legacy = SessionManager::session_path_static(&data_dir.join("sessions"), key);
        Ok(canonical.try_exists()?
            || segments_dir(&canonical).try_exists()?
            || legacy.try_exists()?
            || segments_dir(&legacy).try_exists()?)
    }
}
