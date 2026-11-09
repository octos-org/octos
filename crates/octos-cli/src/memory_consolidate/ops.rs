//! Model output schema + machine-enforced authority gates.
//!
//! `parse_model_output` handles SHAPE (strict JSON / schema — the only
//! failure class that earns the single corrective re-ask). `validate`
//! handles SEMANTICS: id resolution, authority, freezing, INIT 0-loss, the
//! entry-loss guard and consumed/dropped completeness. Every gate trusts
//! only host-written metadata (note frontmatter, extraction
//! `evidence_kind`); nothing the model claims is ever sufficient on its own.
//! Any semantic violation rejects the WHOLE merge — no partial apply.

use std::collections::{HashMap, HashSet};

use chrono::NaiveDate;
use serde::Deserialize;

use super::entry::{Entry, UpdatedStamp};
use super::staging::{EvidenceKind, ExtractionItem, NoteFile, NoteKind, NoteOrigin};

/// One raw op proposed by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Add {
        section: Option<String>,
        text: String,
        sources: Vec<String>,
    },
    Update {
        id: String,
        new_text: String,
        sources: Vec<String>,
    },
    Supersede {
        id: String,
        replacement: Option<String>,
        reason: String,
        sources: Vec<String>,
    },
    Archive {
        id: String,
        reason: String,
        sources: Vec<String>,
    },
    HardDelete {
        id: String,
        authorized_by: String,
    },
}

/// A staging item the model dropped, with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    pub id: String,
    pub reason: String,
}

/// Model-suggested extra candidates for a pending note (may only ADD
/// candidates that also pass the Rust-side binding check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSuggestion {
    pub note_id: String,
    pub entry_ids: Vec<String>,
}

