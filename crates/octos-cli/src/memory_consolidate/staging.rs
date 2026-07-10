//! Staging input parsers: capture notes and extraction files.
//!
//! Formats are FIXED by their writers (`MemoryStore::write_staging_note` for
//! notes, the PR-3 extraction writer for extract files); the parsers here are
//! module-local on purpose — the engine must not grow a dependency on
//! octos-memory internals. Frontmatter is host-written and therefore trusted
//! metadata; every content body is untrusted DATA.

use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, NaiveDate};
use eyre::{Result, WrapErr, bail, eyre};
use serde::{Deserialize, Serialize};

use super::entry::is_id_token;

/// Who authored a staging note. Host notes are the only destruction-capable
/// authority; the shipped `memory_note` tool always stamps `model`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteOrigin {
    Model,
    Host,
}

/// What a staging note captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    UserRequest,
    Correction,
    Fact,
    Forget,
}

impl NoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NoteKind::UserRequest => "user_request",
            NoteKind::Correction => "correction",
            NoteKind::Fact => "fact",
            NoteKind::Forget => "forget",
        }
    }
}

/// One hash-bound pending-confirm candidate (stored in pending note
/// frontmatter as a JSON array).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingCandidate {
    pub entry_id: String,
    /// SHA-256 hex of the candidate entry's full text at binding time.
    pub content_hash: String,
    /// True when the entry was moved to the archive pending confirmation
    /// (sensitive forget notes only).
    pub interim_archived: bool,
}

/// A parsed staging note file.
#[derive(Debug, Clone)]
pub struct NoteFile {
    /// Note id = full filename stem.
    pub id: String,
    pub path: PathBuf,
    pub origin: NoteOrigin,
    pub kind: NoteKind,
    pub created_at: DateTime<FixedOffset>,
    pub session_key: Option<String>,
    pub sensitive: bool,
    pub replaces_id: Option<String>,
    /// Untrusted body text (trimmed).
    pub content: String,
    /// Pending-confirm state (present only after the engine rewrote the note).
    pub candidates: Option<Vec<PendingCandidate>>,
    pub expires_at: Option<DateTime<FixedOffset>>,
    /// Original frontmatter lines (between the `---` fences), for faithful
    /// pending rewrites.
    frontmatter_lines: Vec<String>,
    /// Raw body bytes after the closing fence, preserved for rewrites.
    body_raw: String,
}

impl NoteFile {
    /// A note is pending-confirm once the engine stamped candidates and an
    /// expiry into its frontmatter.
    pub fn is_pending(&self) -> bool {
        self.candidates.is_some() && self.expires_at.is_some()
    }

    /// Entry ids named by an `id:^m…` token in the note content. Host forget
    /// notes with at least one named id are *id-bound* (direct hard-delete
    /// authority / pending confirmation); with none they are *free-text* and
    /// only ever start the pending-confirm flow.
    pub fn named_entry_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        let mut rest = self.content.as_str();
        while let Some(pos) = rest.find("id:") {
            let after = rest[pos + 3..].trim_start();
            let token: String = after.chars().take_while(|c| !c.is_whitespace()).collect();
            let token = token.trim_end_matches([',', '.', ')', ';']);
            if is_id_token(token) && !ids.contains(&token.to_string()) {
                ids.push(token.to_string());
            }
            rest = &rest[pos + 3..];
        }
        ids
    }

    /// True for a host forget note whose content names no entry id — the
    /// pending-confirm path.
    pub fn is_free_text_forget(&self) -> bool {
        self.origin == NoteOrigin::Host
            && self.kind == NoteKind::Forget
            && self.named_entry_ids().is_empty()
    }

    /// Render the note with pending-confirm state stamped into frontmatter.
    /// Any previous `candidates:`/`expires_at:` lines are replaced; original
    /// host-written frontmatter and the body are preserved verbatim.
    pub fn render_pending(
        &self,
        candidates: &[PendingCandidate],
        expires_at: &DateTime<FixedOffset>,
    ) -> String {
        let mut out = String::from("---\n");
        for line in &self.frontmatter_lines {
            if line.starts_with("candidates:") || line.starts_with("expires_at:") {
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&format!(
            "candidates: {}\n",
            serde_json::to_string(candidates).expect("candidates serialize")
        ));
        out.push_str(&format!("expires_at: {}\n", expires_at.to_rfc3339()));
        out.push_str("---");
        out.push_str(&self.body_raw);
        out
    }
}

