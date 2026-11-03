//! Complete-or-error hydrate references, independent of tolerant replay recovery.
use super::*;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};

// Above the default five 10 MiB rotated logs; do not reject that ordinary window.
const SCAN_BYTES: usize = 64 * 1024 * 1024;
const REFERENCE_BYTES: usize = 8 * 1024 * 1024;
const SCAN_FILES: usize = 32;
const MUTATION_COUNT: usize = 1024;
const MUTATION_BYTES: usize = 256 * 1024;

pub(crate) fn hydrate_references_error(reason: &str, limit: Option<usize>) -> RpcError {
    warn!(target: "octos::ledger", reason, limit, "hydrate references unavailable; keeping client snapshot");
    RpcError::internal_error("Cannot restore complete session changes; keep the previous snapshot")
        .with_data(
            json!({"kind":"hydrate_references_unavailable", "reason":reason,
            "limit":limit, "retryable":reason == "read_failed"}),
        )
}

pub(crate) fn file_mutation(event: &UiProtocolLedgerEvent) -> Option<&UiFileMutationNotice> {
    match event {
        UiProtocolLedgerEvent::Progress(event)
        | UiProtocolLedgerEvent::Notification(UiNotification::ProgressUpdated(event)) => {
            event.metadata.file_mutation.as_ref()
        }
        _ => None,
    }
}

fn identity_reference(event: &UiProtocolLedgerEvent) -> bool {
    matches!(event,
        UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(notification))
            if matches!(notification.envelope.payload,
                PayloadV2::AssistantPersisted { .. } | PayloadV2::BackgroundChildCompleted { .. }))
        || matches!(
            event,
            UiProtocolLedgerEvent::Notification(UiNotification::TurnSpawnComplete(_))
        )
}

impl UiProtocolLedger {
    /// A single bounded scan supplies both Changes and canonical row identities.
    pub(crate) fn retained_hydrate_references(
        &self,
        session_id: &SessionKey,
        through: &UiCursor,
        identities: bool,
    ) -> Result<Vec<LedgeredUiProtocolEvent>, RpcError> {
        self.retained_references(session_id, through, |event| {
            file_mutation(event).is_some() || (identities && identity_reference(event))
        })
    }

    #[cfg(test)]
    pub(crate) fn retained_message_identity_references(
        &self,
        session_id: &SessionKey,
        through: &UiCursor,
    ) -> Result<Vec<LedgeredUiProtocolEvent>, RpcError> {
        self.retained_references(session_id, through, identity_reference)
    }

    #[cfg(test)]
    pub(crate) fn retained_file_mutations(
        &self,
        session_id: &SessionKey,
        through: &UiCursor,
    ) -> Result<Vec<UiFileMutationNotice>, RpcError> {
        Ok(self
            .retained_hydrate_references(session_id, through, false)?
            .iter()
            .filter_map(|entry| file_mutation(&entry.event).cloned())
            .collect())
    }

    fn retained_references(
        &self,
        session_id: &SessionKey,
        through: &UiCursor,
        include: impl Fn(&UiProtocolLedgerEvent) -> bool,
    ) -> Result<Vec<LedgeredUiProtocolEvent>, RpcError> {
        let storage_id = self.storage_session_id(session_id);
        validate_cursor_stream(&storage_id, through)?;
        let mut references = References::new(&storage_id, through.seq, include);
        let needs_disk = {
            let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let session = inner.sessions.get(&storage_id);
            if let Some(session) = session {
                for entry in &session.entries {
                    references.push(
                        entry.seq,
                        &entry.event,
                        entry.recorded_at,
                        entry.external_prompt,
                    )?;
                }
            }
            session.is_some_and(|session| session.disk_references_required)
                || session
                    .and_then(|session| session.entries.front())
                    .is_none_or(|first| first.seq > 1)
                    && through.seq > 0
        };
        if needs_disk && let Some(data_dir) = &self.config.data_dir {
            let dir = data_dir
                .join("ui-protocol")
                .join(encode_session_dir_name(&storage_id));
            read_references(&dir, &storage_id, &mut references)?;
        }
        Ok(references.entries.into_values().flatten().collect())
    }
}

struct References<'a, F> {
    session: &'a SessionKey,
    through: u64,
    include: F,
    entries: BTreeMap<u64, Vec<LedgeredUiProtocolEvent>>,
    mutation_count: usize,
    mutation_bytes: usize,
    reference_bytes: usize,
}

impl<'a, F: Fn(&UiProtocolLedgerEvent) -> bool> References<'a, F> {
    fn new(session: &'a SessionKey, through: u64, include: F) -> Self {
        Self {
            session,
            through,
            include,
            entries: BTreeMap::new(),
            mutation_count: 0,
            mutation_bytes: 2,
            reference_bytes: 0,
        }
    }