/// Parsed model reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOutput {
    pub ops: Vec<Op>,
    pub consumed_ids: Vec<String>,
    pub dropped: Vec<Dropped>,
    pub pending: Vec<PendingSuggestion>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    ops: Vec<serde_json::Value>,
    #[serde(default)]
    consumed_ids: Vec<String>,
    #[serde(default)]
    dropped: Vec<RawDropped>,
    #[serde(default)]
    pending: Vec<RawPending>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDropped {
    id: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPending {
    note_id: String,
    #[serde(default)]
    entry_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAdd {
    #[serde(default)]
    section: Option<String>,
    text: String,
    #[serde(default)]
    sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUpdate {
    id: String,
    new_text: String,
    #[serde(default)]
    sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSupersede {
    id: String,
    #[serde(default)]
    replacement: Option<String>,
    reason: String,
    #[serde(default)]
    sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArchive {
    id: String,
    reason: String,
    #[serde(default)]
    sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHardDelete {
    id: String,
    authorized_by: String,
}

/// Strip one layer of markdown code fences if the reply is fenced.
fn strip_code_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    let rest = rest.trim_start_matches(['\r', '\n']);
    rest.strip_suffix("```").map(str::trim_end).unwrap_or(rest)
}

/// Parse the model reply into a [`ModelOutput`]. Errors are `String`s meant
/// to be echoed back in the single corrective re-ask.
pub fn parse_model_output(raw: &str) -> Result<ModelOutput, String> {
    let cleaned = strip_code_fences(raw);
    let parsed: RawOutput = serde_json::from_str(cleaned)
        .map_err(|e| format!("reply is not the required JSON object: {e}"))?;

    let mut ops = Vec::with_capacity(parsed.ops.len());
    for (i, value) in parsed.ops.into_iter().enumerate() {
        let Some(obj) = value.as_object() else {
            return Err(format!("ops[{i}] must be a JSON object"));
        };
        let mut obj = obj.clone();
        let tag = match obj.remove("op") {
            Some(serde_json::Value::String(s)) => s,
            _ => return Err(format!("ops[{i}] is missing the string 'op' tag")),
        };
        let rest = serde_json::Value::Object(obj);
        let op = match tag.as_str() {
            "add" => serde_json::from_value::<RawAdd>(rest)
                .map(|r| Op::Add {
                    section: r.section,
                    text: r.text,
                    sources: r.sources,
                })
                .map_err(|e| format!("ops[{i}] (add): {e}"))?,
            "update" => serde_json::from_value::<RawUpdate>(rest)
                .map(|r| Op::Update {
                    id: r.id,
                    new_text: r.new_text,
                    sources: r.sources,
                })
                .map_err(|e| format!("ops[{i}] (update): {e}"))?,
            "supersede" => serde_json::from_value::<RawSupersede>(rest)
                .map(|r| Op::Supersede {
                    id: r.id,
                    replacement: r.replacement,
                    reason: r.reason,
                    sources: r.sources,
                })
                .map_err(|e| format!("ops[{i}] (supersede): {e}"))?,
            "archive" => serde_json::from_value::<RawArchive>(rest)
                .map(|r| Op::Archive {
                    id: r.id,
                    reason: r.reason,
                    sources: r.sources,
                })
                .map_err(|e| format!("ops[{i}] (archive): {e}"))?,
            "hard_delete" => serde_json::from_value::<RawHardDelete>(rest)
                .map(|r| Op::HardDelete {
                    id: r.id,
                    authorized_by: r.authorized_by,
                })
                .map_err(|e| format!("ops[{i}] (hard_delete): {e}"))?,
            other => return Err(format!("ops[{i}] has unknown op kind '{other}'")),
        };
        ops.push(op);
    }

    Ok(ModelOutput {
        ops,
        consumed_ids: parsed.consumed_ids,
        dropped: parsed
            .dropped
            .into_iter()
            .map(|d| Dropped {
                id: d.id,
                reason: d.reason,
            })
            .collect(),
        pending: parsed
            .pending
            .into_iter()
            .map(|p| PendingSuggestion {
                note_id: p.note_id,
                entry_ids: p.entry_ids,
            })
            .collect(),
    })
}

/// Everything `validate` needs to judge a [`ModelOutput`].
pub struct ValidationCtx<'a> {
    /// Current MEMORY.md entries (post-INIT if this run performed INIT).
    pub entries: &'a [Entry],
    /// Interim-archived pending candidates: entry id → content hash. Valid
    /// hard-delete targets even though they are not in `entries`.
    pub interim: &'a HashMap<String, String>,
    /// Entry ids frozen by unresolved pending notes.
    pub frozen: &'a HashSet<String>,
    /// Batch notes by id (pending notes are NOT here — not consumable).
    pub notes: &'a HashMap<String, &'a NoteFile>,
    /// Extraction items by item id (`<stem>#<idx>`).
    pub items: &'a HashMap<String, &'a ExtractionItem>,
    /// Usage stats by entry id / bank slug (#1586). A recently-used entry
    /// is kept alive against age-based auto-archive.
    pub usage: &'a std::collections::BTreeMap<String, octos_memory::UsageStat>,
    /// This run performed INIT — only `add` ops are allowed (0-loss).
    pub init_mode: bool,
    pub today: NaiveDate,
    pub unused_days: u32,
}

/// A validated op with sanitized text, ready to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckedOp {
    Add {
        text: String,
        unverified: bool,
    },
    Update {
        id: String,
        new_text: String,
    },
    Supersede {
        id: String,
        replacement: Option<String>,
        reason: String,
    },
    Archive {
        id: String,
        reason: String,
    },
    HardDelete {
        id: String,
        authorized_by: String,
    },
}

/// The validated plan (ops in model order, semantics enforced).
#[derive(Debug, Clone, Default)]
pub struct ValidatedOps {
    pub ops: Vec<CheckedOp>,
    pub dropped: Vec<Dropped>,
    pub pending_suggestions: Vec<PendingSuggestion>,
}

impl<'a> ValidationCtx<'a> {
    fn entry(&self, id: &str) -> Option<&'a Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// A source qualifies as validated authority when it is a host note or
    /// an extraction item whose host-computed evidence is `user_said` /
    /// `tool_showed`. Model notes and `assistant_claimed` items never do —
    /// and neither does a free-text host forget note: it is parked as
    /// pending-confirm and carries no authority until the user confirms.
    fn source_qualifies(&self, source: &str) -> bool {
        if let Some(note) = self.notes.get(source) {
            // Forget notes carry hard-delete-only authority over their
            // NAMED ids (checked at the hard_delete gate); they must never
            // qualify as edit authority for update/supersede/archive — a
            // bad reply could otherwise cite a delete confirmation to
            // rewrite unrelated entries.
            return note.origin == NoteOrigin::Host && note.kind != NoteKind::Forget;
        }
        if let Some(item) = self.items.get(source) {
            return matches!(
                item.evidence_kind,
                EvidenceKind::UserSaid | EvidenceKind::ToolShowed
            );
        }
        false
    }

    fn source_known(&self, source: &str) -> bool {
        self.notes.contains_key(source) || self.items.contains_key(source)
    }

    /// Whether `entry` was cited via `record_memory_use` within the
    /// `unused_days` window — a keep-alive signal (#1586).
    fn recently_used(&self, entry: &Entry) -> bool {
        self.usage
            .get(&entry.id)
            .filter(|stat| !stat.last_used.is_empty())
            .and_then(|stat| NaiveDate::parse_from_str(&stat.last_used, "%Y-%m-%d").ok())
            .is_some_and(|used| (self.today - used).num_days() <= i64::from(self.unused_days))
    }

    /// Age-based auto-archive eligibility: REAL `(updated:)` stamp strictly
    /// older than `unused_days`; `unknown` NEVER auto-archives.
    fn age_qualifies(&self, entry: &Entry) -> bool {
        // A recently-USED entry is kept alive even when its `(updated:)`
        // stamp is old — usage feedback extends the life of memories that
        // keep proving useful (#1586). Explicit source-cited archive is
        // unaffected; only the age path is gated.
        if self.recently_used(entry) {
            return false;
        }
        match entry.updated_stamp() {
            Some(UpdatedStamp::Real(date)) => {
                (self.today - date).num_days() > i64::from(self.unused_days)
            }
            Some(UpdatedStamp::Unknown) | None => false,
        }
    }
}

/// Sanitize model-provided entry text: normalize line ends, drop blank lines
/// (an entry is ONE block — a blank line would corrupt the file structure).
/// Errors on effectively-empty text.
fn sanitize_text(kind: &str, text: &str) -> Result<String, String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return Err(format!("{kind} text is empty"));
    }
    // Defense-in-depth for the write-time threat gate (#1585): ingress
    // scanning covers staged notes and extraction items, but the
    // consolidation model SYNTHESIZES entry text — a hostile note that
    // slipped a rephrase past ingress must still not reach MEMORY.md.
    // Every entry-text path (add/update) funnels through here.
    if let Some(threat) = octos_memory::guard::first_threat(text) {
        return Err(format!(
            "{kind} text rejected by the memory content guard ({threat}); \
             drop this content or restate it as a plain fact"
        ));
    }
    Ok(lines.join("\n"))
}

/// Strip a trailing echo of the target entry's own id token (models often
/// copy the id back since they see it in the file). Only the entry's OWN id
/// is stripped; the canonical token is re-appended at render time.
fn strip_id_echo(text: &str, id: &str) -> String {
    let trimmed = text.trim_end();
    if let Some(stripped) = trimmed.strip_suffix(id) {
        stripped.trim_end().to_string()
    } else {
        trimmed.to_string()
    }
}

/// Append engine-enforced markers and the id token to sanitized entry text:
/// `(unverified)` when required, `(updated: <today>)` when the model did not
/// stamp one itself, and the immutable id token last.
pub fn finalize_entry_text(text: &str, unverified: bool, today: NaiveDate, id: &str) -> String {
    let mut out = text.trim_end().to_string();
    if unverified && !out.contains("(unverified)") {
        out.push_str(" (unverified)");
    }
    if !out.contains("(updated:") {
        out.push_str(&format!(" (updated: {})", today.format("%Y-%m-%d")));
    }
    out.push_str(&format!(" {id}"));
    out
}

/// Validate a parsed model output against the authority gates. Any failure
/// rejects the whole merge — the error string is surfaced, never re-asked.
pub fn validate(output: &ModelOutput, ctx: &ValidationCtx) -> Result<ValidatedOps, String> {
    let mut checked: Vec<CheckedOp> = Vec::with_capacity(output.ops.len());
    let mut targeted: HashSet<&str> = HashSet::new();
    let mut removed_from_memory = 0usize;

    for (i, op) in output.ops.iter().enumerate() {
        // INIT is 0-loss: nothing but adds may touch a restructured file.
        if ctx.init_mode && !matches!(op, Op::Add { .. }) {
            return Err(format!(
                "ops[{i}]: INIT runs are 0-loss — only 'add' ops are allowed"
            ));
        }

        // Common source checks.
        let sources: &[String] = match op {
            Op::Add { sources, .. }
            | Op::Update { sources, .. }
            | Op::Supersede { sources, .. }
            | Op::Archive { sources, .. } => sources,
            Op::HardDelete { .. } => &[],
        };
        for src in sources {
            if !ctx.source_known(src) {
                return Err(format!(
                    "ops[{i}]: unknown source id '{src}' (not a batch note or extraction item)"
                ));
            }
        }

        match op {
            Op::Add { text, sources, .. } => {
                let text = sanitize_text("add", text)?;
                // A forget note may NEVER feed an add: a bad reply could
                // otherwise write the very content the user asked to forget
                // back into MEMORY.md while the request stays pending.
                for src in sources {
                    if ctx
                        .notes
                        .get(src)
                        .is_some_and(|n| n.kind == NoteKind::Forget)
                    {
                        return Err(format!(
                            "ops[{i}]: add cites forget note '{src}' — forget requests \
                             cannot source new memory"
                        ));
                    }
                }
                // Adds are otherwise always allowed, but weak sourcing
                // (model notes / assistant_claimed only, or no sources at
                // all) must be visible in the rendered entry.
                let unverified = !sources.iter().any(|s| ctx.source_qualifies(s));
                checked.push(CheckedOp::Add { text, unverified });
            }
            Op::Update {
                id,
                new_text,
                sources,
            } => {
                let entry = ctx
                    .entry(id)
                    .ok_or_else(|| format!("ops[{i}]: unknown entry id '{id}'"))?;
                if !targeted.insert(&entry.id) {
                    return Err(format!("ops[{i}]: duplicate op targeting '{id}'"));
                }
                if ctx.frozen.contains(id) {
                    return Err(format!(
                        "ops[{i}]: entry '{id}' is frozen by an unresolved pending forget note"
                    ));
                }
                if !sources.iter().any(|s| ctx.source_qualifies(s)) {
                    return Err(format!(
                        "ops[{i}]: update of '{id}' lacks validated authority (needs a host \
                         note or user_said/tool_showed evidence)"
                    ));
                }
                let text = sanitize_text("update", &strip_id_echo(new_text, id))?;
                checked.push(CheckedOp::Update {
                    id: id.clone(),
                    new_text: text,
                });
            }
            Op::Supersede {
                id,
                replacement,
                reason,
                sources,
            } => {
                let entry = ctx
                    .entry(id)
                    .ok_or_else(|| format!("ops[{i}]: unknown entry id '{id}'"))?;
                if !targeted.insert(&entry.id) {
                    return Err(format!("ops[{i}]: duplicate op targeting '{id}'"));
                }
                if ctx.frozen.contains(id) {
                    return Err(format!(
                        "ops[{i}]: entry '{id}' is frozen by an unresolved pending forget note"
                    ));
                }
                if !sources.iter().any(|s| ctx.source_qualifies(s)) {
                    return Err(format!(
                        "ops[{i}]: supersede of '{id}' lacks validated authority (needs a host \
                         note or user_said/tool_showed evidence)"
                    ));
                }
                let replacement = replacement
                    .as_deref()
                    .map(|r| sanitize_text("supersede replacement", &strip_id_echo(r, id)))
                    .transpose()?;
                if replacement.is_none() {
                    removed_from_memory += 1;
                }
                checked.push(CheckedOp::Supersede {
                    id: id.clone(),
                    replacement,
                    reason: reason.clone(),
                });
            }
            Op::Archive {
                id,
                reason,
                sources,
            } => {
                let entry = ctx
                    .entry(id)
                    .ok_or_else(|| format!("ops[{i}]: unknown entry id '{id}'"))?;
                if !targeted.insert(&entry.id) {
                    return Err(format!("ops[{i}]: duplicate op targeting '{id}'"));
                }
                if ctx.frozen.contains(id) {
                    return Err(format!(
                        "ops[{i}]: entry '{id}' is frozen by an unresolved pending forget note"
                    ));
                }
                let source_ok = sources.iter().any(|s| ctx.source_qualifies(s));
                if !source_ok && !ctx.age_qualifies(entry) {
                    return Err(format!(
                        "ops[{i}]: archive of '{id}' lacks authority — needs validated \
                         sources or a real (updated:) stamp older than {} days",
                        ctx.unused_days
                    ));
                }
                removed_from_memory += 1;
                checked.push(CheckedOp::Archive {
                    id: id.clone(),
                    reason: reason.clone(),
                });
            }
            Op::HardDelete { id, authorized_by } => {
                let in_memory = ctx.entry(id).is_some();
                if !in_memory && !ctx.interim.contains_key(id) {
                    return Err(format!("ops[{i}]: unknown entry id '{id}'"));
                }
                // Interim-archived targets are tracked too: two ops on one
                // id are a conflict wherever the entry lives.
                if !targeted.insert(id.as_str()) {
                    return Err(format!("ops[{i}]: duplicate op targeting '{id}'"));
                }
                // Machine-enforced hard-delete gate: the authorizing note
                // must be a HOST-authored forget note in THIS batch whose
                // content names exactly this entry id. The model's claim is
                // never sufficient.
                let Some(note) = ctx.notes.get(authorized_by) else {
                    return Err(format!(
                        "ops[{i}]: hard_delete of '{id}' cites unknown authorizing note \
                         '{authorized_by}'"
                    ));
                };
                if note.origin != NoteOrigin::Host || note.kind != NoteKind::Forget {
                    return Err(format!(
                        "ops[{i}]: hard_delete of '{id}' not authorized — '{authorized_by}' \
                         is not a host forget note"
                    ));
                }
                if !note.named_entry_ids().iter().any(|n| n == id) {
                    return Err(format!(
                        "ops[{i}]: hard_delete of '{id}' not authorized — note \
                         '{authorized_by}' does not name id:{id}"
                    ));
                }
                // NOTE: a valid id-bound hard_delete is exempt from freezing
                // — it IS the confirmation path for pending candidates.
                if in_memory {
                    removed_from_memory += 1;
                }
                checked.push(CheckedOp::HardDelete {
                    id: id.clone(),
                    authorized_by: authorized_by.clone(),
                });
            }
        }
    }

    // Entry-loss guard: ops may not remove >50% of entries unless the file
    // is tiny (previous count ≤ 6). INIT already enforced 0 removals above.
    let prev = ctx.entries.len();
    if prev > 6 && removed_from_memory * 2 > prev {
        return Err(format!(
            "entry-loss guard: ops remove {removed_from_memory} of {prev} entries (>50%)"
        ));
    }

    // Add-back guard: a bad or prompt-injected reply must not survive a
    // hard delete by copying the deleted entry's text into an add/update/
    // supersede under a new id. Any folded line of new text matching a
    // hard-deleted target's folded lines rejects the whole merge.
    // Compare CONTENT with bookkeeping stripped: the deleted line carries
    // its `(updated:)` stamp and `^m` id while a re-added copy would not.
    // Strip every `^m<6 of [a-z2-7]>` token: the prompt shows entry ids,
    // so a verbatim-copied line can carry the OLD token — folding it
    // un-stripped would defeat the equality check.
    fn strip_all_id_tokens(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'^'
                && i + 7 < bytes.len() + 1
                && bytes.get(i + 1) == Some(&b'm')
                && bytes[i + 2..]
                    .iter()
                    .take(6)
                    .filter(|c| matches!(c, b'a'..=b'z' | b'2'..=b'7'))
                    .count()
                    == 6
            {
                i += 8;
                continue;
            }
            // Safe: we only skip full ASCII token bytes above.
            let ch_len = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&text[i..i + ch_len]);
            i += ch_len;
        }
        out
    }
    let strip_fold = |id: &str, text: &str| -> Vec<String> {
        let entry = super::entry::Entry {
            id: id.to_string(),
            text: text.to_string(),
        };
        strip_all_id_tokens(&super::pending::strippable_entry_text(&entry))
            .lines()
            .map(super::entry::fold_whitespace)
            .filter(|l| !l.is_empty())
            .collect()
    };
    let mut deleted_lines: HashSet<String> = HashSet::new();
    for op in &checked {
        if let CheckedOp::HardDelete { id, .. } = op {
            if let Some(entry) = ctx.entry(id) {
                deleted_lines.extend(strip_fold(&entry.id, &entry.text));
            } else if let Some(text) = ctx.interim.get(id) {
                deleted_lines.extend(strip_fold(id, text));
            }
        }
    }
    if !deleted_lines.is_empty() {
        for (i, op) in checked.iter().enumerate() {
            let new_text: Option<&str> = match op {
                CheckedOp::Add { text, .. } => Some(text),
                CheckedOp::Update { new_text, .. } => Some(new_text),
                CheckedOp::Supersede { replacement, .. } => replacement.as_deref(),
                _ => None,
            };
            let Some(new_text) = new_text else { continue };
            for folded in strip_fold("", new_text) {
                if deleted_lines.contains(&folded) {
                    return Err(format!(
                        "ops[{i}]: new text repeats a line of a hard-deleted entry — \
                         deleted content may not be re-added in the same merge"
                    ));
                }
            }
        }
    }

    validate_consumption(output, ctx)?;

    Ok(ValidatedOps {
        ops: checked,
        dropped: output.dropped.clone(),
        pending_suggestions: output.pending.clone(),
    })
}