/// Host-computed evidence class of an extraction item — trusted metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    UserSaid,
    ToolShowed,
    AssistantClaimed,
}

impl EvidenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceKind::UserSaid => "user_said",
            EvidenceKind::ToolShowed => "tool_showed",
            EvidenceKind::AssistantClaimed => "assistant_claimed",
        }
    }
}

/// One item from an extraction file body.
#[derive(Debug, Clone)]
pub struct ExtractionItem {
    /// Item id = `<file stem>#<index>`.
    pub id: String,
    pub kind: String,
    /// Untrusted content.
    pub content: String,
    pub evidence_kind: EvidenceKind,
    pub evidence_idx: Vec<u64>,
    pub date: Option<NaiveDate>,
}

/// A parsed extraction file (`memory/staging/extract/`).
#[derive(Debug, Clone)]
pub struct ExtractionFile {
    /// File id = filename stem, normalized to its uuid prefix — legacy
    /// stems carry a sanitized SESSION KEY (untrusted channel metadata)
    /// after the first `-`, and item ids derived from this render in the
    /// merge prompt. Deletion uses `path`, never this id.
    pub id: String,
    pub path: PathBuf,
    pub session_key: Option<String>,
    pub extracted_at: DateTime<FixedOffset>,
    pub model: String,
    pub items: Vec<ExtractionItem>,
}

/// Everything found under `staging/` for one run.
#[derive(Debug, Default)]
pub struct StagingBatch {
    /// Consumable notes (non-pending), filename order.
    pub notes: Vec<NoteFile>,
    /// Notes already in pending-confirm state.
    pub pending: Vec<NoteFile>,
    /// Extraction files, filename order.
    pub extractions: Vec<ExtractionFile>,
    /// Files that failed to parse: (path, error, protected-from-quarantine).
    pub parse_failures: Vec<(PathBuf, String, bool)>,
}

impl StagingBatch {
    /// True when there is nothing consumable — no batch notes, no extraction
    /// items, no parse failures — and no pending notes either.
    pub fn is_clean(&self) -> bool {
        self.notes.is_empty()
            && self.pending.is_empty()
            && self.extractions.is_empty()
            && self.parse_failures.is_empty()
    }
}

/// Split `---` fenced frontmatter. Returns (frontmatter lines, raw body after
/// the closing fence).
fn split_frontmatter(content: &str) -> Result<(Vec<String>, String)> {
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or_else(|| eyre!("missing frontmatter opener '---'"))?;
    let mut fm = Vec::new();
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            let body = &rest[offset + line.len()..];
            return Ok((fm, body.to_string()));
        }
        fm.push(trimmed.to_string());
        offset += line.len();
    }
    bail!("missing frontmatter closer '---'")
}

/// Parse `key: value` from a frontmatter line.
fn key_value(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once(':')?;
    Some((k.trim(), v.trim()))
}

/// Parse a JSON-encoded string value (the writers JSON-encode string values
/// so multi-line / CJK / quote-bearing content can't corrupt the header).
fn json_string(key: &str, value: &str) -> Result<String> {
    serde_json::from_str::<String>(value)
        .wrap_err_with(|| format!("frontmatter key '{key}' is not a JSON string: {value}"))
}

fn rfc3339(key: &str, value: &str) -> Result<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value)
        .wrap_err_with(|| format!("frontmatter key '{key}' is not RFC 3339: {value}"))
}

/// Body = raw body with a single leading blank line stripped, trimmed at the
/// end. The writer emits `---\n\n<content>\n`.
fn body_content(body_raw: &str) -> String {
    let body = body_raw
        .strip_prefix("\r\n")
        .or_else(|| body_raw.strip_prefix('\n'))
        .unwrap_or(body_raw);
    body.trim_end().to_string()
}