    fn push(
        &mut self,
        seq: u64,
        event: &UiProtocolLedgerEvent,
        recorded_at: Option<chrono::DateTime<chrono::Utc>>,
        external_prompt: bool,
    ) -> Result<(), RpcError> {
        if seq > self.through || !(self.include)(event) {
            return Ok(());
        }
        // Preserve contradictory records, deduplicating identical hot/disk copies.
        if self
            .entries
            .get(&seq)
            .is_some_and(|entries| entries.iter().any(|entry| &entry.event == event))
        {
            return Ok(());
        }
        if let Some(notice) = file_mutation(event) {
            if self.mutation_count == MUTATION_COUNT {
                return Err(hydrate_references_error(
                    "mutation_count_limit",
                    Some(MUTATION_COUNT),
                ));
            }
            let bytes = bounded_json_size(
                notice,
                MUTATION_BYTES - self.mutation_bytes,
                "mutation_bytes_limit",
                MUTATION_BYTES,
            )?;
            self.mutation_bytes += bytes + usize::from(self.mutation_count > 0);
            if self.mutation_bytes > MUTATION_BYTES {
                return Err(hydrate_references_error(
                    "mutation_bytes_limit",
                    Some(MUTATION_BYTES),
                ));
            }
            self.mutation_count += 1;
        }
        self.reference_bytes += bounded_json_size(
            event,
            REFERENCE_BYTES - self.reference_bytes,
            "reference_bytes_limit",
            REFERENCE_BYTES,
        )?;
        self.entries
            .entry(seq)
            .or_default()
            .push(LedgeredUiProtocolEvent {
                cursor: UiCursor {
                    stream: self.session.0.clone(),
                    seq,
                },
                event: event.clone(),
                recorded_at,
                from_connection: None,
                external_prompt,
            });
        Ok(())
    }
}

fn bounded_json_size(
    value: &impl Serialize,
    budget: usize,
    reason: &str,
    limit: usize,
) -> Result<usize, RpcError> {
    struct Counter {
        bytes: usize,
        budget: usize,
        exceeded: bool,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.budget - self.bytes {
                self.exceeded = true;
                return Err(std::io::Error::other(
                    "hydrate serialization budget exceeded",
                ));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        bytes: 0,
        budget,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut counter, value) {
        if counter.exceeded {
            return Err(hydrate_references_error(reason, Some(limit)));
        }
        warn!(%error, "failed to serialize hydrate reference");
        return Err(hydrate_references_error("invalid_record", None));
    }
    Ok(counter.bytes)
}

fn read_error(error: std::io::Error) -> RpcError {
    warn!(%error, "failed to read retained hydrate references");
    hydrate_references_error("read_failed", None)
}

fn read_references<F: Fn(&UiProtocolLedgerEvent) -> bool>(
    dir: &Path,
    session: &SessionKey,
    references: &mut References<'_, F>,
) -> Result<(), RpcError> {
    validate_session_directory(dir, session).map_err(read_error)?;
    let mut files = Vec::new();
    let mut remaining = SCAN_BYTES;
    for entry in fs::read_dir(dir).map_err(read_error)? {
        let entry = entry.map_err(read_error)?;
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("ledger-") && name.ends_with(".log"))
        {
            continue;
        }
        let metadata = fs::metadata(&path).map_err(read_error)?;
        if !metadata.is_file() {
            return Err(hydrate_references_error("read_failed", None));
        }
        if metadata.len() > remaining as u64 {
            return Err(hydrate_references_error(
                "scan_bytes_limit",
                Some(SCAN_BYTES),
            ));
        }
        remaining -= metadata.len() as usize;
        if files.len() == SCAN_FILES {
            return Err(hydrate_references_error(
                "scan_files_limit",
                Some(SCAN_FILES),
            ));
        }
        files.push(path);
    }
    // A nonzero trimmed durable head cannot prove an empty history from missing logs.
    if files.is_empty() {
        return Err(hydrate_references_error("read_failed", None));
    }
    files.sort();
    remaining = SCAN_BYTES;
    let mut line = Vec::new();
    for path in files {
        let mut reader = BufReader::new(fs::File::open(path).map_err(read_error)?);
        loop {
            line.clear();
            // Enforce on actual reads too: a concurrent append can grow past stat.
            let bytes = (&mut reader)
                .take((remaining + 1) as u64)
                .read_until(b'\n', &mut line)
                .map_err(read_error)?;
            if bytes == 0 {
                break;
            }
            if bytes > remaining {
                return Err(hydrate_references_error(
                    "scan_bytes_limit",
                    Some(SCAN_BYTES),
                ));
            }
            remaining -= bytes;
            let line = std::str::from_utf8(&line)
                .map_err(|_| hydrate_references_error("invalid_record", None))?;
            if line.trim().is_empty() {
                continue;
            }
            match parse_ledger_disk_record(line) {
                Ok(ParsedLedgerDiskRecord::Record(record)) if record.v == LEDGER_DISK_VERSION => {
                    references.push(
                        record.seq,
                        &record.event,
                        record.recorded_at,
                        record.external_prompt,
                    )?;
                }
                Ok(ParsedLedgerDiskRecord::LegacyMessagePersisted { v, .. })
                    if v == LEDGER_DISK_VERSION => {}
                _ => return Err(hydrate_references_error("invalid_record", None)),
            }
        }
    }
    Ok(())
}