/// Consumed/dropped completeness: every staging item is consumed or dropped
/// with a reason; host asks can never be silently dropped; free-text host
/// forget notes must stay pending.
fn validate_consumption(output: &ModelOutput, ctx: &ValidationCtx) -> Result<(), String> {
    let mut consumed: HashSet<&str> = HashSet::new();
    for id in &output.consumed_ids {
        if !ctx.source_known(id) {
            return Err(format!("consumed_ids contains unknown id '{id}'"));
        }
        if !consumed.insert(id.as_str()) {
            return Err(format!("consumed_ids contains duplicate id '{id}'"));
        }
    }
    let mut dropped: HashSet<&str> = HashSet::new();
    for d in &output.dropped {
        if !ctx.source_known(&d.id) {
            return Err(format!("dropped contains unknown id '{}'", d.id));
        }
        if consumed.contains(d.id.as_str()) {
            return Err(format!("id '{}' is both consumed and dropped", d.id));
        }
        if !dropped.insert(d.id.as_str()) {
            return Err(format!("dropped contains duplicate id '{}'", d.id));
        }
    }

    for (id, note) in ctx.notes.iter() {
        let id = id.as_str();
        if note.is_free_text_forget() {
            // Free-text forget notes go pending — consuming or dropping one
            // would fake a resolution that never happened.
            if consumed.contains(id) || dropped.contains(id) {
                return Err(format!(
                    "free-text host forget note '{id}' must be left pending, not \
                     consumed/dropped"
                ));
            }
            continue;
        }
        // HOST-authored notes are the hard-protected class. A MODEL-captured
        // user_request (memory_note tool) is durable against quarantine but
        // must stay consumable: it carries no delete authority, so a
        // forget-phrased ask could otherwise never be validly consumed and
        // would spin the fast lane forever. Dropping it requires a reason,
        // which the outcome surfaces (the host CLI is the delete path).
        let is_protected = note.origin == NoteOrigin::Host;
        if is_protected {
            if !consumed.contains(id) {
                return Err(format!(
                    "host/user_request note '{id}' was not consumed — a user ask may never \
                     be silently dropped"
                ));
            }
            // Consumption must be BACKED by an accepted op: `ops: []` with
            // the note in consumed_ids would delete the request file while
            // changing nothing — silently dropping an explicit user ask.
            let applied = output.ops.iter().any(|op| match op {
                Op::Add { sources, .. } => sources.iter().any(|s| s == id),
                Op::Update { sources, .. } => sources.iter().any(|s| s == id),
                Op::Supersede { sources, .. } => sources.iter().any(|s| s == id),
                Op::Archive { sources, .. } => sources.iter().any(|s| s == id),
                // A hard_delete covering an id this forget note names
                // applies it too (two notes can name the same id; one op
                // honors both — only one can be the cited authorizer).
                Op::HardDelete {
                    id: target,
                    authorized_by,
                } => {
                    authorized_by == id
                        || (note.kind == NoteKind::Forget
                            && note.named_entry_ids().iter().any(|n| n == target))
                }
            });
            if !applied {
                return Err(format!(
                    "host/user_request note '{id}' is consumed but no op cites it — \
                     a user ask may not be consumed without being applied"
                ));
            }
        } else if !consumed.contains(id) && !dropped.contains(id) {
            return Err(format!(
                "staging note '{id}' is neither consumed nor dropped-with-reason"
            ));
        }

        // Id-bound host forget notes must be honored by matching hard_delete
        // ops — consuming the note without acting on it drops the ask.
        // Two notes can name the SAME id (the user asked twice): one
        // hard_delete honors both — requiring each note to be the cited
        // authorizer would make the pair unsatisfiable (duplicate-target
        // ops are rejected) and wedge consolidation.
        if note.origin == NoteOrigin::Host && note.kind == NoteKind::Forget {
            for named in note.named_entry_ids() {
                // Ids that exist in neither MEMORY.md nor interim were
                // already handled Rust-side (archived-only scrub /
                // satisfied recovery) — the model cannot and need not
                // hard_delete them.
                if ctx.entry(&named).is_none() && !ctx.interim.contains_key(&named) {
                    continue;
                }
                let honored = output.ops.iter().any(|op| {
                    matches!(op, Op::HardDelete { id: target, authorized_by: auth }
                    if *target == named
                        && ctx.notes.get(auth.as_str()).is_some_and(|a| {
                            a.origin == NoteOrigin::Host
                                && a.kind == NoteKind::Forget
                                && a.named_entry_ids().contains(&named)
                        }))
                });
                if !honored {
                    return Err(format!(
                        "id-bound forget note '{id}' names {named} but no hard_delete op \
                         honors it"
                    ));
                }
            }
        }
    }

    for id in ctx.items.keys() {
        if !consumed.contains(id.as_str()) && !dropped.contains(id.as_str()) {
            return Err(format!(
                "extraction item '{id}' is neither consumed nor dropped-with-reason"
            ));
        }
    }

    Ok(())
}