/// Parse one staging note file.
pub fn parse_note(path: &Path, content: &str) -> Result<NoteFile> {
    let id = file_stem(path)?;
    let (fm_lines, body_raw) = split_frontmatter(content)?;

    let mut origin = None;
    let mut kind = None;
    let mut created_at = None;
    let mut session_key = None;
    let mut sensitive = false;
    let mut replaces_id = None;
    let mut candidates = None;
    let mut expires_at = None;

    for line in &fm_lines {
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = key_value(line) else {
            bail!("malformed frontmatter line: {line}");
        };
        match key {
            "origin" => {
                origin = Some(match value {
                    "model" => NoteOrigin::Model,
                    "host" => NoteOrigin::Host,
                    other => bail!("unknown origin '{other}'"),
                });
            }
            "kind" => {
                kind = Some(match value {
                    "user_request" => NoteKind::UserRequest,
                    "correction" => NoteKind::Correction,
                    "fact" => NoteKind::Fact,
                    "forget" => NoteKind::Forget,
                    other => bail!("unknown kind '{other}'"),
                });
            }
            "created_at" => created_at = Some(rfc3339(key, value)?),
            "session_key" => session_key = Some(json_string(key, value)?),
            "sensitive" => sensitive = value == "true",
            "replaces_id" => replaces_id = Some(json_string(key, value)?),
            "candidates" => {
                candidates = Some(
                    serde_json::from_str::<Vec<PendingCandidate>>(value)
                        .wrap_err("frontmatter key 'candidates' is not a candidate array")?,
                );
            }
            "expires_at" => expires_at = Some(rfc3339(key, value)?),
            // Unknown host-written keys are tolerated (forward compat).
            _ => {}
        }
    }

    let origin = origin.ok_or_else(|| eyre!("note missing 'origin'"))?;
    let kind = kind.ok_or_else(|| eyre!("note missing 'kind'"))?;
    let created_at = created_at.ok_or_else(|| eyre!("note missing 'created_at'"))?;
    // Pending state must be all-or-nothing.
    if candidates.is_some() != expires_at.is_some() {
        bail!("note has partial pending state (candidates/expires_at mismatch)");
    }

    Ok(NoteFile {
        id,
        path: path.to_path_buf(),
        origin,
        kind,
        created_at,
        session_key,
        sensitive,
        replaces_id,
        content: body_content(&body_raw),
        candidates,
        expires_at,
        frontmatter_lines: fm_lines,
        body_raw,
    })
}

/// Raw extraction body schema (ONE JSON object).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtractBody {
    items: Vec<RawExtractItem>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtractItem {
    kind: String,
    content: String,
    evidence_kind: String,
    #[serde(default)]
    evidence_idx: Vec<u64>,
    #[serde(default)]
    date: Option<String>,
}

/// Parse one extraction file.
pub fn parse_extraction(path: &Path, content: &str) -> Result<ExtractionFile> {
    // Item ids derive from the stem and are rendered into the merge
    // prompt. New artifacts are opaque UUIDs, but PENDING pre-upgrade
    // artifacts still carry a sanitized session key after the first `-`
    // (email sender/topic — untrusted). Normalize to the uuid prefix
    // (codex round-4 P2).
    let id = file_stem(path)?;
    let id = match id.split_once('-') {
        Some((uuid, _legacy_slug)) => uuid.to_string(),
        None => id,
    };
    let (fm_lines, body_raw) = split_frontmatter(content)?;

    let mut session_key = None;
    let mut extracted_at = None;
    let mut model = None;
    for line in &fm_lines {
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = key_value(line) else {
            bail!("malformed frontmatter line: {line}");
        };
        match key {
            "session_key" => session_key = Some(json_string(key, value)?),
            "extracted_at" => extracted_at = Some(rfc3339(key, value)?),
            "model" => model = Some(json_string(key, value)?),
            _ => {}
        }
    }
    let extracted_at = extracted_at.ok_or_else(|| eyre!("extraction missing 'extracted_at'"))?;
    let model = model.ok_or_else(|| eyre!("extraction missing 'model'"))?;

    let body: RawExtractBody = serde_json::from_str(body_content(&body_raw).as_str())
        .wrap_err("extraction body is not the expected JSON object")?;

    let mut items = Vec::with_capacity(body.items.len());
    for (idx, raw) in body.items.into_iter().enumerate() {
        let evidence_kind = match raw.evidence_kind.as_str() {
            "user_said" => EvidenceKind::UserSaid,
            "tool_showed" => EvidenceKind::ToolShowed,
            "assistant_claimed" => EvidenceKind::AssistantClaimed,
            other => bail!("unknown evidence_kind '{other}'"),
        };
        match raw.kind.as_str() {
            "fact" | "preference" | "correction" | "landmine" => {}
            other => bail!("unknown extraction item kind '{other}'"),
        }
        let date = raw
            .date
            .map(|d| {
                NaiveDate::parse_from_str(&d, "%Y-%m-%d")
                    .wrap_err_with(|| format!("item date '{d}' is not YYYY-MM-DD"))
            })
            .transpose()?;
        items.push(ExtractionItem {
            id: format!("{id}#{idx}"),
            kind: raw.kind,
            content: raw.content,
            evidence_kind,
            evidence_idx: raw.evidence_idx,
            date,
        });
    }

    Ok(ExtractionFile {
        id,
        path: path.to_path_buf(),
        session_key,
        extracted_at,
        model,
        items,
    })
}

fn file_stem(path: &Path) -> Result<String> {
    Ok(path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| eyre!("staging file has no UTF-8 stem: {}", path.display()))?
        .to_string())
}

/// Quarantine protection sniff for files that fail to parse: if the raw
/// content claims host origin or a user request we must NOT signal
/// quarantine for it (never drop a user ask, even a corrupted one).
fn raw_is_protected(content: &str) -> bool {
    content.lines().any(|l| {
        let l = l.trim();
        l == "origin: host" || l == "kind: user_request"
    })
}

/// List `.md` files in a directory, sorted by filename (uuidv7 stems sort in
/// creation order). Missing directory = empty.
fn list_md_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(e) => return Err(e).wrap_err_with(|| format!("failed to list {}", dir.display())),
    };
    for entry in entries {
        let path = entry
            .wrap_err_with(|| format!("failed to list {}", dir.display()))?
            .path();
        if path.extension().is_some_and(|e| e == "md") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Load the full staging state for one run.
pub fn load_staging(memory_dir: &Path) -> Result<StagingBatch> {
    let mut batch = StagingBatch::default();

    for path in list_md_files(&memory_dir.join("staging").join("notes"))? {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                batch.parse_failures.push((path, e.to_string(), false));
                continue;
            }
        };
        match parse_note(&path, &content) {
            Ok(note) if note.is_pending() => batch.pending.push(note),
            Ok(note) => batch.notes.push(note),
            Err(e) => {
                let protected = raw_is_protected(&content);
                batch.parse_failures.push((path, e.to_string(), protected));
            }
        }
    }

    for path in list_md_files(&memory_dir.join("staging").join("extract"))? {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                batch.parse_failures.push((path, e.to_string(), false));
                continue;
            }
        };
        match parse_extraction(&path, &content) {
            Ok(extract) => batch.extractions.push(extract),
            Err(e) => batch.parse_failures.push((path, e.to_string(), false)),
        }
    }

    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-for-byte the format `MemoryStore::write_staging_note` renders.
    const HOST_FORGET: &str = "---\n\
        origin: host\n\
        kind: forget\n\
        created_at: 2026-07-01T10:00:00+00:00\n\
        session_key: \"telegram:123\"\n\
        sensitive: true\n\
        ---\n\
        \n\
        forget everything about the seattle move\n";

    fn note_path(name: &str) -> PathBuf {
        PathBuf::from(format!("/staging/notes/{name}.md"))
    }

    #[test]
    fn should_parse_host_forget_note_when_writer_format_given() {
        let path = note_path("0198a-forget-everything");
        let note = parse_note(&path, HOST_FORGET).unwrap();
        assert_eq!(note.id, "0198a-forget-everything");
        assert_eq!(note.origin, NoteOrigin::Host);
        assert_eq!(note.kind, NoteKind::Forget);
        assert!(note.sensitive);
        assert_eq!(note.session_key.as_deref(), Some("telegram:123"));
        assert_eq!(note.content, "forget everything about the seattle move");
        assert!(!note.is_pending());
        assert!(note.is_free_text_forget());
    }

    #[test]
    fn should_parse_model_note_with_replaces_id_when_correction() {
        let raw = "---\norigin: model\nkind: correction\n\
                   created_at: 2026-07-01T10:00:00.123456+00:00\n\
                   replaces_id: \"^mabc234\"\n---\n\nuser moved to Seattle\n";
        let note = parse_note(&note_path("n1"), raw).unwrap();
        assert_eq!(note.origin, NoteOrigin::Model);
        assert_eq!(note.kind, NoteKind::Correction);
        assert_eq!(note.replaces_id.as_deref(), Some("^mabc234"));
        assert!(!note.is_free_text_forget());
    }

    #[test]
    fn should_reject_note_when_frontmatter_broken() {
        assert!(parse_note(&note_path("x"), "no frontmatter").is_err());
        assert!(parse_note(&note_path("x"), "---\norigin: host\n").is_err());
        let bad_kind =
            "---\norigin: host\nkind: destroy\ncreated_at: 2026-07-01T10:00:00Z\n---\n\nx\n";
        assert!(parse_note(&note_path("x"), bad_kind).is_err());
        let no_created = "---\norigin: host\nkind: forget\n---\n\nx\n";
        assert!(parse_note(&note_path("x"), no_created).is_err());
        let bad_date = "---\norigin: host\nkind: forget\ncreated_at: yesterday\n---\n\nx\n";
        assert!(parse_note(&note_path("x"), bad_date).is_err());
    }

    #[test]
    fn should_extract_named_ids_when_id_tokens_present() {
        let mk = |content: &str| NoteFile {
            content: content.to_string(),
            ..parse_note(&note_path("n"), HOST_FORGET).unwrap()
        };
        assert_eq!(mk("forget id:^mabc234 now").named_entry_ids(), ["^mabc234"]);
        assert_eq!(mk("forget id: ^mabc234.").named_entry_ids(), ["^mabc234"]);
        assert_eq!(
            mk("id:^mabc234 and id:^mdef567").named_entry_ids(),
            ["^mabc234", "^mdef567"]
        );
        // Invalid tokens ignored; free text stays free text.
        assert!(
            mk("forget id:^mabc123 (bad chars)")
                .named_entry_ids()
                .is_empty()
        );
        assert!(mk("forget the seattle stuff").named_entry_ids().is_empty());
        assert!(!mk("id:^mabc234").is_free_text_forget());
    }

    #[test]
    fn should_roundtrip_pending_state_when_rewritten() {
        let note = parse_note(&note_path("n"), HOST_FORGET).unwrap();
        let candidates = vec![PendingCandidate {
            entry_id: "^mabc234".into(),
            content_hash: "deadbeef".into(),
            interim_archived: true,
        }];
        let expires = DateTime::parse_from_rfc3339("2026-07-08T10:00:00+00:00").unwrap();
        let rendered = note.render_pending(&candidates, &expires);

        let reparsed = parse_note(&note_path("n"), &rendered).unwrap();
        assert!(reparsed.is_pending());
        assert_eq!(reparsed.candidates.as_deref(), Some(candidates.as_slice()));
        assert_eq!(reparsed.expires_at, Some(expires));
        // Host-written fields and body survive the rewrite verbatim.
        assert_eq!(reparsed.origin, NoteOrigin::Host);
        assert!(reparsed.sensitive);
        assert_eq!(reparsed.content, note.content);

        // Recompute path: rewriting again replaces, not duplicates.
        let rendered2 = reparsed.render_pending(&candidates, &expires);
        assert_eq!(rendered2.matches("candidates:").count(), 1);
        assert_eq!(rendered2.matches("expires_at:").count(), 1);
    }

    #[test]
    fn should_reject_note_when_partial_pending_state() {
        let raw = "---\norigin: host\nkind: forget\ncreated_at: 2026-07-01T10:00:00Z\n\
                   expires_at: 2026-07-08T10:00:00Z\n---\n\nx\n";
        assert!(parse_note(&note_path("x"), raw).is_err());
    }

    const EXTRACT: &str = "---\n\
        session_key: \"web:9\"\n\
        extracted_at: 2026-07-06T08:00:00+00:00\n\
        model: \"gpt-x\"\n\
        ---\n\
        {\"items\":[\
          {\"kind\":\"fact\",\"content\":\"uses vim\",\"evidence_kind\":\"user_said\",\"evidence_idx\":[3,7],\"date\":\"2026-07-06\"},\
          {\"kind\":\"landmine\",\"content\":\"never email bob\",\"evidence_kind\":\"assistant_claimed\",\"evidence_idx\":[9],\"date\":\"2026-07-06\"}\
        ]}\n";

    #[test]
    fn should_parse_extraction_when_fixed_format_given() {
        let path = PathBuf::from("/staging/extract/0198b-session.md");
        let extract = parse_extraction(&path, EXTRACT).unwrap();
        // Legacy stems ("uuid-sessionslug") normalize to the uuid prefix:
        // the slug is untrusted channel metadata and item ids render in
        // the merge prompt (codex round-4 P2).
        assert_eq!(extract.id, "0198b");
        assert_eq!(extract.model, "gpt-x");
        assert_eq!(extract.session_key.as_deref(), Some("web:9"));
        assert_eq!(extract.items.len(), 2);
        assert_eq!(extract.items[0].id, "0198b#0");
        assert_eq!(extract.items[0].evidence_kind, EvidenceKind::UserSaid);
        assert_eq!(extract.items[0].evidence_idx, [3, 7]);
        assert_eq!(extract.items[1].id, "0198b#1");
        assert_eq!(
            extract.items[1].evidence_kind,
            EvidenceKind::AssistantClaimed
        );
    }

    #[test]
    fn should_reject_extraction_when_body_or_metadata_invalid() {
        let path = PathBuf::from("/staging/extract/x.md");
        let bad_body = "---\nextracted_at: 2026-07-06T08:00:00Z\nmodel: \"m\"\n---\nnot json\n";
        assert!(parse_extraction(&path, bad_body).is_err());
        let bad_evidence = "---\nextracted_at: 2026-07-06T08:00:00Z\nmodel: \"m\"\n---\n\
            {\"items\":[{\"kind\":\"fact\",\"content\":\"x\",\"evidence_kind\":\"model_guessed\",\"evidence_idx\":[]}]}\n";
        assert!(parse_extraction(&path, bad_evidence).is_err());
        let no_model = "---\nextracted_at: 2026-07-06T08:00:00Z\n---\n{\"items\":[]}\n";
        assert!(parse_extraction(&path, no_model).is_err());
        let bad_item_kind = "---\nextracted_at: 2026-07-06T08:00:00Z\nmodel: \"m\"\n---\n\
            {\"items\":[{\"kind\":\"opinion\",\"content\":\"x\",\"evidence_kind\":\"user_said\",\"evidence_idx\":[]}]}\n";
        assert!(parse_extraction(&path, bad_item_kind).is_err());
    }

    #[test]
    fn should_protect_unparseable_host_notes_when_sniffing() {
        assert!(raw_is_protected("---\norigin: host\nkind: forget\nGARBAGE"));
        assert!(raw_is_protected(
            "---\norigin: model\nkind: user_request\nGARBAGE"
        ));
        assert!(!raw_is_protected("---\norigin: model\nkind: fact\nGARBAGE"));
    }

    #[test]
    fn should_partition_staging_when_loading_directory() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let notes = memory_dir.join("staging/notes");
        let extract = memory_dir.join("staging/extract");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::create_dir_all(&extract).unwrap();

        std::fs::write(notes.join("01-a-fact.md"),
            "---\norigin: model\nkind: fact\ncreated_at: 2026-07-01T10:00:00Z\n---\n\nprefers tea\n").unwrap();
        // Pending note (already has candidates + expires_at).
        std::fs::write(notes.join("02-b-forget.md"),
            "---\norigin: host\nkind: forget\ncreated_at: 2026-07-01T10:00:00Z\n\
             candidates: [{\"entry_id\":\"^mabc234\",\"content_hash\":\"aa\",\"interim_archived\":false}]\n\
             expires_at: 2026-07-08T10:00:00Z\n---\n\nold address\n").unwrap();
        // Corrupted host note → parse failure, protected.
        std::fs::write(
            notes.join("03-c-broken.md"),
            "---\norigin: host\nkind: forget\n",
        )
        .unwrap();
        std::fs::write(extract.join("04-d-sess.md"), EXTRACT).unwrap();
        // Non-md files are ignored.
        std::fs::write(notes.join(".consolidate_failures.json"), "{}").unwrap();

        let batch = load_staging(memory_dir).unwrap();
        assert_eq!(batch.notes.len(), 1);
        assert_eq!(batch.pending.len(), 1);
        assert_eq!(batch.extractions.len(), 1);
        assert_eq!(batch.parse_failures.len(), 1);
        assert!(
            batch.parse_failures[0].2,
            "corrupted host note is protected"
        );
        assert!(!batch.is_clean());

        let empty = tempfile::tempdir().unwrap();
        assert!(load_staging(empty.path()).unwrap().is_clean());
    }
}
